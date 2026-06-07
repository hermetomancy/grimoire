# Autonomous Agent Guidelines

Binding engineering rules for the Grimoire codebase. When a rule and convenience conflict,
rule wins. When two rules conflict, prefer correctness and safety, then clarity, then brevity.

## 0. What Grimoire is

Grimoire is a **Rust program that embeds Nushell**. The CLI, package manager core,
transaction logic, and orchestration are Rust. Nushell executes rune (`.rn`) build scripts
in-process and reads/writes NUON data.

`git`, `tar`, `zstd`, and HTTP are provided by linked Rust crates (`gix`, `tar`, `zstd`, `ureq`).
Grimoire does not shell out for its own machinery.

## 1. No shelling out

1. Do not use `std::process::Command` to invoke external CLIs. All Grimoire functionality is
   native and in-process.
2. The only executed code is the embedded Nushell engine running `.rn` rune build scripts.
3. A rune's `build` function may invoke its package's build tooling (`make`, `cc`) — that is the
   package's business, not Grimoire's.
4. If a capability seems to require an external tool, find or vendor a Rust crate. "No suitable
   crate" is a design problem, not a license to shell out.
5. **Exception:** read-only host toolchain discovery (`cc --version`, `ld --version`,
   `xcrun --show-sdk-version`, etc.) may shell out because version strings and platform
   identities are not embedded in a parseable form in all binary formats. This is limited
   to `src/toolchain.rs` and only for identity discovery, not for builds.

## 2. Rust idiom

1. Idiomatic Rust for edition 2024. Prefer the standard library and established crates.
2. `unsafe` is forbidden without a `// SAFETY:` comment stating the upheld invariant.
3. No `unwrap()`, `expect()`, or `panic!` in non-test code. Use `expect()` only for provably
   unreachable invariants, with a message explaining why.
4. All fallible functions return `anyhow::Result<T>`. Attach `.context(...)` / `.with_context(...)`
   at every boundary where a bare error would be ambiguous.
5. Prefer `&str`/`&Path`/`&[T]` in signatures; take ownership only when storing or consuming.
6. Derive (`Debug`, `Clone`, `Serialize`, `Deserialize`, `PartialEq`) by default. Implement by
   hand only when the derive is wrong.
7. Code must pass `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

## 3. Functions and structure

1. Functions do one thing. A function body that does not fit on one screen is a refactor candidate.
2. Nesting beyond three levels is a smell. Use early returns and guard clauses.
3. Follow DRY, but do not invent abstractions for a single caller.
4. Modularise aggressively. Group related modules under folders with a clear root.
5. Keep surfaces minimal and intentional. Do not add `pub` just for cross-module convenience.

## 4. Data formats: the .rn / .nuon contract

> **If Grimoire runs it, it is `.rn`; if Grimoire reads it, it is `.nuon`.**

1. `.rn` files are executable Nushell. They are the only place arbitrary package logic runs.
2. `.nuon` files are inert structured data. Lockfiles, indexes, metadata, and local state are NUON.
3. Exported rune metadata (`package` record) is inert data and must be read as data. Build
   functions run only inside the controlled build context.
4. All NUON read/write goes through `nuon_io`. Do not parse or serialize NUON ad hoc elsewhere.

## 5. Build environment

Managed builds receive a controlled `PATH` in strict priority order:

1. **Core package `bin/` directories** — always prepended. Once bootstrapped, the core
   toolchain (toybox, toolchain-wrappers, etc.) takes precedence over everything.
2. **Build dependency `bin/` directories** — explicit deps declared by the rune being built.
3. **Host compiler boundary symlinks** (`cc`, `c++`, `ar`, `ld`, etc.) — **bootstrap only**.
   Skipped entirely once `toolchain-wrappers` is installed; core compilers take over.
4. **POSIX ambient directories:** `/usr/bin` and `/bin` — permanent platform fallback.

**Rule:** Declare only non-POSIX tools that the build script calls explicitly. Do not declare
POSIX utilities (`sed`, `grep`, `awk`, `find`, `coreutils`, `diffutils`) as build deps — they are
always available via the ambient directories (or, once bootstrapped, via core toybox).

Managed builds also receive a sandboxed environment:

1. Host discovery variables are cleared before the rune runs (`CMAKE_PREFIX_PATH`,
   `PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, language package-manager roots, Homebrew prefixes,
   dynamic-library search paths, etc.).
