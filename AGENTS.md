# Autonomous Agent Guidelines

Binding engineering rules for the Grimoire codebase. When a rule and convenience conflict,
rule wins. When two rules conflict, prefer correctness and safety, then clarity, then brevity.

## 0. What Grimoire is

A **Rust program that embeds Nushell**: the CLI, package-manager core, transaction logic, and
orchestration are Rust; the embedded Nushell engine executes rune (`.rn`) build scripts
in-process and reads/writes NUON data.

## 1. No shelling out

1. Never invoke an external CLI via `std::process::Command`. git, tar, zstd, xz, and HTTP come
   from linked crates (`gix`, `tar`, `zstd`, `xz2`, `ureq`); the only executed code is the
   embedded Nushell engine running runes. If a capability seems to require an external tool,
   find or vendor a crate — "no suitable crate" is a design problem, not a license to shell out.
2. A rune's `build` function may invoke its package's build tooling (`make`, `cc`) — that is
   the package's business, not Grimoire's.
3. **Exception:** read-only host toolchain identity discovery (`cc --version`,
   `xcrun --show-sdk-version`, …) may shell out, confined to `src/toolchain.rs`.

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
6. **File size limits: 500 lines soft, 800 hard.** Past 500, look for a seam to split along; a
   change that would cross 800 must split the file first, into a directory module per rule 4.
   Do not dodge the limit by compressing code or stripping comments — it exists to force
   scoping, not brevity. Integration tests follow the same limits: themed files under
   `tests/`, shared helpers in `tests/support/`.
7. **One module, one concern.** A module's name should predict its contents. Logic shared by
   two modules moves to a common home — never copy-paste it or reach into a sibling's private
   helpers. Split oversized files along responsibilities (parsing vs. orchestration vs. IO),
   not line counts.

## 4. Data formats: the .rn / .nuon contract

> **If Grimoire runs it, it is `.rn`; if Grimoire reads it, it is `.nuon`.**

1. `.rn` files are executable Nushell. They are the only place arbitrary package logic runs.
2. `.nuon` files are inert structured data. Lockfiles, indexes, metadata, and local state are NUON.
3. Exported rune metadata (`package` record) is inert data and must be read as data. Build
   functions run only inside the controlled build context.
4. All NUON read/write goes through `nuon_io`. Do not parse or serialize NUON ad hoc elsewhere.

## 5. Build environment

Managed builds get a controlled `PATH`, in priority order: core package `bin/` dirs → declared
build-dep `bin/` dirs → host compiler boundary symlinks (bootstrap only; skipped once
`toolchain-wrappers` is installed) → POSIX ambient `/usr/bin` and `/bin`.

The environment is sandboxed: host discovery variables (`CMAKE_PREFIX_PATH`,
`PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, language package-manager roots, Homebrew prefixes,
…) are cleared, then declared build-dep prefixes are layered back in through those same
managed variables plus `<DEP>_PREFIX`. `HOME`, temp, and XDG directories point inside
`ctx.work_dir`. External commands launched by the build runner receive blank overrides for
inherited host env vars unless Grimoire deliberately sets them.

**Rules:**

1. Declare every non-POSIX tool and every discoverable dependency the build needs in
   `deps.build`. Never rely on host env vars, Homebrew/MacPorts prefixes, language
   package-manager state, or the user's shell configuration.
2. Do not declare POSIX utilities (`sed`, `grep`, `awk`, `find`, …) as build deps — the
   ambient directories (or core toybox once bootstrapped) always provide them.

## 6. Dependencies

- **`deps.runtime`** — required at execution time. Resolved by the solver, installed into the
  active generation.
- **`deps.build`** — tools required during the build; their `bin/` dirs join the build PATH (§5).
- **`deps.features`** — *(future work)* execution-time capabilities for FHS compatibility.

**Capability resolution:** any `bins` key that differs from the package name is a capability
(`gawk` provides `gawk` and `awk`). Literal names resolve directly; capability names resolve
to any provider, with `grm prefer` breaking ties. Depend on the capability (`awk`) when any
implementation will do; on the literal name (`gawk`) when you need that implementation.

**Platform-conditional build deps:** `'name[platform-glob]'` includes the dep only when the
target triple matches the glob — full triple or prefix (`linux-*`, `linux-*-musl`).

## 7. Rune authoring

The full reference — structure, the `ctx` record, build-script patterns, `bins`/`targets`/
`notes` conventions — lives in [docs/rune-authoring.md](docs/rune-authoring.md). The binding
rules:

1. A rune exports `package` (inert metadata, §4) and `build` (the only place package logic runs).
2. Install into `ctx.package_dir` (the staging area that gets packed); configure against
   `ctx.prefix` (the final store location).
3. **No `sh -c` in runes.** Build steps are native Nushell.
4. Wrap variables in parentheses in external command position — `($ctx.prefix)`, never
   `$ctx.prefix` — or Nushell can silently mis-parse them.
5. Platform logic lives in the rune via `ctx.target` prefix matches; Rust only supplies the triple.

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
   (store paths, package state, lockfile, and active generation) or commit none of it.
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

Grimoire is **POSIX-only**: Linux, macOS, FreeBSD — no `#[cfg(windows)]` code. Gating on
supported POSIX targets is allowed where necessary (`clonefile`, `FICLONE`). The bootstrap
depends on a POSIX userland at `/usr/bin` and `/bin`. Default target triples:
`linux-{x86_64,aarch64}-musl`, `macos-{x86_64,aarch64}-darwin`,
`freebsd-{x86_64,aarch64}-unknown`; the Linux `-gnu` variants remain supported via explicit
`--target`.

## 12. CLI and user-facing output

1. Progress and diagnostics go to **stderr**; final results go to **stdout**.
2. Error messages are for humans. Say what failed and, where possible, what to do.
3. The CLI is imperative and explicit. Commands accept multiple positional packages where
   semantically reasonable; multi-package mutations are one all-or-nothing transaction (§9.3).

## 13. Testing

1. New behavior ships with tests. Bug fixes ship with a regression test.
2. Pure logic is covered by Rust unit tests colocated with the code.
3. End-to-end flows are covered by themed integration tests under `tests/` (shared helpers in
   `tests/support/`) that drive the built binary against local fake tomes and hand-built
   `.tar.zst` archives. Tests run fully offline.
4. Every security invariant from §10 has a test proving the unsafe input is rejected.

Run before considering work done (skippable only for changes touching nothing but `.rn` runes
and/or documentation):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 14. Readability

Names carry meaning; comments explain *why*, never *what*. If a comment restates the code,
delete it. If a piece of logic needs a paragraph, that paragraph belongs in a `// WHY:` comment
next to it — and the code probably wants a better name.

## 15. Project hygiene

1. **Commits are scoped and coherent.** A single commit changes one thing: a feature, a bugfix,
   a refactor, or a documentation update. The commit message describes *what* changed and
   *why*; the diff shows *how*.
2. **Update TODO.md as you go.** Completed items move to **Completed**; new work lands in
   **Active work** or **Remaining**; obsolete todos are deleted. TODO.md is the canonical
   remaining-work list — keep it honest.
3. **Update AGENTS.md when the rules change.** New invariants and conventions are documented
   here immediately. AGENTS.md is a living document, not a fossil.
