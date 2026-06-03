# Threat model

Grimoire installs software from git-backed catalogs ("tomes") and runs source builds defined
by package authors. This document describes what Grimoire does and does not protect against,
so users and tome authors can make informed decisions.

## What Grimoire protects against

- **Tampering of fetched bytes in transit or at rest.** Every source artifact and every
  binary archive is sha256-verified before it is read, extracted, or executed. A mismatch
  aborts the operation. This covers transparent proxies, corrupt CDN caches, partial
  downloads, and bit-rot in the on-disk cache.
- **Archive path-traversal and symlink attacks.** Tar members with absolute paths or `..`
  components are rejected before extraction, as are hard links. Symlinks are allowed in package
  archives *only* when their target resolves within the package (validated against the same root
  rules); an absolute or escaping target is rejected, and no member may be nested under a symlink.
  Source tarballs are stricter still — they reject symlinks and hard links outright. A malicious
  archive therefore cannot write outside its package directory or point a link at host paths.
- **Privilege escalation.** Grimoire installs into a user-local root (`~/.grimoire`, or
  `GRIMOIRE_ROOT`). It never writes to system paths and never asks for elevation, so a compromised
  package cannot directly modify system state.
- **Silent rune drift.** Tome syncs record the git ref and commit hash; the lockfile records
  the commit a build was resolved against. `grm install --locked` rejects anything not in the
  lock, so a reproducible install fails closed rather than silently picking up a moved tag.
- **Failed-install corruption.** Installs stage into a transaction directory and promote
  with atomic renames; a failure after promotion (shim writes, state writes) rolls back to
  the prior version rather than leaving a half-installed package.
- **A compromised static archive host (signed tomes).** A tome may sign its `index.nuon` with
  [minisign](https://jedisct1.github.io/minisign/) and declare the public key in its
  manifest (`packages.signer`). Grimoire then requires a valid `index.nuon.minisig` over the
  index before trusting it, and because the index records every archive's sha256, that
  signature transitively authenticates every binary package. The signing key is **pinned on
  first use**: once a tome has been added with a signer, a later sync that drops the
  signature, fails verification, or presents a *different* key is refused (key rotation needs
  a deliberate remove + re-add). An attacker who later compromises the static host therefore
  cannot substitute a tampered index without the author's private key.
- **Concurrent mutation.** Mutating commands (`install`, `remove`, `upgrade`, `clean`,
  `hold`/`unhold`, `tome add/update/remove`, `addendum add/remove`) take an exclusive
  install-root lock for their duration, so two runs against the same root cannot race shared
  state; the second fails fast. The lock is released by the OS at process exit, so a crash
  leaves no stale lock.

## What Grimoire does *not* protect against

These are explicit non-goals today; addressing them is tracked in
[`TODO.md`](../TODO.md).

- **A compromised tome git host.** Tome *source* contents are still trusted on faith: anyone
  with push access to the git repository can change rune sources, declared sha256 values, and
  build scripts, and Grimoire will execute the result. Index signing (above) covers the
  published binary index, but signing the source runes themselves is planned, not yet built.
- **An unsigned tome.** Signature verification is opt-in: a tome that declares no
  `packages.signer` publishes an unsigned index, and Grimoire installs from it exactly as
  before (verify-if-present). Until you add a tome whose signer you trust, the static-host
  protection above does not apply.
- **First-use trust.** Trust-on-first-use only protects against compromise *after* you first
  add a tome. An attacker controlling the host at the moment of the first `grm tome add` is
  trusted, since that is the key that gets pinned. For higher assurance, verify the published
  key out of band before adding (an explicit `--signer` pin is planned).
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

## Practical guidance

- **Pin tomes by commit, not by branch**, for anything you care about reproducing. `grm tome
  add --ref <commit>` and `install --locked` together give you a verifiable install.
- **Review what you add.** A `grm tome add <url>` or `grm addendum add <url>` is a trust
  decision about everything that repository publishes today and in the future. Prefer
  forking and pinning when the upstream is not under your control.
- **Treat `--from-source` as code execution.** It is. Do not run source builds from tomes
  you would not run a shell script from.
- **Prefer signed tomes, and verify the signer key out of band.** When a tome publishes a
  `packages.signer`, Grimoire pins it on first add and enforces it thereafter; confirming that
  key through a second channel before the first add closes the trust-on-first-use gap.
- **Report security issues privately.** See [`SECURITY.md`](../SECURITY.md) for how to
  disclose.
