# Changelog

All notable changes to Grimoire. Format follows [Keep a Changelog](https://keepachangelog.com/);
versions follow semver. Entries land in **Unreleased** as they merge and move under a version
heading when it is tagged.

## Unreleased

### Added

- Managed build-floor userland: `uutils` (Rust coreutils), `dash` (POSIX sh), `mawk` (awk), and
  `ggrep` (GNU grep) join `gsed` on the build floor (`CORE_PACKAGES`), replacing `toybox` — whose
  set was too thin for autotools `configure` (no awk/sh/tr/expr/…). The host `/usr/bin` tail stays
  only as a shrinking fallback; `grm tome build --hermetic` enumerates what the floor still lacks.
- `build_only` rune metadata: a build-only package (the managed `build-env`, `clang`, and `llvm`)
  is pinned in the store and available to source builds, but its bins stay off the active profile —
  the managed toolchain/userland floor (dash, mawk, uutils, gsed, ggrep, clang, llvm, cmake,
  python3, …) is build machinery, not user commands, so it no longer floods `profiles/current/bin`.
  A build-only package is a PATH
  barrier: its runtime closure is never linked through it. The one exception is an explicit request —
  `grm install clang` puts clang's own bins on PATH (you asked for it by name) without dragging its
  build-only closure along. Build-only packages stay GC roots (survive `grm clean`); `grm list` marks
  them `build-only` under `--all`. This decouples "pinned in the store" from "linked onto PATH"
  (Nix-style: build inputs live in the store but never on your PATH).
- `conflicts`/`replaces` rune metadata: mutual exclusion enforced at install time; renames
  migrate state and requested/held intent, and a bare `grm upgrade` discovers them.
- `upstream_version` metadata field for non-semver upstreams, shown by `grm info`; the
  version normalization policy is documented in rune-authoring.md.
- Content-keyed rune metadata cache (`cache/rune-meta/`), making resolves O(changed runes)
  instead of O(catalog) per command.
- Built-archive cache trust: reinstalling a removed source-built package reuses its verified
  archive from `cache/builds` instead of rebuilding.
- `grm tome build` purity lint: built archives are scanned for baked host paths
  (`/usr/local`, `/opt/homebrew`, …); warns by default, `--strict` fails the build.
- `grm tome build` linkage lint: built binaries are parsed (ELF `DT_NEEDED`, Mach-O
  `LC_LOAD_DYLIB`) for dynamic references to host libraries that are neither a declared
  dependency nor the libc/platform floor — the host-link class the purity scan cannot see (a
  library bound without a baked path). Warns by default, `--strict` fails the build.
- `grm tome build --hermetic` drops the POSIX ambient PATH tail (`/usr/bin`, `/bin`) so a
  build sees only declared deps and the managed core floor — a self-hosting diagnostic that
  surfaces silent host-userland leaks. Hermetic builds now have a distinct store hash from builds
  that can see the ambient tail, so a host-userland leak cannot share an address with the clean
  build.
- Capability runtime deps are content-addressable: the closure walker resolves providers
  like the solver (preference → installed → first by name, deterministically).
- Compiled package store hashes now fold in the resolved build-dependency closure, not only the
  runtime closure. Changing a build tool's recipe or version moves every package built with it to a
  new address. This bumps the compiled hash namespace to `grimoire-store-v5`; old installed
  packages are treated as drifted and re-realized on demand.
- Musl-target compiled store hashes now fold in the implicit managed floor (`musl`,
  `linux-headers`, and `libcxx` when installed) through the build-environment identity. A package
  built before the musl floor exists no longer shares an address with one built against the managed
  libc/header/C++ floor.
- `grm setup --bootstrap` now gates the self-hosting tail of setup: plain `grm setup` prepares the
  store and shell PATH, while the explicit bootstrap flag adds the core/world tomes and installs
  grimoire through itself.

### Changed

- Rune authoring docs now make the external-command boundary explicit: missing structured
  Nushell operations should become curated rune commands, while external invocations must come
  from declared build deps, the package source tree, or the managed POSIX floor.
- Dry-run plans and generation diffs colorize their change markers — `+` add (green), `-` remove
  (red), `~` change (yellow) — on a terminal; piped output keeps the bare `  + name` form byte-for-
  byte. Routed through a new `util::output::plan_item` so every plan renders identically.
- All user-facing output now flows through one module (`util::output`) with typed outputters in
  three tiers: result (`report` ✦ / `warn` ! / `problem` ✗ / `note`), data (`field` / `heading` /
  `print_rows` / `line`), and progress (spinner / `status` / `success` / build log). Bare
  `println!`/`eprintln!` are denied outside that module (clippy `disallowed-macros`). `info`,
  `doctor`, and `tome info` render as `key: value` fields under bold section headings; `doctor`
  health problems are now `✗` on stderr. Piped output stays plain and byte-stable.
- The managed core userland references `python3-minimal` (the stdlib-only build interpreter) rather
  than `python3`: the build-PATH floor and `grm doctor`'s readiness check are updated to match the
  tome split. The full `python3` is no longer a core package.
- Linux musl C++ builds get the managed `libcxx` (libc++) as a floor: once it is installed, every
  musl-target C++ build is pointed at it (`-stdlib=libc++` + its headers/libs, `--unwindlib=libunwind`),
  except libcxx's own build. It is injected as environment, not a declared dep, so a C++ package like
  `cmake` does not cycle with `libcxx` (whose own build deps include cmake).
- Linux musl builds retarget the compiler to musl. Once `musl` and `linux-headers` are installed,
  a musl-target build sets `--target=<arch>-linux-musl` plus a musl sysroot (`-isystem` for musl +
  kernel headers, `-B`/`-L` for musl's CRT and libc, `--rtlib=compiler-rt --unwindlib=none`), so
  the compiler stops defaulting to the host gnu/glibc triple. This closes the host-libc leak that
  made configure probes mis-detect glibc-only symbols (e.g. `sem_clockwait`) and final links pull
  the host glibc CRT. The installed `musl`/`linux-headers` prefixes are also exposed through the
  usual discovery vars (`CPATH`/`LIBRARY_PATH`/`CMAKE_PREFIX_PATH`/`<DEP>_PREFIX`) for cmake and
  pkg-config. The realized floor store hashes are folded into the build-environment identity;
  discovery vars are merged after declared-dep paths (segment-deduped) so an explicitly declared
  library keeps priority. While the floor is itself bootstrapping (musl/linux-headers not yet
  installed) the build falls back to the prior static flags at a distinct address.
- CLI consolidated: `autoremove`, `orphans`, `unrequest`, `delete-generation`, and
  `collect-garbage` removed; removal sweeps orphans in the same transaction and demotes
  still-required packages; `switch [GEN]` activates any generation (or the previous one with
  no argument); `clean [--keep N]` is the one reclamation command.
- CLI reorganized into noun groups: `pkg` (install/upgrade/remove/list/search/info/build plus
  hold/unhold/files/owns/provides/prefer), `tome`, `addendum`, and `generation`
  (list/switch/lock/restore). The seven common package verbs keep root shortcuts
  (`grm install` == `grm pkg install`; aliases ins/add, up, rm/del, ls, sea). Moves:
  `grm switch`→`grm generation switch`, `grm generations`→`grm generation list`,
  `grm restore`→`grm generation restore`, and hold/unhold/files/owns/provides/prefer→`grm pkg …`.
  New: `grm generation lock` (export the current lockfile, the inverse of `restore --lockfile`)
  and `grm tome info`. Hard cutover, no compatibility aliases.
- Removal is store-preserving: store directories survive until `grm clean` collects them, so
  switching back after remove works and reinstalls are cheap.
- Dependency reuse is content-addressed: a package whose rune drifted (same version, new
  store hash) is re-realized instead of being reused by version. Holds pin the installed
  bits, not just the version.
- Generations, the lockfile, `grm list`, and bare `grm upgrade` cover the linked set only;
  store-only packages (cached build deps, residue) never reach the profile.
- Rune command-subset violations fail at `tome add`/`info` time instead of mid-build.
- Split `src/store/closure.rs` into a directory module: `closure/mod.rs` holds the core closure
  walker (simple and split-group addressing), `closure/capability.rs` holds capability resolution,
  and `closure/stale.rs` holds drift detection / `diff_build_env`. No behavior or public API change.
- Rebuild `grimoire.lock.nuon` exactly once per mutating command, at the same finalize boundary
  that activates the generation. Previously the lockfile was regenerated per package install,
  per orphan removal, and on every `held`/`requested` flag change, so a failed multi-package
  command could leave the lock describing a half-applied state. Switching also rebuilds the lock
  from the restored generation snapshot before flipping the profile symlink.
- Introduce `InstalledWorld` as the single in-memory authority for installed-package state. Every
  command now loads `state/packages/*.nuon` once, mutates the world in memory, and commits at one
  explicit transaction boundary. The scattered `installed_states()` / `linked_set()` disk scans and
  the `O(n²)` `sweep_orphans` re-reads are removed.
- The resolver backtracks over `conflicts` metadata during version/provider selection, choosing
  compatible alternatives instead of producing plans that are refused later. `replaces` exempts the
  superseded package symmetrically; the plan-time and realize-time gates remain as safety nets.

### Fixed

- `grm tome build --all --target <triple>` now uses the requested target consistently for rune
  ordering/filtering and for store-only realization of intermediate build outputs.
- Source-root install dry-runs now validate target/conflict gates and compute dependency store
  hashes like named-package dry-runs. Real source-root installs also refuse linked conflicts before
  realizing build dependencies.
- `grm tome build --index` now fails on an unreadable or invalid archive instead of silently
  omitting it from the rebuilt package index.
- Store-hash planning now derives the musl floor identity from the resolved build-dependency
  closure, including floor packages installed earlier in the same transaction, instead of from the
  pre-plan installed world. Source-root installs also install build deps before hashing the root.
- Source extraction now handles codeload tag tarballs. `source_archive_kind` keyed off the URL
  suffix, so `codeload.github.com/.../tar.gz/0.9.0` (a dotted tag ref) was never recognized as a
  tarball and went silently unextracted — leaving `$ctx.sources.main.dir` null and failing the build
  with "Input type not supported." It now recognizes the `/tar.gz/<ref>` path form directly.
- Prebuilt build dependencies from signed tomes now verify their detached archive signatures before
  store-only installation, matching the normal linked substitute path.
- Capability resolution now reads package capabilities through the same package-index loader as
  normal resolution, so HTTP binhost indexes and local `dist/index.nuon` indexes expose the same
  providers.
- Source archive extraction now strips setuid, setgid, and sticky bits after unpacking, matching
  package archive extraction.
- Target-specific archive metadata now embeds only the sources selected for that target and
  preserves `host_libc` selectors for those sources.
- `grm setup --bootstrap` now ensures the `core` and `world` tomes specifically instead of skipping
  bootstrap whenever any tome exists, and setup now fails loudly if the self-install step fails
  after `/grm` is writable.
- `grm doctor` reports a missing macOS SDK (`xcrun --show-sdk-path`) as a health problem.
- `toolchain-wrappers` no longer assumes every non-Darwin target is musl when writing driver
  sysroot flags.
- `musl.rn` now asks clang for compiler-rt only when the host compiler is clang; GCC hosts use the
  normal `libgcc` path instead of failing on an unsupported `--rtlib` flag.
- `grm man` now generates pages for nested subcommands (`grm-pkg-install.1`,
  `grm-generation-switch.1`, …); it recursed only the top level before, so grouped subcommands had
  `--help` but no man page. Each page's synopsis uses the full `grm pkg install` invocation.
- `grm install <name>` no longer fails when the working directory holds an entry of the same name.
  Argument routing distinguished a local archive (and `find_rune` a source rune) by bare path
  existence, so a `grimoire/` source checkout next to `grm install grimoire` was handed to archive
  staging — `File::open` succeeds on a directory, so it surfaced as a cryptic `Is a directory
  (os error 21)` blamed on the transaction destination. Routing is now syntactic (a package name is
  a bare identifier; a local archive looks like a path or carries `.tar.zst`), rune lookup matches
  files only, and speculative rune resolution during a named solve degrades to "no source
  candidate" with a warning instead of aborting — an explicit `--from-source`/`.rn` install, which
  demanded that rune, stays fatal.
- A binary-index-only package (a prebuilt published with no source rune) that is also a capability
  provided by other packages no longer gets a divergent address: the closure walk addresses it by
  its own recorded hash — matching the resolver, which treats any name with candidates as literal
  — instead of resolving it as a capability to a different provider (§9.8).
- Signed remote binhosts now install: an archive's detached `.minisig` is fetched over the same
  transport as the archive and verified against the downloaded bytes, instead of being looked up
  only on the local filesystem (which never existed for an `http` repo, so a signed remote tome
  failed closed and could never serve a binary). Covered by a new served-over-HTTP test.
- Capability resolution no longer reports a satisfiable graph unsatisfiable: when the
  lexically-first provider has no version matching the requirement but another provider does, the
  one that can satisfy it is chosen. The check is requirement-aware over inputs the resolver and
  the closure walk read identically (the provider's rune-declared version plus installed
  versions), so both paths still pick the same provider and the store address stays reproducible
  (§9.8) — no graph-search backtracking, which would have made the address search-dependent.
- The shared `/grm/store` is safe under concurrent multi-user use. Mutating commands now also take
  an exclusive lock on the store directory (in addition to the per-user install-root lock), so two
  users can no longer race store mutations; and `grm clean` treats every user's generations under
  `/grm/profiles/*` as GC roots, so one user's collection can never reclaim a store path another
  user's generation still links to. Isolated `GRIMOIRE_ROOT` installs are unaffected.
- Generations are built stage-then-promote like every other state mutation: a new generation is
  assembled in a `.gen-N.staging` directory and atomically renamed into place, so a crash mid-build
  can no longer leave a registry-adoptable but snapshot-less `gen-N` (§9.1).
- `--locked` installs independently re-assert the pinned artifact at realize time: the chosen
  substitute must match both the locked archive hash and (when recorded) the locked content
  address, rather than relying solely on the solver's candidate filtering. Previously the
  `store_hash` pin went unenforced for a prebuilt-only package (its step hash was the
  substitute's own, so the drift check compared it against itself).
- Re-indexing (`grm tome build --index`) addresses each archive against the target it was built
  for (read from the archive's own metadata) rather than the indexing host's, so a cross-target
  archive is no longer registered under a hash its consumers cannot reproduce (§9.8).
- Standalone source builds recompute the store address from the rune and resolved closure and
  refuse to lay out a store prefix that disagrees with the planned hash — the same cross-check
  the split-group path already had, so a silent mis-address can no longer surface only as a
  later dropped substitution (§9.8).
- Single-package `grm tome build` now holds the install-root lock (previously only `--all` did),
  so the build deps it installs store-only can no longer be reaped by a concurrent `grm clean`.
- Reading rune metadata is now inert: the file-loading parse keywords (`use`, `source`,
  `overlay`, `module`, `register`, `plugin`) are refused before the parser runs, so reading the
  `package` record of an untrusted catalog rune can no longer open arbitrary host files at parse
  time (`grm info`/`search`/plan-time confused-deputy read). §4.3.
- Dependency resolution is bounded: an over-constrained or pathologically large requirement
  graph now aborts with a clear error after a fixed backtracking budget instead of spinning
  indefinitely (the search clones state per candidate, so the worst case was exponential).
- Addendum source patches round-trip their `platform` glob: a platform-scoped patch source no
  longer loses its constraint when the addendum manifest is re-serialized.
- Resolution surfaces a capability-index build failure (corrupt tome cache, unreadable index)
  instead of swallowing it into an empty map and reporting a misleading "no version satisfies".
- Diagnostics polish: registry/news/addendum warnings use the `warn` tier (not a hand-rolled
  `warning:` prefix on the success tier) and print the full error context; a transient read
  error while scanning a split group is surfaced rather than silently dropping a member; the
  build-environment drift summary no longer drops a tool whose name prefixes another's.
- State writes are now durable, not just atomic: `write_nuon` fsyncs the staged file before the
  rename and fsyncs the destination directory after it, and the generation-activation symlink
  flip and state-snapshot restore fsync their directory. Previously a crash right after an
  "atomic" rename could leave a present-but-empty lockfile/state file or a `current` symlink
  pointing at a generation whose contents never reached disk (§9).
- Capability provider selection is now one shared function (`solve::select_provider`) called by
  both the resolver and the closure walker, so the provider folded into a dependent's content
  address is identical on both paths. The walker previously ignored the dependency's version
  requirement when several providers were installed, so it could pick a different provider than
  the resolver — a silent store-address divergence (§9.8) that demoted binary substitution and
  produced phantom drift.
- Tome manifests reject unknown fields: a `signer` key misplaced under `packages` (the form
  the parser never read) is now a loud error instead of a silently-unsigned tome. The signer
  set is declared at the manifest's top level as `signers: [...]`. The signing docs are
  corrected to the implemented per-artifact model — each archive and the `runes-manifest.nuon`
  carry detached `.minisig` signatures verified against the pinned keys; the `index.nuon`
  itself is not signed.
- Index transport can no longer be downgraded: index fetches use a dedicated agent that
  refuses HTTP redirects, so an `https` index that 30x-redirects to plain `http` is rejected
  instead of silently re-fetched in cleartext, and a URL with embedded credentials
  (`http://[::1]@evil.com/…`) can no longer spoof the loopback exemption (AGENTS.md §10.6).
- Split-group members address their external dependencies against the resolver-chosen
  closure rather than an independent re-pick, so the address the resolver predicts always
  equals the one the build produces, and the published address stays a pure function of the
  runes. Prevents silent content-address drift and dropped binary substitution.

## 0.1.0 — unreleased baseline

Initial development line: content-addressed store, generations and semantic switching,
tome/addendum catalogs with minisign trust, source builds via embedded Nushell runes,
binhost substitution keyed by store hash.
