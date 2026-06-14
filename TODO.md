# Grimoire TODO

The canonical remaining-work list (AGENTS.md §15.2). Completed work lives in git history,
not here. When everything below is done and reflected in the design doc, delete this file.

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
compiler-rt runtimes inside), `gmake`, `toybox` (+`gsed`),
`toolchain-wrappers`) from source on every supported target. macOS aarch64 closed the
dogfood loop 2026-06-12 (full chain through rust 1.96 and grimoire itself). The remaining
targets are not equivalent work. Four are *unblocked* — a complete dogfood path exists, so
each needs only a native `grm tome build --all` run plus per-rune platform debugging. Two
are *blocked* on the cross-bootstrap expansion below: no official Rust stage0 host tools
(`rust-stage0.rn`/`rust.rn` publish none), so the loop cannot close natively. The blocked
pair is not on the stable bar.

Unblocked (native build + per-rune debugging):

- ⏳ Linux x86_64 glibc
- ⏳ Linux aarch64 glibc
- ⏳ Linux x86_64 musl
- ⏳ FreeBSD x86_64 — riskiest: no GitHub-hosted runner, and a multi-hour llvm+rust build
  inside a nested VM (no `/grm`, tight disk/time) is unproven; may need a self-hosted builder.

Blocked on the cross-bootstrap expansion (not stable-blocking):

- ⛔ Linux aarch64 musl
- ⛔ FreeBSD aarch64

### Bootstrap stage 2: full self-hosting

The toolchain is already self-hosted (every build driver — cmake, python3, gmake, the
compiler boundary — is a core package; the host compiler seeds bootstrap only and
`host_tool_dirs()` drops out once toolchain-wrappers exists). What remains is the
ambient userland.

The host *userland* link is shadowed, never severed: `/usr/bin` + `/bin` are
unconditionally appended to managed build PATH (`posix_ambient_dirs`), so anything toybox
does not ship (perl, m4, bash-isms) falls through to the host silently — an unhashed
build input, same class as the Homebrew-zstd leak. Path to severance is empirical: a
hermetic build mode that drops the ambient tail, run per rune to enumerate what actually
leaks; passing runes are certified toybox-ambient, failures name what stage 2 must package.
The `grm tome build --hermetic` diagnostic has shipped: it drops the ambient tail so a build
that reaches for an unpackaged tool fails and names the leak. The remaining stage-2 work is
empirical — run it per rune to enumerate what actually leaks, certify the passing runes as
toybox-ambient, and package the failures. One deferred deliverable:

- **Fold a toybox-ambient marker into `build_env_id` (deferred; needs a decision).** The
  cheap interim fix if prebuilts ever come from heterogeneous builders, but blocked by an
  architecture mismatch: `build_env_id` is a process-global cached pure fn with no per-build
  inputs while `--hermetic` is per-build, and the marker must reach all three address
  consumers (resolver `plan.rs`, closure walker `closure.rs`, install state `realize.rs`)
  identically or §9.8 breaks. Decide between a host-property marker and plumbing per-build
  env identity end-to-end before writing it.

## Known debts (not release-blocking)

- **The resolver does not backtrack over conflicts.** `conflicts` metadata refuses a plan
  early and precisely, but pubgrub never steers version selection *around* a conflict.
  Correct for mutual-exclusion semantics; spec before changing.

## Expansion projects (spec before code)

- **Cross bootstrap (linux-aarch64-musl, freebsd-aarch64).** Both lack official Rust
  stage0 host tools, so neither closes the dogfood loop natively (the two blocked stage-1
  targets above). Needs a real cross story: build deps resolved for the *host* triple while
  runtime deps and outputs target the *target* triple (the model currently conflates them —
  single `target_triple` in `util/paths.rs`), cross toolchain-wrappers, rust cross-std.
  Project-sized; write the design doc first.
- **Scoped profiles and dev shells (`grm profile` / `grm shell`).** Named, imperative
  profiles for development against managed libraries — e.g. a `rust-devel` profile where
  `cargo install`'s `openssl-sys` finds the managed OpenSSL instead of host Homebrew.
  Converged design:
  - *Model.* A named profile is the existing profile generalized: its own state (installed
    set, lockfile), generation chain, and rollback; the unnamed default is the reserved
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
  - *Work.* Per-profile state/generation plumbing across install/remove/upgrade/rollback/
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

## Deletion criteria

When everything above is complete and reflected in the design doc, delete this file.
Until then, this is the canonical remaining-work list.
