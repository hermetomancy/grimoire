# Grimoire TODO

The canonical remaining-work list (AGENTS.md §15.2). Completed work lives in git history,
not here. When everything below is done and reflected in the design doc, delete this file.

## In progress / next

- **Verify the toolchain cleanup + build-scratch hardening on a musl host**
- **Generalize / harden**: extend `rust-stage0.rn` to host-gate and fetch
  the correct Tier 2 host-tools tarball for every supported target (aarch64-musl, x86_64-musl,
  freebsd-x86_64, freebsd-aarch64), not just gnu vs. musl on Linux aarch64; the x86_64-musl seed
  currently isn't host-gated because the static release runs anywhere, but the others must select
  the right upstream tarball. Then consider a `host_libc` unit test via the `GRM_HOST_LIBC`
  override.

## Remaining before a stable release

### Release engineering

- **`CHANGELOG.md`** already tracks an `Unreleased` section; at the first tag, promote it
  to a versioned heading (the `release.yml` preflight already enforces tag == Cargo.toml).
  Editorial promotion only — no new content owed.
- **tome-build.yml** is a release-blocking tag gate (`grm tome build --all --strict` per
  platform on every `v*` tag). It fails on any platform where Bootstrap stage 1 has not
  landed — the gate refusing to bless a release whose core does not build. Remaining: close
  stage 1 on the four unblocked targets, and add the musl/FreeBSD matrix rows (containers/VMs)
  as those bootstraps land.

### Bootstrap stage 1: core on all targets

Build the core runes (`linux-headers`, `musl`, `llvm` (+`clang` split member with
compiler-rt runtimes inside), `gmake`, the userland floor (`uutils`/`dash`/`mawk`/`gsed`/`ggrep`),
`toolchain-wrappers`) from source on every supported target. macOS aarch64 closed the
dogfood loop 2026-06-12 (full chain through rust 1.96 and grimoire itself). All six remaining
targets are supported by official Rust host tools (Tier 2 with host tools via rustup), so the
stage-1 loop can close natively once `rust-stage0.rn`/`rust.rn` fetch the correct per-host
tarball. The remaining work is native build + per-rune debugging.

Unblocked (native build + per-rune debugging):

- ⏳ Linux x86_64 glibc
- ⏳ Linux aarch64 glibc
- ⏳ Linux x86_64 musl
- ⏳ Linux aarch64 musl — needs `rust-stage0.rn` updated to fetch the official
  `aarch64-unknown-linux-musl` host tools (currently cross-seeds from the gnu tarball on a glibc
  host). Already proven on a glibc-hosted musl cross build.
- ⏳ FreeBSD x86_64
- ⏳ FreeBSD aarch64 — needs `rust-stage0.rn` updated to fetch the official
  `aarch64-unknown-freebsd` host tools.

### Bootstrap stage 2: full self-hosting

The toolchain is already self-hosted (every build driver — cmake, python3, gmake, the
compiler boundary — is a core package; the host compiler seeds bootstrap only and
`host_tool_dirs()` drops out once toolchain-wrappers exists). What remains is the
ambient userland.

The host *userland* link is shadowed, never severed: `/usr/bin` + `/bin` are
unconditionally appended to managed build PATH (`posix_ambient_dirs`), so anything the floor
does not ship (perl, m4, bash-isms) falls through to the host silently — an unhashed
build input. Path to severance is empirical: a
hermetic build mode that drops the ambient tail, run per rune to enumerate what actually
leaks; passing runes are certified floor-ambient, failures name what stage 2 must package.
The `grm tome build --hermetic` diagnostic has shipped: it drops the ambient tail so a build
that reaches for an unpackaged tool fails and names the leak. The remaining stage-2 work is
empirical — run it per rune to enumerate what actually leaks, certify the passing runes as
floor-ambient, and package the failures. One deferred deliverable:

- **Fold a floor-ambient marker into `build_env_id` (deferred; needs a decision).** The
  cheap interim fix if prebuilts ever come from heterogeneous builders, but blocked by an
  architecture mismatch: `build_env_id` is a process-global cached pure fn with no per-build
  inputs while `--hermetic` is per-build, and the marker must reach all three address
  consumers (resolver `plan.rs`, closure walker `closure.rs`, install state `realize.rs`)
  identically or §9.8 breaks. Decide between a host-property marker and plumbing per-build
  env identity end-to-end before writing it.

