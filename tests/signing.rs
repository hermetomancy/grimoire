//! Signature verification and TOFU key pinning for tomes.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn signed_tome_pins_key_and_rejects_archive_tampering() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let pubkey = keypair.pk.to_base64();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "signedcore", &triple, &keypair);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed tome",
    );
    let update = run(root, &["tome", "update", "signedcore"]);
    assert_success(&update, "update signed tome");
    let update_text = format!("{}{}", stdout(&update), stderr(&update));
    assert!(
        update_text.contains("pinned 1 signer(s)"),
        "update should report the TOFU pin: {update_text}"
    );

    // The pinned key is persisted in tome state (trust-on-first-use).
    let state =
        fs::read_to_string(root.join("state").join("tomes").join("signedcore.nuon")).unwrap();
    assert!(
        state.contains("signer_pubkeys"),
        "state records a pin: {state}"
    );
    assert!(
        state.contains(&pubkey),
        "state records the actual key: {state}"
    );

    // Install verifies the signed archive against the pinned key and succeeds.
    assert_success(
        &run(root, &["install", "sgnpkg"]),
        "install from signed tome",
    );
    assert_eq!(stdout(&run_shim(root, "sgnpkg")).trim(), "signed");

    // Remove it so the next install actually re-resolves against the index rather than
    // short-circuiting as already-installed.
    assert_success(&run(root, &["remove", "sgnpkg"]), "remove before reinstall");

    // Tamper the cached archive AND rewrite the index hash to match, simulating a compromised
    // host/index. The checksum gate then passes, so the per-archive signature — which the attacker
    // cannot forge without the private key — is the only remaining defense and must refuse it.
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let dist = root
        .join("cache")
        .join("tomes")
        .join("signedcore")
        .join("dist");
    let cached_archive = dist.join(&archive_name);
    let original_hash = sha256_file(&cached_archive);
    let mut tampered = fs::read(&cached_archive).unwrap();
    tampered[0] ^= 0xFF; // flip first byte
    fs::write(&cached_archive, &tampered).unwrap();
    let tampered_hash = sha256_file(&cached_archive);
    let index_path = dist.join("index.nuon");
    let index = fs::read_to_string(&index_path).unwrap();
    fs::write(&index_path, index.replace(&original_hash, &tampered_hash)).unwrap();

    let blocked = run(root, &["install", "sgnpkg"]);
    assert_failure_contains(
        &blocked,
        "signature",
        "tampered archive is refused by signature",
    );
}

#[test]
fn signed_binary_installs_over_http() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let pubkey = keypair.pk.to_base64();

    // A fully-signed tome, then re-pointed at an HTTP binhost serving its dist/ (archive,
    // archive.minisig, index.nuon). Installing must fetch the detached `.minisig` over the same
    // transport as the archive and verify it against the downloaded bytes — the path that was
    // previously broken (the signature was only ever looked up on the local filesystem).
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "signedhttp", &triple, &keypair);

    let base_url = serve_dir(tome.join("dist"));
    let manifest = format!(
        "export const tome = {{\n  name: 'signedhttp'\n  signers: ['{pubkey}']\n  packages: {{ repo: '{base_url}', format: 'http', index: 'index.nuon' }}\n}}\n"
    );
    fs::write(tome.join("tome.rn"), &manifest).unwrap();
    sign_to(&tome.join("tome.rn.minisig"), manifest.as_bytes(), &keypair);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed http tome",
    );
    assert_success(
        &run(root, &["tome", "update", "signedhttp"]),
        "update signed http tome",
    );

    assert_success(
        &run(root, &["install", "sgnpkg"]),
        "install signed binary over http",
    );
    assert_eq!(stdout(&run_shim(root, "sgnpkg")).trim(), "signed");
}

#[test]
fn signed_tome_refuses_key_rotation_without_readd() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let key_a = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "rotatecore", &triple, &key_a);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed tome",
    );
    assert_success(
        &run(root, &["tome", "update", "rotatecore"]),
        "pin key A on first update",
    );

    // Re-advertise and re-sign with a *different* key. A silent accept here would defeat the
    // whole point of pinning, so it must be refused.
    let key_b = gen_keypair();
    build_signed_tome(tome, "rotatecore", &triple, &key_b);

    let rotate = run(root, &["tome", "update", "rotatecore"]);
    assert_failure_contains(
        &rotate,
        "signature",
        "key rotation without re-add is refused",
    );
}

