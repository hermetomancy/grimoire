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
- New `tome-core/runes/toolchain-wrappers.rn`: compiler toolchain wrapper scripts that
  symlinks glibc and core libraries from other core packages into a staging
  directory.

This satisfies the design doc's "make-or-break" foreign-binary compat
requirement.

### 2. Release engineering

- Multi-OS CI matrix (Linux, macOS, FreeBSD) + MSRV job.
- Signed release archives for supported targets.
- `grm self-update`.
- `CHANGELOG.md`.

### 3. Boot integration

On a Grimoire-as-primary-distro system, the bootloader should list
generations so a broken kernel or init can roll back at boot time.

- Design a bootloader config fragment (systemd-boot, GRUB, etc.) that points
  each entry at a different generation's kernel + initrd.
- Add a `grm boot-update` command that regenerates the bootloader menu from
  the generation registry.
- Decide GC policy: boot entries are additional GC roots.

Only relevant when Grimoire is the sole package manager.

### 4. System config (`/etc`) management

The design doc says `/etc` is handled conventionally, like a traditional
distro. This is intentionally lightweight — no full NixOS-style etc
overlay — but we still need:

- Document the convention: Grimoire does not manage `/etc`.
- Optional: a minimal `grm etc-track` helper that records which package
  installed which `/etc` file and warns on conflicts. Treat as future work.

### 5. System-level `/usr/bin`

When Grimoire is the sole PM, `/usr/bin` should be a symlink or bind mount
pointing to the active generation's `bin/`. Current user-local
`~/.grimoire/profiles/current/bin` is correct for secondary PM use.

- Add `grm setup --system` or similar that creates `/usr/bin -> /grm/profiles/current/bin`.
- Require root for this step only; day-to-day use stays unprivileged.

### 6. Semantic rollback: activation restores state

Today `grm rollback`/`grm switch` only repoint the `profiles/current`
symlink; `state/packages/` still describes the set rolled back *from*. So
queries (`list`, `info`, `files`, `owns`, `orphans`) report the wrong set,
and the next mutating command rebuilds a generation from the stale state —
silently undoing the rollback. Rollback must mean *actually rolling back*:
activating a generation restores the full package state it was built from.

- `src/profile.rs`: snapshot the complete `PackageState` set into each
  generation at `create_generation` time (`gen.nuon` currently records only
  names + store paths, which cannot reconstruct state — bins, deps, flags,
  requested/held are missing).
- `activate_generation`: rewrite `state/packages/` and rebuild the lockfile
  from the activated generation's snapshot, transactionally (stage + atomic
  promote, per AGENTS.md §9). Decide `requested`/`held` semantics: restore
  from the snapshot (state travels with the generation).
- `profile::gc`: always retain the current generation's rollback target —
  today `gc --keep 1` deletes it and the next `grm rollback` fails.
- `grm doctor`: add a state ↔ active-generation divergence check (also
  useful for pre-migration installs).
- Document the state/generation relationship in AGENTS.md §9.
- Tests: rollback then `grm list` reports the rolled-back set; rollback
  then install does not resurrect rolled-back packages; gc preserves the
  rollback target; crash-window behavior of the state restore.

### 7. Restore-able generations: lockfile as a blueprint

Any Grimoire generation should be reconstructable on a fresh install root
from its lockfile alone. Today the lockfile is only a resolution constraint:
`--locked` requires naming packages, re-resolves against whatever the tome
caches currently hold, and cannot reproduce locally built archives.

- `src/lock.rs` / `src/model.rs`: record `requested`, `held`, and
  `store_hash` per package in `grimoire.lock.nuon` (`package_value`
  currently drops them), so the lock distinguishes roots from dependencies
  and pins the content address, not just the archive hash.
- New `grm restore [--lockfile <path>]`: install every `requested` package
  from the lock under pins, restore `requested`/`held` flags, then sweep
  orphans — yielding the recorded set exactly.
- Enforce recorded tome commits during `--locked`/`restore` resolution:
  check out the pinned commit in the tome cache before resolving (the lock
  records commits today but never uses them, so a moved ref changes what
  `--locked` resolves against).