### Build-env review follow-ups (2026-06-24 bootstrap/ethos review)

- **`toolchain-wrappers.rn:75` gnu crash (one-liner).** `driver_flags` is gated on `is_macos`;
  its `else` dereferences `$env.MUSL_PREFIX`, which is unset for `-gnu` (musl is
  `linux-*-musl`-filtered). Gate on `is_musl` (else `""`), matching line 82. Latent until gnu is a
  built target.
- **Musl floor-flip is not in the store address (§9.8 silent-correctness gap).** Pre-floor
  (`-static`/host-glibc) and post-floor (`--sysroot=<musl>`) builds run under the same host-cc
  banner → identical `build_env_id`, identical hash, different bytes. Same shape as the *boundary*
  flip (host cc → managed clang), which **is** addressed via `managed_boundary_id`; this second
  boundary is not. Fix: append installed musl+linux-headers store hashes to `build_env_id` on musl
  targets, or gate `grm build`/`--index`/publish on floor presence and document the limit. Shares
  the architecture blocker described in the deferred floor-ambient marker above (§ Bootstrap
  stage 2): `build_env_id` is a process-global cached pure fn and the marker must reach all three
  address consumers identically.
- **Decide `-gnu`'s status; make the docs honest.** Managed clang is musl-defaulted on every
  non-Darwin target (`llvm.rn:71`) and no floor/SDK analogue is injected for gnu, so it links host
  glibc/libstdc++ unhashed and undeclared. AGENTS §11 says gnu is "supported"; this list / stage 1
  say not-done. Either wire gnu (clang gnu arm + host-glibc fold into `build_env_id` + floor branch
  in `build_env_for_target`) or soften §11 to "recognized cross-target, not self-hostable yet" and
  drop gnu from rune `targets:` (or emit an early clear error instead of the MUSL_PREFIX crash).
- **Doctor macOS SDK probe.** Once the managed boundary is installed, `doctor` never checks the SDK
  exists; a later Xcode removal → clean `health: ok` then a cryptic `stdio.h not found`. Add a
  macOS-only `macos_sdk_path()` line to `check_source_build_readiness`, mirroring the host-compiler
  field.
- **Cross-host substitution caveat (`host_libc` not in the address).** A glibc-cross host and a
  musl-native host compute the same `rust`/`grimoire` address for different bits (rust-stage0 is
  host-keyed but it is a *build* dep, which does not fold into the closure). Fine for today's
  single-host reality; needs a written caveat in AGENTS §9.8 / multi-os-bootstrap.md. Ties into the
  `host_libc` test line under "In progress / next".
- **`musl.rn:39` clang-host assumption.** The `LIBCC` pin runs `cc --rtlib=compiler-rt …` at the
  floor bottom, where `cc` is still the host compiler; a gcc host fails cryptically. Document the
  clang-host requirement in multi-os-bootstrap.md §1, or probe and only add `--rtlib` when `cc` is
  clang.

### Doc drift (2026-06-24 review)

- **toybox → uutils/dash/mawk/gsed/ggrep** in `README.md:134`, `tome-core/README.md:26` & `:74`,
  `CHANGELOG.md:17` ("toybox's coreutils" present-tense inside the section documenting its removal).
  Authoritative sources (CORE_PACKAGES, build-env.rn, rune-authoring.md) are already correct.
- **`multi-os-bootstrap.md:98`** credits `grimoire.rn` with an `if $is_musl` branch it lacks
  (comment only).
- **AGENTS §1.3** lists only `cc`/`xcrun` but the discovery path also `--version`-probes
  `ld`/`as`/`install_name_tool`/`lipo` and execs `xcrun --show-sdk-path` for provisioning — widen
  the wording; the code is compliant.

### Second-pass review follow-ups (2026-06-24 subsystem review)

