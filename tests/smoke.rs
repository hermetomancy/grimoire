//! End-to-end smoke tests that drive the built `grm` binary.
//!
//! Each test runs against its own `GRIMOIRE_ROOT` temp directory so they can run in
//! parallel without sharing install state. The current working directory is the crate
//! root, so relative paths like `tome-example/runes/hello.rn` resolve as they would for a
//! user invoking grimoire from the project.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use xz2::write::XzEncoder;

const BIN: &str = env!("CARGO_BIN_EXE_grm");
fn core_readiness_packages() -> &'static [&'static str] {
    if std::env::consts::OS == "linux" {
        &[
            "linux-headers",
            "musl",
            "compiler-rt",
            "llvm",
            "clang",
            "cmake",
            "python3",
            "make",
            "toybox",
            "toolchain-wrappers",
        ]
    } else {
        &[
            "llvm",
            "clang",
            "compiler-rt",
            "cmake",
            "python3",
            "make",
            "toybox",
            "toolchain-wrappers",
        ]
    }
}

type ZstdFileEncoder = zstd::stream::write::Encoder<'static, fs::File>;
type GzipFileEncoder = GzEncoder<fs::File>;
type XzFileEncoder = XzEncoder<fs::File>;

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("GRIMOIRE_ROOT", root)
        .output()
        .expect("spawn grimoire")
}

/// Like [`run`], but with extra environment variables — used to pin `GRIMOIRE_BUILD_ENV` so a test
/// can simulate building and installing under different host toolchains.
fn run_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BIN);
    command.args(args).env("GRIMOIRE_ROOT", root);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("spawn grimoire")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} should succeed, exit={:?} stderr={}",
        output.status.code(),
        stderr(output)
    );
}

fn assert_failure_contains(output: &Output, needle: &str, label: &str) {
    assert!(
        !output.status.success(),
        "{label} should fail but succeeded; stdout={}",
        stdout(output)
    );
    let stderr = stderr(output);
    assert!(
        stderr.contains(needle),
        "{label}: expected stderr to contain `{needle}`, got: {stderr}"
    );
}

fn target_triple() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let abi = match os {
        "macos" => "darwin",
        "linux" => "musl",
        _ => "unknown",
    };
    format!("{os}-{arch}-{abi}")
}

fn sha256_file(path: &Path) -> String {
    let mut file = fs::File::open(path).expect("open archive for hashing");
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).expect("read archive");
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn archive_member_text(path: &Path, member: &str) -> String {
    let file = fs::File::open(path).expect("open package archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("decode package archive");
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().expect("read package archive entries") {
        let mut entry = entry.expect("read package archive entry");
        if entry.path().expect("read archive path").as_ref() == Path::new(member) {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .expect("read archive member text");
            return text;
        }
    }
    panic!("archive member {member} was not found");
}

fn run_shim(root: &Path, name: &str) -> Output {
    Command::new(root.join("profiles").join("current").join("bin").join(name))
        .output()
        .expect("run installed shim")
}

#[test]
fn tome_add_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // The tome names itself `core` in its manifest; `add` reads that name rather than
    // taking one on the command line.
    let tome = make_fake_tome();
    let tome_path = tome.path().to_str().unwrap();

    let add = run(root, &["tome", "add", tome_path, "--ref", "stable"]);
    assert_success(&add, "tome add core");

    let state_path = root.join("state").join("tomes").join("core.nuon");
    assert!(state_path.exists(), "tome state should exist");
    let state = fs::read_to_string(&state_path).unwrap();
    assert!(state.contains("name: core"), "state name: {state}");
    assert!(state.contains("ref: stable"), "state ref: {state}");

    let list = run(root, &["tome", "list"]);
    assert_success(&list, "tome list");
    let listed = stdout(&list);
    assert!(listed.contains("core"), "list includes name: {listed}");
    assert!(listed.contains("stable"), "list includes ref: {listed}");

    let duplicate = run(root, &["tome", "add", tome_path]);
    assert_failure_contains(&duplicate, "already exists", "reject duplicate tome");

    let remove = run(root, &["tome", "remove", "core"]);
    assert_success(&remove, "tome remove core");
    assert!(!state_path.exists(), "removed tome state should be gone");
}

#[test]
fn addendum_add_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: localpatches, patches: [] }\n",
    )
    .unwrap();

    let add = run(
        root,
        &[
            "addendum",
            "add",
            addendum_path.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add addendum");

    let state_path = root
        .join("state")
        .join("addendums")
        .join("localpatches.nuon");
    assert!(state_path.exists(), "addendum state should exist");
    let state = fs::read_to_string(&state_path).unwrap();
    assert!(state.contains("name: localpatches"), "state name: {state}");
    assert!(state.contains("ref: main"), "state ref: {state}");
    assert!(
        !state.contains("signer_pubkeys"),
        "unsigned addendum records no pin: {state}"
    );

    let list = run(root, &["addendum", "list"]);
    assert_success(&list, "addendum list");
    assert!(
        stdout(&list).contains("localpatches"),
        "list includes addendum: {}",
        stdout(&list)
    );

    let duplicate = run(root, &["addendum", "add", addendum_path.to_str().unwrap()]);
    assert_failure_contains(&duplicate, "already exists", "reject duplicate addendum");

    let remove = run(root, &["addendum", "remove", "localpatches"]);
    assert_success(&remove, "remove addendum");
    assert!(
        !state_path.exists(),
        "removed addendum state should be gone"
    );
}

#[test]
fn addendum_update_syncs_latest_changes() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [] }\n",
    )
    .unwrap();

    let add = run(
        root,
        &[
            "addendum",
            "add",
            addendum_path.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add addendum");

    let update = run(root, &["addendum", "update", "updatable"]);
    assert_success(&update, "update addendum");

    // Modify the source to add a new patch.
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [{ package: testpkg, version: '0.2.0' }] }\n",
    )
    .unwrap();

    let update2 = run(root, &["addendum", "update", "updatable"]);
    assert_success(&update2, "update addendum after source change");

    let cached_manifest = root
        .join("cache")
        .join("addendums")
        .join("updatable")
        .join("addendum.nuon");
    let cached = fs::read_to_string(&cached_manifest).unwrap();
    assert!(
        cached.contains("testpkg"),
        "cached manifest should reflect updated patch: {cached}"
    );
    assert!(
        cached.contains("0.2.0"),
        "cached manifest should reflect updated version: {cached}"
    );
}

#[test]
fn addendum_staleness_warning_on_use() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'staletome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("stalepkg.rn"),
        "export const package = { name: 'stalepkg' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'stalepkg')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum = addendum.path();
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [] }\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
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
        !first_text.contains("is stale"),
        "fresh addendum should not warn: {first_text}"
    );

    // Modify the addendum source and simulate staleness by making the recorded commit diverge.
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [{ package: stalepkg, version: '0.2.0' }] }\n",
    )
    .unwrap();
    let state_path = root.join("state").join("addendums").join("staleadd.nuon");
    let mut state = fs::read_to_string(&state_path).unwrap();
    state = state.replace('}', ", checked_commit: \"deadbeef\"}");
    fs::write(&state_path, state).unwrap();

    // Second info call should warn about staleness.
    let second = run(root, &["info", "stalepkg"]);
    assert_success(&second, "info succeeds but warns");
    let second_text = format!("{}{}", stdout(&second), stderr(&second));
    assert!(
        second_text.contains("is stale"),
        "stale addendum should warn: {second_text}"
    );
}

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
fn command_parsing() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            &format!("--output={}", out.display()),
            "--quiet",
        ],
    );
    assert_success(&build, "build supports --output=value");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    assert!(archive.exists(), "--output=value archive should exist");

    // `--ref=value` form. A local tome lets `add` read its manifest name offline.
    let add = run(root, &["tome", "add", "./tome-example", "--ref=stable"]);
    assert_success(&add, "tome add supports --ref=value");
    let state = fs::read_to_string(root.join("state").join("tomes").join("tome-example.nuon"))
        .expect("tome-example state");
    assert!(state.contains("ref: stable"), "--ref=value state: {state}");

    let remove = run(root, &["tome", "remove", "tome-example"]);
    assert_success(&remove, "remove tome-example tome");

    let extra = run(root, &["build", "./tome-example/runes/hello.rn", "extra"]);
    assert_failure_contains(
        &extra,
        "unexpected argument 'extra' found",
        "reject extra build argument",
    );

    let unknown = run(root, &["doctor", "--unknown"]);
    assert_failure_contains(
        &unknown,
        "unexpected argument '--unknown' found",
        "reject unknown option",
    );

    let missing = run(root, &["build", "hello", "--output", "--quiet"]);
    assert_failure_contains(
        &missing,
        "a value is required for '--output <OUTPUT>'",
        "reject missing option value",
    );

    let bool_value = run(root, &["install", "hello", "--quiet=true"]);
    assert_failure_contains(
        &bool_value,
        "unexpected value 'true' for '--quiet'",
        "reject bool option value",
    );
}

#[test]
fn build_respects_musl_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let rune_path = root.join("test.rn");
    fs::write(
        &rune_path,
        "export const package = { name: 'testpkg' version: '0.1.0' }\n\nexport def build [ctx] {\n  echo $ctx.target | save ($ctx.package_dir | path join 'target.txt')\n}\n",
    ).unwrap();

    let build = run(
        root,
        &[
            "build",
            rune_path.to_str().unwrap(),
            &format!("--output={}", out.display()),
            "--target",
            "linux-x86_64-musl",
            "--bootstrap",
        ],
    );
    assert_success(&build, "build with musl target");

    let archive = out.join("testpkg-0.1.0-linux-x86_64-musl.tar.zst");
    assert!(archive.exists(), "musl archive should exist: {archive:?}");

    let target_text = archive_member_text(&archive, "target.txt");
    assert_eq!(
        target_text.trim(),
        "linux-x86_64-musl",
        "build context target"
    );
}

#[test]
fn install_from_configured_tome() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = make_fake_tome();
    let tome_path = tome.path().to_str().unwrap();

    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add local core");

    let update = run(root, &["tome", "update", "core"]);
    assert_success(&update, "tome update local core");
    assert!(
        stdout(&update).contains("updated tome core (main "),
        "update reports checked ref and commits: {}",
        stdout(&update)
    );

    let state = fs::read_to_string(root.join("state").join("tomes").join("core.nuon")).unwrap();
    assert!(state.contains("checked_ref: main"), "checked ref: {state}");
    assert!(state.contains("name: core"), "manifest name: {state}");
    assert!(
        state.contains("index: \"index.nuon\""),
        "package index: {state}"
    );

    // A configured tome's rune takes precedence over the bundled example rune.
    let install_hello = run(root, &["install", "hello"]);
    assert_success(&install_hello, "install hello prefers configured tome");
    let hello = run_shim(root, "hello");
    assert_success(&hello, "run tome-preferred hello");
    assert_eq!(
        stdout(&hello).trim(),
        "hello from configured tome",
        "tome rune precedence"
    );

    let remove_hello = run(root, &["remove", "hello"]);
    assert_success(&remove_hello, "remove tome-preferred hello");

    let install = run(root, &["install", "tomehello"]);
    assert_success(&install, "install tomehello from configured tome");
    assert!(
        root.join("cache")
            .join("tomes")
            .join("core")
            .join("runes")
            .join("tomehello.rn")
            .exists(),
        "cached tome rune should exist"
    );
    let tomehello = run_shim(root, "tomehello");
    assert_success(&tomehello, "run tome-installed package");
    assert_eq!(
        stdout(&tomehello).trim(),
        "hello from tome",
        "tome package shim output"
    );

    let remove_tome = run(root, &["tome", "remove", "core"]);
    assert_success(&remove_tome, "remove local core tome");
}

#[test]
fn install_from_example_tome() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    let update = run(root, &["tome", "update", "tome-example"]);
    assert_success(&update, "tome update tome-example");

    let install = run(root, &["install", "hello"]);
    assert_success(&install, "install hello from tome-example");
    assert!(
        root.join("cache")
            .join("tomes")
            .join("tome-example")
            .join("runes")
            .join("hello.rn")
            .exists(),
        "cached tome-example rune should exist"
    );

    let hello = run_shim(root, "hello");
    assert_success(&hello, "run example hello");
    assert_eq!(
        stdout(&hello).trim(),
        "hello from grimoire",
        "example hello output"
    );

    let remove_hello = run(root, &["remove", "hello"]);
    assert_success(&remove_hello, "remove tome-example hello");

    let remove_tome = run(root, &["tome", "remove", "tome-example"]);
    assert_success(&remove_tome, "remove tome-example");
}

#[test]
fn tome_init_rune_authoring_flow() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Author a tome from scratch: scaffold the tome skeleton, then add a package rune to it.
    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("mytome");
    let tome_path = tome_dir.to_str().unwrap();

    let init = run(
        root,
        &[
            "tome",
            "init",
            "mytome",
            "--path",
            tome_path,
            "--description",
            "Authoring smoke test",
        ],
    );
    assert_success(&init, "tome init");
    assert!(tome_dir.join("tome.rn").exists(), "tome.rn scaffolded");
    assert!(tome_dir.join("runes").is_dir(), "runes/ scaffolded");
    assert!(tome_dir.join("dist").is_dir(), "dist/ scaffolded");
    assert!(
        fs::read_to_string(tome_dir.join(".gitignore"))
            .unwrap()
            .contains("/dist/"),
        ".gitignore ignores dist/"
    );

    let rune = run(root, &["tome", "rune", "widget", "--path", tome_path]);
    assert_success(&rune, "tome rune");
    assert!(
        tome_dir.join("runes").join("widget.rn").exists(),
        "widget rune scaffolded"
    );

    // The scaffolded tome is valid: it can be added and the rune builds and installs.
    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add authored");

    let install = run(root, &["install", "widget", "--from-source"]);
    assert_success(&install, "install authored widget");

    let widget = run_shim(root, "widget");
    assert_success(&widget, "run authored widget");
    assert_eq!(
        stdout(&widget).trim(),
        "widget is not implemented yet",
        "authored widget stub output"
    );
}

