//! End-to-end smoke tests that drive the built `grm` binary.
//!
//! Each test runs against its own `GRIMOIRE_ROOT` temp directory so they can run in
//! parallel without sharing install state. The current working directory is the crate
//! root, so relative paths like `example/runes/hello.rn` resolve as they would for a
//! user invoking grimoire from the project.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_grm");

type ZstdFileEncoder = zstd::stream::write::Encoder<'static, fs::File>;

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("GRIMOIRE_ROOT", root)
        .output()
        .expect("spawn grimoire")
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
        "windows" | "linux" => "gnu",
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

fn run_shim(root: &Path, name: &str) -> Output {
    Command::new(root.join("bin").join(name))
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
fn command_parsing() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./example/runes/hello.rn",
            &format!("--output={}", out.display()),
            "--quiet",
        ],
    );
    assert_success(&build, "build supports --output=value");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    assert!(archive.exists(), "--output=value archive should exist");

    // `--ref=value` form. A local tome lets `add` read its manifest name offline.
    let add = run(root, &["tome", "add", "./example", "--ref=stable"]);
    assert_success(&add, "tome add supports --ref=value");
    let state = fs::read_to_string(root.join("state").join("tomes").join("example.nuon"))
        .expect("example state");
    assert!(state.contains("ref: stable"), "--ref=value state: {state}");

    let remove = run(root, &["tome", "remove", "example"]);
    assert_success(&remove, "remove example tome");

    let extra = run(root, &["install", "hello", "extra"]);
    assert_failure_contains(
        &extra,
        "unexpected argument 'extra' found",
        "reject extra install argument",
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
fn install_from_configured_tome() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = make_fake_tome();
    let tome_path = tome.path().to_str().unwrap();

    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add local core");

    let update = run(root, &["tome", "update", "core"]);
    assert_success(&update, "tome update local core");

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

    let add = run(root, &["tome", "add", "./example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    let update = run(root, &["tome", "update", "example"]);
    assert_success(&update, "tome update example");

    let install = run(root, &["install", "hello"]);
    assert_success(&install, "install hello from example");
    assert!(
        root.join("cache")
            .join("tomes")
            .join("example")
            .join("runes")
            .join("hello.rn")
            .exists(),
        "cached example rune should exist"
    );

    let hello = run_shim(root, "hello");
    assert_success(&hello, "run example hello");
    assert_eq!(
        stdout(&hello).trim(),
        "hello from grimoire",
        "example hello output"
    );

    let remove_hello = run(root, &["remove", "hello"]);
    assert_success(&remove_hello, "remove example hello");

    let remove_tome = run(root, &["tome", "remove", "example"]);
    assert_success(&remove_tome, "remove example");
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
    assert!(
        tome_dir.join("index.nuon").exists(),
        "index.nuon scaffolded"
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
        .join("packages")
        .join(format!("widget-0.1.0-{target}.tar.zst"));
    assert!(archive.exists(), "built archive should exist: {archive:?}");

    let archive_rel = format!("packages/widget-0.1.0-{target}.tar.zst");
    let index = fs::read_to_string(tome_dir.join("index.nuon")).unwrap();
    assert!(index.contains("widget"), "index lists widget: {index}");
    assert!(
        index.contains(&archive_rel),
        "index records archive path: {index}"
    );

    // The published prebuilt archive is installable without --from-source.
    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add authored");
    let install = run(root, &["install", "widget"]);
    assert_success(&install, "install prebuilt widget");
    let widget = run_shim(root, "widget");
    assert_eq!(
        stdout(&widget).trim(),
        "widget is not implemented yet",
        "prebuilt widget stub output"
    );

    // A rebuild replaces the entry in place rather than duplicating it.
    let rebuild = run(root, &["tome", "build", "widget", "--path", tome_path]);
    assert_success(&rebuild, "tome build rebuild");
    let index = fs::read_to_string(tome_dir.join("index.nuon")).unwrap();
    assert_eq!(
        index.matches(&archive_rel).count(),
        1,
        "rebuild should upsert, not duplicate: {index}"
    );
}

#[test]
fn example_tome_runtime_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./example", "--ref", "main"]);
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

    let add = run(root, &["tome", "add", "./example", "--ref", "main"]);
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
fn example_tome_checksummed_source() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./example", "--ref", "main"]);
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
fn build_install_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./example/runes/hello.rn",
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
        !root.join("bin").join("hello").exists(),
        "removed shim should be gone"
    );
    assert!(
        !root.join("packages").join("hello").join("0.1.0").exists(),
        "removed package dir should be gone"
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
            "./example/runes/hello.rn",
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
    assert!(
        empty_out.contains("health: ok"),
        "empty health: {empty_out}"
    );

    let build = run(
        root,
        &[
            "build",
            "./example/runes/hello.rn",
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
    fs::remove_dir_all(root.join("packages").join("hello").join("0.1.0")).unwrap();
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
            "./example/runes/hello.rn",
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
        !root.join("bin").join("hello").exists(),
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
        "export const package = {{\n  name: 'srctool'\n  version: '0.1.0'\n  sources: {{ main: {{ url: 'payload.txt', sha256: '{payload_hash}' }} }}\n  bins: {{ srctool: 'bin/srctool' }}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'srctool')\n}}\n"
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
    let bad_src = "export const package = {\n  name: 'badsrc'\n  version: '0.1.0'\n  sources: { main: { url: 'payload.txt', sha256: 'sha256:0000000000000000000000000000000000000000000000000000000000000000' } }\n  bins: { badsrc: 'bin/badsrc' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'badsrc')\n}\n";
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
fn install_resolves_binary_from_index() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // A tome whose package repo is itself (`.`): it ships a rune *and* a pre-built archive.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'bincore'\n  packages: { repo: '.', format: 'git', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    // The source rune for the same package prints a different marker, so a successful
    // install proves the binary archive — not a source build — was used.
    fs::write(
        runes.join("binpkg.rn"),
        "export const package = {\n  name: 'binpkg'\n  version: '0.1.0'\n  bins: { binpkg: 'bin/binpkg' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from source\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'binpkg')\n}\n",
    )
    .unwrap();

    let archive_name = format!("binpkg-0.1.0-{triple}.tar.zst");
    let archive = make_indexed_archive(
        &tome.join(&archive_name),
        "binpkg",
        &triple,
        "#!/usr/bin/env sh\nprintf 'from binary\\n'\n",
    );
    let hash = sha256_file(&archive);
    fs::write(
        tome.join("index.nuon"),
        format!(
            "{{\n  packages: [\n    {{ name: \"binpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: [] }}\n  ]\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add bincore");
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
        "binary archive must be preferred over source build"
    );
}

#[test]
fn install_pulls_in_runtime_dependencies() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // A tome whose package repo is itself (`.`): `app` declares a runtime dependency on `lib`,
    // and both ship as pre-built archives. Installing `app` must install `lib` too.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'depcore'\n  packages: { repo: '.', format: 'git', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // The binary archives are preferred, but a tome must define at least one rune.
    for pkg in ["app", "lib"] {
        fs::write(
            runes.join(format!("{pkg}.rn")),
            format!("export const package = {{\n  name: '{pkg}'\n  version: '0.1.0'\n  bins: {{}}\n}}\n\nexport def build [ctx] {{ }}\n"),
        )
        .unwrap();
    }

    let app_name = format!("app-0.1.0-{triple}.tar.zst");
    let app = make_indexed_archive(
        &tome.join(&app_name),
        "app",
        &triple,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
    );
    let app_hash = sha256_file(&app);

    let lib_name = format!("lib-0.1.0-{triple}.tar.zst");
    let lib = make_indexed_archive(
        &tome.join(&lib_name),
        "lib",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib\\n'\n",
    );
    let lib_hash = sha256_file(&lib);

    fs::write(
        tome.join("index.nuon"),
        format!(
            "{{\n  packages: [\n    {{ name: \"app\", version: \"0.1.0\", target: \"{triple}\", archive: \"{app_name}\", archive_hash: \"{app_hash}\", runtime_deps: [\"lib\"] }}\n    {{ name: \"lib\", version: \"0.1.0\", target: \"{triple}\", archive: \"{lib_name}\", archive_hash: \"{lib_hash}\", runtime_deps: [] }}\n  ]\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add depcore");
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
fn reject_symlink_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_symlink_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_failure_contains(&install, "contains a symlink", "reject symlink archive");
    assert!(
        !root.join("packages").join("badlink").join("0.1.0").exists(),
        "rejected package dir should not exist"
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
        "export const package = {\n  name: '../bad'\n  version: '0.1.0'\n  bins: {}\n}\n",
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
        "export const package = {\n  name: 'badbinpath'\n  version: '0.1.0'\n  bins: { tool: '../escape' }\n}\n",
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
        "{format: 1, name: \"notarget\", version: \"0.1.0\", bins: {notarget: \"bin/notarget\"}}\n",
    );
    assert_failure_contains(
        &run(root, &["install", notarget.to_str().unwrap()]),
        "missing required field `target`",
        "reject archive missing target",
    );

    let wrong_target = make_package_archive(
        out,
        "wrongtarget",
        "{format: 1, name: \"wrongtarget\", version: \"0.1.0\", target: \"wrong-target\", bins: {wrongtarget: \"bin/wrongtarget\"}}\n",
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
            "{{format: 1, name: \"badbinpath\", version: \"0.1.0\", target: \"{triple}\", bins: {{badbinpath: \"../bin/badbinpath\"}}}}\n"
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
            "{{format: 1, name: \"badversiontype\", version: 1, target: \"{triple}\", bins: {{badversiontype: \"bin/badversiontype\"}}}}\n"
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

fn make_fake_tome() -> TempDir {
    let dir = TempDir::new().unwrap();
    let runes = dir.path().join("runes");
    fs::create_dir_all(&runes).unwrap();

    fs::write(
        dir.path().join("tome.rn"),
        "export const tome = {\n  name: 'core'\n  packages: { repo: '.', format: 'git', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("hello.rn"),
        "export const package = {\n  name: 'hello'\n  version: '9.9.9'\n  bins: { hello: 'bin/hello' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'hello from configured tome\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'hello')\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("tomehello.rn"),
        "export const package = {\n  name: 'tomehello'\n  version: '0.1.0'\n  bins: { tomehello: 'bin/tomehello' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'hello from tome\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tomehello')\n}\n",
    )
    .unwrap();

    dir
}

fn open_archive(path: &Path) -> tar::Builder<ZstdFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = zstd::stream::write::Encoder::new(file, 0).expect("zstd encoder");
    tar::Builder::new(encoder)
}

fn finish_archive(builder: tar::Builder<ZstdFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish zstd");
}

fn append_file(builder: &mut tar::Builder<ZstdFileEncoder>, path: &str, data: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(&mut header, path, data)
        .expect("append file");
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

/// Builds a complete `.tar.zst` package archive at `path` whose single bin is `bin_script`.
/// Used to stage a pre-built binary in a fake package repository.
fn make_indexed_archive(path: &Path, name: &str, triple: &str, bin_script: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"0.1.0\", target: \"{triple}\", bins: {{{name}: \"bin/{name}\"}}}}\n"
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

fn make_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("badlink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"badlink\", version: \"0.1.0\", target: \"{}\", bins: {{badlink: \"bin/badlink\"}}}}\n",
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
