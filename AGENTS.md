# Autonomous Agent Guidelines

These are binding engineering rules for working in the Grimoire codebase. They are
intentionally strict. When a rule and convenience conflict, the rule wins. When two
rules appear to conflict, prefer correctness and safety over everything else, then
clarity, then brevity.

## 0. What Grimoire actually is

Grimoire is a **Rust program that embeds the Nushell engine**. It is a Rust project
first and foremost. Nushell is a dependency, not the implementation language.

- The CLI, package manager core, transaction logic, and orchestration are Rust.
- Nushell (`nu-engine`, `nu-protocol`, `nu-command`, `nuon`) is embedded to *execute*
  rune (`.rn`) package definitions and to read/write NUON data.
- `git`, `tar`, and `zstd` capability is provided through Rust crates (`gix`, `tar`,
  `zstd`) — see §1a, Grimoire does not shell out for them.

## 1a. Everything is native — no shelling out

Grimoire is a self-contained Rust binary. **All functionality is implemented natively
in-process. Grimoire does not shell out to, spawn, or depend on any external CLI tool.**

1. There are no calls to `git`, `tar`, `zstd`, `curl`, `wget`, or any other external
   program. `std::process::Command` (and equivalents) must not be used to invoke
   third-party CLIs. Git operations use `gix`, archives use `tar`, compression uses
   `zstd`, downloads use a Rust HTTP client — all as linked crates.
2. The *only* program Grimoire executes is the embedded Nushell engine running `.rn`
   rune build scripts, and that runs in-process via `nu-engine`, not as a spawned
   `nu` subprocess. A user with the Grimoire binary needs nothing else on their `PATH`.
3. A package's own `build` rune may legitimately invoke that package's build tooling
   (e.g. `make`, `cc`) inside the controlled build context — that is the package's
   business, not Grimoire's. Grimoire's *own* machinery still shells out to nothing.
4. If a capability seems to require an external tool, the answer is to find or vendor a
   Rust crate, not to add a subprocess. "No suitable crate exists" is a design problem
   to raise, never a license to shell out.

## 1. Rust idiom is non-negotiable

1. Write idiomatic, modern Rust for edition 2024. Prefer the standard library and
   established crates over hand-rolled equivalents.
2. `unsafe` is forbidden without an explicit, reviewed justification documented in a
   `// SAFETY:` comment that states the invariant being upheld.
3. No `unwrap()`, `expect()`, or `panic!` in non-test code paths. The only tolerated
   exceptions are provably-unreachable invariants, and those must use `expect()` with a
   message explaining *why* it cannot fail. Production code returns errors; it does not
   abort.
4. All fallible functions return `anyhow::Result<T>`. Attach context with
   `.context(...)` / `.with_context(...)` at every boundary where a bare error would be
   ambiguous to a user reading stderr.
5. Prefer `&str`/`&Path`/`&[T]` in function signatures over owned types; take ownership
   only when the function must store or consume the value.
6. Derive, don't implement, when a derive is correct (`Debug`, `Clone`, `Serialize`,
   `Deserialize`, `PartialEq`). Implement by hand only when the derive is wrong.