Pass 2 (solver / splits / transactions / security / runes / FreeBSD + design premises) found no trust
bypass, no crash on a supported path, and no data-loss path. It corroborated the **`build_env_id` musl
floor-flip above as the single highest-priority item** — the process-global `OnceLock` cache is the
shared structural root of the floor-flip, hermetic-marker, and host_libc address gaps, and the
musl floor-hash interim is the cheapest correct fix. New items:

- **doctor is blind to interrupted-restore artifacts.** A torn-rename kill in `restore_state_snapshot`
  leaves `state/.packages-old` / `.packages-staging`; `check_stale_backups` only scans the store +
  cache for `*.grimoire-old`, never `state/`. Recovery is correct (next `grm switch` deletes them) but
  they sit silently and doctor reports clean. The comment at `generations.rs:258` falsely claims
  `.packages-old` is "detectable by grm doctor" — it is not. Fix: add the two dirs to the stale scan
  (or clean unconditionally at activation start) and fix the comment.
- **split `check_symlinks` misses directory-target dangling symlinks.** `pre_partition` records only
  files (`split.rs:282-297`), so a relative symlink to a directory whose contents were partitioned into
  another member ships dangling with no error (`split.rs:324-356`). Latent today (clang's hazard is a
  file target, which is caught). Fix: also record pre-partition dir paths and bail when a resolved
  target matches one.
- **python3-minimal bare `patch` (hygiene).** `python3-minimal.rn:36` runs ambient host `patch` on the
  musl branch — undeclared, no `patch` rune, an unhashed *tool* (the patch file is content-addressed).
  Add a one-line ambient-tool comment, mirroring openssl.rn's perl note.
- **Unbounded decompression + ungated local install (post-trust DoS).** `archive/unpack.rs` /
  `validate.rs` have no decompressed-size / member-count cap and an unbounded `read_to_string` on the
  captured `package.nuon`; gated behind index checksum trust EXCEPT `install_local_root` with
  `sha256: None` (`steps.rs:67`), which decompresses with zero gating. Document the "archives here are
  checksum-trusted" assumption now; wrap the decoder in a byte-counting reader if untrusted/local
  archives ever become first-class.
- **FreeBSD: the middle is missing — fail loudly.** `build_env_for_target` has no freebsd branch (no
  sysroot/SDK/floor) and `llvm.rn`'s host_triple match hard-errors with no freebsd row, so freebsd
  lands one step *before* the gnu MUSL_PREFIX crash (no toolchain at all). All documented-deferred, but
  add an early "freebsd build env not yet wired" bail so the eventual failure is not a cryptic link
  error. Soften AGENTS §11 to mark freebsd (esp. `freebsd-aarch64`, which no rune row satisfies) as
  anticipated, matching multi-os-bootstrap.md.
- **Resolver lost-conflict edge (low).** A linked installed package whose candidate has disappeared
  returns empty `installed_metadata` (`resolver.rs:315`), dropping a conflict it declares toward the
  new install at resolve time. The `refuse_plan_conflicts` gate (`install/mod.rs`) catches it before any
  fetch/build, so safety impact is nil — just note the lost edge.

Test gaps (cheap, named): the torn directory-level rename window (rename `state/packages` → aside,
assert doctor flags divergence and `grm switch` reconstructs from the snapshot); the resolver
lost-conflict edge; a split directory-target dangling-symlink rejection (after the fix above); an
https-only index e2e (non-loopback `http://` repo → resolve fails on the scheme, not a timeout).

## Expansion projects (spec before code)

