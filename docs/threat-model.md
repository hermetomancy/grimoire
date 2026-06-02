# Threat model

Grimoire installs software from git-backed catalogs ("tomes") and runs source builds defined
by package authors. This document describes what Grimoire does and does not protect against,
so users and tome authors can make informed decisions.

## What Grimoire protects against

- **Tampering of fetched bytes in transit or at rest.** Every source artifact and every
  binary archive is sha256-verified before it is read, extracted, or executed. A mismatch
  aborts the operation. This covers transparent proxies, corrupt CDN caches, partial
  downloads, and bit-rot in the on-disk cache.
- **Archive path-traversal and symlink attacks.** Tar members with absolute paths, `..`
  components, or symbolic/hard links are rejected before extraction. A malicious archive
  cannot write outside its package directory.
- **Privilege escalation.** Grimoire installs into a user-local root under the platform data
  directory (or `GRIMOIRE_ROOT`). It never writes to system paths and never asks for
  elevation, so a compromised package cannot directly modify system state.
- **Silent rune drift.** Tome syncs record the git ref and commit hash; the lockfile records
  the commit a build was resolved against. `grm install --locked` rejects anything not in the
  lock, so a reproducible install fails closed rather than silently picking up a moved tag.
- **Failed-install corruption.** Installs stage into a transaction directory and promote
  with atomic renames; a failure after promotion (shim writes, state writes) rolls back to
  the prior version rather than leaving a half-installed package.

## What Grimoire does *not* protect against

These are explicit non-goals today; addressing them is tracked in
[`TODO.md`](../TODO.md).

- **A compromised tome git host.** Tome contents are trusted on faith: anyone with push
  access to the tome's git repository can change rune sources, sha256 values, and build
  scripts, and Grimoire will execute the result. Tome commit signatures and an
  installer-side signer trust list are planned but not yet enforced.
- **A compromised static archive host.** `index.nuon` itself is not signed. An attacker who
  controls the host can serve an `index.nuon` that points at archives with attacker-chosen
  sha256 values, and Grimoire will accept them (the checksum check verifies that what was
  *served* matches what the *index says* — not that either reflects the tome author's
  intent). Signed indexes are planned.
- **Malicious build scripts.** Runes are Nushell code that runs with the user's full
  privileges during a source build. Grimoire fetches and checksums inputs, but does not
  sandbox the `build` function. Treat installing from an untrusted tome as equivalent to
  running an untrusted shell script.
- **Malicious addenda.** Addenda are inert NUON data — no hooks or scripts run from them —
  but a hostile addendum can still point a source URL at attacker-controlled bytes with a
  matching sha256, effectively swapping the package's source while leaving the rune's build
  script unchanged. Review addenda you add, especially remote ones, the same way you'd
  review a tome.
- **Supply-chain attacks on Grimoire itself.** Grimoire is currently installed via
  `cargo install grimoire`, which trusts crates.io and the Rust toolchain. Signed release
  artifacts for direct download are planned.
- **Concurrent mutation.** Two `grm install` or `grm remove` runs against the same install
  root can race shared state. Until an install-root lockfile lands, do not run multiple
  mutating commands concurrently.

## Practical guidance

- **Pin tomes by commit, not by branch**, for anything you care about reproducing. `grm tome
  add --ref <commit>` and `install --locked` together give you a verifiable install.
- **Review what you add.** A `grm tome add <url>` or `grm addendum add <url>` is a trust
  decision about everything that repository publishes today and in the future. Prefer
  forking and pinning when the upstream is not under your control.
- **Treat `--from-source` as code execution.** It is. Do not run source builds from tomes
  you would not run a shell script from.
- **Report security issues privately.** See [`SECURITY.md`](../SECURITY.md) (planned) for
  the disclosure address. In the interim, open a minimal-detail issue asking for a contact
  and continue privately.
