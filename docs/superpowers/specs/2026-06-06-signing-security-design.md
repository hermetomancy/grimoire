# Signing and Security Architecture Design

**Date:** 2026-06-06
**Status:** Approved
**Approach:** A (Graceful Rotation + Rune Manifest + Addendum Auto-Sync)

## Context

Grimoire's current signing model uses TOFU-pinned root-level `signers` on tome and addendum manifests, with per-package detached minisign signatures on runes (`.rn.minisig`) and archives (`.tar.zst.minisig`). Exact-set matching prevents silent key rotation on sync.

This design addresses three threat classes:
- **Compromised tome/addendum repo** — attacker pushes malicious packages
- **Compromised author key** — attacker steals a maintainer's minisign key
- **Malicious mirror / MITM** — attacker serves tampered content in transit

Since Grimoire has no existing users or third-party tomes, backwards compatibility is not required.

---

## Section 1: Graceful Key Rotation

### Problem

Exact-set matching makes key rotation impossible without every user removing and re-adding the tome/addendum. If an author's key is compromised or lost, recovery is a manual, user-visible breakage.

### Design

`capture_signer` (and `capture_addendum_signer`) keeps exact-set matching as the default fast path, but adds an escape hatch for authorized transitions:

1. Compare advertised `signers` with pinned `signer_pubkeys`.
2. If exact set match → accept.
3. If not exact, look for `tome.rn.minisig` (or `addendum.nuon.minisig`) next to the manifest.
4. Verify the manifest signature against the **currently pinned keys**.
5. If verification passes → accept the new `signers` array, re-pin it, and emit a warning:
   *"tome `foo` rotated signing keys (old: [A] → new: [B])"*.
6. If no signature or verification fails → error as before.

### Proof of Possession on First Sync

Since backwards compatibility is not required, any tome/addendum that declares `signers` **must** ship a detached signature for its manifest:

- `tome.rn` → `tome.rn.minisig`
- `addendum.nuon` → `addendum.nuon.minisig`

On first sync, before TOFU-pinning the advertised `signers`, Grimoire verifies the manifest signature against one of the advertised keys. This proves the author possesses the private key and prevents a compromised host from advertising arbitrary signer keys.

Unsigned tomes/addenda (no `signers` declared) skip all signature verification.

### Threat Coverage

- **Compromised repo without key**: Cannot rotate keys — no `tome.rn.minisig` valid under pinned keys.
- **Compromised key**: Author uses a surviving uncompromised key to sign the manifest with the new `signers` array. Users auto-accept the rotation.
- **MITM**: Manifest signature fails if tampered in transit.

---

## Section 2: Rune Manifest

### Problem

Per-package signing verifies each rune's signature individually, but nothing prevents a compromised repo from adding a *new* `evil.rn` + `evil.rn.minisig` signed with the pinned key. The new package would verify and be installable.

### Design

A signed `runes-manifest.nuon` lists every authorized rune and its sha256:

```nuon
{
  format: 1,
  runes: {
    "hello.rn": "sha256:abc123...",
    "world.rn": "sha256:def456..."
  }
}
```

Detached signature: `runes-manifest.nuon.minisig`

### Verification on Sync

In `validate_tome_cache`, if the tome has pinned signers:

1. Require `runes-manifest.nuon` to exist.
2. Verify `runes-manifest.nuon.minisig` against pinned keys.
3. Compute sha256 of every `.rn` file in `runes/`.
4. Ensure every rune is listed in the manifest and hashes match.
5. Ensure no extra runes exist in `runes/` that aren't in the manifest.

Any mismatch is fatal.

### Threat Coverage

- **Compromised repo**: Cannot add new packages (new rune not in manifest) or modify existing ones (hash mismatch).
- **Compromised key**: Attacker can sign valid packages, but only for runes already in the manifest. Cannot expand the attack surface with new packages.
- **MITM**: Manifest signature fails if tampered.

### Build-Time Verification

`verify_rune` already checks `.rn.minisig` before source builds. The rune manifest is a sync-time defense-in-depth check. Both must pass.

---

## Section 3: Addendum Auto-Sync

### Problem

Addenda sync once on first use (`ensure_addendum_cache` only syncs if the manifest is missing) and never update. An addendum that patches a vulnerability stays stale forever unless the user manually deletes the cache.

### Design

Two changes:

1. **`grm addendum update [name]`** — explicit sync command, mirroring `grm tome update`. If `name` is omitted, updates all addenda. Same git-clone/copy → validate → promote flow as tomes, including signature verification and TOFU key capture.

2. **Staleness warning in `apply_patches`** — before applying patches, compare `git::head_commit(cache)` against `state.checked_commit`. If they differ, emit a warning:
   *"addendum `foo` is stale; run `grm addendum update foo`"*
   
   The cached (previously verified) patches are still applied. Auto-sync is intentionally **not** triggered here to avoid silent network I/O during `info`/`install`.

### Threat Coverage

- **Compromised repo**: Addenda are verified on every sync (signature + TOFU), same as tomes.
- **Compromised key**: Graceful rotation applies (same `capture_addendum_signer` logic as Section 1).
- **MITM**: Sync verifies signatures; stale cache means old but *verified* data, not tampered data.

---

## Files to Modify

| File | Change |
|------|--------|
| `src/tome/mod.rs` | Update `capture_signer` for graceful rotation; add `verify_tome_manifest`; enforce manifest signature on first sync |
| `src/tome/mod.rs` | Add rune manifest verification in `validate_tome_cache` |
| `src/addendum.rs` | Update `capture_addendum_signer` for graceful rotation; add `verify_addendum_manifest`; enforce manifest signature on first sync |
| `src/addendum.rs` | Add `addendum update` command dispatch; add staleness warning in `apply_patches` |
| `src/cli.rs` | Add `AddendumUpdateArgs` and `AddendumCommand::Update` variant |
| `src/main.rs` | Dispatch `AddendumCommand::Update` to `addendum::update` |
| `src/signing.rs` | Add `verify_tome_manifest` and `verify_addendum_manifest` helpers (or reuse `verify_detached`) |
| `tests/smoke.rs` | Update `build_signed_tome` to generate `tome.rn.minisig` and `runes-manifest.nuon` + signatures; add addendum update tests |

## Testing Requirements

- Key rotation: build tome with key A, sync, rebuild with key B signed by A, sync succeeds and re-pins
- Key rotation rejection: rebuild with key B signed by B (not A), sync fails
- First sync proof-of-possession: tome with `signers` but no `tome.rn.minisig` is rejected
- Rune manifest rejection: extra rune in `runes/` not in manifest → sync fails
- Rune manifest rejection: rune hash mismatch → sync fails
- Addendum update command: explicit update syncs and re-verifies
- Addendum staleness warning: `info` on stale addendum emits warning but succeeds

## Open Questions

None. Design is complete.