2. Build dependency prefixes are layered back in explicitly through managed discovery variables
   such as `CMAKE_PREFIX_PATH`, `PKG_CONFIG_PATH`/`PKG_CONFIG_LIBDIR`, `CPATH`, `LIBRARY_PATH`,
   `ACLOCAL_PATH`, and `<DEP>_PREFIX`.
3. `HOME`, `TMPDIR`, `TEMP`, `TMP`, and XDG directories point inside `ctx.work_dir`, so package
   build systems cannot read or write the user's normal home/config/cache/temp state by default.
4. External commands launched by the embedded Nushell build runner receive blank overrides for
   inherited host environment variables unless Grimoire deliberately sets them.

**Rule:** If a build system needs to discover a dependency, that dependency must be declared in
`deps.build`; do not rely on host env vars, Homebrew/MacPorts prefixes, language package-manager
state, or the user's shell configuration.

## 6. Dependencies

### Capability-based resolution

A rune's `bins` map declares provided commands. Any key that differs from the package `name` is a
**capability** (e.g. `gawk` provides `gawk` and `awk`).

- Literal names resolve directly (`gawk` → `gawk`).
- Capability names fall back to any package whose `bins` map contains the name (`awk` → `gawk`).

Prefer capability names when you need the command semantically (`awk`); use literal names when
you require a specific implementation (`gawk`).

### Dependency categories

- **`deps.runtime`** — packages required at execution time. Resolved by the solver and installed
  into the active generation.
- **`deps.build`** — tools required during the build. Their `bin/` dirs are prepended to PATH for
  the rune's build context.
- **`deps.features`** — *(future work)* execution-time capabilities for FHS compatibility.

### Platform-conditional build deps

A build dependency may carry a platform filter in brackets: `'name[platform-glob]'`.
The dep is included only when the current target triple matches the glob. This lets runes
declare platform-specific tools (e.g. `linux-headers[linux-*]`) without breaking builds on
other targets. Globs may match the full triple or a prefix (e.g. `linux-*-musl`).

## 7. Rune authoring

A rune is a Nushell module exporting `package` (inert metadata) and `build` (the build function).
Runes are the only place arbitrary package logic lives (§4). Follow these conventions so builds are
predictable, portable, and compose correctly.

### Structure

```rn
export const package = {
  name: "example"
  version: "1.0.0"
  summary: "One-line description"
  homepage: "https://example.com/"
  license: "MIT"
  targets: ["linux-x86_64-musl" "macos-aarch64-darwin"]

  sources: {
    main: {
      url: "https://example.com/example-1.0.0.tar.gz"
      sha256: "sha256:..."
    }
  }

  deps: {
    build: { default: ["cmake"] }
    runtime: ["libc"]
  }

  bins: {
    example: "bin/example"
    ex: "bin/example"  # capability alias
  }
}

export def build [ctx] {
  let source_dir = ($ctx.sources.main.dir | path join "example-1.0.0")
  cd $source_dir
  ./configure --prefix=$ctx.prefix
  make -j($ctx.nproc)
  make install DESTDIR=$ctx.package_dir
}
```

### The `ctx` record

| Field | Type | Meaning |
|---|---|---|
| `ctx.package_dir` | string | Staging root for this build. Install files here; Grimoire packs this directory into the archive. |
| `ctx.prefix` | string | Final install prefix (e.g. `/grm/store/<hash>/example-1.0.0`). Bake this into configure-time metadata so the package knows where it will live. |
| `ctx.store_path` | string | Alias for `ctx.prefix`. |
| `ctx.work_dir` | string | Scratch directory for build artifacts. Use for out-of-tree builds. |
| `ctx.target` | string | Target triple (e.g. `linux-x86_64-musl`). |
| `ctx.sources.<name>.dir` | string | Extracted source directory for the named source. Always use `.dir`, not `.path` (which is the raw archive). |
| `ctx.sources.<name>.path` | string | Raw archive path in the cache. Rarely needed. |
| `ctx.build_flags` | record | Key-value flags from the rune metadata. Use for feature toggles. |
| `ctx.env.PATH` | string | The build PATH (§5). |
| `ctx.env.GRIMOIRE_VERBOSITY` | string | `"quiet"`, `"normal"`, or `"verbose"`. |
| `ctx.env.*` | string | Extra env vars from `BuildEnv` (e.g. `CC`, `CFLAGS` for musl static builds). |

