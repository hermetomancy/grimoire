# Signing and Security Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement graceful key rotation, signed rune manifests, and addendum auto-sync per the approved design spec.

**Architecture:** Extend existing TOFU capture functions to verify manifest signatures for proof-of-possession (first sync) and authorized rotation (subsequent syncs). Add runes-manifest verification to prevent package sprawl. Add explicit `grm addendum update` command.

**Tech Stack:** Rust 2024, minisign (via `minisign-verify` crate), NUON data format, embedded Nushell runtime.

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/tome/mod.rs` | TOFU capture for tomes, rune manifest verification, manifest signature verification |
| `src/addendum.rs` | TOFU capture for addenda, addendum update command, staleness warning |
| `src/cli.rs` | CLI argument structs and subcommand definitions |
| `src/main.rs` | Subcommand dispatch |
| `src/model.rs` | (No changes needed — `signers`/`signer_pubkeys` already added) |
| `tests/smoke.rs` | End-to-end tests for signing, rotation, rune manifest, addendum update |

---

### Task 1: Tome Manifest Proof-of-Possession on First Sync

**Files:**
- Modify: `src/tome/mod.rs`
- Test: `tests/smoke.rs`

**Context:** Currently, `capture_signer` TOFU-pins advertised signers on first sync without verification. We now require proof-of-possession: the manifest must carry a detached signature from one of its advertised keys.

- [ ] **Step 1: Update `capture_signer` to require manifest signature on first sync**

Modify `capture_signer` in `src/tome/mod.rs` (around line 885). The `existing.is_empty() && !advertised.is_empty()` branch must verify `tome.rn.minisig` before pinning.

```rust
fn capture_signer(
    tome: &TomeState,
    cache: &Path,
    manifest: &TomeManifest,
    existing: &[String],
) -> Result<Vec<String>> {
    let advertised = manifest.signers.clone();

    if existing.is_empty() && advertised.is_empty() {
        return Ok(Vec::new());
    }

    if !existing.is_empty() && advertised.is_empty() {
        bail!(
            "tome `{}` previously advertised signers but no longer does; refusing. \
             Remove and re-add the tome to trust an unsigned manifest.",
            tome.name
        );
    }

    if existing.is_empty() && !advertised.is_empty() {
        // Proof-of-possession: verify the manifest is signed by one of the advertised keys.
        let manifest_path = cache.join("tome.rn");
        signing::verify_detached(&manifest_path, &advertised)
            .with_context(|| format!(
                "tome `{}` declares signers but its manifest signature does not verify; \
                 refusing. The tome author must sign `tome.rn` with one of the keys listed in `signers`.",
                tome.name
            ))?;
        report(&format!(
            "pinned {} signer(s) for tome `{}` (trust on first use)",
            advertised.len(),
            tome.name
        ));
        return Ok(advertised);
    }

    // Both have signers: exact match is the fast path.
    let sets_match =
        existing.len() == advertised.len() && existing.iter().all(|k| advertised.contains(k));
    if !sets_match {
        // Fall through to rotation check in Task 2.
        bail!(
            "tome `{}` now advertises a different set of signing keys than the one pinned on \
             first use; refusing. Remove and re-add the tome to trust the new keys.",
            tome.name
        );
    }

    Ok(existing.to_vec())
}
```

- [ ] **Step 2: Update `build_signed_tome` helper to sign the manifest**

In `tests/smoke.rs`, the `build_signed_tome` helper must now also create `tome.rn.minisig`.

After writing `tome.rn`, add:

```rust
sign_to(
    &tome.join("tome.rn.minisig"),
    &fs::read(tome.join("tome.rn")).unwrap(),
    keypair,
);
```

- [ ] **Step 3: Add test for proof-of-possession rejection**

Add a new test in `tests/smoke.rs`:

```rust
#[test]
fn signed_tome_rejects_manifest_without_signature_on_first_sync() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    // Build a tome with signers but do NOT sign the manifest
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    ).unwrap();
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: 'nominisig'\n  signers: ['{}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n",
            keypair.pk.to_base64()
        ),
    ).unwrap();

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add tome",
    );
    let update = run(root, &["tome", "update", "nominisig"]);
    assert_failure_contains(
        &update,
        "manifest signature does not verify",
        "unsigned manifest with declared signers is rejected",
    );
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test signed_tome -- --test-threads=1
```
Expected: All existing signed tome tests pass (they now generate `tome.rn.minisig`). The new rejection test passes.

- [ ] **Step 5: Commit**

```bash
git add src/tome/mod.rs tests/smoke.rs
git commit -m "feat: require tome manifest signature on first sync (proof-of-possession)"
```

---

### Task 2: Graceful Key Rotation for Tomes

**Files:**
- Modify: `src/tome/mod.rs`
- Test: `tests/smoke.rs`

**Context:** When exact-set matching fails, check if the manifest is signed by a previously pinned key. If so, accept the new signer set.

- [ ] **Step 1: Update `capture_signer` rotation logic**

Replace the `!sets_match` branch in `capture_signer`:

```rust
    if !sets_match {
        // Graceful rotation: allow if the manifest is signed by a previously pinned key.
        let manifest_path = cache.join("tome.rn");
        if signing::verify_detached(&manifest_path, existing).is_ok() {
            report(&format!(
                "tome `{}` rotated signing keys ({} -> {})",
                tome.name,
                existing.len(),
                advertised.len()
            ));
            return Ok(advertised);
        }
        bail!(
            "tome `{}` now advertises a different set of signing keys than the one pinned on \
             first use; refusing. Remove and re-add the tome to trust the new keys.",
            tome.name
        );
    }