#[test]
fn tome_build_publishes_prebuilt_into_index() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("mytome");
    let tome_path = tome_dir.to_str().unwrap();

    let init = run(root, &["tome", "init", "mytome", "--path", tome_path]);
    assert_success(&init, "tome init");
    let rune = run(root, &["tome", "rune", "widget", "--path", tome_path]);
    assert_success(&rune, "tome rune");

    // Build the rune into the tome's package repo and register it in the index.
    let build = run(root, &["tome", "build", "widget", "--path", tome_path]);
    assert_success(&build, "tome build");

    let target = target_triple();
    let archive = tome_dir
        .join("dist")
        .join(format!("widget-0.1.0-{target}.tar.zst"));
    assert!(archive.exists(), "built archive should exist: {archive:?}");

    let archive_rel = format!("widget-0.1.0-{target}.tar.zst");
    let index = fs::read_to_string(tome_dir.join("dist").join("index.nuon")).unwrap();
    assert!(index.contains("widget"), "index lists widget: {index}");
    assert!(
        index.contains(&archive_rel),
        "index records archive path: {index}"
    );

    // Point the tome at its built `dist/` directory as a local package repo so the published
    // prebuilt archive is installable without --from-source.
    fs::write(
        tome_dir.join("tome.rn"),
        "export const tome = {\n  name: 'mytome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add authored");
    let install = run(root, &["install", "widget"]);
    assert_success(&install, "install prebuilt widget");
    // The published prebuilt's store hash matches the local rune, so it is used as a substitute
    // rather than rebuilt: no source-build archive is produced for widget.
    assert!(
        !root
            .join("cache")
            .join("builds")
            .join(format!("widget-0.1.0-{target}.tar.zst"))
            .exists(),
        "matching prebuilt should be substituted, not built from source"
    );
    let widget = run_shim(root, "widget");
    assert_eq!(
        stdout(&widget).trim(),
        "widget is not implemented yet",
        "prebuilt widget stub output"
    );

    // A rebuild replaces the entry in place rather than duplicating it.
    let rebuild = run(root, &["tome", "build", "widget", "--path", tome_path]);
    assert_success(&rebuild, "tome build rebuild");
    let index = fs::read_to_string(tome_dir.join("dist").join("index.nuon")).unwrap();
    assert_eq!(
        index.matches(&archive_rel).count(),
        1,
        "rebuild should upsert, not duplicate: {index}"
    );
}

/// A prebuilt whose published `store_hash` does not match the local rune's inputs is stale and must
/// not be substituted: the binhost is keyed by store hash, so a mismatch forces a source build.
#[test]
fn stale_prebuilt_is_rebuilt_from_source() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    let dist = tome.join("dist");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'staletome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    // The source rune produces a bin that announces it was built from source.
    fs::write(
        runes.join("stalepkg.rn"),
        "export const package = {\n  name: 'stalepkg'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'built from source\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stalepkg')\n}\n",
    )
    .unwrap();

    // A prebuilt that announces itself, published with a store_hash that does NOT match the rune.
    let archive_name = format!("stalepkg-0.1.0-{triple}.tar.zst");
    let prebuilt = make_versioned_archive(
        &dist.join(&archive_name),
        "stalepkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'stale prebuilt\\n'\n",
    );
    let hash = sha256_file(&prebuilt);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"0000000000000000\": {{ name: \"stalepkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add stale tome");

    let install = run(root, &["install", "stalepkg"]);
    assert_success(&install, "install stalepkg");

    // The stale prebuilt is rejected; the package is built from the rune instead.
    let stalepkg = run_shim(root, "stalepkg");
    assert_eq!(
        stdout(&stalepkg).trim(),
        "built from source",
        "stale prebuilt must be rebuilt from source, not substituted"
    );
    assert!(
        root.join("cache")
            .join("builds")
            .join(&archive_name)
            .exists(),
        "a source build should have run because the prebuilt was stale"
    );
}

/// A prebuilt published by one host toolchain must not be substituted on a host with a different
/// toolchain identity: the build environment is part of the store hash, so the hashes diverge and
/// the installer rebuilds. The same prebuilt *is* substituted when the toolchain identity matches.
#[test]
fn prebuilt_is_toolchain_specific() {
    let triple = target_triple();
    let build_root = TempDir::new().unwrap();
    let build_root = build_root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("tktome");
    let tome_path = tome_dir.to_str().unwrap();

    assert_success(
        &run(build_root, &["tome", "init", "tktome", "--path", tome_path]),
        "tome init",
    );
    assert_success(
        &run(build_root, &["tome", "rune", "tk", "--path", tome_path]),
        "tome rune",
    );

    // Publish a prebuilt whose store hash is computed under toolchain "alpha".
    assert_success(
        &run_env(
            build_root,
            &["tome", "build", "tk", "--path", tome_path],
            &[("GRIMOIRE_BUILD_ENV", "alpha")],
        ),
        "tome build under toolchain alpha",
    );

    // Serve the built dist/ as a local package repo.
    fs::write(
        tome_dir.join("tome.rn"),
        "export const tome = {\n  name: 'tktome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let build_archive = |root: &Path| {
        root.join("cache")
            .join("builds")
            .join(format!("tk-0.1.0-{triple}.tar.zst"))
    };

    // Same toolchain identity → the prebuilt is a valid substitute, so no source build runs.
    let matching = TempDir::new().unwrap();
    let matching = matching.path();
    assert_success(
        &run(matching, &["tome", "add", tome_path, "--ref", "main"]),
        "add tome (matching toolchain)",
    );
    assert_success(
        &run_env(
            matching,
            &["install", "tk"],
            &[("GRIMOIRE_BUILD_ENV", "alpha")],
        ),
        "install tk under matching toolchain",
    );
    assert!(
        !build_archive(matching).exists(),
        "matching toolchain should substitute the prebuilt, not build"
    );

    // Different toolchain identity → the prebuilt is not a match, so tk is rebuilt from source.
    let differing = TempDir::new().unwrap();
    let differing = differing.path();
    assert_success(
        &run(differing, &["tome", "add", tome_path, "--ref", "main"]),
        "add tome (differing toolchain)",
    );
    assert_success(
        &run_env(
            differing,
            &["install", "tk"],
            &[("GRIMOIRE_BUILD_ENV", "beta")],
        ),
        "install tk under differing toolchain",
    );
    assert!(
        build_archive(differing).exists(),
        "differing toolchain should rebuild rather than reuse the alpha prebuilt"
    );
}

#[test]
fn example_tome_runtime_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    // Installing `greeter` must pull in its runtime dependency `hello`.
    let install = run(root, &["install", "greeter"]);
    assert_success(&install, "install greeter");

    let listed = stdout(&run(root, &["list"]));
    assert!(listed.contains("greeter"), "greeter should be listed");
    assert!(
        listed.contains("hello"),
        "runtime dependency hello should be installed; got: {listed}"
    );

    let greeter = run_shim(root, "greeter");
    assert_success(&greeter, "run greeter");
    assert!(
        stdout(&greeter).contains("greetings from grimoire"),
        "greeter output: {}",
        stdout(&greeter)
    );
}

#[test]
fn example_tome_build_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    // `hello` is a build dependency of `forge`: it must be installed before the build,
    // so the install of `forge` succeeds end to end.
    let install = run(root, &["install", "forge"]);
    assert_success(&install, "install forge");

    let forge = run_shim(root, "forge");
    assert_success(&forge, "run forge");
    assert_eq!(stdout(&forge).trim(), "forged by grimoire", "forge output");
}

#[test]
fn build_dependency_bins_are_on_build_path() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'pathtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add path tome");
    let install = run(root, &["install", "usespath"]);
    assert_success(&install, "install package using build dep PATH");

    let output = run_shim(root, "usespath");
    assert_success(&output, "run usespath");
    assert_eq!(stdout(&output).trim(), "from build dependency");
}

#[test]
fn build_dependency_bins_take_precedence_over_host_tools() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'prectome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("managedmake.rn"),
        "export const package = {\n  name: 'managedmake'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'managed make\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'make')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usesmake.rn"),
        "export const package = {\n  name: 'usesmake'\n  version: '0.1.0'\n  deps: { build: { default: ['managedmake'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let made = (make | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($made)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usesmake')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add precedence tome");
    let install = run(root, &["install", "usesmake"]);
    assert_success(&install, "install package using managed make");

    let output = run_shim(root, "usesmake");
    assert_success(&output, "run usesmake");
    assert_eq!(stdout(&output).trim(), "managed make");
}

#[test]
fn doctor_reports_managed_core_ready_after_minimal_core_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = make_fake_core_tome(&triple);
    let tome_path = tome.path();

    let add = run(
        root,
        &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add fake core tome");

    let packages = core_readiness_packages();
    for package in packages {
        let install = run(root, &["install", package]);
        assert_success(&install, &format!("install core package {package}"));
    }

    let doctor = run(root, &["doctor"]);
    assert_success(&doctor, "doctor after core readiness install");
    let out = stdout(&doctor);
    let expected = format!("managed core userland: ready ({n}/{n})", n = packages.len());
    assert!(
        out.contains(&expected),
        "doctor reports managed core readiness: {out}"
    );
}

#[test]
fn example_tome_checksummed_source() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    // `bundle` fetches and verifies a checksummed source before building from it.
    let install = run(root, &["install", "bundle"]);
    assert_success(&install, "install bundle");

    let bundle = run_shim(root, "bundle");
    assert_success(&bundle, "run bundle");
    assert_eq!(
        stdout(&bundle).trim(),
        "grimoire example payload",
        "bundle output reflects the verified source"
    );
}

#[test]
fn source_tar_zst_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.zst", TestArchiveKind::TarZst);
}

#[test]
fn source_tar_gz_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.gz", TestArchiveKind::TarGz);
}

#[test]
fn source_tar_xz_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.xz", TestArchiveKind::TarXz);
}

fn source_archive_is_extracted_into_build_context(
    archive_name: &str,
    archive_kind: TestArchiveKind,
) {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let source_archive = src.join(archive_name);
    write_test_source_archive(&source_archive, archive_kind);
    let source_hash = sha256_file(&source_archive);

    let rune = src.join("extractor.rn");
    fs::write(
        &rune,
        format!(
            "export const package = {{\n  name: 'extractor'\n  version: '0.1.0'\n  sources: {{ main: {{ url: '{archive_name}', sha256: '{source_hash}' }} }}\n  bins: {{default: {{ extractor: 'bin/extractor' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  let message = (open --raw ($ctx.sources.main.dir | path join 'payload' 'message.txt') | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($message)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'extractor')\n}}\n"
        ),
    )
    .unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build from extracted source archive");
    let archive = out.join(format!("extractor-0.1.0-{}.tar.zst", target_triple()));
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install extracted source package");

    let output = run_shim(root, "extractor");
    assert_success(&output, "run extractor");
    assert_eq!(stdout(&output).trim(), "hello from extracted source");
}

#[test]
fn reject_source_archive_nested_under_symlink() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let source_archive = src.join("payload.tar.zst");
    write_nested_symlink_source_archive(&source_archive);
    let source_hash = sha256_file(&source_archive);

    let rune = src.join("extractor.rn");
    fs::write(
        &rune,
        format!(
            "export const package = {{\n  name: 'extractor'\n  version: '0.1.0'\n  sources: {{ main: {{ url: 'payload.tar.zst', sha256: '{source_hash}' }} }}\n  bins: {{default: {{ extractor: 'bin/extractor' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  'echo ok' | save ($ctx.package_dir | path join 'bin' 'extractor')\n}}\n"
        ),
    )
    .unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &build,
        "nested under symlink",
        "reject source archive member nested under symlink",
    );
}

#[test]
fn source_build_supports_configure_make_install_contract() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    let sources = tome.join("sources");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(&sources).unwrap();
    let dist = tome.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'realbuild'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let source_archive = runes.join("realpkg-1.0.0.tar.zst");
    let mut builder = open_archive(&source_archive);
    append_file(
        &mut builder,
        "realpkg-1.0.0/message.txt",
        b"built from source\n",
        0o644,
    );
    append_file(
        &mut builder,
        "realpkg-1.0.0/configure",
        br#"#!/usr/bin/env sh
set -eu
prefix=
source_dir=${SOURCE_DIR:-.}
for arg in "$@"; do
  case "$arg" in
    --prefix=*) prefix=${arg#--prefix=} ;;
  esac
done
if [ -z "$prefix" ]; then
  echo "missing --prefix" >&2
  exit 2
fi
printf '%s\n' "$prefix" > configured-prefix.txt
{
  printf '%s\n' '#!/usr/bin/env sh'
  printf '%s\n' 'set -eu'
  printf '%s\n' "IFS= read -r message < '$source_dir/message.txt'"
  printf '%s\n' 'printf "%s\n" "$message" > built-message.txt'
} > build.sh
{
  printf '%s\n' '#!/usr/bin/env sh'
  printf '%s\n' 'set -eu'
  printf '%s\n' 'destdir=$1'
  printf '%s\n' 'IFS= read -r message < built-message.txt'
  printf '%s\n' 'IFS= read -r configured < configured-prefix.txt'
  printf '%s\n' '{'
  printf '%s\n' "  printf '%s\n' '#!/usr/bin/env sh'"
  printf '%s\n' "  printf \"printf '%%s\\\\n' '%s via %s'\\n\" \"\$message\" \"\$configured\""
  printf '%s\n' '} > "$destdir$configured/realpkg"'
} > install.sh
"#,
        0o755,
    );
    finish_archive(builder);
    let source_hash = sha256_file(&source_archive);

    let minimake_archive_name = format!("minimake-0.1.0-{}.tar.zst", target_triple());
    let minimake_archive = dist.join(&minimake_archive_name);
    let mut builder = open_archive(&minimake_archive);
    let minimake_metadata = format!(
        "{{format: 1, name: \"minimake\", version: \"0.1.0\", target: \"{}\", store_path: \"{}\", bins: {{default: {{make: \"bin/make\"}}}}}}\n",
        target_triple(),
        fake_store_basename("minimake", "0.1.0")
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        minimake_metadata.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        "bin/make",
        b"#!/usr/bin/env sh\nset -eu\ntarget=${1:-all}\ncase \"$target\" in\n  all) sh ./build.sh ;;\n  install) destdir=\"\"; for arg in \"$@\"; do case \"$arg\" in DESTDIR=*) destdir=${arg#DESTDIR=} ;; esac; done; if [ -z \"$destdir\" ]; then echo 'missing DESTDIR' >&2; exit 2; fi; sh ./install.sh \"$destdir\" ;;\n  *) echo \"unsupported target: $target\" >&2; exit 2 ;;\nesac\n",
        0o755,
    );
    finish_archive(builder);
    let minimake_hash = sha256_file(&minimake_archive);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"minimake\", version: \"0.1.0\", target: \"{}\", archive: \"{minimake_archive_name}\", archive_hash: \"{minimake_hash}\", runtime_deps: []}}\n  }}\n}}\n",
            target_triple()
        ),
    )
    .unwrap();

    fs::write(
        runes.join("realpkg.rn"),
        format!(
            "export const package = {{\n  name: 'realpkg'\n  version: '1.0.0'\n  sources: {{ main: {{ url: 'realpkg-1.0.0.tar.zst', sha256: '{source_hash}' }} }}\n  deps: {{ build: {{ default: ['minimake'] }}, runtime: [] }}\n  bins: {{default: {{ realpkg: 'realpkg' }}}}\n}}\n\nexport def build [ctx] {{\n  let source_dir = ($ctx.sources.main.dir | path join 'realpkg-1.0.0')\n  let build_dir = ($ctx.package_dir | path join 'build')\n  let staged_prefix = ($ctx.package_dir | path join ($ctx.prefix | str replace -r '^/' ''))\n  mkdir $build_dir\n  mkdir $staged_prefix\n  let result = (sh -c $\"cd '($build_dir)' && SOURCE_DIR='($source_dir)' '($source_dir)/configure' --prefix='($ctx.prefix)' && make && make install DESTDIR='($ctx.package_dir)'\" | complete)\n  if $result.exit_code != 0 {{\n    error make {{ msg: $result.stderr }}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add real build tome");
    let install = run(root, &["install", "realpkg"]);
    assert_success(&install, "install configure/make style source package");

    let built_archive = root
        .join("cache")
        .join("builds")
        .join(format!("realpkg-1.0.0-{}.tar.zst", target_triple()));
    let package_metadata = archive_member_text(&built_archive, ".grimoire/package.nuon");
    assert!(
        package_metadata.contains("store_path"),
        "built archive metadata should record its final store path: {package_metadata}"
    );
    assert!(
        package_metadata.contains("-realpkg-1.0.0")
            && !package_metadata.contains("packages/realpkg"),
        "store path should be the content-addressed store basename, not a packages/ dir: {package_metadata}"
    );

    let output = run_shim(root, "realpkg");
    assert_success(&output, "run realpkg");
    let line = stdout(&output);
    assert!(
        line.starts_with("built from source via "),
        "realpkg output should reflect configured source build: {line}"
    );
    assert!(
        line.contains("/store/") && line.trim_end().ends_with("-realpkg-1.0.0"),
        "ctx.prefix should point at the final store path, not the temporary staging dir: {line}"
    );
    assert!(
        !line.trim_end().ends_with("/package"),
        "ctx.prefix should not leak the temporary staging package dir: {line}"
    );
}

