# Grimoire TODO

This file tracks the path from the current working package manager to the target architecture in
[`docs/design/store-and-distro-model.md`](docs/design/store-and-distro-model.md): a fixed
content-addressed `/grm/store`, profiles, generations, rollback, garbage collection, imperative
runes, and a signed binhost. The README describes the user-facing shape; this file is the work
queue.

## Focus Next

1. **Strict managed-build dogfooding with the `core` tome.**
   - Publish a real `core` tome with stage-0 archives for the supported targets.
   - Continue packaging the minimal build userland: `bash`, `make`, `coreutils`, `sed`, `grep`,
     `gawk`, `diffutils`, plus archive/compression tools and autotools as needed.
   - Keep the host C compiler/linker/system SDK as the explicit stage-0 boundary for now.
   - `doctor` should distinguish host compiler readiness from managed `core` userland readiness.
   - Source builds should use managed build-dependency bins first, then only the allowlisted host
     compiler boundary.

2. **Content-addressed store phase 1.**
   - ~~Replace `packages/<name>/<version>` as the identity of an install with
     `/grm/store/<hash>-<name>-<version>`.~~ Done: installs now promote into
     `store_root()/<hash>-<name>-<version>` (canonically `/grm/store`, or `<GRIMOIRE_ROOT>/store`
     for tests/isolated installs). The archive embeds its store basename as the install identity;
     state records the absolute store path; the `packages/<name>/<version>` layout is retired.
   - ~~Define the input hash: source hashes, rune bytes, resolved dependency closure, target triple,
     build flags.~~ Done in `store::compute_store_hash`.
   - Make package archives/substitutions keyed by store path/hash rather than name/version alone.
     Mostly done: `grm tome build` records `store_hash` in `index.nuon`, and installs now treat a
     prebuilt as a *substitute* only when its published `store_hash` matches the hash recomputed from
     the local rune (with the resolved runtime-dependency versions); a mismatch — stale sources,
     flags, or closure — falls back to a source build. The check is skipped under `--locked`, where
     the lockfile already pins the exact artifact. The store-hash formula lives in one place
     (`store::store_hash_for_metadata`), shared by the builder and the installer.
   - ~~Fold build-environment versioning into the input hash.~~ Done: the host toolchain identity
     (`toolchain::build_env_id`, the C compiler's `--version` banner, overridable via
     `GRIMOIRE_BUILD_ENV`) is part of the store hash, so a binary built against a different toolchain
     resolves to a different store path and is not substituted on a mismatched host. A host with no
     compiler boundary cannot build anyway, so it accepts the published prebuilt as authoritative.
     Next here: extend the identity beyond the C compiler (linker, system SDK) as those boundaries
     start to matter.
   - ~~Key the binhost lookup by store hash.~~ Done: the solver now merges candidates per version
     (the rune is authoritative for a version's runtime deps) and hands each plan step its rune plus
     the full set of prebuilt substitutes; the installer realizes a step by querying those
     substitutes for the wanted store hash, building from the rune when none match. A substitute
     without a published `store_hash` is unverifiable and trusted (legacy index, or a host with no
     compiler boundary).
   - Still open in #1:
     - Extend the build-environment identity beyond the C compiler (linker, system SDK) as those
       boundaries start to matter.
     - One-time privileged creation of `/grm` so installs work on a foreign host (`store_root()`
       returns `/grm/store` but nothing creates it yet).

3. **Profiles, generations, rollback, and GC roots.**
   - Replace root-level shims with **thin-profile hard-link forests**.
     - Because Grimoire bakes absolute store paths, profiles only need `bin/`, `share/man/`,
       completions, and desktop files — not a full FHS tree.
     - Each generation is a real directory tree whose files are hard links (or APFS clonefile /
       Linux reflink on CoW filesystems) into the store. Real files avoid symlink-traversal issues
       and give better `argv[0]` / `/proc/self/exe` behavior.
     - The active generation is selected by a single symlink: `profiles/current -> gen-N`.
   - Every install/remove/upgrade creates a new generation and atomically switches the active
     profile by repointing `profiles/current`.
   - Add `grm rollback` and generation listing.
   - Generations themselves are the GC roots; `grm gc` walks generation trees, collects referenced
     store basenames, and deletes unreferenced store paths.
   - Keep rollback byte-exact: it repoints `current` to an existing generation directory and never
     rebuilds.

4. **FHS compatibility layer.**
   - On a Grimoire distro, `/usr/bin` (or the user-local equivalent) is a symlink to
     `profiles/current/bin` — a real-file view, not a symlink farm.
   - Do not globally symlink libraries into `/lib` or `/usr/lib`.
   - Design the bounded foreign-binary compat world: loader path, default library set, and an
     invocation story similar to `nix-ld`, `buildFHSEnv`, or `steam-run`.

5. **1.0 self-hosting.**
   - Package a managed compiler/linker/runtime story for each supported platform.
   - Package enough Rust toolchain for Grimoire to build Grimoire through Grimoire.
   - Treat this as a 1.0 milestone after the store/generation foundation is real.

## Trust / Supply Chain

- **Signed source rune digests.** Signed binary indexes authenticate prebuilt archives, but source
  runes are still trusted as git content. Generate and sign a digest over rune paths + sha256, and
  verify it against the tome's pinned key before source resolution/build.
- **Signed addenda.** Addenda can change source URLs, hashes, dependencies, and build flags, so
  apply the same signature/TOFU model to `addendum.nuon`.
- **Stronger trust establishment.** Add `grm tome add --signer <key>` for out-of-band pinning,
  deliberate key-rotation acceptance, and a policy knob for requiring signatures once `core` and
  official tomes are signed.

## Release Engineering

- **Multi-OS CI matrix.** Run `cargo test` on Linux, macOS, and Windows, plus an MSRV job pinned to
  `Cargo.toml`'s `rust-version`.
- **Release workflow.** Produce signed `grm` release archives for supported targets so users do not
  need Rust installed.
- **`grm self-update`.** Blocked on signed release artifacts.
- **`CHANGELOG.md`.** The lockfile, package index, addendum, and future store schemas are
  user-visible; document breaking changes before releases rely on them.

## Documentation

- Keep [`docs/layout.md`](docs/layout.md) accurate for the current implementation until the store
  migration lands.
- Keep [`docs/design/store-and-distro-model.md`](docs/design/store-and-distro-model.md) as the
  target architecture, not a claim about current behavior.
- Expand the threat model as source-rune signatures, addendum signatures, and the fixed store
  arrive.
- Add authoring docs for the final-prefix/staging build contract as soon as it becomes the rune
  API.

## Done

1. **Native package manager core.** Grimoire is a Rust CLI embedding the Nushell engine for `.rn`
   execution. Git, archive, compression, HTTP, NUON, signing, and install logic are implemented
   natively in-process; Grimoire does not shell out for its own machinery.
2. **Git-native tomes.** Tomes are git repositories containing `tome.rn` and `runes/`; Grimoire can
   add, update, list, remove, cache, and validate them. Updates report ref/commit movement and
   `upgrade` syncs tomes before resolving versions.
3. **Rune source builds.** Runes declare metadata, dependencies, sources, build flags, bins, and an
   imperative Nushell `build` function. Source archives are fetched, checksum-verified, safely
   extracted natively (`.tar.gz`, `.tar.xz`, `.tar.zst`), and built in a controlled context.
4. **Binary package indexes.** Tomes can publish `dist/index.nuon` plus `.tar.zst` archives over
   local paths or HTTP. Installs prefer verified target-matching binaries and fall back to source.
5. **Version-aware solver.** Installs resolve semver constraints across runtime dependencies and
   order dependencies before dependents. Build dependencies are installed just in time for source
   builds.
6. **Transactional installs and removals.** Installs stage work and promote atomically, with rollback
   when later state/shim steps fail. Removes cascade orphaned runtime dependencies.
7. **Lockfile and locked installs.** `grimoire.lock.nuon` records package/tome/addendum state,
   versions, hashes, dependencies, and tome commits. `install --locked` constrains resolution to the
   recorded graph.
8. **Addenda.** Addenda are data-only overlays that patch package metadata and build flags without
   executing hooks.
9. **Signed binary indexes.** Tome indexes may be signed with minisign. Keys are TOFU-pinned on
   first sync; later unsigned, invalid, or key-rotated indexes are refused.
10. **CLI ergonomics.** `install`, `remove`, `upgrade`, `hold`, `unhold`, `clean`, `doctor`,
    `search`, `info`, `list`, shell completions, and man page generation work. Output follows the
    stderr-progress/stdout-result split, with quiet/verbose modes and live build-log tails.
11. **Concurrency lock.** Mutating commands take an OS advisory lock on the install root.
12. **Core tome bootstrap work.** A local `tome-core/` exists and can package real GNU tools using
    Grimoire-managed build dependencies plus the host compiler boundary.
13. **Store-prep build contract.** Source builds now separate staging from final prefix:
    `ctx.package_dir` is the transaction staging root, `ctx.prefix`/`ctx.store_path` point at the
    final package path, configure-style runes can use `--prefix` plus `DESTDIR`, and built package
    metadata records and validates the final `store_path`.

## Testing Gaps

- CLI output across verbosity levels, including TTY-only spinner/color behavior.
- Windows shim generation and execution on a real Windows CI runner.
- Broader addendum combinations: dependency policy, target policy, binary/source preference, and
  signature enforcement.
- Store/generation behavior once the new model lands: profile switch atomicity, rollback, GC roots,
  and foreign-binary compatibility.

## Current Baseline

Run the full local gate with:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
