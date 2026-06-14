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

Initial development line: content-addressed store, generations and semantic rollback,
tome/addendum catalogs with minisign trust, source builds via embedded Nushell runes,
binhost substitution keyed by store hash.