#[test]
fn source_build_failure_surfaces_diagnostic_and_output_tail() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'brokentome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    // A build whose external command writes a recognizable error to stderr and exits non-zero —
    // as the build's *final* statement. This must abort the build (surfacing the exit code and the
    // output tail), not silently succeed and pack a broken archive. Regression test for a failing
    // trailing external being swallowed because the result was never drained / exit-checked.
    fs::write(
        runes.join("brokenpkg.rn"),
        "export const package = {\n  name: 'brokenpkg'\n  version: '1.0.0'\n  sources: {}\n  deps: { build: {}, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  sh -c \"echo 'configure: error: no acceptable C compiler found in $PATH' >&2; exit 1\"\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add broken build tome");

    let install = run(root, &["install", "brokenpkg"]);
    // The Nushell diagnostic names the exit code instead of the opaque default message...
    assert_failure_contains(
        &install,
        "external command exited with code 1",
        "build failure reports the exit code",
    );
    // ...and the build's own stderr (the real cause) is carried up in the output tail.
    assert_failure_contains(
        &install,
        "no acceptable C compiler found",
        "build failure surfaces the underlying build output",
    );
}

#[test]
fn build_install_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello");

    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    assert!(archive.exists(), "built archive should exist");

    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install built archive");

    let state = fs::read_to_string(root.join("state").join("packages").join("hello.nuon")).unwrap();
    let expected = format!("archive_hash: \"{}\"", sha256_file(&archive));
    assert!(state.contains(&expected), "installed archive hash: {state}");

    let hello = run_shim(root, "hello");
    assert_success(&hello, "run installed hello");
    assert_eq!(
        stdout(&hello).trim(),
        "hello from grimoire",
        "installed shim output"
    );

    let list = run(root, &["list"]);
    assert_success(&list, "list installed packages");
    assert!(
        stdout(&list).contains("hello"),
        "list includes package name"
    );

    let remove = run(root, &["remove", "hello"]);
    assert_success(&remove, "remove installed package");
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("hello")
            .exists(),
        "removed shim should be gone"
    );
    assert!(
        !store_has_package(root, "hello"),
        "removed store dir should be gone"
    );
}

/// True when the install root's content-addressed store holds a directory for `name`
/// (`<hash>-<name>-<version>`). Used by tests that assert install/removal of a package without
/// knowing its build-input hash.
fn store_has_package(root: &std::path::Path, name: &str) -> bool {
    let store = root.join("store");
    let Ok(entries) = fs::read_dir(&store) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|file| file.contains(&format!("-{name}-")))
    })
}

#[test]
fn built_archive_installs_under_a_different_root() {
    let build_root = TempDir::new().unwrap();
    let build_root = build_root.path();
    let install_root = TempDir::new().unwrap();
    let install_root = install_root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        build_root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello in first root");

    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    let metadata = archive_member_text(&archive, ".grimoire/package.nuon");
    assert!(
        metadata.contains("store_path") && metadata.contains("-hello-0.1.0"),
        "built archive should record a portable root-relative store basename: {metadata}"
    );

    let install = run(install_root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install built archive into second root");
    assert_success(&run_shim(install_root, "hello"), "run portable install");
}

#[test]
fn install_rejects_archive_with_wrong_store_path() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let archive = out
        .path()
        .join(format!("wrongpath-1.0.0-{}.tar.zst", target_triple()));

    let mut builder = open_archive(&archive);
    let metadata = format!(
        "{{format: 1, name: \"wrongpath\", version: \"1.0.0\", target: \"{}\", store_path: \"/wrong/store/path\", bins: {{default: {{wrongpath: \"bin/wrongpath\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        metadata.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        "bin/wrongpath",
        b"#!/usr/bin/env sh\nprintf 'wrong path\\n'\n",
        0o755,
    );
    finish_archive(builder);

    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(
        &install,
        "metadata store_path",
        "reject wrong archive store path",
    );
    assert!(
        !store_has_package(root, "wrongpath"),
        "package with wrong store_path should not be promoted"
    );
}

#[test]
fn lockfile_tracks_installs_and_removals() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));

    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install built archive");

    let lock_path = root.join("state").join("grimoire.lock.nuon");
    let lock = fs::read_to_string(&lock_path).expect("lockfile should be written on install");
    let archive_hash = sha256_file(&archive);
    assert!(lock.contains("version: 1"), "lock version: {lock}");
    assert!(
        lock.contains(&archive_hash),
        "lock records archive hash: {lock}"
    );
    assert!(lock.contains("hello"), "lock lists package: {lock}");

    let remove = run(root, &["remove", "hello"]);
    assert_success(&remove, "remove installed package");
    let lock_after = fs::read_to_string(&lock_path).expect("lockfile should persist after remove");
    assert!(
        !lock_after.contains(&archive_hash),
        "removed package should leave the lock: {lock_after}"
    );
}

#[test]
fn doctor_reports_health_and_problems() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    // A clean, empty root is healthy.
    let empty = run(root, &["doctor"]);
    assert_success(&empty, "doctor on empty root");
    let empty_out = stdout(&empty);
    assert!(
        empty_out.contains("installed packages: 0"),
        "doctor counts packages: {empty_out}"
    );
    let packages = core_readiness_packages();
    let missing = packages.join(", ");
    let expected = format!(
        "managed core userland: incomplete (0/{n}, missing {missing})",
        n = packages.len()
    );
    assert!(
        empty_out.contains(&expected),
        "doctor reports missing managed core tools: {empty_out}"
    );
    assert!(
        empty_out.contains("health: ok"),
        "empty health: {empty_out}"
    );

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install built archive");

    let healthy = run(root, &["doctor"]);
    assert_success(&healthy, "doctor after install");
    let healthy_out = stdout(&healthy);
    assert!(
        healthy_out.contains("installed packages: 1"),
        "doctor counts installed package: {healthy_out}"
    );
    assert!(
        healthy_out.contains("health: ok"),
        "installed health: {healthy_out}"
    );

    // Corrupt the install: the package's files vanish but its recorded state remains.
    fs::remove_dir_all(installed_store_dir(root, "hello").expect("hello store dir")).unwrap();
    let degraded = run(root, &["doctor"]);
    assert_success(&degraded, "doctor on degraded install");
    assert!(
        stdout(&degraded).contains("problem(s) found"),
        "doctor reports problem count: {}",
        stdout(&degraded)
    );
    assert!(
        stderr(&degraded).contains("files are missing"),
        "doctor diagnoses missing files on stderr: {}",
        stderr(&degraded)
    );
}

#[test]
fn install_verifies_archive_hash() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    let actual = sha256_file(&archive);

    // A correct expected hash installs cleanly.
    let ok = run(
        root,
        &["install", archive.to_str().unwrap(), "--sha256", &actual],
    );
    assert_success(&ok, "install with matching --sha256");
    let remove = run(root, &["remove", "hello"]);
    assert_success(&remove, "remove after verified install");

    // A wrong expected hash is a hard failure and installs nothing.
    let bad = run(
        root,
        &[
            "install",
            archive.to_str().unwrap(),
            "--sha256",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ],
    );
    assert_failure_contains(&bad, "hash mismatch", "reject mismatched --sha256");
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("hello")
            .exists(),
        "mismatched verify must not create a shim"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("hello.nuon")
            .exists(),
        "mismatched verify must not write package state"
    );
}

#[test]
fn build_fetches_and_verifies_sources() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    // A local source artifact resolved relative to the rune directory; no network needed.
    let payload = src.join("payload.txt");
    fs::write(&payload, b"verified source payload\n").unwrap();
    let payload_hash = sha256_file(&payload);

    let rune = src.join("srctool.rn");
    let rune_src = format!(
        "export const package = {{\n  name: 'srctool'\n  version: '0.1.0'\n  sources: {{ main: {{ url: 'payload.txt', sha256: '{payload_hash}' }} }}\n  bins: {{default: {{ srctool: 'bin/srctool' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'srctool')\n}}\n"
    );
    fs::write(&rune, rune_src).unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build with verified source");
    let hex = payload_hash.strip_prefix("sha256:").unwrap();
    assert!(
        root.join("cache").join("sources").join(hex).exists(),
        "verified source should be cached by hash"
    );

    // A wrong checksum is a hard failure before the build runs.
    let bad_rune = src.join("badsrc.rn");
    let bad_src = "export const package = {\n  name: 'badsrc'\n  version: '0.1.0'\n  sources: { main: { url: 'payload.txt', sha256: 'sha256:0000000000000000000000000000000000000000000000000000000000000000' } }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'badsrc')\n}\n";
    fs::write(&bad_rune, bad_src).unwrap();
    let bad = run(
        root,
        &[
            "build",
            bad_rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_failure_contains(&bad, "hash mismatch", "reject mismatched source checksum");
}

#[test]
fn addendum_patches_source_metadata_before_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'patchtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let old_payload = runes.join("old.txt");
    let new_payload = runes.join("new.txt");
    fs::write(
        &old_payload,
        b"#!/usr/bin/env sh\nprintf 'old payload\\n'\n",
    )
    .unwrap();
    fs::write(
        &new_payload,
        b"#!/usr/bin/env sh\nprintf 'new payload\\n'\n",
    )
    .unwrap();
    let old_hash = sha256_file(&old_payload);
    let new_hash = sha256_file(&new_payload);

    fs::write(
        runes.join("patched.rn"),
        format!(
            "export const package = {{\n  name: 'patched'\n  version: '0.1.0'\n  summary: 'original summary'\n  sources: {{ main: {{ url: 'old.txt', sha256: '{old_hash}' }} }}\n  bins: {{default: {{ patched: 'bin/patched' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'patched')\n}}\n"
        ),
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum = addendum.path();
    fs::write(
        addendum.join("addendum.nuon"),
        format!(
            "{{\n  name: patchset\n  patches: [\n    {{\n      tome: patchtome\n      package: patched\n      version: \"0.2.0\"\n      summary: \"patched summary\"\n      sources: {{ main: {{ url: \"new.txt\", sha256: \"{new_hash}\" }} }}\n    }}\n  ]\n}}\n"
        ),
    )
    .unwrap();

    let add_tome = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add_tome, "add patch tome");
    let add_patch = run(
        root,
        &[
            "addendum",
            "add",
            addendum.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add_patch, "add patch addendum");

    let info = run(root, &["info", "patched"]);
    assert_success(&info, "info patched package");
    let info_out = stdout(&info);
    assert!(
        info_out.contains("version: 0.2.0"),
        "info should show patched version: {info_out}"
    );
    assert!(
        info_out.contains("patched summary"),
        "info should show patched summary: {info_out}"
    );

    let install = run(root, &["install", "patched"]);
    assert_success(&install, "install patched package");
    let output = run_shim(root, "patched");
    assert_success(&output, "run patched package");
    assert_eq!(stdout(&output).trim(), "new payload");

    let state = fs::read_to_string(root.join("state").join("packages").join("patched.nuon"))
        .expect("patched package state");
    assert!(state.contains("version: \"0.2.0\""), "state: {state}");
    assert!(
        state.contains(&new_hash),
        "state records patched source hash: {state}"
    );

    let lock = fs::read_to_string(root.join("state").join("grimoire.lock.nuon"))
        .expect("lockfile after patched install");
    assert!(lock.contains("patchset"), "lock records addendum: {lock}");
}

#[test]
fn direct_source_install_preserves_runtime_deps() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let src = TempDir::new().unwrap();
    let src = src.path();
    let runes = src.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        src.join("tome.rn"),
        "export const tome = {\n  name: 'directdeps'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("dep.rn"),
        "export const package = {\n  name: 'dep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'dep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'dep')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['dep'], build: {} }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", src.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add direct deps tome");

    let install = run(root, &["install", runes.join("app.rn").to_str().unwrap()]);
    assert_success(&install, "install direct source app");
    assert!(
        root.join("state")
            .join("packages")
            .join("dep.nuon")
            .exists(),
        "runtime dep from embedded archive metadata should be installed"
    );
}

#[test]
fn locked_source_install_rejects_rebuilt_hash_drift() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let rune = src.join("locksrc.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v1\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
    )
    .unwrap();

    let install = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&install, "initial source install");
    let lock_path = root.join("state").join("grimoire.lock.nuon");
    let locked = fs::read_to_string(&lock_path).expect("lockfile after source install");

    let remove = run(root, &["remove", "locksrc"]);
    assert_success(&remove, "remove source package");
    fs::write(&lock_path, locked).unwrap();

    fs::write(
        &rune,
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v2\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
    )
    .unwrap();

    let locked_install = run(root, &["install", rune.to_str().unwrap(), "--locked"]);
    assert_failure_contains(
        &locked_install,
        "hash mismatch",
        "locked source install rejects changed same-version source",
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("locksrc.nuon")
            .exists(),
        "failed locked source install should not write package state"
    );
}

