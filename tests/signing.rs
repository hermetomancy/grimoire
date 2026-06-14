//! Signature verification and TOFU key pinning for tomes and addenda.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn signed_addendum_pins_key_and_rejects_tampering() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let keypair = gen_keypair();
    let pubkey = keypair.pk.to_base64();

    // Tome with a package the addendum can patch.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'adtome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("adpatch.rn"),
        "export const package = { name: 'adpatch' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'adpatch')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: signedpatches signers: ['{pubkey}'] patches: [{{ package: adpatch version: '0.2.0' }}] }}\n"),
    )
    .unwrap();
    sign_to(
        &addendum_path.join("addendum.nuon.minisig"),
        &fs::read(addendum_path.join("addendum.nuon")).unwrap(),
        &keypair,
    );

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add tome for signed addendum test",
    );
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
        "add signed addendum",
    );

    // `info` triggers addendum sync (first use = TOFU pin).
    let info = run(root, &["info", "adpatch"]);
    assert_success(&info, "info triggers addendum sync and TOFU pin");
    let info_text = format!("{}{}", stdout(&info), stderr(&info));
    assert!(
        info_text.contains("pinned 1 signer(s)"),
        "first use should report TOFU pin: {info_text}"
    );

    let state = fs::read_to_string(
        root.join("state")
            .join("addendums")
            .join("signedpatches.nuon"),
    )
    .unwrap();
    assert!(
        state.contains("signer_pubkeys"),
        "state records a pin: {state}"
    );
    assert!(
        state.contains(&pubkey),
        "state records the actual key: {state}"
    );

    // Tamper the cached manifest without re-signing.
    let cached_manifest = root
        .join("cache")
        .join("addendums")
        .join("signedpatches")
        .join("addendum.nuon");
    let tampered = fs::read_to_string(&cached_manifest)
        .unwrap()
        .replace("signedpatches", "evilpatches");
    fs::write(&cached_manifest, tampered).unwrap();

    // Re-running `info` should verify the addendum signature and fail.
    let blocked = run(root, &["info", "adpatch"]);
    assert_failure_contains(&blocked, "signature", "tampered addendum is refused");
}

#[test]
fn signed_addendum_refuses_key_rotation_without_readd() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let key_a = gen_keypair();

    // Tome with a package.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'adtome2' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("adpatch2.rn"),
        "export const package = { name: 'adpatch2' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'adpatch2')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: rotatepatches signers: ['{}'] patches: [{{ package: adpatch2 version: '0.2.0' }}] }}\n", key_a.pk.to_base64()),
    )
    .unwrap();
    sign_to(
        &addendum_path.join("addendum.nuon.minisig"),
        &fs::read(addendum_path.join("addendum.nuon")).unwrap(),
        &key_a,
    );

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add tome for signed addendum rotation test",
    );
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
        "add signed addendum",
    );

    // First use syncs and pins key A.
    let info = run(root, &["info", "adpatch2"]);
    assert_success(&info, "first use pins key A");

    // Rebuild addendum with key B.
    let key_b = gen_keypair();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: rotatepatches signers: ['{}'] patches: [{{ package: adpatch2 version: '0.2.0' }}] }}\n", key_b.pk.to_base64()),
    )
    .unwrap();
    sign_to(
        &addendum_path.join("addendum.nuon.minisig"),
        &fs::read(addendum_path.join("addendum.nuon")).unwrap(),
        &key_b,
    );

    // Delete cache to force re-sync on next use.
    let cache = root.join("cache").join("addendums").join("rotatepatches");
    fs::remove_dir_all(&cache).unwrap();

    // Re-sync should detect key rotation and fail.
    let rotate = run(root, &["info", "adpatch2"]);
    assert_failure_contains(
        &rotate,
        "different set of signing keys",
        "key rotation without re-add is refused",
    );
}

#[test]
fn signed_addendum_rejects_manifest_without_signature_on_first_sync() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let keypair = gen_keypair();
    let pubkey = keypair.pk.to_base64();

    // Tome with a package the addendum can patch.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'nosigtome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("nosigpatch.rn"),
        "export const package = { name: 'nosigpatch' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'nosigpatch')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    // Advertise a signer but do NOT create the .minisig file.
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: nosigpatches signers: ['{pubkey}'] patches: [{{ package: nosigpatch version: '0.2.0' }}] }}\n"),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add tome for no-sig addendum test",
    );
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
        "add addendum with signer but no signature",
    );

    let info = run(root, &["info", "nosigpatch"]);
    assert_failure_contains(
        &info,
        "manifest signature does not verify",
        "unsigned manifest on first sync is refused",
    );
}

#[test]
fn signed_addendum_allows_graceful_key_rotation() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let key_a = gen_keypair();

    // Tome with a package.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'gracetome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("gracepatch.rn"),
        "export const package = { name: 'gracepatch' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'gracepatch')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: gracepatches signers: ['{}'] patches: [{{ package: gracepatch version: '0.2.0' }}] }}\n", key_a.pk.to_base64()),
    )
    .unwrap();
    sign_to(
        &addendum_path.join("addendum.nuon.minisig"),
        &fs::read(addendum_path.join("addendum.nuon")).unwrap(),
        &key_a,
    );

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add tome for graceful rotation test",
    );
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
        "add signed addendum",
    );

    // First use syncs and pins key A.
    let info = run(root, &["info", "gracepatch"]);
    assert_success(&info, "first use pins key A");

    // Rebuild addendum with key B, but sign the manifest with key A (old key).
    let key_b = gen_keypair();
    fs::write(
        addendum_path.join("addendum.nuon"),
        format!("{{ name: gracepatches signers: ['{}'] patches: [{{ package: gracepatch version: '0.2.0' }}] }}\n", key_b.pk.to_base64()),
    )
    .unwrap();
    sign_to(
        &addendum_path.join("addendum.nuon.minisig"),
        &fs::read(addendum_path.join("addendum.nuon")).unwrap(),
        &key_a,
    );

    // Delete cache to force re-sync on next use.
    let cache = root.join("cache").join("addendums").join("gracepatches");
    fs::remove_dir_all(&cache).unwrap();

    // Re-sync should accept the rotation because the old key signed the new manifest.
    let rotate = run(root, &["info", "gracepatch"]);
    assert_success(&rotate, "graceful key rotation is accepted");
    let rotate_text = format!("{}{}", stdout(&rotate), stderr(&rotate));
    assert!(
        rotate_text.contains("rotated signing keys"),
        "rotation should be reported: {rotate_text}"
    );
}

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