```

- [ ] **Step 2: Update key rotation test**

The existing `signed_tome_refuses_key_rotation_without_readd` test currently builds a new tome with key B (unsigned by A) and expects failure. We need a NEW test for successful rotation:

```rust
#[test]
fn signed_tome_allows_graceful_key_rotation() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let key_a = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "rotateok", &triple, &key_a);

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add signed tome",
    );
    assert_success(
        &run(root, &["tome", "update", "rotateok"]),
        "pin key A on first update",
    );

    // Rotate to key B, but sign the manifest with key A (the pinned key).
    let key_b = gen_keypair();
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: 'rotateok'\n  signers: ['{}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n",
            key_b.pk.to_base64()
        ),
    ).unwrap();
    // Sign the manifest with the OLD key (A) to authorize the rotation.
    sign_to(
        &tome.join("tome.rn.minisig"),
        &fs::read(tome.join("tome.rn")).unwrap(),
        &key_a,
    );
    // Rebuild archive and index with key B signing the archive.
    build_signed_tome(tome, "rotateok", &triple, &key_b);

    let rotate = run(root, &["tome", "update", "rotateok"]);
    assert_success(&rotate, "graceful key rotation succeeds");
    let rotate_text = format!("{}{}", stdout(&rotate), stderr(&rotate));
    assert!(
        rotate_text.contains("rotated signing keys"),
        "rotation should be reported: {rotate_text}"
    );
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test signed_tome -- --test-threads=1
```
Expected: New rotation test passes; existing rotation-rejection test still passes.

- [ ] **Step 4: Commit**

```bash
git add src/tome/mod.rs tests/smoke.rs
git commit -m "feat: graceful key rotation for signed tomes"
```

---

### Task 3: Proof-of-Possession and Graceful Rotation for Addenda

**Files:**
- Modify: `src/addendum.rs`
- Test: `tests/smoke.rs`

**Context:** Mirror Tasks 1-2 for addenda. Update `capture_addendum_signer` to require `addendum.nuon.minisig` on first sync and allow rotation when signed by a pinned key.

- [ ] **Step 1: Update `capture_addendum_signer`**

In `src/addendum.rs`, modify `capture_addendum_signer` (added in previous work):

```rust
fn capture_addendum_signer(
    state: &AddendumState,
    cache: &Path,
    manifest: &AddendumManifest,
    existing: &[String],
) -> Result<Vec<String>> {
    let advertised = manifest.signers.clone();

    if existing.is_empty() && advertised.is_empty() {
        return Ok(Vec::new());
    }

    if !existing.is_empty() && advertised.is_empty() {
        bail!(
            "addendum `{}` previously advertised signers but no longer does; refusing. \
             Remove and re-add the addendum to trust an unsigned manifest.",
            state.name
        );
    }

    if existing.is_empty() && !advertised.is_empty() {
        let manifest_path = cache.join(MANIFEST);
        signing::verify_detached(&manifest_path, &advertised)
            .with_context(|| format!(
                "addendum `{}` declares signers but its manifest signature does not verify; refusing.",
                state.name
            ))?;
        report(&format!(
            "pinned {} signer(s) for addendum `{}` (trust on first use)",
            advertised.len(),
            state.name
        ));
        return Ok(advertised);
    }

    let sets_match =
        existing.len() == advertised.len() && existing.iter().all(|k| advertised.contains(k));
    if !sets_match {
        let manifest_path = cache.join(MANIFEST);
        if signing::verify_detached(&manifest_path, existing).is_ok() {
            report(&format!(
                "addendum `{}` rotated signing keys ({} -> {})",
                state.name,
                existing.len(),
                advertised.len()
            ));
            return Ok(advertised);
        }
        bail!(
            "addendum `{}` now advertises a different set of signing keys than the one pinned on \
             first use; refusing. Remove and re-add the addendum to trust the new keys.",
            state.name
        );
    }

    Ok(existing.to_vec())
}
```

- [ ] **Step 2: Update signed addendum tests to sign the manifest**

In the `signed_addendum_pins_key_and_rejects_tampering` and `signed_addendum_refuses_key_rotation_without_readd` tests (in `tests/smoke.rs`), the addendum manifest is already being signed. But we need to add a test for proof-of-possession rejection:

```rust
#[test]
fn signed_addendum_rejects_manifest_without_signature_on_first_sync() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let keypair = gen_keypair();
    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: nosig signers: ['{}'] patches: [] }}\n", keypair.pk.to_base64()),
    ).unwrap();
    // Intentionally do NOT create addendum.nuon.minisig

    assert_success(
        &run(
            root,
            &[
                "addendum",
                "add",
                addendum_path.to_str().unwrap(),
                "--ref",
                "main",
            ],
        ),
        "add addendum",
    );

    // Trigger sync by running info on any package (addenda are loaded).
    let blocked = run(root, &["info", "dummy"]);
    assert_failure_contains(
        &blocked,
        "manifest signature does not verify",
        "unsigned addendum manifest with declared signers is rejected",
    );
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test signed_addendum -- --test-threads=1
```
Expected: All signed addendum tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/addendum.rs tests/smoke.rs
git commit -m "feat: proof-of-possession and graceful rotation for signed addenda"
```

