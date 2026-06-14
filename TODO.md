# Grimoire TODO

The canonical remaining-work list (AGENTS.md §15.2). Completed work lives in git history,
not here. When everything below is done and reflected in the design doc, delete this file.

## Remaining before a stable release

### Release engineering

- **`CHANGELOG.md`** at the first tag.
- **tome-build.yml** stays manual until Bootstrap stage 1 lands, then becomes a
  release-blocking `grm tome build --all` CI job per platform.

### Bootstrap stage 1: core on all targets

Build the core runes (`linux-headers`, `musl`, `llvm` (+`clang` split member with
compiler-rt runtimes inside), `gmake`, `toybox` (+`gsed` on BSD-userland hosts),
`toolchain-wrappers`) from source on every supported target. macOS aarch64 closed the
dogfood loop 2026-06-12 (full chain through rust 1.96 and grimoire itself); remaining:

- ⏳ Linux x86_64 glibc
- ⏳ Linux aarch64 glibc
- ⏳ Linux x86_64 musl
- ⏳ Linux aarch64 musl
- ⏳ FreeBSD x86_64
- ⏳ FreeBSD aarch64

### Bootstrap stage 2: full self-hosting

The toolchain is already self-hosted (every build driver — cmake, python3, gmake, the
compiler boundary — is a core package; the host compiler seeds bootstrap only and
`host_tool_dirs()` drops out once toolchain-wrappers exists). What remains is the
ambient userland.

The host *userland* link is shadowed, never severed: `/usr/bin` + `/bin` are
unconditionally appended to managed build PATH (`posix_ambient_dirs`), so anything toybox
does not ship (perl, m4, bash-isms) falls through to the host silently — an unhashed
build input, same class as the Homebrew-zstd leak. Path to severance is empirical: a
hermetic build mode (`tome build --hermetic`?) that drops the ambient tail, run per rune
to enumerate what actually leaks; passing runes are certified toybox-ambient, failures
name what stage 2 must package. Folding "toybox-ambient or not" into `build_env_id` is
the cheap interim fix if prebuilts ever come from heterogeneous builders.

### Store-address determinism

A package's content address must be a single-source-of-truth, path-independent function of
its declared inputs — no two code paths may derive it differently (AGENTS §9.8). Today they
can: normal packages fold the *resolver-chosen* dep versions into the hash, but split members
can't supply them, so `store_hash_for_rune_with_deps` falls back to a deterministic *closure
walk*. Each path is deterministic alone, but when they pick different versions for the same
dep the resolver's *expected* address and the artifact's *actual* address diverge — silently:
phantom drift cascades into world rebuilds, and a computed hash that no longer matches the
published index drops binary substitution to source. Dormant today (llvm/clang is the only
split group and resolves unambiguously), but a latent correctness landmine — harden before
stable:

- Collapse to one hash path: feed the resolver's chosen versions into split-member
  addressing so there is no closure-walk fallback to diverge from.
- Guard the class with a property test — every address-derivation path produces an identical
  hash for the same inputs — so future code that adds a new derivation path is caught.

(Distinct from *intended* drift: a rune/source/build-env edit re-addressing a package is the
content-addressing working as designed. The bug class is *accidental* divergence — same
inputs, different hash depending on who asks.)

## Known debts (not release-blocking)

- **The resolver does not backtrack over conflicts.** `conflicts` metadata refuses a plan
  early and precisely, but pubgrub never steers version selection *around* a conflict.
  Correct for mutual-exclusion semantics; spec before changing.
- **Linked-libraries lint.** Configure-time feature detection can link a host library
  without baking a path string (the LLVM-22/Homebrew-zstd incident), invisible to the
  purity lint. A `tome build` check diffing actual linkage against declared deps would
  catch the class.

## Expansion projects (spec before code)

- **linux-aarch64-musl cross bootstrap.** No official rust host tools — needs a real
  cross story: build deps resolved for the *host* triple while runtime deps and outputs
  target the *target* triple (the model currently conflates them), cross
  toolchain-wrappers, rust cross-std. Project-sized; write the design doc first.
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