#[test]
fn install_resolves_binary_from_index() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The tome ships `binpkg`'s rune *and* a prebuilt whose published store hash matches it, so the
    // prebuilt is a valid substitute. The rune builds a bin printing "from source" while the
    // prebuilt prints "from binary", so the install output proves the prebuilt was substituted.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'bincore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("binpkg.rn"),
        "export const package = {\n  name: 'binpkg'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from source\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'binpkg')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add bincore");

    // Publish a prebuilt whose store hash is exactly the one the installer will recompute.
    let store_hash = store_hash(root, "binpkg");
    let archive_name = format!("binpkg-0.1.0-{triple}.tar.zst");
    let archive = make_prebuilt(
        &tome.join("dist").join(&archive_name),
        "binpkg",
        "0.1.0",
        &triple,
        &store_hash,
        "#!/usr/bin/env sh\nprintf 'from binary\\n'\n",
    );
    let hash = sha256_file(&archive);
    fs::write(
        tome.join("dist").join("index.nuon"),
        solo_index(
            "binpkg",
            "0.1.0",
            &triple,
            &archive_name,
            &hash,
            &store_hash,
            "[]",
        ),
    )
    .unwrap();
    let update = run(root, &["tome", "update", "bincore"]);
    assert_success(&update, "tome update bincore");

    let install = run(root, &["install", "binpkg"]);
    assert_success(&install, "install binpkg from binary index");
    assert!(
        root.join("cache")
            .join("archives")
            .join(hash.strip_prefix("sha256:").unwrap())
            .exists(),
        "verified binary archive should be cached by hash"
    );

    let shim = run_shim(root, "binpkg");
    assert_success(&shim, "run binary-installed binpkg");
    assert_eq!(
        stdout(&shim).trim(),
        "from binary",
        "the matching prebuilt is substituted instead of building the rune"
    );
}

#[test]
fn install_resolves_binary_over_http() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The published index + archive live in a directory served over HTTP; the tome.rn points
    // at that base URL with format "http". Installing must fetch and verify the prebuilt archive
    // over the network. This is a pure binary repo with no rune.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();

    // Stage the published artifacts (archive + index) in a directory the HTTP server hosts.
    let published = TempDir::new().unwrap();
    let published = published.path();
    let archive_name = format!("httppkg-0.1.0-{triple}.tar.zst");
    let archive = make_indexed_archive(
        &published.join(&archive_name),
        "httppkg",
        &triple,
        "#!/usr/bin/env sh\nprintf 'from binary\\n'\n",
    );
    let hash = sha256_file(&archive);
    fs::write(
        published.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"httppkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let base_url = serve_dir(published.to_path_buf());
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: 'httpcore'\n  packages: {{ repo: '{base_url}', format: 'http', index: 'index.nuon' }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add httpcore");
    let update = run(root, &["tome", "update", "httpcore"]);
    assert_success(&update, "tome update httpcore");

    let install = run(root, &["install", "httppkg"]);
    assert_success(&install, "install httppkg from http index");
    assert!(
        root.join("cache")
            .join("archives")
            .join(hash.strip_prefix("sha256:").unwrap())
            .exists(),
        "verified http archive should be cached by hash"
    );

    let shim = run_shim(root, "httppkg");
    assert_success(&shim, "run http-installed httppkg");
    assert_eq!(
        stdout(&shim).trim(),
        "from binary",
        "http binary repo installs the published prebuilt"
    );
}

#[test]
fn install_pulls_in_runtime_dependencies() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The tome ships the runes for `app` (which declares a runtime dep on `lib`) and `lib`, plus a
    // prebuilt for each whose published store hash matches. `app`'s content address folds in `lib`'s
    // (the transitive closure), so the seam computes `app`'s hash only after `lib`'s rune exists.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'depcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['lib'] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("lib.rn"),
        "export const package = {\n  name: 'lib'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'lib\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'lib')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add depcore");

    let lib_store = store_hash(root, "lib");
    let app_store = store_hash(root, "app");
    let app_name = format!("app-0.1.0-{triple}.tar.zst");
    let app = make_prebuilt(
        &tome.join("dist").join(&app_name),
        "app",
        "0.1.0",
        &triple,
        &app_store,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
    );
    let app_hash = sha256_file(&app);
    let lib_name = format!("lib-0.1.0-{triple}.tar.zst");
    let lib = make_prebuilt(
        &tome.join("dist").join(&lib_name),
        "lib",
        "0.1.0",
        &triple,
        &lib_store,
        "#!/usr/bin/env sh\nprintf 'lib\\n'\n",
    );
    let lib_hash = sha256_file(&lib);

    fs::write(
        tome.join("dist").join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"{app_store}\": {{ name: \"app\", version: \"0.1.0\", target: \"{triple}\", archive: \"{app_name}\", archive_hash: \"{app_hash}\", runtime_deps: [\"lib\"]}}\n    \"{lib_store}\": {{ name: \"lib\", version: \"0.1.0\", target: \"{triple}\", archive: \"{lib_name}\", archive_hash: \"{lib_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let update = run(root, &["tome", "update", "depcore"]);
    assert_success(&update, "tome update depcore");

    let install = run(root, &["install", "app"]);
    assert_success(&install, "install app with runtime dependency");

    let list = run(root, &["list"]);
    let listing = stdout(&list);
    assert!(
        listing.contains("app"),
        "app should be installed: {listing}"
    );
    assert!(
        listing.contains("lib"),
        "runtime dependency lib should be installed: {listing}"
    );

    let lib_shim = run_shim(root, "lib");
    assert_success(&lib_shim, "run dependency shim lib");
    assert_eq!(stdout(&lib_shim).trim(), "lib");
}

#[test]
fn install_selects_constrained_dependency_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The index offers two versions of `lib`; `app` constrains it to `<2.0`. The solver must
    // pick `lib` 1.0.0 even though 2.0.0 is newer, proving version-aware resolution end to end.
    // A pure binary repo: the constraint lives in `app`'s index entry.
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
        "export const tome = {\n  name: 'vercore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let dist = tome.join("dist");
    let app_name = format!("app-1.0.0-{triple}.tar.zst");
    let app = make_versioned_archive_with_hash(
        &dist.join(&app_name),
        "app",
        "1.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
        "cafef00dcafef000",
    );
    let app_hash = sha256_file(&app);

    let lib1_name = format!("lib-1.0.0-{triple}.tar.zst");
    let lib1 = make_versioned_archive_with_hash(
        &dist.join(&lib1_name),
        "lib",
        "1.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib 1.0\\n'\n",
        "cafef00dcafef001",
    );
    let lib1_hash = sha256_file(&lib1);

    let lib2_name = format!("lib-2.0.0-{triple}.tar.zst");
    let lib2 = make_versioned_archive_with_hash(
        &dist.join(&lib2_name),
        "lib",
        "2.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib 2.0\\n'\n",
        "cafef00dcafef002",
    );
    let lib2_hash = sha256_file(&lib2);

    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"app\", version: \"1.0.0\", target: \"{triple}\", archive: \"{app_name}\", archive_hash: \"{app_hash}\", runtime_deps: [{{ name: \"lib\", version: \"<2.0\" }}]}}\n    \"cafef00dcafef001\": {{ name: \"lib\", version: \"1.0.0\", target: \"{triple}\", archive: \"{lib1_name}\", archive_hash: \"{lib1_hash}\", runtime_deps: []}}\n    \"cafef00dcafef002\": {{ name: \"lib\", version: \"2.0.0\", target: \"{triple}\", archive: \"{lib2_name}\", archive_hash: \"{lib2_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add vercore");
    let update = run(root, &["tome", "update", "vercore"]);
    assert_success(&update, "tome update vercore");

    let install = run(root, &["install", "app"]);
    assert_success(&install, "install app with constrained lib");

    let lib_shim = run_shim(root, "lib");
    assert_success(&lib_shim, "run constrained lib shim");
    assert_eq!(
        stdout(&lib_shim).trim(),
        "lib 1.0",
        "solver must honor the `<2.0` constraint and pick lib 1.0.0"
    );
}

#[test]
fn source_install_keeps_pulled_build_dependency_after_success() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `usespath` is a source rune that lists `stampdep` as a build dep and shells out to its
    // `stamp` binary during the build. Build dependencies are kept after a successful source
    // install so the managed build userland remains available for later builds.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'cleantome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add cleantome");

    assert_success(&run(root, &["install", "usespath"]), "install usespath");

    // The just-built package still works end to end.
    let output = run_shim(root, "usespath");
    assert_success(&output, "run usespath");
    assert_eq!(stdout(&output).trim(), "from build dependency");

    // stampdep remains installed — state, package dir, and shim — because it is part of the
    // managed build environment now.
    assert!(
        root.join("state")
            .join("packages")
            .join("stampdep.nuon")
            .exists(),
        "stampdep state should remain"
    );
    assert!(
        store_has_package(root, "stampdep"),
        "stampdep package dir should remain"
    );
    assert!(
        root.join("profiles")
            .join("current")
            .join("bin")
            .join("stamp")
            .exists(),
        "stampdep shim should remain"
    );

    // `usespath` itself stays installed; it is the explicit target, not a build dep.
    assert!(
        root.join("state")
            .join("packages")
            .join("usespath.nuon")
            .exists(),
        "usespath should remain installed"
    );

    // The built archive still lives in cache/builds/ for reproducible locked/source rebuilds.
    let builds = root.join("cache").join("builds");
    let cached: Vec<_> = fs::read_dir(&builds)
        .map(|iter| {
            iter.filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("stampdep-"))
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !cached.is_empty(),
        "stampdep's built archive should remain in cache/builds for future reuse"
    );
}

#[test]
fn source_install_keeps_user_installed_build_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Same shape as the previous test, but the user installs `stampdep` explicitly first. Keeping
    // build deps after source installs means this should behave the same either way.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'keeptome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add keeptome");

    assert_success(
        &run(root, &["install", "stampdep"]),
        "install stampdep explicitly",
    );
    assert_success(&run(root, &["install", "usespath"]), "install usespath");

    assert!(
        root.join("state")
            .join("packages")
            .join("stampdep.nuon")
            .exists(),
        "explicit stampdep install must remain after the source build"
    );
}

#[test]
fn remove_autoremoves_orphaned_runtime_dependencies() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // Two top-level packages, `app` and `other`, that both depend on the same `lib`. After
    // removing `app`, `lib` must stay because `other` still needs it; after removing `other`,
    // `lib` becomes truly unreferenced and the cascade autoremove must take it out too.
    // A pure binary repo: `app` and `other` both declare a runtime dep on `lib` in their index
    // entries and embedded archive metadata.
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
        "export const tome = {\n  name: 'rmcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let dist = tome.join("dist");
    let mut entries = Vec::new();
    for (pkg, deps) in [("app", "[\"lib\"]"), ("other", "[\"lib\"]"), ("lib", "[]")] {
        let name = format!("{pkg}-0.1.0-{triple}.tar.zst");
        // Embed deps in the archive's package.nuon, not just the index entry: the install state
        // record reads from the archive, and the autoremove cascade reads from that state.
        let package_nuon = format!(
            "{{format: 1, name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{pkg}: \"bin/{pkg}\"}}}}, deps: {{ runtime: {deps} }}}}\n",
            fake_store_basename_with_hash(pkg, "0.1.0", &format!("cafef00dcafef00d-{pkg}"))
        );
        let archive_path = dist.join(&name);
        if let Some(parent) = archive_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut builder = open_archive(&archive_path);
        append_file(
            &mut builder,
            ".grimoire/package.nuon",
            package_nuon.as_bytes(),
            0o644,
        );
        append_file(
            &mut builder,
            &format!("bin/{pkg}"),
            format!("#!/usr/bin/env sh\nprintf '{pkg}\\n'\n").as_bytes(),
            0o755,
        );
        finish_archive(builder);
        let hash = sha256_file(&archive_path);
        entries.push(format!(
            "    \"cafef00dcafef00d-{pkg}\": {{ name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", archive: \"{name}\", archive_hash: \"{hash}\", runtime_deps: {deps}}}"
        ));
    }
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n{}\n  }}\n}}\n",
            entries.join("\n")
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add rmcore");
    let update = run(root, &["tome", "update", "rmcore"]);
    assert_success(&update, "tome update rmcore");

    assert_success(&run(root, &["install", "app"]), "install app");
    assert_success(&run(root, &["install", "other"]), "install other");

    let lib_state = root.join("state").join("packages").join("lib.nuon");
    assert!(lib_state.exists(), "lib should be installed as a dep");

    // First removal: lib is still needed by `other`, so it must survive.
    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    let remove_app_out = stdout(&remove_app);
    assert!(
        remove_app_out.contains("removed app"),
        "should report app removal: {remove_app_out}"
    );
    assert!(
        !remove_app_out.contains("autoremoved unused dependency lib"),
        "lib must not be autoremoved while other still depends on it: {remove_app_out}"
    );
    assert!(lib_state.exists(), "lib should still be installed");

    // Second removal: nothing else references lib now, so it must be cascaded out.
    let remove_other = run(root, &["remove", "other"]);
    assert_success(&remove_other, "remove other");
    let remove_other_out = stdout(&remove_other);
    assert!(
        remove_other_out.contains("autoremoved unused dependency lib"),
        "lib should be autoremoved when no package depends on it: {remove_other_out}"
    );
    assert!(!lib_state.exists(), "lib state should be gone");
    assert!(
        !store_has_package(root, "lib"),
        "lib package dir should be gone"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("lib")
            .exists(),
        "lib shim should be gone"
    );
}

/// Writes a prebuilt archive for `pkg` with declared runtime deps into `dist` and returns the
/// index entry line describing it. `deps` is NUON list source, e.g. `["lib"]`.
fn dep_archive_entry(
    dist: &Path,
    pkg: &str,
    version: &str,
    triple: &str,
    deps: &str,
    store_hash: &str,
) -> String {
    let name = format!("{pkg}-{version}-{triple}.tar.zst");
    let package_nuon = format!(
        "{{format: 1, name: \"{pkg}\", version: \"{version}\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{pkg}: \"bin/{pkg}\"}}}}, deps: {{ runtime: {deps} }}}}\n",
        fake_store_basename_with_hash(pkg, version, store_hash)
    );
    fs::create_dir_all(dist).unwrap();
    let archive_path = dist.join(&name);
    let mut builder = open_archive(&archive_path);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{pkg}"),
        format!("#!/usr/bin/env sh\nprintf '{pkg}-{version}\\n'\n").as_bytes(),
        0o755,
    );
    finish_archive(builder);
    let hash = sha256_file(&archive_path);
    format!(
        "    \"{store_hash}\": {{ name: \"{pkg}\", version: \"{version}\", target: \"{triple}\", archive: \"{name}\", archive_hash: \"{hash}\", runtime_deps: {deps}}}"
    )
}