- Source-built packages: pin rune identity (tome + rune sha256) so a locked
  source build is rebuilt from the same recipe; fail loudly when the
  pinned rune/archive is unavailable anywhere.
- Per-generation lockfiles fall out of item 6's state snapshots: restoring
  generation N = `grm restore` against N's snapshot lock.
- Update the README "Reproducible state" claim to match whatever lands.
- Tests: restore onto an empty root reproduces packages, flags, and
  versions byte-for-byte against state files; moved tome ref under
  `--locked` still resolves the pinned set.

## Completed

### Store-hash-keyed binary cache

- Index entries keyed by `store_hash` (`BTreeMap<String, IndexEntry>`).
- Solver resolves versions for planning; binhost queries use computed store
  hash for content-addressed lookup.
- Prebuilt substitutes accepted only when published `store_hash` matches.

### Input hash / closure resolution

- `Plan::compute_store_hashes()` computes store hashes before realization.
- Mismatches caught before fetching or building.

### Build-environment identity beyond the C compiler

- `toolchain::build_env_id()` captures compiler, linker, assembler, and
  platform-specific post-link tools.
- macOS SDK version captured via `xcrun --show-sdk-version`.

### Managed build environment sandboxing

- Managed builds clear host discovery variables and layer declared build dependency prefixes
  back in through `CMAKE_PREFIX_PATH`, `PKG_CONFIG_*`, `CPATH`, `LIBRARY_PATH`, `ACLOCAL_PATH`,
  and `<DEP>_PREFIX`.
- `HOME`, temp directories, and XDG directories point inside `ctx.work_dir` for rune builds.
- External build commands receive blank overrides for inherited host environment variables unless
  Grimoire deliberately provides them.

### CoW filesystem optimization

- `clone_or_hard_link()` in `src/profile.rs` tries APFS `clonefile` (macOS)
  or `FICLONE` reflink (Linux Btrfs/XFS) before falling back to hard links.
- Unsupported filesystems gracefully fall back to hard links.

### Trust hardening

- Signed rune manifests (`runes-manifest.nuon` with per-rune sha256s),
  verified against pinned keys before source builds.
- Signed addenda with TOFU key pinning and graceful key rotation.
- `grm tome add --signer <key>` for out-of-band key pinning.
- Manifest signature verified on every sync (not just rotation scenarios).

### AGENTS.md compliance audit

- Full audit of codebase against AGENTS.md completed.
- FreeBSD compilation failures identified and fixes in progress.
- `expect()` calls in non-test code identified and fixes in progress.
- Missing tests identified and additions in progress.
- AGENTS.md updated to reflect current architecture (rune authoring, PATH
  order, platform-conditional deps, multi-package CLI, project hygiene).

### Security and correctness audit fixes (high impact)

- **Archive TOCTOU** — `install_store_only` now copies the archive into its
  private transaction directory before hashing, validating, and extracting.
  `extract_source_archive` copies source archives into the private `work_dir`
  before validation and extraction.
- **Source archive nested-under-symlink check** — `validate_tar_entries` now
  rejects members nested under earlier symlinks, matching the binary-archive
  validator.
- **setup chown TOCTOU** — Replaced `std::os::unix::fs::chown` with
  `libc::lchown` so symlinks are not followed during ownership changes.
- **is_writable symlink safety** — Returns `false` when the path is a symlink,
  preventing probe-file creation through an attacker-controlled link.
- **closure.rs signature verification** — `store_hash_for_rune_with_deps` now
  calls `tome::verify_rune` before hashing, closing the bypass the solver used.
- **tome build target-blind skip** — `build_runes` now filters catalog entries
  by the current target triple instead of matching names across targets.

## Active work

### Phase 1: Bootstrap core on all targets

Build the 8/6 core runes (`linux-headers`, `musl`, `compiler-rt`, `llvm`,
`clang`, `make`, `toybox`, `toolchain-wrappers`) from source on every
supported target. Current status:

