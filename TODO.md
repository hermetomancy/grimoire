# Grimoire TODO

This file tracks the remaining work before Grimoire is ready for a stable
release. Once every item below is done, this file should be deleted.


## Remaining

### 1. `deps.features` + FHS compat layer

Add a new dependency category `deps.features` for packages that provide
execution-time capabilities rather than direct binaries on PATH.

- `src/model.rs`: add `features: Vec<Dependency>` to `Deps`, `PackageState`,
  and `IndexEntry`; update `parse_deps` and serialization.
- `src/install.rs`: resolve and install `features` deps alongside runtime
  deps (store-only is fine).
- `src/profile.rs`: when linking a package with `features` deps into a
  generation, create wrapper scripts in `gen-N/bin/` instead of hard-linking
  the binaries directly. The wrapper invokes `grm fhs-run` with the FHS tree
  store path and the real binary store path.
- New `src/fhs.rs`: implement `grm fhs-run <tree> <binary> [args...]` using
  `unshare(CLONE_NEWNS)` + recursive bind mounts. No external dependencies,
  no root required on normal Linux kernels.
- New `src/cli.rs` subcommand: `grm fhs-run`.
- New `tome-core/runes/fhs-compat.rn`: a baseline FHS tree package that
  symlinks glibc and core libraries from other core packages into a staging
  directory.

This satisfies the design doc's "make-or-break" foreign-binary compat
requirement.

### 2. Store-hash-keyed binary cache

The index is currently keyed by `name/version/target`. The design doc wants
the substitution cache keyed by content address:

- Index entries should be looked up primarily by `store_hash`.
- The solver should still resolve versions, but the binhost query should
  use the computed store hash as the lookup key.
- Relaxes the requirement that a tome publishes every version for every
target; instead it publishes store hashes.

### 3. Input hash / closure resolution

The solver resolves versions today. The design doc envisions resolution that
computes the input hash / closure directly:

- Keep version resolution for user-facing planning.
- Internally, every resolved plan step should compute and verify its store
  hash early, so mismatches are caught before any fetching or building.

### 4. CoW filesystem optimization

Generations currently use hard links. APFS `clonefile` and Linux reflink
(Btrfs/XFS) would give instant, space-free generation creation.

- Add platform-gated `clone_file()` / `FICLONE` / `clonefile()` in
  `src/profile.rs` as a drop-in replacement for `fs::hard_link` where
  supported.
- Fall back to hard links when CoW is unavailable.

### 5. Build-environment identity beyond the C compiler

The store hash currently folds in `toolchain::build_env_id()`, derived from
`cc --version`. Extend this to capture linker, system SDK, and any other
host boundaries that affect binary output as they become relevant.

### 6. Trust hardening

- Signed source rune digests: sign a manifest of rune paths + sha256s and
  verify it against the tome's pinned key before source builds.
- Signed addenda: apply the same signature/TOFU model to `addendum.nuon`.
- `grm tome add --signer <key>` for out-of-band key pinning.

### 7. Release engineering

- Multi-OS CI matrix (Linux, macOS, FreeBSD) + MSRV job.
- Signed release archives for supported targets.
- `grm self-update`.
- `CHANGELOG.md`.

### 8. AGENTS.md compliance audit

Review the entire project against [`AGENTS.md`](AGENTS.md) before considering
it release-ready:

- Remove dead code, unused modules, and orphaned test fixtures.
- Split files that have grown past one screen of meaningful logic; prefer
  small, single-responsibility modules.
- Check that no module reaches into another module's internals through
  convenience-only `pub` items.
- Verify every fallible path returns `anyhow::Result` with `.context()` at
  boundaries.
- Eliminate any remaining `unwrap()`, `expect()`, or `panic!` outside
  genuinely unreachable invariants (with `// SAFETY:` or explanatory
  messages).
- Confirm `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test` pass cleanly.
- Add regression tests for any bug fixed during the audit.

### 9. Boot integration

On a Grimoire-as-primary-distro system, the bootloader should list
generations so a broken kernel or init can roll back at boot time.

- Design a bootloader config fragment (systemd-boot, GRUB, etc.) that points
  each entry at a different generation's kernel + initrd.
- Add a `grm boot-update` command that regenerates the bootloader menu from
  the generation registry.
- Decide GC policy: boot entries are additional GC roots.

Only relevant when Grimoire is the sole package manager.

### 10. System config (`/etc`) management

The design doc says `/etc` is handled conventionally, like a traditional
distro. This is intentionally lightweight — no full NixOS-style etc
overlay — but we still need:

- Document the convention: Grimoire does not manage `/etc`.
- Optional: a minimal `grm etc-track` helper that records which package
  installed which `/etc` file and warns on conflicts. Treat as future work.

### 11. System-level `/usr/bin`

When Grimoire is the sole PM, `/usr/bin` should be a symlink or bind mount
pointing to the active generation's `bin/`. Current user-local
`~/.grimoire/profiles/current/bin` is correct for secondary PM use.

- Add `grm setup --system` or similar that creates `/usr/bin -> /grm/profiles/current/bin`.
- Require root for this step only; day-to-day use stays unprivileged.

## Deletion criteria

When all items above are complete and reflected in the design doc,
delete this file. Until then, this is the canonical remaining-work list.