/// Scaffolds a binary-only tome (manifest, dummy rune, index built from `entries`) at `tome`.
fn write_dep_tome(tome: &Path, tome_name: &str, entries: &[String]) {
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: '{tome_name}'\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
        ),
    )
    .unwrap();
    write_dep_index(tome, entries);
}

fn write_dep_index(tome: &Path, entries: &[String]) {
    fs::write(
        tome.join("dist").join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n{}\n  }}\n}}\n",
            entries.join("\n")
        ),
    )
    .unwrap();
}

fn state_text(root: &Path, pkg: &str) -> String {
    fs::read_to_string(
        root.join("state")
            .join("packages")
            .join(format!("{pkg}.nuon")),
    )
    .unwrap()
}

#[test]
fn install_marks_roots_requested_and_promotes_explicit_deps() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "reqcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add reqcore",
    );
    assert_success(&run(root, &["tome", "update", "reqcore"]), "tome update");

    assert_success(&run(root, &["install", "app"]), "install app");
    assert!(
        state_text(root, "app").contains("requested: true"),
        "the named root must be marked requested: {}",
        state_text(root, "app")
    );
    assert!(
        state_text(root, "lib").contains("requested: false"),
        "a solver-pulled dep must not be requested: {}",
        state_text(root, "lib")
    );

    // An explicit install of an already-installed dependency promotes it, exempting it from
    // the autoremove cascade when its last dependent goes away.
    assert_success(&run(root, &["install", "lib"]), "explicit install lib");
    assert!(
        state_text(root, "lib").contains("requested: true"),
        "explicit install must promote the dep: {}",
        state_text(root, "lib")
    );
    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    assert!(
        !stdout(&remove_app).contains("autoremoved unused dependency lib"),
        "requested lib must survive removal of its dependent: {}",
        stdout(&remove_app)
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "lib state must remain"
    );
}

#[test]
fn held_dependency_survives_autoremove_cascade() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "heldcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add heldcore",
    );
    assert_success(&run(root, &["tome", "update", "heldcore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");
    assert_success(&run(root, &["hold", "lib"]), "hold lib");

    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    assert!(
        !stdout(&remove_app).contains("autoremoved unused dependency lib"),
        "held lib must not be autoremoved: {}",
        stdout(&remove_app)
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "held lib state must remain"
    );
}

#[test]
fn upgrade_sweeps_dependencies_dropped_by_new_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let v1_entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app1",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "upsweep", &v1_entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add upsweep",
    );
    assert_success(&run(root, &["tome", "update", "upsweep"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app 0.1.0");
    assert!(
        root.join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "lib installed as dep of app 0.1.0"
    );

    // app 0.2.0 no longer depends on lib; the upgrade must sweep the now-stale dep.
    let mut v2_entries = v1_entries.clone();
    v2_entries.push(dep_archive_entry(
        &dist,
        "app",
        "0.2.0",
        &triple,
        "[]",
        "cafef00dcafef00d-app2",
    ));
    write_dep_index(tome, &v2_entries);

    let upgrade = run(root, &["upgrade", "app"]);
    assert_success(&upgrade, "upgrade app");
    assert!(
        stdout(&upgrade).contains("autoremoved unused dependency lib"),
        "upgrade must sweep the dropped dep: {}",
        stdout(&upgrade)
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "lib state must be gone after the upgrade sweep"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("lib")
            .exists(),
        "lib shim must be gone from the new generation"
    );
    assert_eq!(stdout(&run_shim(root, "app")).trim(), "app-0.2.0");
}

#[test]
fn orphans_lists_and_autoremove_reclaims() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "orphcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add orphcore",
    );
    assert_success(&run(root, &["tome", "update", "orphcore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");

    // Nothing is orphaned while the requested root holds the chain.
    let orphans = run(root, &["orphans"]);
    assert_success(&orphans, "orphans");
    assert!(
        stdout(&orphans).contains("no orphaned packages"),
        "no orphans expected: {}",
        stdout(&orphans)
    );

    // Demoting the root orphans the whole chain; `orphans` lists it without removing.
    assert_success(&run(root, &["unrequest", "app"]), "unrequest app");
    let orphans = run(root, &["orphans"]);
    assert_success(&orphans, "orphans after unrequest");
    let listed = stdout(&orphans);
    assert!(
        listed.contains("app\t0.1.0") && listed.contains("lib\t0.1.0"),
        "both packages should be orphaned: {listed}"
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("app.nuon")
            .exists(),
        "orphans must not remove anything"
    );

    let autoremove = run(root, &["autoremove"]);
    assert_success(&autoremove, "autoremove");
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("app.nuon")
            .exists()
            && !root
                .join("state")
                .join("packages")
                .join("lib.nuon")
                .exists(),
        "autoremove must reclaim the orphaned chain"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("app")
            .exists(),
        "reclaimed bins must leave the active generation"
    );
}

/// Like [`dep_archive_entry`], but the archive ships a bin named `bin_name` (instead of the
/// package name) whose shim prints the package name — for contested-capability scenarios.
fn capability_archive_entry(
    dist: &Path,
    pkg: &str,
    bin_name: &str,
    triple: &str,
    store_hash: &str,
) -> String {
    let name = format!("{pkg}-0.1.0-{triple}.tar.zst");
    let package_nuon = format!(
        "{{format: 1, name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{bin_name}: \"bin/{bin_name}\"}}}}, deps: {{ runtime: [] }}}}\n",
        fake_store_basename_with_hash(pkg, "0.1.0", store_hash)
    );
    fs::create_dir_all(dist).unwrap();
    let archive_path = dist.join(&name);
    let mut builder = open_archive(&archive_path);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{bin_name}"),
        format!("#!/usr/bin/env sh\nprintf '{pkg}\\n'\n").as_bytes(),
        0o755,
    );
    finish_archive(builder);
    let hash = sha256_file(&archive_path);
    format!(
        "    \"{store_hash}\": {{ name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", archive: \"{name}\", archive_hash: \"{hash}\", runtime_deps: [], provides: [\"{bin_name}\"]}}"
    )
}

#[test]
fn prefer_resolves_contested_bins_between_installed_packages() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // Both packages declare the bin `tool`; their shims print their own package name so the
    // test can observe which provider owns the contested bin in the active generation.
    let entries = vec![
        capability_archive_entry(&dist, "alpha", "tool", &triple, "cafef00dcafef00d-alf"),
        capability_archive_entry(&dist, "beta", "tool", &triple, "cafef00dcafef00d-bet"),
    ];
    write_dep_tome(tome, "prefcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add prefcore",
    );
    assert_success(&run(root, &["tome", "update", "prefcore"]), "tome update");

    assert_success(&run(root, &["install", "alpha"]), "install alpha");
    // Installing the second claimant must fail with an actionable pointer at `grm prefer`.
    assert_failure_contains(
        &run(root, &["install", "beta"]),
        "grm prefer tool",
        "contested bin without preference",
    );

    // Preferring a package that doesn't provide the capability is rejected with providers.
    assert_failure_contains(
        &run(root, &["prefer", "tool", "nosuchpkg"]),
        "does not provide `tool`",
        "prefer a non-provider",
    );

    assert_success(&run(root, &["prefer", "tool", "beta"]), "prefer beta");
    assert_success(
        &run(root, &["install", "beta"]),
        "install beta after prefer",
    );
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "beta",
        "preferred package must own the contested bin"
    );

    // Switching the preference relinks the active generation without reinstalling.
    assert_success(&run(root, &["prefer", "tool", "alpha"]), "prefer alpha");
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "alpha",
        "switching the preference must flip the bin"
    );

    // The listing shows the recorded choice.
    let listing = run(root, &["prefer"]);
    assert_success(&listing, "prefer listing");
    assert!(
        stdout(&listing).contains("tool\talpha"),
        "listing should show the preference: {}",
        stdout(&listing)
    );

    // Clearing the preference is refused while the bin is still contested; once only one
    // claimant remains it succeeds.
    assert_failure_contains(
        &run(root, &["prefer", "--unset", "tool"]),
        "would leave it contested",
        "unset while contested",
    );
    assert_success(&run(root, &["remove", "beta"]), "remove beta");
    assert_success(
        &run(root, &["prefer", "--unset", "tool"]),
        "unset after remove",
    );
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "alpha",
        "sole remaining claimant owns the bin"
    );
}

#[test]
fn solver_resolves_capability_dependency_through_preference() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // `consumer` depends on the capability `tool`, provided by both alpha and beta. Without
    // a preference the pick is arbitrary; with one it must be the preferred provider.
    let entries = vec![
        capability_archive_entry(&dist, "alpha", "tool", &triple, "cafef00dcafef00d-alf"),
        capability_archive_entry(&dist, "beta", "tool", &triple, "cafef00dcafef00d-bet"),
        dep_archive_entry(
            &dist,
            "consumer",
            "0.1.0",
            &triple,
            "[\"tool\"]",
            "cafef00dcafef00d-con",
        ),
    ];
    write_dep_tome(tome, "capcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add capcore",
    );
    assert_success(&run(root, &["tome", "update", "capcore"]), "tome update");

    assert_success(&run(root, &["prefer", "tool", "beta"]), "prefer beta");
    assert_success(&run(root, &["install", "consumer"]), "install consumer");
    assert!(
        root.join("state")
            .join("packages")
            .join("beta.nuon")
            .exists(),
        "preferred provider must be installed"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("alpha.nuon")
            .exists(),
        "non-preferred provider must not be installed"
    );
}

#[test]
fn post_install_notes_surface_and_replay() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // A binary archive whose embedded metadata carries static notes.
    let pkg = "notepkg";
    let archive_name = format!("{pkg}-0.1.0-{triple}.tar.zst");
    let package_nuon = format!(
        "{{format: 1, name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{pkg}: \"bin/{pkg}\"}}}}, deps: {{ runtime: [] }}, notes: [\"run notepkg --init once\"]}}\n",
        fake_store_basename_with_hash(pkg, "0.1.0", "cafef00dcafef00d-note")
    );
    fs::create_dir_all(&dist).unwrap();
    let archive_path = dist.join(&archive_name);
    let mut builder = open_archive(&archive_path);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{pkg}"),
        format!("#!/usr/bin/env sh\nprintf '{pkg}\\n'\n").as_bytes(),
        0o755,
    );
    finish_archive(builder);
    let hash = sha256_file(&archive_path);
    let entries = vec![format!(
        "    \"cafef00dcafef00d-note\": {{ name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}"
    )];
    write_dep_tome(tome, "notecore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add notecore",
    );
    assert_success(&run(root, &["tome", "update", "notecore"]), "tome update");

    let install = run(root, &["install", pkg]);
    assert_success(&install, "install notepkg");
    let out = stdout(&install);
    assert!(
        out.contains("notes for notepkg:") && out.contains("run notepkg --init once"),
        "install should print the package notes after committing: {out}"
    );

    assert!(
        state_text(root, pkg).contains("run notepkg --init once"),
        "notes must persist in package state: {}",
        state_text(root, pkg)
    );
    let info = run(root, &["info", pkg]);
    assert_success(&info, "info notepkg");
    assert!(
        stdout(&info).contains("notes:") && stdout(&info).contains("run notepkg --init once"),
        "info should replay the notes: {}",
        stdout(&info)
    );
}

#[test]
fn build_returned_notes_merge_with_static_notes() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // The rune declares a static note and its build returns a dynamic one; both must reach
    // the installed state and the post-install report.
    let rune_dir = TempDir::new().unwrap();
    let rune = rune_dir.path().join("dualnote.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'dualnote'\n  version: '0.1.0'\n  notes: ['static note']\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'dualnote\\\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'dualnote')\n  { bins: { default: { dualnote: 'bin/dualnote' } }, notes: ['dynamic note'] }\n}\n",
    )
    .unwrap();

    let install = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&install, "install dualnote from rune");
    let out = stdout(&install);
    assert!(
        out.contains("static note") && out.contains("dynamic note"),
        "both static and build-returned notes should print: {out}"
    );
    let state = state_text(root, "dualnote");
    assert!(
        state.contains("static note") && state.contains("dynamic note"),
        "both notes must persist in state: {state}"
    );
}

#[test]
fn tome_news_surfaces_once_after_updates() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![dep_archive_entry(
        &dist,
        "newspkg",
        "0.1.0",
        &triple,
        "[]",
        "cafef00dcafef00d-nws",
    )];
    write_dep_tome(tome, "newscore", &entries);
    let news_dir = tome.join("news");
    fs::create_dir_all(&news_dir).unwrap();
    fs::write(
        news_dir.join("2026-01-01-alpha.md"),
        "# Alpha note\n\nbody-alpha\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add newscore",
    );
    // First sync: the pre-existing backlog is marked seen silently, not dumped.
    let first = run(root, &["tome", "update", "newscore"]);
    assert_success(&first, "first tome update");
    assert!(
        !stdout(&first).contains("Alpha note"),
        "first sync must not dump the news backlog: {}",
        stdout(&first)
    );

    // A new item published after the add is printed exactly once.
    fs::write(
        news_dir.join("2026-06-10-beta.md"),
        "# Beta note\n\nbody-beta\n",
    )
    .unwrap();
    let second = run(root, &["tome", "update", "newscore"]);
    assert_success(&second, "second tome update");
    assert!(
        stdout(&second).contains("news [newscore] Beta note")
            && stdout(&second).contains("body-beta"),
        "new news item should print on update: {}",
        stdout(&second)
    );
    let third = run(root, &["tome", "update", "newscore"]);
    assert_success(&third, "third tome update");
    assert!(
        !stdout(&third).contains("Beta note"),
        "already-seen news must not repeat: {}",
        stdout(&third)
    );

    // `tome news --all` re-reads everything without disturbing the marker.
    let all = run(root, &["tome", "news", "newscore", "--all"]);
    assert_success(&all, "tome news --all");
    assert!(
        stdout(&all).contains("Alpha note") && stdout(&all).contains("Beta note"),
        "tome news --all should print every item: {}",
        stdout(&all)
    );
    let unread = run(root, &["tome", "news", "newscore"]);
    assert_success(&unread, "tome news");
    assert!(
        stdout(&unread).contains("no unread news"),
        "everything is seen: {}",
        stdout(&unread)
    );

    let state = fs::read_to_string(root.join("state").join("tomes").join("newscore.nuon")).unwrap();
    assert!(
        state.contains("2026-06-10-beta.md"),
        "seen marker must be recorded in tome state: {state}"
    );
}

