# Grimoire TODO

## Focus next

The README is an end-user overview; this is what is designed but not built yet, in priority
order. The theme: the catalog/solver/install machinery is done, but nothing has actually
*compiled* a real package — closing that gap comes first.

1. **Strict managed-build dogfooding (`core` tome + host compiler precheck).**
   - Ship a minimal `core` tome of prebuilt, relocatable build tools (`make`, `pkg-config`, `m4`,
     `autoconf`, `automake`, `libtool`, `gettext`, `bash`, `coreutils`, `sed`, `gawk`, `grep`,
     `tar`, `gzip`, `xz`, `zstd`).
   - Use the **host** C toolchain (cc/binutils/libc/system SDK) only as the explicit stage-0
     compiler boundary. Source builds should otherwise prefer the Grimoire-managed `core` tools
     and run in a strict build environment: build-dependency `bin/` directories first, then only
     an allowlisted host fallback for the compiler/linker/system SDK pieces we have not packaged.
   - Add a `doctor`-style precheck that fails source builds early with a clear message when the
     host compiler boundary is missing, and separately reports whether the managed `core`
     userland is installed and usable for source builds.
   - Bootstrap flow: use the host toolchain to build stage-0 `core` archives, install those
     archives through Grimoire, then rebuild at least one source package (eventually `core`
     itself) using the installed `core` tools plus the allowlisted host compiler boundary.

2. **1.0 self-hosting goal: managed compiler + Grimoire builds Grimoire.**
   - Package a relocatable compiler/linker/runtime story for each supported platform (`clang`/`lld`,
     `zig`, GCC/binutils/libc, or another reviewed approach). This is gated on platform-specific
     relocatability work such as RPATH/loader handling, macOS SDK handling, and Windows ABI policy.
   - Once the compiler boundary is managed by `core`, package the Rust toolchain enough for
     Grimoire to build itself through Grimoire. Treat this as a 1.0 milestone, not the first
     dogfooding implementation.

## Hardening / real-world gaps

Not part of the original MVP design, but a package manager wants these before it is trustworthy
in daily use. Listed so they are tracked, not necessarily scheduled.

- **`grm self-update`.** Blocked on the release-engineering work below: until tagged, signed
  release artifacts exist for each target, a self-update command has nothing to download.
- **Published `core` catalog.** Same story — the runes are designed but no public tome ships
  the prebuilt archives yet.

## Trust / supply chain