---

### Task 4: Signed Rune Manifest

**Files:**
- Modify: `src/tome/mod.rs`
- Modify: `tests/smoke.rs`
- Test: `tests/smoke.rs`

**Context:** A `runes-manifest.nuon` lists all authorized runes and their sha256s. Verified on sync against pinned (or advertised) signers.

- [ ] **Step 1: Add `verify_runes_manifest` function**

Add to `src/tome/mod.rs` (after `verify_archive`):

```rust
/// Verifies a tome's `runes-manifest.nuon` against `pubkeys`. Checks that every `.rn` file in
/// `runes/` is listed with a matching sha256, and that no extra runes exist.
pub fn verify_runes_manifest(cache: &Path, pubkeys: &[String]) -> Result<()> {
    let manifest_path = cache.join("runes-manifest.nuon");
    if !manifest_path.exists() {
        bail!("signed tome is missing runes-manifest.nuon");
    }
    signing::verify_detached(&manifest_path, pubkeys)
        .with_context(|| "verify runes-manifest.nuon signature")?;

    let manifest = nuon_io::read_nuon(&manifest_path)
        .with_context(|| format!("read runes manifest {}", manifest_path.display()))?;
    let record = expect_record(manifest, "runes manifest")?;
    let runes = match record.get("runes") {
        Some(Value::Record { val, .. }) => val,
        _ => bail!("runes manifest field `runes` must be a record"),
    };

    let runes_dir = cache.join("runes");
    if !runes_dir.exists() {
        bail!("tome cache is missing runes/ directory");
    }

    // Check every rune in the manifest exists and hashes match.
    for (rune_name, hash_value) in runes.iter() {
        let rune_path = runes_dir.join(rune_name);
        if !rune_path.exists() {
            bail!("runes manifest lists `{}` but it is missing", rune_name);
        }
        let expected_hash = hash_value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("runes manifest hash for `{}` must be a string", rune_name))?;
        let actual_hash = format!("sha256:{}", sha256_file(&rune_path));
        if actual_hash != expected_hash {
            bail!(
                "rune `{}` hash mismatch: manifest says `{}`, actual is `{}`",
                rune_name,
                expected_hash,
                actual_hash
            );
        }
    }

    // Check no extra runes exist.
    for entry in std::fs::read_dir(&runes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rn") {
            continue;
        }
        let rune_name = path.file_name().unwrap().to_string_lossy();
        if !runes.contains(&rune_name) {
            bail!(
                "rune `{}` exists in runes/ but is not listed in runes-manifest.nuon",
                rune_name
            );
        }
    }

    Ok(())
}

fn sha256_file(path: &std::path::Path) -> String {
    use sha2::{Sha256, Digest};
    let mut file = std::fs::File::open(path).expect("open file for sha256");
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).expect("hash file");
    hex::encode(hasher.finalize())
}
```