#[test]
fn signed_tome_allows_graceful_key_rotation() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let key_a = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "rotatecore", &triple, &key_a);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed tome",
    );
    assert_success(
        &run(root, &["tome", "update", "rotatecore"]),
        "pin key A on first update",
    );

    // Graceful rotation: change signers to key B, but sign the manifest with key A
    // (the currently pinned key) to authorize the rotation.
    let key_b = gen_keypair();
    let pubkey_b = key_b.pk.to_base64();
    let manifest_body = format!(
        "export const tome = {{\n  name: 'rotatecore'\n  signers: ['{pubkey_b}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
    );
    fs::write(tome.join("tome.rn"), &manifest_body).unwrap();
    sign_to(
        &tome.join("tome.rn.minisig"),
        manifest_body.as_bytes(),
        &key_a,
    );

    // Rebuild archive and index, signing the archive with the NEW key (key_b).
    let dist = tome.join("dist");
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let archive = make_versioned_archive(
        &dist.join(&archive_name),
        "sgnpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'signed\n'\n",
    );
    let hash = sha256_file(&archive);
    let index_body = format!(
        "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"sgnpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
    );
    fs::write(dist.join("index.nuon"), &index_body).unwrap();
    sign_to(
        &dist.join(format!("{archive_name}.minisig")),
        &fs::read(&archive).unwrap(),
        &key_b,
    );

    // Update should succeed because the old key authorized the rotation.
    let rotate = run(root, &["tome", "update", "rotatecore"]);
    assert_success(&rotate, "graceful key rotation should succeed");
    let rotate_text = format!("{}{}", stdout(&rotate), stderr(&rotate));
    assert!(
        rotate_text.contains("rotated signing keys"),
        "update should report key rotation: {rotate_text}"
    );

    // The new key is now pinned.
    let state =
        fs::read_to_string(root.join("state").join("tomes").join("rotatecore.nuon")).unwrap();
    assert!(
        state.contains(&pubkey_b),
        "state should record the new key after rotation: {state}"
    );

    // Installing from the tome should also succeed because the archive is signed with the new key.
    assert_success(
        &run(root, &["install", "sgnpkg"]),
        "install from rotated tome",
    );
    assert_eq!(stdout(&run_shim(root, "sgnpkg")).trim(), "signed");
}

#[test]
fn signed_tome_rejects_manifest_without_signature_on_first_sync() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let pubkey = keypair.pk.to_base64();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();
    // Generate and sign runes-manifest.nuon so validation reaches the manifest signature check.
    let hash = sha256_file(&tome.join("runes").join("dummy.rn"));
    let runes_manifest_body = format!("{{ format: 1, runes: {{ \"dummy.rn\": \"{hash}\" }} }}\n");
    fs::write(tome.join("runes-manifest.nuon"), &runes_manifest_body).unwrap();
    sign_to(
        &tome.join("runes-manifest.nuon.minisig"),
        runes_manifest_body.as_bytes(),
        &keypair,
    );
    // Write a manifest that advertises signers but omit the .minisig.
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: 'unsignedmanifest'\n  signers: ['{pubkey}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
        ),
    )
    .unwrap();

    let dist = tome.join("dist");
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let archive = make_versioned_archive(
        &dist.join(&archive_name),
        "sgnpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'signed\n'",
    );
    let hash = sha256_file(&archive);
    let index_body = format!(
        "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"sgnpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
    );
    fs::write(dist.join("index.nuon"), &index_body).unwrap();
    sign_to(
        &dist.join(format!("{archive_name}.minisig")),
        &fs::read(&archive).unwrap(),
        &keypair,
    );

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed tome without manifest signature",
    );
    let update = run(root, &["tome", "update", "unsignedmanifest"]);
    assert_failure_contains(
        &update,
        "manifest signature does not verify",
        "first sync of unsigned manifest is refused",
    );
}

#[test]
fn signed_tome_rejects_extra_rune_not_in_manifest() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let keypair = gen_keypair();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    build_signed_tome(tome, "extracore", &triple, &keypair);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add signed tome",
    );
    assert_success(
        &run(root, &["tome", "update", "extracore"]),
        "first update pins key",
    );

    // Add an extra rune not in the manifest.
    fs::write(
        tome.join("runes").join("evil.rn"),
        "export const package = { name: 'evil' version: '0.0.1' }\n",
    )
    .unwrap();

    let update = run(root, &["tome", "update", "extracore"]);
    assert_failure_contains(
        &update,
        "extra rune",
        "extra rune not in manifest is refused",
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
    build_signed_tome(tome, "missingcore", &triple, &keypair);

    // Remove the manifest before adding.
    fs::remove_file(tome.join("runes-manifest.nuon")).unwrap();

    // Local tomes are validated on add, so the missing manifest is caught immediately.
    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_failure_contains(
        &add,
        "runes-manifest.nuon is missing",
        "missing runes manifest is refused",
    );
}

#[test]
fn unsigned_tome_leaves_no_pin() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // A tome that publishes an index but declares no `signer` stays unsigned: no pin recorded,
    // installs proceed without signature verification (verify-if-present).
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'plaincore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let dist = tome.join("dist");
    let archive_name = format!("plainpkg-0.1.0-{triple}.tar.zst");
    let archive = make_versioned_archive(
        &dist.join(&archive_name),
        "plainpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'plain\\n'\n",
    );
    let hash = sha256_file(&archive);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"plainpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add unsigned tome",
    );
    assert_success(
        &run(root, &["tome", "update", "plaincore"]),
        "update unsigned tome",
    );
    let state =
        fs::read_to_string(root.join("state").join("tomes").join("plaincore.nuon")).unwrap();
    assert!(
        !state.contains("signer_pubkeys"),
        "unsigned tome records no pin: {state}"
    );
    assert_success(
        &run(root, &["install", "plainpkg"]),
        "install from unsigned tome",
    );
}
