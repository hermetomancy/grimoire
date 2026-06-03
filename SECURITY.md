# Security policy

## Reporting a vulnerability

Please report security issues **privately**, not as public GitHub issues.

- Open a private vulnerability report via GitHub: the **Security → Report a vulnerability**
  tab on <https://github.com/grimoire-of-glass/grimoire>. This opens a private advisory only
  the maintainers can see.
- If you cannot use GitHub's private reporting, open a regular issue that contains **no
  details** beyond a request for a private contact, and we will follow up.

Please include enough to reproduce: the `grm` version (`grm --version`), your platform, the
tome/addendum involved (if any), and the steps that trigger the issue. If you have a
proof-of-concept, attach it to the private advisory rather than posting it publicly.

We aim to acknowledge a report within a few days and to keep you updated as we work on a fix.
Coordinated disclosure is appreciated: give us a reasonable window to ship a fix before any
public write-up.

## What's in scope

Grimoire's security model is documented in [docs/threat-model.md](docs/threat-model.md). The
most valuable reports concern Grimoire's own trust boundaries, for example:

- Bypassing checksum verification of a fetched source or archive.
- Bypassing **package-index signature verification** or the trust-on-first-use key pin
  (installing a tampered or attacker-substituted index against a pinned signer).
- Archive extraction escaping the package directory (path traversal, link following).
- Privilege escalation or writes outside the user-local install root.

## What's not a vulnerability

Some properties are **known non-goals**, documented in the threat model — please don't report
these as vulnerabilities:

- A malicious tome you have chosen to trust running arbitrary code during a source build.
  Runes are not sandboxed; installing from an untrusted tome is equivalent to running an
  untrusted script.
- An attacker who controls a tome's host at the moment you **first** add it (trust-on-first-use
  trusts the key it first sees). Use an out-of-band key check for higher assurance.
- A compromised addendum you have chosen to add (addendum signing is planned; see the threat
  model and `TODO.md`).