#[test]
fn files_owns_and_provides_resolve_package_contents() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "filescore", &entries);
    // A source rune whose bin name differs from the package name: a capability (`ex`)
    // resolvable through `grm provides` while the package is still only available.
    fs::write(
        tome.join("runes").join("gex.rn"),
        "export const package = {\n  name: 'gex'\n  version: '0.3.0'\n  bins: {default: {ex: 'bin/ex'}}\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'ex\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'ex')\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add filescore",
    );
    assert_success(&run(root, &["tome", "update", "filescore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");

    // files: relative paths of everything the package staged into the store.
    let files = run(root, &["files", "app"]);
    assert_success(&files, "files app");
    let listed = stdout(&files);
    assert!(
        listed.contains("bin/app") && listed.contains(".grimoire/package.nuon"),
        "files should list the package contents: {listed}"
    );
    assert_failure_contains(
        &run(root, &["files", "nosuchpkg"]),
        "is not installed",
        "files for unknown package",
    );

    // owns: a profile bin path (through the `current` symlink) and a raw store path.
    let profile_bin = root
        .join("profiles")
        .join("current")
        .join("bin")
        .join("app");
    let owns = run(root, &["owns", profile_bin.to_str().unwrap()]);
    assert_success(&owns, "owns profile bin");
    assert!(
        stdout(&owns).contains("app\t0.1.0"),
        "profile bin should resolve to app: {}",
        stdout(&owns)
    );

    let store_file = installed_store_dir(root, "lib").unwrap().join("bin/lib");
    let owns_store = run(root, &["owns", store_file.to_str().unwrap()]);
    assert_success(&owns_store, "owns store path");
    assert!(
        stdout(&owns_store).contains("lib\t0.1.0"),
        "store path should resolve to lib: {}",
        stdout(&owns_store)
    );

    let foreign = root.join("not-owned.txt");
    fs::write(&foreign, "hello").unwrap();
    assert_failure_contains(
        &run(root, &["owns", foreign.to_str().unwrap()]),
        "is not owned by any installed package",
        "owns on a foreign path",
    );

    // provides: installed packages report `installed`, rune-only capabilities `available`.
    let provides_app = run(root, &["provides", "app"]);
    assert_success(&provides_app, "provides app");
    assert!(
        stdout(&provides_app).contains("app\t0.1.0\tinstalled"),
        "installed package should be marked installed: {}",
        stdout(&provides_app)
    );

    let provides_ex = run(root, &["provides", "ex"]);
    assert_success(&provides_ex, "provides ex");
    assert!(
        stdout(&provides_ex).contains("gex\t0.3.0\tavailable"),
        "capability from an uninstalled rune should be available: {}",
        stdout(&provides_ex)
    );

    assert_failure_contains(
        &run(root, &["provides", "nothing-has-this"]),
        "nothing provides",
        "provides with no providers",
    );
}

#[test]
fn completions_and_man_render_from_cli_definition() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Bash completion script for `grm` — the script ships the names of our subcommands so
    // a shell user can tab through them. Smoke check that a few of ours made it in.
    let bash = run(root, &["completions", "bash"]);
    assert_success(&bash, "completions bash");
    let bash_out = stdout(&bash);
    assert!(
        bash_out.contains("_grm") || bash_out.contains("_grm()"),
        "bash completion defines a function: {bash_out}"
    );
    for subcommand in ["install", "remove", "tome", "addendum", "hold"] {
        assert!(
            bash_out.contains(subcommand),
            "completion enumerates `{subcommand}`"
        );
    }

    // `man` writes one `.1` file per subcommand plus a `grm.1` root page.
    let out = TempDir::new().unwrap();
    let out = out.path().join("man");
    let man = run(root, &["man", "--output", out.to_str().unwrap()]);
    assert_success(&man, "man --output");
    let root_page = fs::read_to_string(out.join("grm.1")).expect("read grm.1");
    assert!(
        root_page.contains(".TH grm"),
        "grm.1 is a troff page with the expected title header: {root_page:.200}"
    );
    for sub in ["install", "remove", "clean", "hold"] {
        let path = out.join(format!("grm-{sub}.1"));
        assert!(
            path.exists(),
            "man page for `{sub}` should exist at {path:?}"
        );
    }
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

    // Tamper the cached archive without re-signing: the read path must refuse it before
    // extracting.
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let cached_archive = root
        .join("cache")
        .join("tomes")
        .join("signedcore")
        .join("dist")
        .join(&archive_name);
    let tampered = fs::read(&cached_archive).unwrap();
    let mut tampered = tampered;
    tampered[0] ^= 0xFF; // flip first byte
    fs::write(&cached_archive, tampered).unwrap();

    let blocked = run(root, &["install", "sgnpkg"]);
    assert_failure_contains(&blocked, "signature", "tampered archive is refused");
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

#[test]
fn install_dry_run_prints_plan_without_touching_state() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // Tome with `app` (binary) that depends on `lib` (binary). A dry-run install of `app`
    // must show both steps and *not* leave a state record, shim, or package directory behind.
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
        "export const tome = {\n  name: 'drycore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let dist = tome.join("dist");
    let app_name = format!("app-0.1.0-{triple}.tar.zst");
    let app = make_indexed_archive(
        &dist.join(&app_name),
        "app",
        &triple,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
    );
    let app_hash = sha256_file(&app);
    let lib_name = format!("lib-0.1.0-{triple}.tar.zst");
    let lib = make_indexed_archive(
        &dist.join(&lib_name),
        "lib",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib\\n'\n",
    );
    let lib_hash = sha256_file(&lib);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"app\", version: \"0.1.0\", target: \"{triple}\", archive: \"{app_name}\", archive_hash: \"{app_hash}\", runtime_deps: [\"lib\"]}}\n    \"cafef00dcafef001\": {{ name: \"lib\", version: \"0.1.0\", target: \"{triple}\", archive: \"{lib_name}\", archive_hash: \"{lib_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add drycore",
    );
    assert_success(&run(root, &["tome", "update", "drycore"]), "tome update");

    let dry = run(root, &["install", "app", "--dry-run"]);
    assert_success(&dry, "install --dry-run");
    let out = stdout(&dry);
    assert!(
        out.starts_with("plan:"),
        "dry-run starts with plan header: {out}"
    );
    assert!(
        out.contains("lib 0.1.0"),
        "plan includes runtime dep lib: {out}"
    );
    assert!(out.contains("app 0.1.0"), "plan includes app: {out}");
    assert!(
        out.contains(&app_name) && out.contains(&lib_name),
        "plan names the binary archives: {out}"
    );

    // Nothing was installed.
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("app.nuon")
            .exists(),
        "dry-run must not write state for app"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "dry-run must not write state for lib"
    );
    assert!(
        !store_has_package(root, "app"),
        "dry-run must not write a package dir"
    );

    // `--explain` is an alias for `--dry-run` and produces the same output.
    let explain = run(root, &["install", "app", "--explain"]);
    assert_success(&explain, "install --explain");
    assert_eq!(stdout(&dry), stdout(&explain), "alias matches");
}

#[test]
fn dry_run_runs_while_install_root_is_locked() {
    use fs4::fs_std::FileExt;

    // Dry-run is non-mutating and must not be blocked by a concurrent mutating run holding
    // the install-root lock — otherwise users can't preview an install while another grm is
    // working.
    let root = TempDir::new().unwrap();
    let root = root.path();
    fs::create_dir_all(root).unwrap();

    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".grimoire-lock"))
        .unwrap();
    let acquired = FileExt::try_lock_exclusive(&holder).expect("acquire test-side lock");
    assert!(acquired);

    // `install --dry-run` of a missing package fails on resolution (no tomes), not on the
    // lock — the message tells us the lock was bypassed successfully.
    let dry = run(root, &["install", "nothing", "--dry-run"]);
    assert!(
        !dry.status.success(),
        "dry-run for unknown package should fail"
    );
    let err = stderr(&dry);
    assert!(
        !err.contains("another `grm` process is mutating"),
        "dry-run must not trip the install-root lock: {err}"
    );

    FileExt::unlock(&holder).expect("release test-side lock");
}

#[test]
fn hold_skips_upgrade_until_released() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // A tome that starts with only v0.1.0 of `holdpkg`. After installing, we'll publish v0.2.0
    // and walk through the hold lifecycle: implicit upgrade skips, explicit upgrade errors,
    // unhold makes the upgrade go through.
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
        "export const tome = {\n  name: 'holdcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let dist = tome.join("dist");
    let v1_name = format!("holdpkg-0.1.0-{triple}.tar.zst");
    let v1 = make_versioned_archive_with_hash(
        &dist.join(&v1_name),
        "holdpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v1\\n'\n",
        "cafef00dcafef000",
    );
    let v1_hash = sha256_file(&v1);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"holdpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add holdcore",
    );
    assert_success(&run(root, &["tome", "update", "holdcore"]), "tome update");
    assert_success(&run(root, &["install", "holdpkg"]), "install holdpkg 0.1.0");

    let hold = run(root, &["hold", "holdpkg"]);
    assert_success(&hold, "hold holdpkg");
    assert!(
        stdout(&hold).contains("holdpkg held"),
        "hold reports success: {}",
        stdout(&hold)
    );

    // Hold is reflected in the `list` output as a fourth column.
    let list = run(root, &["list"]);
    assert!(
        stdout(&list).contains("holdpkg") && stdout(&list).contains("held"),
        "list shows held marker: {}",
        stdout(&list)
    );

    // Publish a newer release and refresh the tome so the upgrader sees it.
    let v2_name = format!("holdpkg-0.2.0-{triple}.tar.zst");
    let v2 = make_versioned_archive_with_hash(
        &dist.join(&v2_name),
        "holdpkg",
        "0.2.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v2\\n'\n",
        "cafef00dcafef001",
    );
    let v2_hash = sha256_file(&v2);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"holdpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: []}}\n    \"cafef00dcafef001\": {{ name: \"holdpkg\", version: \"0.2.0\", target: \"{triple}\", archive: \"{v2_name}\", archive_hash: \"{v2_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();
    assert_success(&run(root, &["tome", "update", "holdcore"]), "tome resync");

    // Implicit upgrade skips with a message; the installed version is unchanged.
    let upgrade_all = run(root, &["upgrade"]);
    assert_success(&upgrade_all, "upgrade (all)");
    let upgrade_out = stdout(&upgrade_all);
    assert!(
        upgrade_out.contains("holdpkg is held"),
        "implicit upgrade reports skip: {upgrade_out}"
    );
    assert!(
        stdout(&run(root, &["list"])).contains("holdpkg\t0.1.0"),
        "implicit upgrade must not bump a held package: {}",
        stdout(&run(root, &["list"]))
    );

    // Explicit upgrade is refused — silence here would be an even worse footgun.
    let upgrade_named = run(root, &["upgrade", "holdpkg"]);
    assert_failure_contains(
        &upgrade_named,
        "is held; run `grm unhold holdpkg`",
        "explicit upgrade of held package fails",
    );

    // Release and try again — now the upgrade goes through.
    let unhold = run(root, &["unhold", "holdpkg"]);
    assert_success(&unhold, "unhold holdpkg");
    assert!(
        stdout(&unhold).contains("holdpkg released"),
        "unhold reports release: {}",
        stdout(&unhold)
    );

    assert_success(&run(root, &["upgrade", "holdpkg"]), "upgrade after unhold");
    assert!(
        stdout(&run(root, &["list"])).contains("holdpkg\t0.2.0"),
        "post-release upgrade picks up newest: {}",
        stdout(&run(root, &["list"]))
    );
}

#[test]
fn upgrade_syncs_configured_tomes_before_resolving_versions() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'upcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let v1_name = format!("uppkg-0.1.0-{triple}.tar.zst");
    let v1 = make_versioned_archive_with_hash(
        &dist.join(&v1_name),
        "uppkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v1\\n'\n",
        "cafef00dcafef000",
    );
    let v1_hash = sha256_file(&v1);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"uppkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add upcore",
    );
    assert_success(
        &run(root, &["tome", "update", "upcore"]),
        "initial tome sync",
    );
    assert_success(&run(root, &["install", "uppkg"]), "install uppkg 0.1.0");

    let v2_name = format!("uppkg-0.2.0-{triple}.tar.zst");
    let v2 = make_versioned_archive_with_hash(
        &dist.join(&v2_name),
        "uppkg",
        "0.2.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v2\\n'\n",
        "cafef00dcafef001",
    );
    let v2_hash = sha256_file(&v2);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"uppkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: []}}\n    \"cafef00dcafef001\": {{ name: \"uppkg\", version: \"0.2.0\", target: \"{triple}\", archive: \"{v2_name}\", archive_hash: \"{v2_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let upgrade = run(root, &["upgrade", "uppkg"]);
    assert_success(
        &upgrade,
        "upgrade should sync tome and install newest package",
    );
    assert!(
        stdout(&upgrade).contains("updated tome upcore"),
        "upgrade should report the tome sync: {}",
        stdout(&upgrade)
    );
    assert!(
        stdout(&run(root, &["list"])).contains("uppkg\t0.2.0"),
        "upgrade should see the freshly synced index: {}",
        stdout(&run(root, &["list"]))
    );
    assert_eq!(stdout(&run_shim(root, "uppkg")).trim(), "v2");
}

#[test]
fn mutating_commands_refuse_when_install_root_is_locked() {
    use fs4::fs_std::FileExt;

    let root = TempDir::new().unwrap();
    let root = root.path();
    fs::create_dir_all(root).unwrap();

    // Take the install-root lock from the test harness, simulating a concurrent `grm` that
    // is mid-mutation. The actual command we run is a fast no-op (`clean` against an empty
    // root) but it still has to pass through the lock acquisition, so it must fail fast.
    let lock_path = root.join(".grimoire-lock");
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    let acquired = FileExt::try_lock_exclusive(&holder).expect("acquire test-side lock");
    assert!(acquired, "test should own the lock");

    let blocked = run(root, &["clean"]);
    assert_failure_contains(
        &blocked,
        "another `grm` process is mutating",
        "clean refuses while lock is held",
    );

    let list = run(root, &["list"]);
    assert_success(&list, "read-only `list` is not gated by the lock");

    // Release the lock — the next mutating command should succeed normally.
    FileExt::unlock(&holder).expect("release test-side lock");
    drop(holder);

    let after = run(root, &["clean"]);
    assert_success(&after, "clean succeeds after the lock is released");
}