Signed binary indexes (below, Done #19) close the static-archive-host gap. These remaining items
extend the same model to source runes and addenda and harden the trust-establishment step.

- **Signed source runes.** Index signing covers the published binary index, but a tome's
  *source* runes are still trusted on faith. Sign a generated digest of the runes (each rune's
  sha256) with the same minisign machinery, verified on source resolve against the pinned key.
- **Signed addenda.** An addendum can swap a source URL + sha256, so it is as dangerous as a
  tome. Apply the index/digest signing model symmetrically: `addendum.nuon` declares a signer,
  the addendum publishes a signature, and the key is TOFU-pinned in `AddendumState`.
- **Stronger trust establishment.** An explicit `grm tome add --signer <key>` to pin out of
  band (defeating the first-use attack), deliberate key-rotation acceptance (a signed rotation
  statement, or a `grm tome resign`-style re-pin), and an optional `require_signatures` policy
  / a signed-required `core` once the ecosystem has signed catalogs.

## Release engineering

Today `.github/workflows/` has only `lint.yml`, and `cargo install grimoire` is the only install
path the README advertises — which is in tension with the "one binary, every desktop" promise.

- **Multi-OS CI matrix.** Run `cargo test` on linux/macos/windows, plus an MSRV job pinned to
  the `rust-version` in `Cargo.toml`, alongside the existing `cargo fmt --check` and
  `cargo clippy --all-targets -- -D warnings`.
- **Release workflow.** Tag-triggered job that cross-builds `grm` for every supported target
  triple and attaches the archives to a GitHub release, so users on Linux/macOS/Windows can
  install without a Rust toolchain.
- **`CHANGELOG.md`.** The lockfile, `index.nuon`, and `addendum.nuon` schemas are user-visible;
  we want a changelog in place before the first breaking change ships.

## Documentation

- Module-level `//!` docs cover every module and `///` rustdoc documents key types and command
  entry points, so `cargo doc --no-deps` builds warning-free. Extend coverage as the surface
  grows.
- Prose docs in `docs/`: a threat model (what `grm` does and does not trust, given git-native
  catalogs + arbitrary build scripts + addenda) and an operating-layout reference (where state
  lives under the install root, what is safe to delete, how to relocate via `GRIMOIRE_ROOT`).

## Done

1. **Source fetching and checksum verification** — runes' declared `sources.*` are fetched
   into the build context via a native HTTP client (`ureq`, no shelling out) and verified
   against the rune's `sha256` before `build` runs; a mismatch aborts the build.
2. **Binary package index (`index.nuon`)** — schema modelled and read/validated through
   `nuon_io`; a tome's `packages.repo` / `packages.index` resolve real archives.
3. **Binary download and resolution** — a bare package name resolves against configured tome
   indexes, preferring a target-matching binary archive (download + `archive_hash` verified
   before extraction) over a source build.
4. **Version-aware dependency resolution** — a backtracking solver (`src/solve.rs`) picks a
   concrete version for every package in the runtime graph that satisfies all accumulated
   semver requirements; candidates are a tome index's binary archives plus the source rune,
   highest satisfying version first (binary preferred over source at equal version). Build
   deps install just-in-time before a source build. Cycle-guarded; ordered deps-before-dependents.
5. **Lockfile (`grimoire.lock.nuon`)** — regenerated under install-root state after every
   install/remove from recorded package + tome state (target, versions, archive/source
   hashes, runtime/build deps, tome commits). `upgrade` reinstalls and so refreshes it.
   `install --locked` reads it back and constrains resolution to the recorded versions and
   archive hashes (rejecting anything not in the lock) for a reproducible reinstall.
6. **`doctor` health checks** — validates configured tome caches, installed-state integrity
   (package dirs + shims), and lockfile presence; counts to stdout, problems to stderr.
7. **Tome authoring (`grm tome init` / `grm tome rune`)** — scaffolds a new tome (manifest,
   `runes/`, `sources/`, a git-ignored `dist/`, and `.gitignore`) and templated, buildable
   runes, so a catalog can be authored locally and installed from without hand-writing the layout.
8. **Tome publishing (`grm tome build`)** — builds a tome's rune into a `.tar.zst` under the
   git-ignored `dist/` and upserts its entry (name, version, target, archive filename, hash,
   runtime deps) into `dist/index.nuon`. `--all` builds every rune in the tome in one pass.
   `dist/` is the publishing unit: the git repo carries only runes + `tome.rn`, and `dist/` is
   uploaded to a static webserver. Manifests select the repo via `packages.format`: `"http"`
   (repo is an http(s) base URL — index and checksum-verified archives fetched over HTTP, with
   connect/read timeouts and bounded retries on transient failures) or `"local"` (repo is a
   filesystem path, for testing).
9. **Output and verbosity** — global `--quiet`/`--verbose` flags select a process-wide level
   (`src/progress.rs`): granular progress collapses into a transient pacman-style spinner on
   stderr at the default level, becomes persistent colored step lines under `--verbose`, and is
   suppressed under `--quiet`. Color and decorations are TTY-gated and `NO_COLOR`-aware, so piped
   output stays plain. Result lines go to stdout (AGENTS.md §7); `install` reports a no-op when a
   package is already up to date instead of printing nothing.
10. **Build-environment contract** — native `.tar.zst` source extraction, build-dependency `PATH`
    wiring, and a configure/make/install-style source package fixture are covered by smoke tests.
    Author-facing README prose documents `ctx.sources.<name>.path`, optional
    `ctx.sources.<name>.dir`, `ctx.env.PATH`, `ctx.package_dir`, `ctx.work_dir`, and `ctx.prefix`.
11. **Addenda** — `grm addendum add/list/remove` persists NUON state, clones/copies addendum
    repositories natively, records addenda in the lockfile, and applies inert `addendum.nuon`
    package metadata patches to rune source candidates before search/info, resolution, source
    fetching, and builds. Patched `build_flags` are exposed as inert `ctx.build_flags`; no
    addendum hooks execute.
12. **Cascade autoremove on `grm remove`** — removing a package also removes any runtime
    dependencies it pulled in that no other installed package still lists in its
    `runtime_deps`, walked transitively. Build deps are not considered; once a package is
    installed they are no longer load-bearing for it. Each cascaded remove is its own
    transaction, so a failure mid-cascade leaves earlier removals committed and a clean state.
14. **Ephemeral build dependencies** — a source install resolves and installs the rune's
    `deps.build`, runs the build, then uninstalls every build dep it pulled in itself at the
    end of the run (build deps the user already had stay; build deps now referenced by an
    installed package's `runtime_deps` stay). The downloaded `.tar.zst` remains in
    `cache/archives/`, so a future install that needs them is a cheap re-extract rather than
    a re-download or re-build.
13. **`grm clean`** — empties `cache/sources/`, `cache/archives/`, `cache/builds/`, and any
    leftover `transactions/` staging directories under the install root, reporting bytes
    freed. Installed packages, shims, state, tomes, addenda, and the lockfile are untouched;
    everything cleaned is reproducible from the original sources on the next install.
15. **Concurrency lock** — mutating CLI entry points (`install`, `remove`, `upgrade`, `clean`,
    `hold`/`unhold`, `tome add/update/remove`, `addendum add/remove`) acquire an exclusive
    OS-level advisory lock on `<install root>/.grimoire-lock` before doing any work and hold
    it until the command exits, so two concurrent mutating runs can't corrupt shared state.
    Read-only commands (`list`, `search`, `info`, `doctor`, `--dry-run` previews) skip it.
    Crash-safe: the file lock is released by the OS at process exit, never leaves a stale
    sentinel.
16. **Hold / unhold** — `grm hold <pkg>` (alias `pin`) and `grm unhold <pkg>` (alias `unpin`)
    flip a `held: true` flag on the package's state record. `grm upgrade` skips held packages
    with a message; `grm upgrade <held>` named explicitly fails fast pointing at `grm unhold`.
    The flag is preserved across reinstalls and shown in `grm list` as a fourth column.
17. **`--dry-run` / `--explain`** — on `grm install` and `grm upgrade`, prints the solver's
    resolved plan (package, version, source rune or binary archive) to stdout and exits
    without touching state, fetching, or building. Non-mutating, so it bypasses the
    install-root lock and can preview while another `grm` is mid-mutation.
18. **Shell completions and man pages** — `grm completions <shell>` (bash/zsh/fish/powershell/
    elvish) writes a completion script to stdout via `clap_complete`. `grm man --output <dir>`
    renders `grm.1` plus a `grm-<sub>.1` per subcommand via `clap_mangen`. Both derive from
    the same `Cli` definition the binary uses, so they stay in sync as the CLI evolves.
19. **Signed package index (minisign) + TOFU (Phase 1 of trust/supply-chain)** — a tome may
    declare a minisign public key in its manifest (`packages.signer`) and publish a detached
    `index.nuon.minisig`. `grm` verifies the signature over the index before parsing it or
    fetching any archive; since the index already records every archive's sha256, the signature
    transitively authenticates every binary package (`src/signing.rs`, verify-only via
    `minisign-verify` — authors sign with the standard `minisign` tool). The key is pinned on
    first sync (trust-on-first-use, stored as `signer_pubkey` in tome state); a later sync that
    drops the signature, fails to verify, or advertises a different key is refused. Unsigned
    tomes keep working (verify-if-present). Plus `SECURITY.md` and a threat-model update.

## Testing gaps (AGENTS.md §8)

- **CLI output across verbosity levels.** The `--quiet`/`--verbose` stdout-vs-stderr split, the
  pacman-style spinner + color path (TTY-gated, `NO_COLOR`-aware), and the `install` no-op
  "already installed and up to date" message all currently lack assertions. Tests run with
  captured (non-TTY) output, so the spinner/color are auto-disabled — add coverage for the plain
  output each level produces.
- Windows shim generation/execution (blocked on a Windows test environment).
- Addendum coverage is source-metadata focused; broader overlay combinations (dependency policy,
  target policy, and binary/source preference interactions) could use more end-to-end fixtures.

## Current working baseline

- Single self-contained Rust binary embedding the Nushell engine; native git/tar/zstd
  via `gix`/`tar`/`zstd`, no shelling out (AGENTS.md §0, §1a).
- `src/` is modularised: `archive/`, `nu/`, `tome/` with module roots plus `build`,
  `install`, `fetch`, `index`, `solve`, `lock`, `doctor`, `query`, `paths`, `model`, `cli`.
- Build, install (binary archive from a tome index, local archive, or source build),
  remove, list, search, info, upgrade, and a health-checking doctor are functional.
- Source builds fetch and checksum-verify declared sources; bare-name installs resolve a
  version-satisfying set across the dependency graph, prefer a verified binary archive over a
  source build, and pull in runtime/build dependencies (cycle-guarded). Binary archives and
  their index are served either from a local filesystem path or over HTTP.
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
