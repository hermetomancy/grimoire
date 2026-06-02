# Grimoire TODO

## Missing functionality

The README is an end-user overview; this is what is designed but not built yet.

1. **Addendums**
   - Entirely stubbed in `main.rs` (`would add ...` / `addendum state is not wired yet`).
   - Persist addendum state as NUON, clone via `gix`, and patch rune data declaratively
     (sources, mirrors, checksums, build flags, target policy, metadata).
   - Data-only — no execution hooks (AGENTS.md §5.5).

## Documentation

- Write `///` rustdoc on the public surface (modules, key types, command entry points)
  so `cargo doc` produces useful API documentation. This is the documentation plan —
  no separate prose docs for now.

## Done

1. **Source fetching and checksum verification** — runes' declared `sources.*` are fetched
   into the build context via a native HTTP client (`ureq`, no shelling out) and verified
   against the rune's `sha256` before `build` runs; a mismatch aborts the build.
2. **Binary package index (`index.nuon`)** — schema modelled and read/validated through
   `nuon_io`; a tome's `packages.repo` / `packages.index` resolve real archives.
3. **Binary download and resolution** — a bare package name resolves against configured tome
   indexes, preferring a target-matching binary archive (download + `archive_hash` verified
   before extraction) over a source build.
4. **Dependency resolution** — runtime deps install with a package; build deps install before
   a source build. Name-based, cycle-guarded, no constraint solver (deferred scope).
5. **Lockfile (`grimoire.lock.nuon`)** — regenerated under install-root state after every
   install/remove from recorded package + tome state (target, versions, archive/source
   hashes, runtime/build deps, tome commits). `upgrade` reinstalls and so refreshes it.
6. **`doctor` health checks** — validates configured tome caches, installed-state integrity
   (package dirs + shims), and lockfile presence; counts to stdout, problems to stderr.
7. **Tome authoring (`grm tome init` / `grm tome rune`)** — scaffolds a new tome (manifest,
   `runes/`, `sources/`, empty index) and templated, buildable runes, so a catalog can be
   authored locally and installed from without hand-writing the layout.
8. **Tome publishing (`grm tome build`)** — builds a tome's rune into a `.tar.zst` under
   `packages/` and upserts its entry (name, version, target, archive path, hash, runtime deps)
   into the tome's `index.nuon`, so prebuilt archives can be published from a local tome
   (`packages.repo = "."`). External package repos are not supported yet.

## Testing gaps (AGENTS.md §8)

- CLI output and `--quiet` behavior (stderr vs. stdout split).
- Windows shim generation/execution (blocked on a Windows test environment).
- Addendum-override end-to-end fixtures (blocked on the remaining item).

## Current working baseline

- Single self-contained Rust binary embedding the Nushell engine; native git/tar/zstd
  via `gix`/`tar`/`zstd`, no shelling out (AGENTS.md §0, §1a).
- `src/` is modularised: `archive/`, `nu/`, `tome/` with module roots plus `build`,
  `install`, `fetch`, `index`, `resolve`, `lock`, `doctor`, `query`, `paths`, `model`, `cli`.
- Build, install (binary archive from a tome index, local archive, or source build),
  remove, list, search, info, upgrade, and a health-checking doctor are functional.
- Source builds fetch and checksum-verify declared sources; bare-name installs prefer a
  verified binary archive and pull in runtime/build dependencies (cycle-guarded).
- A `grimoire.lock.nuon` snapshot is regenerated under install-root state on every change.
- Installs stage into a transaction directory and promote with atomic renames; failures
  after promotion (shims, state writes) roll back to the prior version (AGENTS.md §4).
- Security invariants §5.1–§5.4 are enforced and tested: verify-before-trust checksums on
  every download (sources and archives), `--sha256` archive verification before
  read/extract, archive member path validation, symlink rejection, user-local install root
  with no privilege escalation.
- Installed and tome state are inspectable NUON under the install root, read/written only
  through the `nuon_io` layer. Tome sync records refs and commits; runes resolve from
  cached tome repositories.
- CLI parsing via `clap`: rejects extra positionals, supports `--flag=value`, reports
  option errors.
- Tests: Rust unit tests colocated with code plus the integration suite in
  `tests/smoke.rs`, all offline.

Run the full test suite with:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
