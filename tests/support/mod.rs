//! Shared helpers for the integration tests: running the built `grm` binary against a
//! temporary install root, reading its state files, and scaffolding fake tomes/indexes.
//! Archive/keypair construction lives in [`fixtures`], re-exported here.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

mod fixtures;
pub use fixtures::*;

pub const BIN: &str = env!("CARGO_BIN_EXE_grm");

type ZstdFileEncoder = zstd::stream::write::Encoder<'static, fs::File>;
pub fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("GRIMOIRE_ROOT", root)
        .output()
        .expect("spawn grimoire")
}

/// Like [`run`], but with an explicit working directory — used to reproduce argument routing that
/// depends on the cwd contents (e.g. a same-named directory shadowing a package name).
pub fn run_in_dir(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("GRIMOIRE_ROOT", root)
        .current_dir(cwd)
        .output()
        .expect("spawn grimoire")
}

/// Like [`run`], but with extra environment variables — used to pin `GRIMOIRE_BUILD_ENV` so a test
/// can simulate building and installing under different host toolchains.
pub fn run_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BIN);
    command.args(args).env("GRIMOIRE_ROOT", root);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("spawn grimoire")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} should succeed, exit={:?} stderr={}",
        output.status.code(),
        stderr(output)
    );
}

pub fn assert_failure_contains(output: &Output, needle: &str, label: &str) {
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

pub fn target_triple() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let abi = match os {
        "macos" => "darwin",
        "linux" => "musl",
        _ => "unknown",
    };
    format!("{os}-{arch}-{abi}")
}

pub fn sha256_file(path: &Path) -> String {
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

pub fn archive_member_text(path: &Path, member: &str) -> String {
    let file = fs::File::open(path).expect("open package archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("decode package archive");
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().expect("read package archive entries") {
        let mut entry = entry.expect("read package archive entry");
        let entry_path: &Path = &entry.path().expect("read archive path");
        if entry_path == Path::new(member) {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .expect("read archive member text");
            return text;
        }
    }
    panic!("archive member {member} was not found");
}

pub fn run_shim(root: &Path, name: &str) -> Output {
    Command::new(root.join("profiles").join("current").join("bin").join(name))
        .output()
        .expect("run installed shim")
}

/// True when the install root's content-addressed store holds a directory for `name`
/// (`<hash>-<name>-<version>`). Used by tests that assert install/removal of a package without
/// knowing its build-input hash.
pub fn store_has_package(root: &std::path::Path, name: &str) -> bool {
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

/// Writes a prebuilt archive for `pkg` with declared runtime deps into `dist` and returns the
/// index entry line describing it. `deps` is NUON list source, e.g. `["lib"]`.
pub fn dep_archive_entry(
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
pub fn write_dep_tome(tome: &Path, tome_name: &str, entries: &[String]) {
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

pub fn write_dep_index(tome: &Path, entries: &[String]) {
    fs::write(
        tome.join("dist").join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n{}\n  }}\n}}\n",
            entries.join("\n")
        ),
    )
    .unwrap();
}

pub fn state_text(root: &Path, pkg: &str) -> String {
    fs::read_to_string(
        root.join("state")
            .join("packages")
            .join(format!("{pkg}.nuon")),
    )
    .unwrap()
}

/// Like [`dep_archive_entry`], but the archive ships a bin named `bin_name` (instead of the
/// package name) whose shim prints the package name — for contested-capability scenarios.
pub fn capability_archive_entry(
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

pub fn make_fake_tome() -> TempDir {
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

pub fn make_fake_core_tome(triple: &str) -> TempDir {
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

    // doctor's readiness check is just build-env's presence (its closure is the build requirement),
    // so that's all the fake core tome needs to stock.
    let mut entries = String::from("{\n  format: 2,\n    entries: {\n");
    let package = "build-env";
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
    entries.push_str("  }\n}\n");
    fs::write(dist.join("index.nuon"), entries).unwrap();

    dir
}

/// Serves the files in `dir` over a minimal HTTP/1.1 server on an ephemeral local port and
/// returns the base URL. A request for `/name` returns that file (200) or 404 if absent. The
/// listener thread is detached and lives for the rest of the test process.
pub fn serve_dir(dir: PathBuf) -> String {
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

/// A deterministic, syntactically valid store basename (`<hash>-<name>-<version>`) for a
/// hand-built archive fixture. The hash bytes are arbitrary: a local-archive install derives its
/// store location from this embedded basename, and no store-hash cross-check runs unless an index
/// entry carries one (the fixtures here do not).
pub fn fake_store_basename(name: &str, version: &str) -> String {
    format!("cafef00dcafef00d-{name}-{version}")
}

pub fn fake_store_basename_with_hash(name: &str, version: &str, hash: &str) -> String {
    format!("{hash}-{name}-{version}")
}

/// The absolute store directory a package was installed into, read from its recorded state. Tests
/// use this instead of guessing the content-addressed path; returns `None` when not installed.
pub fn installed_store_dir(root: &Path, name: &str) -> Option<PathBuf> {
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

/// The store hash `grm` will assign to `package`, via the hidden `store-hash` seam. Lets a fixture
/// register a prebuilt whose published `store_hash` matches what the installer will recompute, so
/// the prebuilt is accepted as a substitute. `package` may be a known name (the tome must be added)
/// or a path to a `.rn`.
pub fn store_hash(root: &Path, package: &str) -> String {
    let out = run(root, &["store-hash", package]);
    assert_success(&out, &format!("store-hash {package}"));
    stdout(&out).trim().to_string()
}

/// A single-package `index.nuon` body for a prebuilt, with the matching `store_hash`.
pub fn solo_index(
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

/// A server that accepts connections and never answers — an unreachable/hung binhost.
/// Returns the base URL; the listener thread holds every connection open silently.
pub fn serve_black_hole() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind black-hole server");
    let port = listener.local_addr().expect("local addr").port();
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream); // keep the connection open, say nothing
        }
    });
    format!("http://127.0.0.1:{port}")
}
