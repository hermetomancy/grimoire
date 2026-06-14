# Changelog

All notable changes to Grimoire. Format follows [Keep a Changelog](https://keepachangelog.com/);
versions follow semver. Entries land in **Unreleased** as they merge and move under a version
heading when it is tagged.

## Unreleased

### Added

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
  surfaces silent host-userland leaks. No effect on the store hash.
- Capability runtime deps are content-addressable: the closure walker resolves providers
  like the solver (preference → installed → first by name, deterministically).

### Changed

- CLI consolidated: `autoremove`, `orphans`, `unrequest`, `switch`, `delete-generation`, and
  `collect-garbage` removed; removal sweeps orphans in the same transaction and demotes
  still-required packages; `rollback [GEN]` absorbs switch; `clean [--keep N]` is the one
  reclamation command.
- Removal is store-preserving: store directories survive until `grm clean` collects them, so
  rollback after remove works and reinstalls are cheap.
- Dependency reuse is content-addressed: a package whose rune drifted (same version, new
  store hash) is re-realized instead of being reused by version. Holds pin the installed
  bits, not just the version.
- Generations, the lockfile, `grm list`, and bare `grm upgrade` cover the linked set only;
  store-only packages (cached build deps, residue) never reach the profile.
- Rune command-subset violations fail at `tome add`/`info` time instead of mid-build.

### Fixed

- Split-group members address their external dependencies against the resolver-chosen
  closure rather than an independent re-pick, so the address the resolver predicts always
  equals the one the build produces, and the published address stays a pure function of the
  runes. Prevents silent content-address drift and dropped binary substitution.

## 0.1.0 — unreleased baseline

Initial development line: content-addressed store, generations and semantic rollback,
tome/addendum catalogs with minisign trust, source builds via embedded Nushell runes,
binhost substitution keyed by store hash.