### Installation convention

**Install into `ctx.package_dir`, not `ctx.prefix`.** `package_dir` is the staging area that gets
packed into the archive. `prefix` is the final location after extraction. For autotools:

```rn
./configure --prefix='($ctx.prefix)'
make install DESTDIR='($ctx.package_dir)'
```

For CMake:

```rn
cmake -S . -B build -DCMAKE_INSTALL_PREFIX='($ctx.prefix)'
cmake --build build
cmake --install build --prefix '($ctx.package_dir)'
```

### Build script patterns

**Do not use `sh -c` in runes.** Rune `build` functions are native Nushell. Use Nushell's own
control flow, variables, and external command invocation. Shelling out to `sh` forfeits error
handling, obscures the build logic, and makes runes harder to read and test. If a build step is
too complex for native Nushell, decompose it into smaller steps rather than hiding them in a
shell script.

**Use explicit parentheses for variable interpolation in external commands.** Bare record-field
access like `$ctx.prefix` or `$env.VAR` in external command position can be parsed incorrectly by
Nushell, silently producing wrong paths or empty strings. Always wrap the expression in
parentheses: `($ctx.prefix)`, `($env.VAR)`, `($nproc)`. This applies to both `ctx` fields and
local variables when they are passed to external commands.

**Parallel builds:** Pass `-j` to `make` and parallel flags to build systems. The build environment
provides `ctx.nproc` for the host's parallelism. If absent, omit the flag and let the build system
default.

**Out-of-tree builds:** Use `ctx.work_dir` for build artifacts when the source system supports it
(CMake, Meson). This keeps the source tree clean and avoids packing build artifacts.

### Platform conditionals

Use `ctx.target` for platform-specific logic. Prefer prefix matching over exact triples:

```rn
let is_macos = ($ctx.target | str starts-with "macos-")
let is_linux = ($ctx.target | str starts-with "linux-")
let is_musl = ($ctx.target | str ends-with "-musl")
```

Keep platform logic in the rune, not in Rust. The Rust side only provides the target triple.

### `bins` conventions

- The package `name` is always implicitly a bin (e.g. `example` → `bin/example`).
- Additional entries are **capabilities**: `ex: "bin/example"` means `ex` resolves to this package.
- Paths are relative to `package_dir` (e.g. `bin/foo`, `sbin/bar`, `libexec/baz`).
- Only declare commands that end users or other runes invoke. Internal helpers (private scripts)
  do not need entries.

### `targets` filtering

A rune whose `targets` list is non-empty and does not include the current triple is skipped by
`grm tome build --all`. Use this for platform-specific packages (`linux-headers`, `musl`). A
run with no `targets` builds on every platform.

### No sources

A rune may declare `sources: {}` and generate all outputs in `build` (e.g. `toolchain-wrappers`,
which writes shell wrapper scripts). This is valid for meta-packages and pure-script tools.

## 8. Store-only installation

`grm tome build` installs built packages **store-only**: extracted to the store and recorded in
`state/packages/{name}.nuon`, but the lockfile and active generation are not updated. This lets a
tome bootstrap itself (`grm tome build --all`) without polluting the user's PATH. Single-package
`grm tome build` also installs missing build deps store-only before building.

## 9. Transactional state

Grimoire has no database. Durability is explicit transaction directories plus atomic `rename`.

1. Never mutate an installed package directory or state file in place. Stage, then promote.
2. An operation either fully completes or leaves the previous state intact. Because state is
   promoted via atomic `rename`, a failure partway through leaves the old state untouched.