Add `hex` to `[dependencies]` in `Cargo.toml` (sha2 is already there):

```toml
hex = "0.4"
```

Also add `hex` to `[dev-dependencies]`.

- [ ] **Step 2: Integrate rune manifest verification into `validate_tome_cache`**

In `validate_tome_cache` in `src/tome/mod.rs`, after existing validation logic, add:

```rust
    // Verify runes manifest for signed tomes.
    if !manifest.signers.is_empty() {
        let pubkeys = if tome.signer_pubkeys.is_empty() {
            // First sync: verify against advertised signers (proof-of-possession).
            &manifest.signers
        } else {
            &tome.signer_pubkeys
        };
        verify_runes_manifest(cache_path, pubkeys)
            .with_context(|| format!("verify runes manifest for tome `{}`", tome.name))?;
    }
```

- [ ] **Step 3: Update `build_signed_tome` helper to generate runes manifest**

In `tests/smoke.rs`, after creating runes and before signing, generate `runes-manifest.nuon`:

```rust
fn build_signed_tome(tome: &Path, name: &str, triple: &str, keypair: &minisign::KeyPair) {
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    ).unwrap();
    let pubkey = keypair.pk.to_base64();
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: '{name}'\n  signers: ['{pubkey}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
        ),
    ).unwrap();

    // Generate runes-manifest.nuon
    let mut runes_map = std::collections::BTreeMap::new();
    for entry in fs::read_dir(tome.join("runes")).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rn") {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let hash = sha256_file(&path);
            runes_map.insert(name, format!("sha256:{hash}"));
        }
    }
    let runes_manifest = format!(
        "{{\n  format: 1,\n  runes: {{ {} }}\n}}\n",
        runes_map
            .iter()
            .map(|(k, v)| format!("\"{}\": \"{}\"", k, v))
            .collect::<Vec<_>>()
            .join(", ")
    );
    fs::write(tome.join("runes-manifest.nuon"), &runes_manifest).unwrap();
    sign_to(
        &tome.join("runes-manifest.nuon.minisig"),
        runes_manifest.as_bytes(),
        keypair,
    );

    // Sign tome.rn
    sign_to(
        &tome.join("tome.rn.minisig"),
        &fs::read(tome.join("tome.rn")).unwrap(),
        keypair,
    );

    // ... rest of existing build_signed_tome (dist, archive, index, archive signature)
```

Also need a `sha256_file` helper in tests if not already present. Check if it exists:

```bash
grep -n "fn sha256_file" tests/smoke.rs
```

If not, add it near other test helpers:

```rust
fn sha256_file(path: &Path) -> String {
    use sha2::{Sha256, Digest};
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).unwrap();
    hex::encode(hasher.finalize())
}
```

Add `hex` to `[dev-dependencies]` in `Cargo.toml` if not present (sha2 is already there):

```bash
grep "^hex" Cargo.toml
```

- [ ] **Step 4: Add tests for rune manifest rejection**