- **Scoped profiles and dev shells (`grm profile` / `grm shell`).** Named, imperative
  profiles for development against managed libraries — e.g. a `rust-devel` profile where
  `cargo install`'s `openssl-sys` finds the managed OpenSSL instead of host Homebrew.
  Converged design:
  - *Model.* A named profile is the existing profile generalized: its own state (installed
    set, lockfile), generation chain, and switching; the unnamed default is the reserved
    case. The store stays shared (cross-profile sharing is free); GC roots become the union
    over all profiles' retained generations.
  - *Activation = spawn a subshell* (not in-place eval — sidesteps the PATH
    idempotency/restore bookkeeping and the per-shell-dialect codegen entirely). `grm
    profile rust-devel` / `grm shell <pkgs>` exec `$SHELL` with the computed env; `exit`
    leaves; nesting is a literal shell stack; `--pure` clears host discovery vars (additive,
    managed-prepended, otherwise); `-- <cmd>` runs non-interactively for CI. `GRM_PROFILE`
    marks the active one and is the default target for flag-less `grm install`.
  - *Named profiles are a real per-profile prefix.* The generation links not just `bin/` +
    `share/` but also merged `lib/`, `include/`, and `lib/pkgconfig/` symlink forests, and
    the spawned env points at the *stable* `…/profiles/<name>/current/{bin,lib,include,
    lib/pkgconfig,share/man}`. So `grm install … --profile rust-devel` (the default target
    when inside it) flips `current` and the new package is live *immediately* — bins and
    headers/libs/`.pc` alike — with no shell re-exec. The **default** profile stays
    `bin/`+`share/` only: no global `lib/include` prefix (that reintroduces cross-package
    collisions and ambient, non-hermetic builds — rejected under the host-floor rule, §5).
    Collisions within a named profile's forests resolve like contested bins (`grm prefer`).
  - *Bright line.* Rune builds remain hermetic — they never read a profile or ambient state,
    so package reproducibility is unaffected. A binary built *inside* a dev profile is pinned
    to that profile (and, additively, possibly host) — fine for dev tools, not a
    reproducible artifact.
  - *Surface.* `grm profile create|list|rm <name>`; `grm profile <name> [--pure] [-- cmd]`;
    `grm shell <pkgs> [--pure] [-- cmd]`; `grm install <pkg> [--profile <name>]`.
  - *Work.* Per-profile state/generation plumbing across install/remove/upgrade/switch/
    gc/doctor; the stable per-profile-prefix builder + forest collision handling; reuse the
    build env's discovery-var computation (`src/nu/runtime/`) for the forest contents.

## Planned: Grimoire OS

Work that only matters when Grimoire is the operating system's sole package manager — a
Grimoire-based distribution. None of it blocks a stable release of Grimoire as a
standalone/secondary package manager.

### 1. `deps.features` + FHS compat layer

Add a new dependency category `deps.features` for packages that provide execution-time
capabilities rather than direct binaries on PATH.

- `src/model/`: add `features: Vec<Dependency>` to `Deps`, `PackageState`, and
  `IndexEntry`; update `parse_deps` and serialization.
- `src/install/`: resolve and install `features` deps alongside runtime deps (store-only
  is fine).
- `src/profile/`: when linking a package with `features` deps into a generation, create
  wrapper scripts in `gen-N/bin/` instead of symlinking the binaries directly. The
  wrapper invokes `grm fhs-run` with the FHS tree store path and the real binary store
  path.
- New `src/fhs.rs`: implement `grm fhs-run <tree> <binary> [args...]` using
  `unshare(CLONE_NEWNS)` + recursive bind mounts. No external dependencies, no root
  required on normal Linux kernels.
- New `src/cli.rs` subcommand: `grm fhs-run`.

This satisfies the design doc's "make-or-break" foreign-binary compat requirement.

### 2. Boot integration

On a Grimoire-as-primary-distro system, the bootloader should list generations so a
broken kernel or init can roll back at boot time.

- Design a bootloader config fragment (systemd-boot, GRUB, etc.) that points each entry
  at a different generation's kernel + initrd.
- Add a `grm boot-update` command that regenerates the bootloader menu from the
  generation registry.
- Decide GC policy: boot entries are additional GC roots.

### 3. System config (`/etc`) management

The design doc says `/etc` is handled conventionally, like a traditional distro. This is
intentionally lightweight — no full NixOS-style etc overlay — but we still need:

- Document the convention: Grimoire does not manage `/etc`.
- Optional: a minimal `grm etc-track` helper that records which package installed which
  `/etc` file and warns on conflicts. Treat as future work.

### 4. System-level `/usr/bin`

When Grimoire is the sole PM, `/usr/bin` should be a symlink or bind mount pointing to
the active generation's `bin/`. Current user-local `~/.grimoire/profiles/current/bin` is
correct for secondary PM use.

- Add `grm setup --system` or similar that creates
  `/usr/bin -> /grm/profiles/current/bin`.
- Require root for this step only; day-to-day use stays unprivileged.