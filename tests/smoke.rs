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

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
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
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n  bins: { stamp: 'bin/stamp' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n  bins: { usespath: 'bin/usespath' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
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
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let source_archive = src.join("payload.tar.zst");
    let mut builder = open_archive(&source_archive);
    append_file(
        &mut builder,
        "payload/message.txt",
        b"hello from extracted source\n",
        0o644,
    );
    finish_archive(builder);
    let source_hash = sha256_file(&source_archive);

    let rune = src.join("extractor.rn");
    fs::write(
        &rune,
        format!(
            "export const package = {{\n  name: 'extractor'\n  version: '0.1.0'\n  sources: {{ main: {{ url: 'payload.tar.zst', sha256: '{source_hash}' }} }}\n  bins: {{ extractor: 'bin/extractor' }}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  let message = (open --raw ($ctx.sources.main.dir | path join 'payload' 'message.txt') | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($message)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'extractor')\n}}\n"
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
script_dir=$(dirname "$0")
source_dir=$(cd "$script_dir" && pwd)
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
cat > build.sh <<BUILD
#!/usr/bin/env sh
set -eu
cp '$source_dir/message.txt' built-message.txt
BUILD
cat > install.sh <<'INSTALL'
#!/usr/bin/env sh
set -eu
prefix=$1
mkdir -p "$prefix/bin"
message=$(cat built-message.txt)
configured=$(cat configured-prefix.txt)
{
  printf '%s\n' '#!/usr/bin/env sh'
  printf "printf '%%s\\n' '%s via %s'\n" "$message" "$configured"
} > "$prefix/bin/realpkg"
chmod +x "$prefix/bin/realpkg"
INSTALL
chmod +x build.sh install.sh
"#,
        0o755,
    );
    finish_archive(builder);
    let source_hash = sha256_file(&source_archive);

    let minimake_archive_name = format!("minimake-0.1.0-{}.tar.zst", target_triple());
    let minimake_archive = dist.join(&minimake_archive_name);
    let mut builder = open_archive(&minimake_archive);
    let minimake_metadata = format!(
        "{{format: 1, name: \"minimake\", version: \"0.1.0\", target: \"{}\", bins: {{make: \"bin/make\"}}}}\n",
        target_triple()
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
        b"#!/usr/bin/env sh\nset -eu\ntarget=${1:-all}\ncase \"$target\" in\n  all) sh ./build.sh ;;\n  install) prefix=\"\"; for arg in \"$@\"; do case \"$arg\" in PREFIX=*) prefix=${arg#PREFIX=} ;; esac; done; if [ -z \"$prefix\" ]; then echo 'missing PREFIX' >&2; exit 2; fi; sh ./install.sh \"$prefix\" ;;\n  *) echo \"unsupported target: $target\" >&2; exit 2 ;;\nesac\n",
        0o755,
    );
    finish_archive(builder);
    let minimake_hash = sha256_file(&minimake_archive);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  packages: [\n    {{ name: \"minimake\", version: \"0.1.0\", target: \"{}\", archive: \"{minimake_archive_name}\", archive_hash: \"{minimake_hash}\", runtime_deps: [] }}\n  ]\n}}\n",
            target_triple()
        ),
    )
    .unwrap();

    fs::write(
        runes.join("realpkg.rn"),
        format!(
            "export const package = {{\n  name: 'realpkg'\n  version: '1.0.0'\n  sources: {{ main: {{ url: 'realpkg-1.0.0.tar.zst', sha256: '{source_hash}' }} }}\n  deps: {{ build: {{ default: ['minimake'] }}, runtime: [] }}\n  bins: {{ realpkg: 'bin/realpkg' }}\n}}\n\nexport def build [ctx] {{\n  let source_dir = ($ctx.sources.main.dir | path join 'realpkg-1.0.0')\n  let build_dir = ($ctx.package_dir | path join 'build')\n  let result = (sh -c $\"mkdir -p '($build_dir)' && cd '($build_dir)' && '($source_dir)/configure' --prefix='($ctx.prefix)' && make && make install PREFIX='($ctx.package_dir)'\" | complete)\n  if $result.exit_code != 0 {{\n    error make {{ msg: $result.stderr }}\n  }}\n}}\n"
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

    let output = run_shim(root, "realpkg");
    assert_success(&output, "run realpkg");
    let line = stdout(&output);
    assert!(
        line.starts_with("built from source via "),
        "realpkg output should reflect configured source build: {line}"
    );
    assert!(
        line.trim_end().ends_with("/package"),
        "ctx.prefix/package_dir should point at the temporary staging package dir: {line}"
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
            "export const package = {{\n  name: 'patched'\n  version: '0.1.0'\n  summary: 'original summary'\n  sources: {{ main: {{ url: 'old.txt', sha256: '{old_hash}' }} }}\n  bins: {{ patched: 'bin/patched' }}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'patched')\n}}\n"
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
        "export const package = {\n  name: 'dep'\n  version: '0.1.0'\n  bins: { dep: 'bin/dep' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'dep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'dep')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['dep'], build: {} }\n  bins: { app: 'bin/app' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
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
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n  bins: { locksrc: 'bin/locksrc' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v1\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
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
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n  bins: { locksrc: 'bin/locksrc' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v2\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
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

    // A tome that publishes its package repo to a local `dist/` directory: it ships a rune
    // *and* a pre-built archive alongside the index in `dist/`.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'bincore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
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
        &tome.join("dist").join(&archive_name),
        "binpkg",
        &triple,
        "#!/usr/bin/env sh\nprintf 'from binary\\n'\n",
    );
    let hash = sha256_file(&archive);
    fs::write(
        tome.join("dist").join("index.nuon"),
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
fn install_resolves_binary_over_http() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The published index + archive live in a directory served over HTTP; the tome.rn points
    // at that base URL with format "http". Installing must fetch and verify the archive over
    // the network rather than building the source rune (which prints a different marker).
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        runes.join("httppkg.rn"),
        "export const package = {\n  name: 'httppkg'\n  version: '0.1.0'\n  bins: { httppkg: 'bin/httppkg' }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from source\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'httppkg')\n}\n",
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
            "{{\n  packages: [\n    {{ name: \"httppkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: [] }}\n  ]\n}}\n"
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
        "http binary archive must be preferred over source build"
    );
}

#[test]
fn install_pulls_in_runtime_dependencies() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // A tome that publishes to a local `dist/` directory: `app` declares a runtime dependency
    // on `lib`, and both ship as pre-built archives. Installing `app` must install `lib` too.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'depcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
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
        &tome.join("dist").join(&app_name),
        "app",
        &triple,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
    );
    let app_hash = sha256_file(&app);

    let lib_name = format!("lib-0.1.0-{triple}.tar.zst");
    let lib = make_indexed_archive(
        &tome.join("dist").join(&lib_name),
        "lib",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib\\n'\n",
    );
    let lib_hash = sha256_file(&lib);

    fs::write(
        tome.join("dist").join("index.nuon"),
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
fn install_selects_constrained_dependency_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The index offers two versions of `lib`; `app` constrains it to `<2.0`. The solver must
    // pick `lib` 1.0.0 even though 2.0.0 is newer, proving version-aware resolution end to end.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'vercore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    for pkg in ["app", "lib"] {
        fs::write(
            runes.join(format!("{pkg}.rn")),
            format!("export const package = {{\n  name: '{pkg}'\n  version: '1.0.0'\n  bins: {{}}\n}}\n\nexport def build [ctx] {{ }}\n"),
        )
        .unwrap();
    }

    let dist = tome.join("dist");
    let app_name = format!("app-1.0.0-{triple}.tar.zst");
    let app = make_versioned_archive(
        &dist.join(&app_name),
        "app",
        "1.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'app\\n'\n",
    );
    let app_hash = sha256_file(&app);

    let lib1_name = format!("lib-1.0.0-{triple}.tar.zst");
    let lib1 = make_versioned_archive(
        &dist.join(&lib1_name),
        "lib",
        "1.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib 1.0\\n'\n",
    );
    let lib1_hash = sha256_file(&lib1);

    let lib2_name = format!("lib-2.0.0-{triple}.tar.zst");
    let lib2 = make_versioned_archive(
        &dist.join(&lib2_name),
        "lib",
        "2.0.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'lib 2.0\\n'\n",
    );
    let lib2_hash = sha256_file(&lib2);

    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  packages: [\n    {{ name: \"app\", version: \"1.0.0\", target: \"{triple}\", archive: \"{app_name}\", archive_hash: \"{app_hash}\", runtime_deps: [{{ name: \"lib\", version: \"<2.0\" }}] }}\n    {{ name: \"lib\", version: \"1.0.0\", target: \"{triple}\", archive: \"{lib1_name}\", archive_hash: \"{lib1_hash}\", runtime_deps: [] }}\n    {{ name: \"lib\", version: \"2.0.0\", target: \"{triple}\", archive: \"{lib2_name}\", archive_hash: \"{lib2_hash}\", runtime_deps: [] }}\n  ]\n}}\n"
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
fn install_locked_reproduces_pinned_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // The index offers two versions of `lockpkg`; the lockfile pins the older 0.1.0. A
    // `--locked` install must reproduce the pinned 0.1.0 even though 0.2.0 is newer and present.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'lockcore'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("lockpkg.rn"),
        "export const package = {\n  name: 'lockpkg'\n  version: '0.1.0'\n  bins: {}\n}\n\nexport def build [ctx] { }\n",
    )
    .unwrap();

    let dist = tome.join("dist");
    let v1_name = format!("lockpkg-0.1.0-{triple}.tar.zst");
    let v1 = make_versioned_archive(
        &dist.join(&v1_name),
        "lockpkg",
        "0.1.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v0.1.0\\n'\n",
    );
    let v1_hash = sha256_file(&v1);

    let v2_name = format!("lockpkg-0.2.0-{triple}.tar.zst");
    let v2 = make_versioned_archive(
        &dist.join(&v2_name),
        "lockpkg",
        "0.2.0",
        &triple,
        "#!/usr/bin/env sh\nprintf 'v0.2.0\\n'\n",
    );
    let v2_hash = sha256_file(&v2);

    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  packages: [\n    {{ name: \"lockpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{v1_name}\", archive_hash: \"{v1_hash}\", runtime_deps: [] }}\n    {{ name: \"lockpkg\", version: \"0.2.0\", target: \"{triple}\", archive: \"{v2_name}\", archive_hash: \"{v2_hash}\", runtime_deps: [] }}\n  ]\n}}\n"
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
            "{{\n  version: 1,\n  packages: [\n    {{ name: \"lockpkg\", version: \"0.1.0\", target: \"{triple}\", archive_hash: \"{v1_hash}\", source_hashes: {{}}, runtime_deps: [], build_deps: [] }}\n  ]\n}}\n"
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
        "export const tome = {\n  name: 'core'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", bins: {{{name}: \"bin/{name}\"}}}}\n"
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