```rust
#[test]
fn signed_tome_rejects_extra_rune_not_in_manifest() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "extrarune", &triple, &keypair);

    // Add an extra rune not in the manifest
    fs::write(
        tome.join("runes").join("evil.rn"),
        "export const package = { name: 'evil' version: '0.0.1' }\n",
    ).unwrap();

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add tome",
    );
    let update = run(root, &["tome", "update", "extrarune"]);
    assert_failure_contains(
        &update,
        "not listed in runes-manifest.nuon",
        "extra rune outside manifest is rejected",
    );
}

#[test]
fn signed_tome_rejects_missing_runes_manifest() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "nomanifest", &triple, &keypair);
    fs::remove_file(tome.join("runes-manifest.nuon")).unwrap();

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add tome",
    );
    let update = run(root, &["tome", "update", "nomanifest"]);
    assert_failure_contains(
        &update,
        "missing runes-manifest.nuon",
        "missing runes manifest is rejected",
    );
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test signed_tome -- --test-threads=1
```
Expected: All tests pass, including new rune manifest tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/tome/mod.rs tests/smoke.rs
git commit -m "feat: signed runes-manifest.nuon for package sprawl prevention"
```

---

### Task 5: Addendum Update CLI

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `src/addendum.rs`
- Test: `tests/smoke.rs`

**Context:** Add `grm addendum update [name]` command, mirroring `grm tome update`.

- [ ] **Step 1: Add CLI definitions**

In `src/cli.rs`, add to `AddendumCommand`:

```rust
#[derive(Debug, Subcommand)]
pub enum AddendumCommand {
    /// Add an addendum by cloning a git repository of data-only rune overlays.
    Add(TomeAddArgs),
    /// Remove a configured addendum.
    #[command(visible_alias = "rm")]
    Remove(TomeRemoveArgs),
    /// List configured addenda.
    #[command(visible_alias = "ls")]
    List,
    /// Sync configured addenda, fetching the latest commit for their tracked ref.
    #[command(visible_aliases = ["up", "sync"])]
    Update(TomeUpdateArgs),
}
```

Reuse `TomeUpdateArgs` (same structure: optional name).

- [ ] **Step 2: Add dispatch in main.rs**

In `src/main.rs`, add to the `AddendumCommand` match:

```rust
AddendumCommand::Update(args) => addendum::update(args),
```

- [ ] **Step 3: Implement `addendum::update`**

In `src/addendum.rs`, add:

```rust
pub fn update(args: TomeUpdateArgs) -> Result<()> {
    if let Some(name) = args.name {
        let state = load_addendum(&name)?;
        sync_addendum_cache(&state)?;
        report(&format!("updated addendum {name}"));
        Ok(())
    } else {
        let states = load_addendums()?;
        if states.is_empty() {
            report("no addenda configured");
            return Ok(());
        }
        for state in states {
            if let Err(err) = sync_addendum_cache(&state) {
                report(&format!("failed to update addendum {}: {}", state.name, err));
            } else {
                report(&format!("updated addendum {}", state.name));
            }
        }
        Ok(())
    }
}
```

Note: `sync_addendum_cache` is currently private. Make it `pub(crate)` or `pub`.

- [ ] **Step 4: Add test for addendum update**

```rust
#[test]
fn addendum_update_syncs_latest_changes() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [] }\n",
    ).unwrap();

    assert_success(
        &run(
            root,
            &[
                "addendum",
                "add",
                addendum_path.to_str().unwrap(),
                "--ref",
                "main",
            ],
        ),
        "add addendum",
    );

    // First update should sync (cache doesn't exist yet).
    let first = run(root, &["addendum", "update", "updatable"]);
    assert_success(&first, "first update syncs");

    // Modify the addendum.
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [{ package: dummy, version: '0.2.0' }] }\n",
    ).unwrap();

    // Second update should re-sync.
    let second = run(root, &["addendum", "update", "updatable"]);
    assert_success(&second, "second update re-syncs");

    // Verify the new manifest is in cache.
    let cached = root
        .join("cache")
        .join("addendums")
        .join("updatable")
        .join("addendum.nuon");
    let contents = fs::read_to_string(&cached).unwrap();
    assert!(contents.contains("0.2.0"), "cached manifest reflects update: {contents}");
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test addendum_update -- --test-threads=1
```
Expected: New update test passes.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs src/main.rs src/addendum.rs tests/smoke.rs
git commit -m "feat: grm addendum update command"
```

---

### Task 6: Addendum Staleness Warning

**Files:**
- Modify: `src/addendum.rs`
- Test: `tests/smoke.rs`

**Context:** Before applying patches, check if the addendum cache is stale and warn.

- [ ] **Step 1: Add staleness check to `apply_patches`**

In `src/addendum.rs`, modify `apply_patches`:

```rust
pub fn apply_patches(
    metadata: &mut PackageMetadata,
    tome_name: Option<&str>,
    rune: &Path,
) -> Result<()> {
    for state in load_addendums()? {
        let cache = ensure_addendum_cache(&state)?;
        verify_addendum(&cache, &state)
            .with_context(|| format!("verify addendum `{}`", state.name))?;

        // Staleness warning.
        if let Ok(current_commit) = tome::git::head_commit(&cache) {
            if state.checked_commit.as_ref() != Some(&current_commit) {
                report(&format!(
                    "warning: addendum `{}` is stale; run `grm addendum update {}`",
                    state.name, state.name
                ));
            }
        }

        let manifest =
            read_manifest(&cache).with_context(|| format!("read addendum `{}`", state.name))?;
        // ... rest of existing patch loop
```

- [ ] **Step 2: Add test for staleness warning**

```rust
#[test]
fn addendum_staleness_warning_on_use() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: stalepatch, patches: [] }\n",
    ).unwrap();

    assert_success(
        &run(
            root,
            &[
                "addendum",
                "add",
                addendum_path.to_str().unwrap(),
                "--ref",
                "main",
            ],
        ),
        "add addendum",
    );

    // Trigger first sync.
    let info = run(root, &["info", "dummy"]);
    // info fails because dummy doesn't exist, but addendum syncs first.
    // Instead, use a tome with a real package to test cleanly.
}
```

Actually, testing the warning precisely is tricky because we need a real package for `info` to succeed. Reuse the `addendum_patches_source_metadata_before_install` setup but with a local addendum that gets modified after first sync:

```rust
#[test]
fn addendum_staleness_warning_on_use() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Reuse the patchtome setup from addendum_patches_source_metadata_before_install
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'staletome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    ).unwrap();
    fs::write(
        runes.join("stalepkg.rn"),
        "export const package = { name: 'stalepkg' version: '0.1.0' bins: { stalepkg: 'bin/stalepkg' } }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'stalepkg')\n}\n",
    ).unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum = addendum.path();
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [] }\n",
    ).unwrap();

    assert_success(
        &run(root, &["tome", "add", tome.to_str().unwrap(), "--ref", "main"]),
        "add tome",
    );
    assert_success(
        &run(
            root,
            &[
                "addendum",
                "add",
                addendum.to_str().unwrap(),
                "--ref",
                "main",
            ],
        ),
        "add addendum",
    );

    // First info call syncs the addendum.
    let first = run(root, &["info", "stalepkg"]);
    assert_success(&first, "info succeeds on first use");
    let first_text = format!("{}{}", stdout(&first), stderr(&first));
    assert!(
        !first_text.contains("stale"),
        "fresh addendum should not warn: {first_text}"
    );

    // Modify the addendum source.
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [{ package: stalepkg, version: '0.2.0' }] }\n",
    ).unwrap();

    // Second info call should warn about staleness.
    let second = run(root, &["info", "stalepkg"]);
    assert_success(&second, "info succeeds but warns");
    let second_text = format!("{}{}", stdout(&second), stderr(&second));
    assert!(
        second_text.contains("stale"),
        "stale addendum should warn: {second_text}"
    );
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test addendum_staleness -- --test-threads=1
```
Expected: Staleness warning test passes.

- [ ] **Step 4: Commit**

```bash
git add src/addendum.rs tests/smoke.rs
git commit -m "feat: warn when addendum cache is stale"
```

---

### Task 7: Final Verification

- [ ] **Step 1: Run full test suite**

```bash
cargo test
```
Expected: All tests pass.

- [ ] **Step 2: Run linting**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
Expected: Clean.

- [ ] **Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: address fmt/clippy warnings"
```

---

## Plan Self-Review

**Spec coverage:**
- Section 1 (graceful rotation) → Tasks 1, 2, 3 ✓
- Section 2 (rune manifest) → Task 4 ✓
- Section 3 (addendum auto-sync) → Tasks 5, 6 ✓

**Placeholder scan:** No TBDs, no TODOs, all code is concrete.

**Type consistency:** `capture_signer` and `capture_addendum_signer` both use `&[String]` for pubkeys and `Result<Vec<String>>` for return. `verify_detached` signature is reused consistently. All task signatures match.

**Dependencies:** `sha2` and `hex` crates needed for sha256 computation in rune manifest. Verify they're in `Cargo.toml` or add them.