3. Mutating package commands are command-atomic. `grm install a b c`, `grm remove x y`, upgrades,
   and any dependency/autoremove work they trigger either commit the whole requested state change
   (store paths, package state, lockfile, and active generation) or commit none of it. On any error,
   no package from that command is installed, removed, upgraded, autoremoved, held, or unheld.
4. Installed package version directories are immutable once promoted. Upgrades create new version
   directories.
5. Local state is inspectable NUON under the install root. No databases.

## 10. Security invariants

These must never be regressed:

1. **Verify before trust.** Checksum every downloaded source and archive. Hash mismatch is fatal.
2. **Validate every archive member path.** Reject traversal (`..`), absolute paths, and escapes.
3. **Reject unsafe archive contents.** Hard links are rejected. Symlinks are allowed only when the
   target resolves within the package, and no member may be nested under a symlink.
4. **No privilege escalation.** Installs target a user-local root and must not require or assume
   root/admin. Never write outside the install root.
5. **Rune/addendum execution is the trust boundary.** Addendums patch data only. Do not let
   addendum data trigger execution.

## 11. Platform support

Grimoire is **POSIX-only**: Linux, macOS, FreeBSD. No `#[cfg(windows)]` code.
Platform-gated code for supported POSIX targets (`macos`, `linux`, `freebsd`) is allowed
where necessary (e.g. filesystem features like `clonefile` or `FICLONE`).

The bootstrap depends on a POSIX userland at `/usr/bin` and `/bin`. Default target triples are
`linux-x86_64-musl`, `linux-aarch64-musl`, `macos-x86_64-darwin`, `macos-aarch64-darwin`,
`freebsd-x86_64-unknown`, and `freebsd-aarch64-unknown`. The `-gnu` Linux variants remain
supported via explicit `--target` but are no longer the default.

Rune `targets` filtering: a rune whose `targets` list does not include the current triple is
skipped during `grm tome build --all`.

## 12. CLI and user-facing output

1. Progress and diagnostics go to **stderr**; final results go to **stdout**.
2. Error messages are for humans. Say what failed and, where possible, what to do.
3. The CLI is imperative and explicit. Commands directly and transactionally update state.
4. Commands that operate on packages accept multiple positional arguments where semantically
   reasonable (`grm install a b c`, `grm remove x y`). Multi-package mutations are a single
   all-or-nothing transaction: validate, resolve, build/fetch, and stage everything needed for the
   whole command before committing any user-visible state. If any package fails, the command fails
   and the user's installed state remains unchanged.

## 13. Testing

1. New behavior ships with tests. Bug fixes ship with a regression test.
2. Pure logic is covered by Rust unit tests colocated with the code.
3. End-to-end flows are covered by integration tests in `tests/smoke.rs` that drive the built
   binary against local fake tomes and hand-built `.tar.zst` archives. Tests run fully offline.
4. Every security invariant from §10 has a test proving the unsafe input is rejected.

Run before considering work done:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

**Exception:** When a change touches only rune (`.rn`) files and/or documentation (`.md`, `README`, etc.), the Rust checks above are not required. Rune changes are validated by parsing and smoke-test coverage; doc changes do not affect compilation.

## 14. Readability

Names carry meaning; comments explain *why*, never *what*. If a comment restates the code,
delete it. If a piece of logic needs a paragraph, that paragraph belongs in a `// WHY:` comment
next to it — and the code probably wants a better name.

## 15. Project hygiene

1. **Commits are scoped and coherent.** A single commit changes one thing: a feature, a bugfix,
   a refactor, or a documentation update. Do not bundle unrelated changes. The commit message
   describes *what* changed and *why*; the diff shows *how*.
2. **Update TODO.md as you go.** When you complete an item, move it to **Completed**. When you
   start new work, add it to **Active work** or **Remaining**. When a todo becomes obsolete,
   delete it. TODO.md is the canonical remaining-work list — keep it honest.
3. **Update AGENTS.md when the rules change.** If you add a new invariant, change the build
   environment, or introduce a new convention, document it here immediately. AGENTS.md is a
   living document, not a fossil.