7. Code must pass `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
   with zero warnings. Clippy lints are treated as errors, not suggestions.

## 2. Functions and structure

1. Functions do one thing. Prefer many small, named functions over large nested ones.
   A function whose body does not fit on one screen is a refactor candidate.
2. Nesting beyond three levels is a smell. Use early returns and guard clauses
   (`bail!`, `?`, `let ... else`) to keep the happy path flat.
3. Follow DRY. Two copies of non-trivial logic is one too many — extract a function.
   But do not invent abstractions for a single caller; duplication is cheaper than the
   wrong abstraction.
4. Modularise aggressively. As `src/` grows, group related modules into folders
   (e.g. `archive/`, `repo/`, `nu/`, `state/`) with a clear module root rather than
   accumulating a flat pile of files. A module should have one reason to change.
5. No module should reach into another module's private internals through `pub` that
   exists only for convenience. Keep surfaces minimal and intentional.

## 3. Data formats: the .rn / .nuon contract

The single rule: **if Grimoire runs it, it is `.rn`; if Grimoire reads it, it is
`.nuon`.**

1. `.rn` files are executable Nushell. They are the *only* place arbitrary package
   logic (source builds) is allowed to run.
2. `.nuon` files are inert structured data. Grimoire reads them; it never executes
   them. Lockfiles, package metadata, indexes, and local state are all NUON.
3. Exported rune metadata (the `package` record) is inert data and must be read as
   data. Build functions run only during a source build, inside the controlled build
   context — never as a side effect of reading metadata.
4. All NUON read/write goes through the shared `nuon_io` layer. Do not parse or
   serialize NUON ad hoc elsewhere.

## 4. Transactional state — the real "ACID" rule

Grimoire has no database. Its durability model is **explicit transaction directories
plus atomic renames on the filesystem**. Honor it strictly.

1. Never mutate an installed package directory or state file in place. Stage all work
   in a temporary transaction directory, then promote with an atomic `rename`.
2. An operation either fully completes or leaves the previous state intact. There is no
   partially-installed state. If promotion can fail partway (shims, state writes),
   provide rollback that restores the prior version.
3. Installed package version directories are immutable once promoted. Upgrades create
   new version directories; they do not edit existing ones.
4. Local state is inspectable NUON under the install root. Prefer plain, auditable data
   files over hidden or opaque storage. Do not introduce a real database unless the
   filesystem model provably cannot meet a need.

## 5. Security invariants

These are not optional and must never be regressed for convenience:

1. **Verify before trust.** Checksum every downloaded source and every package archive
   before using it. A hash mismatch is a hard failure.
2. **Validate every archive member path.** Reject path traversal (`..`), absolute
   paths, and anything that escapes the extraction root — before extraction, not after.
3. **Reject unsafe archive contents.** Symlinks in archives are rejected until a
   reviewed, sandboxed handling story exists.
4. **No privilege escalation.** Installs target a user-local root and must not require
   or assume root/admin. Never write outside the install root or the user's `PATH` shim
   directory.
5. **Rune/addendum execution is the trust boundary.** Addendums patch data only — they
   do not run hooks in v1. Do not add code that lets addendum data trigger execution.

## 6. Cross-platform parity

1. Treat Linux, macOS, and Windows as first-class. Windows is supported natively, not
   via WSL.
2. The default Windows target is GNU ABI (`windows-x86_64-gnu`); MSVC is opt-in. Do not
   write code that assumes a single OS's path, ABI, or shim format.
3. Platform-specific behavior is isolated behind `#[cfg(...)]` or a small platform
   module, never smeared inline through general logic.
4. Use `camino`/`Path` correctly; never assume UTF-8 paths or `/` separators outside of
   logic that is explicitly platform-gated.

## 7. CLI and user-facing output

1. Progress and diagnostics go to **stderr**; final results go to **stdout**. Respect
   `--quiet`/`-q` for the progress stream while still emitting the final result.
2. Error messages are for humans. They say what failed and, where possible, what to do
   about it. No raw debug dumps as the primary message.
3. The CLI is imperative and explicit. Commands like `install`, `remove`, `upgrade`
   directly and transactionally update installed and lock state.

## 8. Testing

1. New behavior ships with tests. Bug fixes ship with a regression test that fails
   before the fix.
2. Pure logic (NUON parsing, metadata validation, path validation, install-root
   resolution, shim generation, lockfile serialization) is covered by Rust unit tests
   colocated with the code.
3. End-to-end flows are covered by Rust integration tests (`tests/smoke.rs`) that drive
   the built binary against local fake tomes and tiny hand-built `.tar.zst` archives.
   Tests must run fully offline.
4. Security invariants from §5 each have a test proving the unsafe input is rejected.

Run before considering work done:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 9. Readability

Code is written for the next human. Names carry meaning; comments explain *why*, never
*what*. If a comment restates the code, delete it. If a piece of logic needs a paragraph
to justify, that paragraph belongs in a `// WHY:` comment next to it — and the code
probably wants a name that makes the paragraph shorter.
