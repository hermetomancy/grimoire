# Grimoire TODO

This file tracks the remaining work before Grimoire is ready for a stable
release, plus the planned Grimoire-OS work that follows it. Once every item
in the **Roadmap** is done and the **Planned** items have graduated into
real work, this file should be deleted.


## Roadmap

Phases are ordered by dependency and urgency; items within a phase are
independent. Each item records the *decided* design, not just the problem.

### Phase 0 — land the working tree ✅

Done. The session's five semantic strands were co-developed and co-tested
(`state.rs`/`orphans.rs`/`closure.rs` interleave all of them), so synthetic
per-feature intermediate commits would have been untested fake history;
landed as one comprehensive, fully-tested commit ("consolidate the CLI and
make installs content-addressed end to end") plus this roadmap. The
tome-core runes (`rust-stage0` + `rust` 1.96.0, llvm `LLVM_INSTALL_UTILS`)
live outside this repo's history — `tome-core/` is gitignored; commit them
in the tome repository.

### Phase 1 — small correctness/UX fixes ✅ (2026-06-12)

- **Early rune-subset validation.** Const extraction
  (`nu/runtime/eval.rs` bare-core context) accepts runes the build runner
  later rejects (`str join` incident: 4-minute toolchain build wasted on a
  parse error). Switch const-extraction to `add_rune_command_context` so
  `info`/`search`/`tome build` reject subset violations before any fetch.
  Test: a rune using `each`/`str join` fails at `grm info`.
- **`tome remove` warning.** Name installed packages whose runes resolve
  from the tome being removed — they keep working but silently lose drift
  detection and rebuildability.
- **Preference-aware `find_dep_state`.** The capability fallback is
  first-match; solver and closure walker are preference-aware. Align it:
  preference, else first provider by name. Affects linked-set edges and
  `<DEP>_PREFIX` env vars when multiple providers are installed.
- **Doctor: rollback-target snapshot check.** Validate that retained
  generations' snapshots reference existing store paths, before a rollback
  discovers the gap.
- **Addendum staleness.** `apply_patches` warns on a moved addendum commit
  and continues with stale patches; auto-resync instead (or error with the
  `grm addendum update` hint). Stale patches silently change store hashes.
- **`xz2 = { features = ["static"] }`.** `lzma-sys` otherwise links host
  liblzma via pkg-config — a host dependency leak, and a blocker for the
  `grimoire` rune.

### Phase 2 — rune metadata cache ✅ (2026-06-12)

Every staleness walk nu-evals the rune of every installed package; building
a `CapabilityIndex` nu-evals every rune in every tome. Linear in installed
set × catalog size, paid per mutating command. Design:

- `build::read_rune_metadata` routes through a cache keyed by
  sha256(rune bytes): per-process `HashMap` first (the same rune is
  currently evaluated several times per command), then on-disk
  `cache/rune-meta/<sha256>.nuon` holding the serialized `PackageMetadata`.
- Cache the **pre-patch** eval result; apply addendum patches after the
  cache (patches are cheap data merges, and this keeps the key independent
  of addendum state).
- Version-stamp the cache directory (rune-eval semantics may change across
  grimoire versions); `grm clean` wipes it like any cache.

### Phase 3 — catalog-readiness ✅ (2026-06-12)

- **Version policy: normalize, don't vercmp.** Keep `semver::Version` as
  the only internal ordering. Codify in rune-authoring.md: `version` must
  be semver-orderable; add an optional `upstream_version` metadata field
  (shown by `info`/`search`, used in source URLs) with per-ecosystem
  mapping recipes (e.g. `2025a` → `2025.1.0`, `9.9p1` → `9.9.1`).
  Rationale: pacman-style vercmp would replace `Version`/`VersionReq`
  through resolver, lock, and index for marginal gain; the mapping
  convention is one doc section plus an optional `tome build` lint.
- **`conflicts`/`replaces` metadata.** Add both to rune metadata and
  `IndexEntry`. `replaces: ["old"]`: installing/upgrading the new package
  removes `old` in the same transaction and migrates its
  `requested`/`held` intent; `upgrade` treats a replacer as an upgrade
  candidate for the replaced name. `conflicts: ["x"]`: resolution fails
  when both would be installed; install refuses while the conflicting
  package is installed unless it leaves in the same command. Renames
  become catalog-expressible (the `rust` → `rust-stage0` rename was free
  only because nothing was released).

### Phase 4 — build pipeline ✅ (2026-06-12, two user actions remain)

Done except: (1) generate the minisign release keypair before the first tag
(`minisign -GW`, `MINISIGN_SECRET_KEY` secret, public key in README) — a
key-custody action only the maintainer can take; (2) pin a real commit +
tarball hash into tome-core's `grimoire.rn` once these commits are pushed,
and commit the rune in the tome repository. The tome-build.yml workflow is
manual until Bootstrap stage 1 lands, then becomes a release gate.

Original items for reference:

- **Built-archive cache trust.** Remove → reinstall of a source-built
  package currently rebuilds from scratch (unrecorded store dirs are
  rightly never re-trusted — §10.1). Instead: before building a source
  step, scan `cache/builds/` for an archive whose embedded store basename
  matches the step's computed store hash; on match, install it through the
  normal verified local-archive path. Same acceptance check substitutes
  get; makes remove→reinstall of rust ~free. `clean` still wipes the cache.
- **Impurity lint instead of hard isolation.** Decision: namespace/chroot
  isolation is deferred to Grimoire OS; build-time network stays allowed
  (cargo `--locked` policy). What we add now: a post-build `tome build`
  scan of staged artifacts for absolute host paths outside the store root
  (`/usr/local`, `/opt/homebrew`, build dirs) — warn, `--strict` errors.
  Catches the common purity leaks cheaply on every platform.
- **Release engineering.** `CHANGELOG.md` at first tag; release-blocking
  `grm tome build --all` CI job per platform (blocked on the bootstrap in
  Active work); generate the minisign release keypair before the first tag
  (`minisign -GW`, `MINISIGN_SECRET_KEY` secret, public key in README).
- **Package grimoire as `grimoire`.** Rune in tome-core: codeload tarball
  pinned by tag, `cargo build --release --locked` (crates fetched at build
  time per the vendoring decision), `bins: { grm }`, build dep `rust`.
  Version = release tags; between-tag pins use `-dev.YYYYMMDD` prerelease
  (orders correctly under semver). `grm upgrade grimoire` then *is*
  self-update — replaces the old `grm self-update` item (safe: the running
  binary keeps its inode; the generation flip is atomic).

### Phase 5 — expansion projects (spec before code)

- **linux-aarch64-musl cross bootstrap.** No official rust host tools —
  needs a real cross story: build deps resolved for the *host* triple
  while runtime deps and outputs target the *target* triple (the model
  currently conflates them), cross toolchain-wrappers, rust cross-std.
  Project-sized; write the design doc first.

## Planned: Grimoire OS

Work that only matters when Grimoire is the operating system's sole package
manager — a Grimoire-based distribution. None of it blocks a stable release
of Grimoire as a standalone/secondary package manager.

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

### 2. Boot integration

On a Grimoire-as-primary-distro system, the bootloader should list
generations so a broken kernel or init can roll back at boot time.

- Design a bootloader config fragment (systemd-boot, GRUB, etc.) that points
  each entry at a different generation's kernel + initrd.
- Add a `grm boot-update` command that regenerates the bootloader menu from
  the generation registry.
- Decide GC policy: boot entries are additional GC roots.

### 3. System config (`/etc`) management

The design doc says `/etc` is handled conventionally, like a traditional
distro. This is intentionally lightweight — no full NixOS-style etc
overlay — but we still need:

- Document the convention: Grimoire does not manage `/etc`.
- Optional: a minimal `grm etc-track` helper that records which package
  installed which `/etc` file and warns on conflicts. Treat as future work.

### 4. System-level `/usr/bin`

When Grimoire is the sole PM, `/usr/bin` should be a symlink or bind mount
pointing to the active generation's `bin/`. Current user-local
`~/.grimoire/profiles/current/bin` is correct for secondary PM use.

- Add `grm setup --system` or similar that creates `/usr/bin -> /grm/profiles/current/bin`.
- Require root for this step only; day-to-day use stays unprivileged.

## Completed

### Stale-plan duplicate realization fix

- Overlapping build-dependency plans no longer realize a shared package once per
  plan: `execute_step` and `ensure_build_deps_installed_inner` skip a step whose
  exact install already landed (`step_already_realized`: state matches the step's
  name, version, and content address, and the recorded store path still exists).
  Previously `grm install` could rebuild llvm from source multiple times in one
  command because clang's build-dep plan was resolved before compiler-rt's nested
  recursion installed llvm. Regression test: shared build dep builds exactly once.
- Store directories without a matching state record are deliberately not adopted:
  state is written only after hash verification (AGENTS.md §10.1), so unrecorded
  residue still realizes normally.

### CLI output restyle

- One result-line vocabulary in `util/progress` (AGENTS.md §12.4): `✦` confirmations, `!`
  cautions (`warn`), `strong`/`faint` inline emphasis, `→` for version transitions; piped
  output stays plain.
- Install result lines name their origin (`prebuilt, checksum verified` / `built from
  source` / `local archive` / store-only variants) via `InstallOrigin` threaded through
  `install_archive`/`install_store_only`.
- Mutations end with `generation N is now current`; rollback/switch print `switching profile
  to generation N…` plus a timed `rolled back to generation N in X.XXs`; duplicate
  confirmations from `main.rs` removed.
- `grm generations` (new `src/cmd/generations.rs`) diffs each generation against its
  predecessor (`+ added 1.2.3, ~ moved 1.0 → 1.1, - removed`, snapshot-aware) and ends with
  `profiles/current → gen-N`.
- Major-version upgrades warn persistently with a `grm hold` hint.

### Dependency minimization

- `nu-command` replaced by a curated rune command set (`src/nu/commands/`):
  run-external/complete adapted from upstream, purpose-built fs/path/str/
  filter commands, with the subset documented as a contract in
  rune-authoring.md. gix trimmed to clone/open/head_commit features; unused
  `hex` dep and `clap/env` feature dropped. Release dependency graph:
  ~535 → 359 unique crates. The gix reqwest-transport replacement was
  evaluated and deliberately declined (would duplicate ~500 lines of
  upstream clone orchestration to remove pure-Rust crates that
  cross-compile cleanly).

### Semantic rollback and lockfile restore

- Every generation embeds a full `state.nuon` snapshot; `grm switch`/`rollback`
  restore `state/packages/` and the lockfile from it before flipping the
  symlink (AGENTS.md §9.6). GC always retains the rollback target; doctor
  flags state/generation divergence; re-activating the current generation is
  the crash repair path.
- The lockfile records `requested`, `held`, and `store_hash` per package and
  is read back as a blueprint: `grm restore [--lockfile <path>]` installs the
  requested set under pins, restores intent flags, and sweeps the rest.
  Locked operations verify pinned tome commits (a moved ref fails loudly) and
  pinned store hashes (recipe/source/build-env drift fails before any fetch
  or build). Hold/requested changes refresh the lock.


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

### Source tree reorganization

- Hard-limit violators split into directory modules with re-export roots:
  `src/model/`, `src/install/`, `src/solve/`, `src/tome/`, `src/nu/runtime/`,
  `src/profile/`, `src/archive/` (hash root + pack/unpack/validate, with
  extraction moved home from install). Soft-limit residents split too
  (resolver tests, build sources/output, addendum catalog, installer steps).
- Root regrouped from ~30 modules to 14: `src/cmd/`, `src/util/`,
  `src/store/` (+closure), `src/build/` (+toolchain), `src/catalog/`
  (sync_common, addendum, signing); `lock` → `install/`, `preferences` →
  `model/`, tests-only `index.rs` folded into `model/index.rs`.
- `tests/smoke.rs` (5,761 lines) split into ten themed integration crates
  with shared helpers in `tests/support/`; AGENTS.md §3.6 exemption retired.
- Every file in the repository is within the §3.6 limits (500 soft/800 hard).

### Security and correctness audit fixes (post-subagent review)

- **Cross-filesystem generations** — `clone_or_hard_link` falls back to a
  plain file copy on `EXDEV` when CoW clone and hard link are both impossible.
- **Streaming pack** — `pack::append_file` streams from disk instead of
  buffering whole files.
- **Spinner vs. panic** — a panic hook tears the live spinner down before the
  panic report prints. (The audited "process hang" was a misdiagnosis — a
  non-main thread never keeps a Rust process alive — but the garbled-stderr
  race was real.)
- **Rune `provides` capabilities** — `record_capabilities_from_rune` harvests
  declared `provides` like index entries do.
- **Deterministic publish order** — `rune_names_ordered` uses an ordered
  ready-set for every node, not just seeds.
- **Shared HTTP agent** — downloads reuse one process-wide agent.
- **Fewer archive passes** — staging hashes during the copy and captures
  embedded metadata during path validation (five reads → three).
- **Destdir prefix hardening** — `relative_destdir_prefix` rejects `..` and
  Windows prefixes instead of silently stripping them.
- **`find_dep_state`** no longer allocates per `provides` lookup.
- **Doctor checks added** — whitespace install root, broken
  `profiles/current` symlink, corrupt state files, contested bins without a
  preference, stale `.grimoire-old` backups.
- **By design, now documented:** `append_dir`'s fixed `0o755` matches the
  fixed file modes — archives must be byte-identical across builders, so
  host umask must not leak in.

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

### Bootstrap stage 1: core on all targets

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

### Bootstrap stage 2: full self-hosting

Add `cmake` and `python3` to `core` so no non-POSIX host tools are required
for bootstrap. Removes the need for `host_tool_dirs()`.


## Deletion criteria

When all items in **Remaining** are complete and reflected in the design doc,
delete this file. Until then, this is the canonical remaining-work list.