#[test]
fn clean_empties_caches_and_transactions() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Populate every directory `grm clean` is supposed to wipe with a recognizable marker.
    let dirs = [
        root.join("cache").join("sources"),
        root.join("cache").join("archives"),
        root.join("cache").join("builds"),
        root.join("transactions"),
    ];
    for dir in &dirs {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("marker.bin"), vec![0u8; 4096]).unwrap();
    }
    // Also put a nested directory inside transactions/, which is the realistic shape: an
    // in-flight install stages an entire `package/` subtree under a temp dir.
    let nested = root
        .join("transactions")
        .join("grimoire-abcd")
        .join("package");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("payload.bin"), vec![0u8; 8192]).unwrap();

    // Things `clean` must leave alone, so we can assert it does not touch installed state.
    let state_dir = root.join("state").join("packages");
    fs::create_dir_all(&state_dir).unwrap();
    let state_file = state_dir.join("keep.nuon");
    fs::write(&state_file, b"keep me\n").unwrap();
    let packages_file = root
        .join("store")
        .join(fake_store_basename("keep", "0.1.0"))
        .join("file");
    fs::create_dir_all(packages_file.parent().unwrap()).unwrap();
    fs::write(&packages_file, b"keep me too\n").unwrap();

    let clean = run(root, &["clean"]);
    assert_success(&clean, "clean");
    let clean_out = stdout(&clean);
    assert!(
        clean_out.contains("cleaned") && clean_out.contains("KiB"),
        "clean should report bytes freed: {clean_out}"
    );

    for dir in &dirs {
        assert!(
            dir.exists(),
            "{} should still exist after clean",
            dir.display()
        );
        let leftover: Vec<_> = fs::read_dir(dir).unwrap().collect();
        assert!(
            leftover.is_empty(),
            "{} should be empty after clean, found {} entries",
            dir.display(),
            leftover.len()
        );
    }

    assert!(state_file.exists(), "state files must not be touched");
    assert!(
        packages_file.exists(),
        "installed packages must not be touched"
    );

    // A second clean against an already-empty layout is a no-op, not an error.
    let again = run(root, &["clean"]);
    assert_success(&again, "second clean");
    let again_out = stdout(&again);
    assert!(
        again_out.contains("cleaned 0 entries"),
        "second clean reports nothing freed: {again_out}"
    );
}

#[test]
fn tome_build_all_builds_every_rune() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("multitome");
    let tome_path = tome_dir.to_str().unwrap();

    let init = run(root, &["tome", "init", "multitome", "--path", tome_path]);
    assert_success(&init, "tome init");
    for rune in ["alpha", "beta", "gamma"] {
        let out = run(root, &["tome", "rune", rune, "--path", tome_path]);
        assert_success(&out, "tome rune");
    }

    // `--all` builds every rune in one pass and registers each in the single index.
    let build = run(root, &["tome", "build", "--all", "--path", tome_path]);
    assert_success(&build, "tome build --all");

    let target = target_triple();
    let dist = tome_dir.join("dist");
    let index = fs::read_to_string(dist.join("index.nuon")).unwrap();
    for rune in ["alpha", "beta", "gamma"] {
        let archive_rel = format!("{rune}-0.1.0-{target}.tar.zst");
        assert!(
            dist.join(&archive_rel).exists(),
            "built archive for {rune} should exist"
        );
        assert!(
            index.contains(&archive_rel),
            "index should record {rune}: {index}"
        );
    }

    // A second `--all` build upserts rather than duplicating entries.
    let rebuild = run(root, &["tome", "build", "--all", "--path", tome_path]);
    assert_success(&rebuild, "tome build --all rebuild");
    let index = fs::read_to_string(dist.join("index.nuon")).unwrap();
    let alpha_rel = format!("alpha-0.1.0-{target}.tar.zst");
    assert_eq!(
        index.matches(&alpha_rel).count(),
        1,
        "rebuild should upsert, not duplicate: {index}"
    );

    // Naming a package while passing --all is rejected by the CLI.
    let conflict = run(
        root,
        &["tome", "build", "alpha", "--all", "--path", tome_path],
    );
    assert!(
        !conflict.status.success(),
        "passing both a package and --all should fail"
    );
}

#[test]
fn tome_build_all_skips_non_matching_targets() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("targettome");
    let tome_path = tome_dir.to_str().unwrap();

    let init = run(root, &["tome", "init", "targettome", "--path", tome_path]);
    assert_success(&init, "tome init");

    fs::write(
        tome_dir.join("runes").join("macosonly.rn"),
        "export const package = {\n  name: 'macosonly'\n  version: '0.1.0'\n  targets: ['macos-aarch64-darwin']\n  sources: {}\n  deps: { build: {} runtime: [] }\n \n}\n\nexport def build [ctx] {\n  let bin_dir = ($ctx.package_dir | path join 'bin')\n  mkdir $bin_dir\n  \"#!/usr/bin/env sh\\nprintf 'macosonly\\n'\" | save ($bin_dir | path join 'macosonly')\n}\n",
    )
    .unwrap();

    fs::write(
        tome_dir.join("runes").join("linuxonly.rn"),
        "export const package = {\n  name: 'linuxonly'\n  version: '0.1.0'\n  targets: ['linux-x86_64-musl']\n  sources: {}\n  deps: { build: {} runtime: [] }\n \n}\n\nexport def build [ctx] {\n  let bin_dir = ($ctx.package_dir | path join 'bin')\n  mkdir $bin_dir\n  \"#!/usr/bin/env sh\\nprintf 'linuxonly\\n'\" | save ($bin_dir | path join 'linuxonly')\n}\n",
    )
    .unwrap();

    let build = run(root, &["tome", "build", "--all", "--path", tome_path]);
    assert_success(&build, "tome build --all with target filtering");

    let target = target_triple();
    let dist = tome_dir.join("dist");

    let current_is_macos = target.starts_with("macos-");
    let current_is_linux = target.starts_with("linux-");

    if current_is_macos {
        assert!(
            dist.join(format!("macosonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "macosonly should be built on macos"
        );
        assert!(
            !dist
                .join(format!("linuxonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "linuxonly should be skipped on macos"
        );
    } else if current_is_linux {
        assert!(
            dist.join(format!("linuxonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "linuxonly should be built on linux"
        );
        assert!(
            !dist
                .join(format!("macosonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "macosonly should be skipped on linux"
        );
    } else {
        assert!(
            !dist
                .join(format!("macosonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "macosonly should be skipped on non-macos"
        );
        assert!(
            !dist
                .join(format!("linuxonly-0.1.0-{target}.tar.zst"))
                .exists(),
            "linuxonly should be skipped on non-linux"
        );
    }

    let index = fs::read_to_string(dist.join("index.nuon")).unwrap();
    if current_is_macos {
        assert!(
            index.contains("macosonly"),
            "index should contain macosonly"
        );
        assert!(
            !index.contains("linuxonly"),
            "index should not contain linuxonly"
        );
    } else if current_is_linux {
        assert!(
            index.contains("linuxonly"),
            "index should contain linuxonly"
        );
        assert!(
            !index.contains("macosonly"),
            "index should not contain macosonly"
        );
    } else {
        assert!(
            !index.contains("macosonly"),
            "index should not contain macosonly"
        );
        assert!(
            !index.contains("linuxonly"),
            "index should not contain linuxonly"
        );
    }
}

#[test]
fn install_locked_reproduces_pinned_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The index offers two versions of `lockpkg`; the lockfile pins the older 0.1.0. A
    // `--locked` install must reproduce the pinned 0.1.0 even though 0.2.0 is newer and present.
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
        "export const tome = {\n  name: 'lockcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let dist = tome.join("dist");
    let v1_name = format!("lockpkg-0.1.0-{triple}.tar.zst");
    let v1 = make_versioned_archive_with_hash(
        &dist.join(&v1_name),
        "lockpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v0.1.0\\n'\n",
        "cafef00dcafef000",
    );
    let v1_hash = sha256_file(&v1);

    let v2_name = format!("lockpkg-0.2.0-{triple}.tar.zst");
    let v2 = make_versioned_archive_with_hash(
        &dist.join(&v2_name),
        "lockpkg",
        "0.2.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v0.2.0\\n'\n",
        "cafef00dcafef001",
    );
    let v2_hash = sha256_file(&v2);

    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef000\": {{ name: \"lockpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: []}}\n    \"cafef00dcafef001\": {{ name: \"lockpkg\", version: \"0.2.0\", target: \"{triple}\", archive: \"{v2_name}\", archive_hash: \"{v2_hash}\", runtime_deps: []}}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add lockcore");
    let update = run(root, &["tome", "update", "lockcore"]);
    assert_success(&update, "tome update lockcore");

    // Hand-write a lockfile pinning the older 0.1.0 (with its real archive hash). A locked
    // install must honor this rather than resolving to the newest available release.
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("grimoire.lock.nuon"),
        format!(
            "{{\n  version: 1,\n  packages: [\n    {{ name: \"lockpkg\", version: \"0.1.0\", archive_hash: \"{v1_hash}\", source_hashes: {{}}, runtime_deps: [], build_deps: [] }}\n  ]\n}}\n"
        ),
    )
    .unwrap();

    let locked = run(root, &["install", "lockpkg", "--locked"]);
    assert_success(&locked, "locked install of lockpkg");
    let installed = run(root, &["list"]);
    assert!(
        stdout(&installed).contains("lockpkg\t0.1.0"),
        "locked install must reproduce pinned 0.1.0, not newest: {}",
        stdout(&installed)
    );

    // A package absent from the lockfile cannot be installed under `--locked`.
    let unpinned = run(root, &["install", "lockpkg-missing", "--locked"]);
    assert_failure_contains(
        &unpinned,
        "not recorded in the lockfile",
        "locked install of unpinned package",
    );
}

#[test]
fn reject_bad_archive_path() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_bad_path_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(
        &install,
        "archive contains unsafe paths",
        "reject absolute archive path",
    );
}

#[test]
fn reject_hard_link_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_hard_link_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(&install, "hard link", "reject hard link archive");
}

#[test]
fn reject_symlink_archive_with_escaping_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    // A symlink pointing outside the package (`bin/badlink -> /tmp`) must be refused: internal
    // symlinks are now preserved, but an escaping target could leak host paths into the install.
    let archive = make_symlink_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(
        &install,
        "escapes the package",
        "reject escaping symlink archive",
    );
    assert!(
        !store_has_package(root, "badlink"),
        "rejected package dir should not exist"
    );
}

#[test]
fn reject_archive_member_nested_under_symlink() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    // `lib -> sub` is an in-package (valid) symlink, but a member `lib/evil` underneath it would
    // be written *through* the link during extraction; that whole shape must be refused.
    let archive = make_nested_symlink_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(
        &install,
        "nested under symlink",
        "reject member nested under symlink",
    );
}

#[test]
fn source_build_preserves_internal_symlink() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    // A build that emits a real binary plus an in-package symlink alias (like gawk's `awk`).
    let rune = src.join("aliased.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'aliased'\n  version: '0.1.0'\n  sources: {}\n \n}\n\nexport def build [ctx] {\n  let bin = ($ctx.package_dir | path join 'bin')\n  mkdir $bin\n  \"#!/usr/bin/env sh\\nprintf 'real tool\\n'\\n\" | save ($bin | path join 'real')\n  ^ln -s real ($bin | path join 'alias')\n}\n",
    )
    .unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
            "--bootstrap",
        ],
    );
    assert_success(&build, "build package with internal symlink");

    let archive = out.join(format!("aliased-0.1.0-{}.tar.zst", target_triple()));
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install package with internal symlink");

    // The alias is preserved as a symlink in the installed package, not dereferenced into a copy.
    let installed_alias = installed_store_dir(root, "aliased")
        .expect("aliased store dir")
        .join("bin")
        .join("alias");
    let meta = fs::symlink_metadata(&installed_alias).expect("installed alias exists");
    assert!(
        meta.file_type().is_symlink(),
        "installed `alias` should remain a symlink"
    );

    // And it resolves: invoking the alias shim runs the real tool.
    let output = run_shim(root, "alias");
    assert_success(&output, "run alias shim");
    assert_eq!(stdout(&output).trim(), "real tool");
}

#[test]
fn install_safe_symlink_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_safe_symlink_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install package with safe internal symlink");

    let alias = installed_store_dir(root, "safelink")
        .expect("safelink store dir")
        .join("bin")
        .join("alias");
    let meta = fs::symlink_metadata(&alias).expect("alias exists");
    assert!(
        meta.file_type().is_symlink(),
        "installed alias should remain a symlink"
    );
}

#[test]
fn reject_unsafe_symlink_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_unsafe_symlink_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(
        &install,
        "escapes the package",
        "reject unsafe symlink archive",
    );
}

#[test]
fn reject_bad_rune_metadata() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();

    let bad_name = src.path().join("badname.rn");
    fs::write(
        &bad_name,
        "export const package = {\n  name: '../bad'\n  version: '0.1.0'\n  bins: { default: {} }\n}\n",
    )
    .unwrap();
    let bad_name_result = run(
        root,
        &[
            "build",
            bad_name.to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &bad_name_result,
        "unsupported characters",
        "reject invalid package name",
    );

    // `bins` is optional (a library may declare none), but any bin path that escapes the
    // package dir must still be rejected.
    let bad_bin_path = src.path().join("badbinpath.rn");
    fs::write(
        &bad_bin_path,
        "export const package = {\n  name: 'badbinpath'\n  version: '0.1.0'\n  bins: { default: { tool: '../escape' } }\n}\n",
    )
    .unwrap();
    let bad_bin_path_result = run(
        root,
        &[
            "build",
            bad_bin_path.to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &bad_bin_path_result,
        "must not contain empty or parent components",
        "reject bin path traversal",
    );
}

#[test]
fn reject_bad_archive_metadata() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let triple = target_triple();

    let notarget = make_package_archive(
        out,
        "notarget",
        "{format: 1, name: \"notarget\", version: \"0.1.0\", bins: {default: {notarget: \"bin/notarget\"}}}\n",
    );
    assert_failure_contains(
        &run(root, &["install", notarget.to_str().unwrap()]),
        "missing required field `target`",
        "reject archive missing target",
    );

    let wrong_target = make_package_archive(
        out,
        "wrongtarget",
        "{format: 1, name: \"wrongtarget\", version: \"0.1.0\", target: \"wrong-target\", bins: {default: {wrongtarget: \"bin/wrongtarget\"}}}\n",
    );
    assert_failure_contains(
        &run(root, &["install", wrong_target.to_str().unwrap()]),
        "does not match current target",
        "reject wrong-target archive",
    );

    let bad_bin_path = make_package_archive(
        out,
        "badbinpath",
        &format!(
            "{{format: 1, name: \"badbinpath\", version: \"0.1.0\", target: \"{triple}\", bins: {{default: {{badbinpath: \"../bin/badbinpath\"}}}}}}\n"
        ),
    );
    assert_failure_contains(
        &run(root, &["install", bad_bin_path.to_str().unwrap()]),
        "must not contain empty or parent components",
        "reject bad bin path",
    );

    let bad_version = make_package_archive(
        out,
        "badversiontype",
        &format!(
            "{{format: 1, name: \"badversiontype\", version: 1, target: \"{triple}\", bins: {{default: {{badversiontype: \"bin/badversiontype\"}}}}}}\n"
        ),
    );
    assert_failure_contains(
        &run(root, &["install", bad_version.to_str().unwrap()]),
        "package metadata field `version` must be a string",
        "reject non-string version",
    );

    let bad_bins = make_package_archive(
        out,
        "badbinstype",
        &format!(
            "{{format: 1, name: \"badbinstype\", version: \"0.1.0\", target: \"{triple}\", bins: [\"bin/badbinstype\"]}}\n"
        ),
    );
    assert_failure_contains(
        &run(root, &["install", bad_bins.to_str().unwrap()]),
        "package field `bins` must be a record",
        "reject non-record bins",
    );
}