- 🔄 macOS aarch64: in progress (M4 Pro)
- ⏳ Linux x86_64 glibc
- ⏳ Linux aarch64 glibc
- ⏳ Linux x86_64 musl
- ⏳ Linux aarch64 musl
- ⏳ FreeBSD x86_64
- ⏳ FreeBSD aarch64

### Phase 2: Full self-hosting

Add `cmake` and `python3` to `core` so no non-POSIX host tools are required
for bootstrap. Removes the need for `host_tool_dirs()`.

### Source tree reorganization

AGENTS.md §3.6 codifies the limits (500 lines soft, 800 hard). **Done:** the
hard violators are split into directory modules with re-export roots —
`src/model/` (package/deps/state/index/catalog/value), `src/install/`
(realize/build_deps/state/orphans/transaction), `src/solve/`
(capabilities/candidates/plan/resolver), `src/tome/`
(authoring/publish/sync/verify + git/news), `src/nu/runtime/` (env/eval),
and `src/profile/` (generations/gc/linking). Every `src/` file is now under
the hard limit.

The root is regrouped too: `src/cmd/` (thin CLI handlers: clean, doctor,
files, man, prefer, query, setup), `src/util/` (fs_util, paths,
process_lock, progress, time_util), `src/store/` (+closure), `src/build/`
(+toolchain), `src/catalog/` (sync_common, addendum, signing), `lock` →
`src/install/`, `preferences` → `src/model/`, and the tests-only `index.rs`
folded into `model/index.rs` — 14 modules at the root, down from ~30.

Remaining:

- **Soft-limit residents** (split when next touched, per §3.6):
  `solve/resolver.rs` (~690, ~380 of it tests), `build/mod.rs` (~660),
  `model/catalog.rs` (~540), `install/mod.rs` (~540).
- **Split `src/archive/`** into `pack.rs`, `unpack.rs`, `validate.rs` (from
  the current monolithic `archive/mod.rs` and `archive/pack.rs`).

### 6. Security and correctness audit fixes (post-subagent review)

Items identified during the comprehensive subagent audit that remain unfixed.

#### Medium impact

- **Profile generation hard links fail across filesystems**  
  `src/profile.rs:473-475` / `src/profile.rs:589` — `clone_or_hard_link` tries CoW clone, then hard link. If `/grm/store` and `/grm/profiles` are on separate mounts, both fail and `create_generation` bails with no fallback to file copy.

- **`archive/pack.rs::append_file` loads entire files into memory**  
  `src/archive/pack.rs:178-181` — `read_to_end` buffers the whole file. Large debug info or static libraries could cause OOM during `grm tome build`.

- **Progress spinner thread leak on panic**  
  `src/progress.rs:178-193` — The spinner thread is not a daemon. If the main thread panics, the process hangs indefinitely because the spinner thread keeps the process alive.

- **`CapabilityIndex` only harvests capabilities from `bins`, not `provides`**  
  `src/solve.rs:97-111` — Index entries can declare non-binary capabilities via `provides`, but source runes cannot because `record_capabilities_from_rune` only looks at `bins_for(target)`.

#### Low impact

- **`archive/pack.rs::append_dir` hardcodes `0o755`**  
  Directory permissions from the build are lost.

- **`archive/pack.rs::relative_destdir_prefix` silently strips `..`**  
  `RootDir`, `Prefix`, and `ParentDir` components are all dropped silently.

- **`find_dep_state` allocates `String` for `provides` lookup**  
  Minor allocation on every orphan check.

- **`rune_names_ordered` only sorts initial seed nodes**  
  Nodes that become ready later are appended in insertion order.

- **`fetch.rs` creates a new HTTP agent per request**  
  Wastes TCP/TLS handshakes.

- **`install_store_only` reads the archive 3 times**  
  Hash verification, path validation, metadata inspection, and extraction each open the archive independently.

- **`doctor.rs` missing health checks**  
  Doesn't detect broken `profiles/current` symlink, whitespace in install root, stale `.grimoire-old` backups, corrupt state files, or duplicate bin collisions.

## Deletion criteria

When all items in **Remaining** are complete and reflected in the design doc,
delete this file. Until then, this is the canonical remaining-work list.
