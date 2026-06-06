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

1. Grimoire-managed build dependency `bin/` directories.
2. Host compiler boundary symlinks (`cc`, `c++`, `ar`, `ld`, etc.).
3. POSIX ambient directories: `/usr/bin` and `/bin`.

**Rule:** Declare only non-POSIX tools that the build script calls explicitly. Do not declare
POSIX utilities (`sed`, `grep`, `awk`, `find`, `coreutils`, `diffutils`) as build deps — they are
always available via the ambient directories.

## 6. Capability-based dependencies

A rune's `bins` map declares provided commands. Any key that differs from the package `name` is a
**capability** (e.g. `gawk` provides `gawk` and `awk`).

- Literal names resolve directly (`gawk` → `gawk`).
- Capability names fall back to any package whose `bins` map contains the name (`awk` → `gawk`).

Prefer capability names when you need the command semantically (`awk`); use literal names when
you require a specific implementation (`gawk`).

## 7. Store-only installation

`grm tome build` installs built packages **store-only**: extracted to the store and recorded in
`state/packages/{name}.nuon`, but the lockfile and active generation are not updated. This lets a
tome bootstrap itself (`grm tome build --all`) without polluting the user's PATH. Single-package
`grm tome build` also installs missing build deps store-only before building.

## 8. Transactional state

Grimoire has no database. Durability is explicit transaction directories plus atomic `rename`.

1. Never mutate an installed package directory or state file in place. Stage, then promote.
2. An operation either fully completes or leaves the previous state intact. Rollback restores the
   prior version if promotion fails partway.
3. Installed package version directories are immutable once promoted. Upgrades create new version
   directories.
4. Local state is inspectable NUON under the install root. No databases.

## 9. Security invariants

These must never be regressed:

1. **Verify before trust.** Checksum every downloaded source and archive. Hash mismatch is fatal.
2. **Validate every archive member path.** Reject traversal (`..`), absolute paths, and escapes.
3. **Reject unsafe archive contents.** Hard links are rejected. Symlinks are allowed only when the
   target resolves within the package, and no member may be nested under a symlink.
4. **No privilege escalation.** Installs target a user-local root and must not require or assume
   root/admin. Never write outside the install root.
5. **Rune/addendum execution is the trust boundary.** Addendums patch data only. Do not let
   addendum data trigger execution.

## 10. Platform support

Grimoire is **POSIX-only**: Linux, macOS, FreeBSD. No `#[cfg(windows)]` code.

The bootstrap depends on a POSIX userland at `/usr/bin` and `/bin`. Default target triples are
`linux-x86_64-gnu`, `linux-aarch64-gnu`, `macos-x86_64-darwin`, `macos-aarch64-darwin`,
`freebsd-x86_64-unknown`, and `freebsd-aarch64-unknown`.

## 11. CLI and user-facing output

1. Progress and diagnostics go to **stderr**; final results go to **stdout**.
2. Error messages are for humans. Say what failed and, where possible, what to do.
3. The CLI is imperative and explicit. Commands directly and transactionally update state.

## 12. Testing

1. New behavior ships with tests. Bug fixes ship with a regression test.
2. Pure logic is covered by Rust unit tests colocated with the code.
3. End-to-end flows are covered by integration tests in `tests/smoke.rs` that drive the built
   binary against local fake tomes and hand-built `.tar.zst` archives. Tests run fully offline.
4. Every security invariant from §9 has a test proving the unsafe input is rejected.

Run before considering work done:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 13. Readability

Names carry meaning; comments explain *why*, never *what*. If a comment restates the code,
delete it. If a piece of logic needs a paragraph, that paragraph belongs in a `// WHY:` comment
next to it — and the code probably wants a better name.