#[test]
fn platform_conditional_build_deps_only_set_matching_prefix() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'prefixtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("matchdep.rn"),
        "export const package = {\n  name: 'matchdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'matchdep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'matchdep')\n}\n",
    )
    .unwrap();

    let other_os = if std::env::consts::OS == "linux" {
        "macos"
    } else {
        "linux"
    };
    fs::write(
        runes.join("skipdep.rn"),
        "export const package = {\n  name: 'skipdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'skipdep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'skipdep')\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("consumer.rn"),
        format!(
            "export const package = {{\n  name: 'consumer'\n  version: '0.1.0'\n  deps: {{ build: {{ default: ['matchdep', 'skipdep[{}]'] }}, runtime: [] }}\n  bins: {{default: {{ consumer: 'bin/consumer' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  sh -c $\"env > '($ctx.package_dir | path join 'env.txt')'\"\n  \"#!/usr/bin/env sh\\nprintf 'consumer\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'consumer')\n}}\n",
            other_os
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add prefix tome");

    assert_success(&run(root, &["install", "matchdep"]), "install matchdep");

    let build = run(
        root,
        &[
            "build",
            runes.join("consumer.rn").to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build consumer");

    let archive = out.join(format!("consumer-0.1.0-{}.tar.zst", target_triple()));
    let env_text = archive_member_text(&archive, "env.txt");
    assert!(
        env_text.contains("MATCHDEP_PREFIX="),
        "MATCHDEP_PREFIX should be set for matching platform dep: {env_text}"
    );
    assert!(
        !env_text.contains("SKIPDEP_PREFIX="),
        "SKIPDEP_PREFIX should not be set for non-matching platform dep: {env_text}"
    );
}

fn make_fake_tome() -> TempDir {
    let dir = TempDir::new().unwrap();
    let runes = dir.path().join("runes");
    fs::create_dir_all(&runes).unwrap();

    fs::write(
        dir.path().join("tome.rn"),
        "export const tome = {\n  name: 'core'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("hello.rn"),
        "export const package = {\n  name: 'hello'\n  version: '9.9.9'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'hello from configured tome\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'hello')\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("tomehello.rn"),
        "export const package = {\n  name: 'tomehello'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'hello from tome\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tomehello')\n}\n",
    )
    .unwrap();

    dir
}

fn make_fake_core_tome(triple: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let dist = dir.path().join("dist");
    let runes = dir.path().join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(&dist).unwrap();

    fs::write(
        dir.path().join("tome.rn"),
        "export const tome = {\n  name: 'core'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    // validate_tome_cache requires at least one rune, but we want binary installs.
    // A dummy rune satisfies the validator without causing a source-build fallback.
    fs::write(
        runes.join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();

    let mut entries = String::from("{\n  format: 2,\n    entries: {\n");
    for package in core_readiness_packages() {
        let store_hash = format!("cafef00dcafef00d-{package}");
        let archive_name = format!("{package}-0.1.0-{triple}.tar.zst");
        let archive = make_versioned_archive_with_hash(
            &dist.join(&archive_name),
            package,
            "0.1.0",
            triple,
            &format!("#!/usr/bin/env sh\nprintf '{package}\\n'\n"),
            &store_hash,
        );
        let archive_hash = sha256_file(&archive);
        entries.push_str(&format!(
            "    \"cafef00dcafef00d-{package}\": {{ name: \"{package}\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{archive_hash}\", runtime_deps: []}}\n"
        ));
    }
    entries.push_str("  }\n}\n");
    fs::write(dist.join("index.nuon"), entries).unwrap();

    dir
}

/// Serves the files in `dir` over a minimal HTTP/1.1 server on an ephemeral local port and
/// returns the base URL. A request for `/name` returns that file (200) or 404 if absent. The
/// listener thread is detached and lives for the rest of the test process.
fn serve_dir(dir: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
    let port = listener.local_addr().expect("local addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain the remaining request headers so the client's write completes.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let name = path.trim_start_matches('/');
            let response = match fs::read(dir.join(name)) {
                Ok(body) => {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(&body);
                    head
                }
                Err(_) => {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                }
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn open_archive(path: &Path) -> tar::Builder<ZstdFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = zstd::stream::write::Encoder::new(file, 0).expect("zstd encoder");
    tar::Builder::new(encoder)
}

fn open_gzip_archive(path: &Path) -> tar::Builder<GzipFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = GzEncoder::new(file, Compression::default());
    tar::Builder::new(encoder)
}

fn open_xz_archive(path: &Path) -> tar::Builder<XzFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = XzEncoder::new(file, 6);
    tar::Builder::new(encoder)
}

fn finish_archive(builder: tar::Builder<ZstdFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish zstd");
}

fn finish_gzip_archive(builder: tar::Builder<GzipFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip");
}

fn finish_xz_archive(builder: tar::Builder<XzFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish xz");
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)] // all fixtures are tarballs; the prefix is meaningful
enum TestArchiveKind {
    TarGz,
    TarXz,
    TarZst,
}

fn write_test_source_archive(path: &Path, kind: TestArchiveKind) {
    match kind {
        TestArchiveKind::TarGz => {
            let mut builder = open_gzip_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_gzip_archive(builder);
        }
        TestArchiveKind::TarXz => {
            let mut builder = open_xz_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_xz_archive(builder);
        }
        TestArchiveKind::TarZst => {
            let mut builder = open_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_archive(builder);
        }
    }
}

fn write_nested_symlink_source_archive(path: &Path) {
    let mut builder = open_archive(path);
    // `payload -> sub` has an in-package target (so the target check passes), but the member
    // `payload/evil.txt` underneath it would extract through the link.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "payload", "sub")
        .expect("append symlink");
    append_file(&mut builder, "payload/evil.txt", b"x\n", 0o644);
    finish_archive(builder);
}

fn append_file<W: Write>(builder: &mut tar::Builder<W>, path: &str, data: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(&mut header, path, data)
        .expect("append file");
}

/// A deterministic, syntactically valid store basename (`<hash>-<name>-<version>`) for a
/// hand-built archive fixture. The hash bytes are arbitrary: a local-archive install derives its
/// store location from this embedded basename, and no store-hash cross-check runs unless an index
/// entry carries one (the fixtures here do not).
fn fake_store_basename(name: &str, version: &str) -> String {
    format!("cafef00dcafef00d-{name}-{version}")
}

fn fake_store_basename_with_hash(name: &str, version: &str, hash: &str) -> String {
    format!("{hash}-{name}-{version}")
}

/// The absolute store directory a package was installed into, read from its recorded state. Tests
/// use this instead of guessing the content-addressed path; returns `None` when not installed.
fn installed_store_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let state = root
        .join("state")
        .join("packages")
        .join(format!("{name}.nuon"));
    let text = fs::read_to_string(state).ok()?;
    let marker = "store_path: \"";
    let start = text.find(marker)? + marker.len();
    let end = text[start..].find('"')? + start;
    Some(PathBuf::from(&text[start..end]))
}

fn make_package_archive(out: &Path, name: &str, package_nuon: &str) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("{name}-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        b"#!/usr/bin/env sh\nexit 0\n",
        0o755,
    );
    finish_archive(builder);
    archive
}

/// The store hash `grm` will assign to `package`, via the hidden `store-hash` seam. Lets a fixture
/// register a prebuilt whose published `store_hash` matches what the installer will recompute, so
/// the prebuilt is accepted as a substitute. `package` may be a known name (the tome must be added)
/// or a path to a `.rn`.
fn store_hash(root: &Path, package: &str) -> String {
    let out = run(root, &["store-hash", package]);
    assert_success(&out, &format!("store-hash {package}"));
    stdout(&out).trim().to_string()
}

/// A prebuilt archive whose embedded store basename is `<store_hash>-<name>-<version>`, so it
/// installs as a valid substitute for a rune of the same content address.
fn make_prebuilt(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    store_hash: &str,
    bin_script: &str,
) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", store_path: \"{store_hash}-{name}-{version}\", bins: {{default: {{{name}: \"bin/{name}\"}}}}}}\n"
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        bin_script.as_bytes(),
        0o755,
    );
    finish_archive(builder);
    path.to_path_buf()
}

/// A single-package `index.nuon` body for a prebuilt, with the matching `store_hash`.
fn solo_index(
    name: &str,
    version: &str,
    triple: &str,
    archive: &str,
    archive_hash: &str,
    store_hash: &str,
    runtime_deps: &str,
) -> String {
    format!(
        "{{\n  format: 2,\n    entries: {{\n    \"{store_hash}\": {{ name: \"{name}\", version: \"{version}\", target: \"{triple}\", archive: \"{archive}\", archive_hash: \"{archive_hash}\", runtime_deps: {runtime_deps}}}\n  }}\n}}\n"
    )
}

/// Builds a complete `.tar.zst` package archive at `path` whose single bin is `bin_script`.
/// Used to stage a pre-built binary in a fake package repository.
fn make_indexed_archive(path: &Path, name: &str, triple: &str, bin_script: &str) -> PathBuf {
    make_versioned_archive(path, name, "0.1.0", triple, bin_script)
}

/// Like [`make_indexed_archive`] but with an explicit `version`, so a test can stage several
/// versions of the same package in one index.
fn make_versioned_archive(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    bin_script: &str,
) -> PathBuf {
    make_versioned_archive_with_hash(path, name, version, triple, bin_script, "cafef00dcafef00d")
}

fn make_versioned_archive_with_hash(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    bin_script: &str,
    store_hash: &str,
) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{name}: \"bin/{name}\"}}}}}}\n",
        fake_store_basename_with_hash(name, version, store_hash)
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        bin_script.as_bytes(),
        0o755,
    );
    finish_archive(builder);
    path.to_path_buf()
}

fn gen_keypair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().expect("generate keypair")
}

/// Writes a detached minisign signature for `data` at `path`, in the `.minisig` text form
/// `grm` verifies.
fn sign_to(path: &Path, data: &[u8], keypair: &minisign::KeyPair) {
    let signature = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        std::io::Cursor::new(data),
        Some("grimoire test"),
        Some("grimoire test"),
    )
    .expect("sign")
    .into_string();
    fs::write(path, signature).expect("write signature");
}

/// Builds a local tome that publishes a single prebuilt `sgnpkg` archive with a signed
/// `index.nuon`, declaring `keypair`'s public key as `packages.signer`. Re-running it with a
/// different keypair rewrites the manifest's signer and re-signs the (byte-identical) index,
/// which is exactly the key-rotation scenario.
fn build_signed_tome(tome: &Path, name: &str, triple: &str, keypair: &minisign::KeyPair) {
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();

    // Generate runes-manifest.nuon listing all .rn files with their sha256 hashes.
    let mut runes_map = std::collections::BTreeMap::new();
    for entry in fs::read_dir(tome.join("runes")).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rn") {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap()
                .to_owned();
            let hash = sha256_file(&path);
            runes_map.insert(file_name, hash);
        }
    }
    let mut manifest_entries: Vec<String> = Vec::new();
    for (file_name, hash) in &runes_map {
        manifest_entries.push(format!("\"{file_name}\": \"{hash}\""));
    }
    let runes_manifest_body = format!(
        "{{ format: 1, runes: {{ {} }} }}\n",
        manifest_entries.join(", ")
    );
    fs::write(tome.join("runes-manifest.nuon"), &runes_manifest_body).unwrap();
    sign_to(
        &tome.join("runes-manifest.nuon.minisig"),
        runes_manifest_body.as_bytes(),
        keypair,
    );

    let pubkey = keypair.pk.to_base64();
    let manifest_body = format!(
        "export const tome = {{\n  name: '{name}'\n  signers: ['{pubkey}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
    );
    fs::write(tome.join("tome.rn"), &manifest_body).unwrap();
    sign_to(
        &tome.join("tome.rn.minisig"),
        manifest_body.as_bytes(),
        keypair,
    );

    let dist = tome.join("dist");
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let archive = make_versioned_archive(
        &dist.join(&archive_name),
        "sgnpkg",
        "0.1.0",
        triple,
        "#!/usr/bin/env sh\nprintf 'signed\\n'\n",
    );
    let hash = sha256_file(&archive);
    let index_body = format!(
        "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"sgnpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
    );
    fs::write(dist.join("index.nuon"), &index_body).unwrap();
    sign_to(
        &dist.join(format!("{archive_name}.minisig")),
        &fs::read(&archive).unwrap(),
        keypair,
    );
}

fn make_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("badlink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"badlink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{badlink: \"bin/badlink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/badlink", "/tmp")
        .expect("append symlink");
    finish_archive(builder);
    archive
}

fn make_nested_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("nested-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"nested\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    // `lib -> sub` has an in-package target (so the target check passes), but the member
    // `lib/evil` underneath it would extract through the link.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "lib", "sub")
        .expect("append symlink");
    append_file(&mut builder, "lib/evil", b"x\n", 0o644);
    finish_archive(builder);
    archive
}

fn make_bad_path_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("badpath-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);

    let data = b"unsafe\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    // Write a raw absolute name to bypass tar's relative-path normalization, producing the
    // unsafe member that install must reject.
    let gnu = header.as_gnu_mut().expect("gnu header");
    let name = b"/grimoire-absolute-bad";
    gnu.name[..name.len()].copy_from_slice(name);
    header.set_cksum();
    builder
        .append(&header, &data[..])
        .expect("append raw entry");

    finish_archive(builder);
    archive
}

fn make_hard_link_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("hardlink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"hardlink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{hardlink: \"bin/hardlink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(0o644);
    builder
        .append_link(&mut header, "bin/hardlink", "bin/real")
        .expect("append hard link");
    finish_archive(builder);
    archive
}

fn make_safe_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("safelink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"safelink\", version: \"0.1.0\", target: \"{}\", store_path: \"{}\", bins: {{default: {{safelink: \"bin/real\", alias: \"bin/alias\"}}}}}}\n",
        target_triple(),
        fake_store_basename("safelink", "0.1.0")
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );
    append_file(
        &mut builder,
        "bin/real",
        b"#!/usr/bin/env sh\nprintf 'real tool\n'",
        0o755,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/alias", "real")
        .expect("append safe symlink");
    finish_archive(builder);
    archive
}

fn make_unsafe_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("unsafelink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"unsafelink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{unsafelink: \"bin/unsafelink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/unsafelink", "../../etc/passwd")
        .expect("append unsafe symlink");
    finish_archive(builder);
    archive
}
