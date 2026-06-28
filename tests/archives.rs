//! Archive safety validation: traversal, hard links, symlink escapes, bad metadata.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

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
fn reject_bad_archive_path() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();

    let archive = make_bad_path_archive(out.path());
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
    assert_failure_contains(
        &install,
        "nested under symlink",
        "reject member nested under symlink",
    );
}

#[test]
fn reject_archive_with_oversized_embedded_metadata() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let archive = out
        .path()
        .join(format!("hugemeta-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let huge = vec![b'a'; 1024 * 1024 + 1];
    append_file(&mut builder, ".grimoire/package.nuon", &huge, 0o644);
    finish_archive(builder);

    assert_failure_contains(
        &run(root, &["install", archive.to_str().unwrap(), "--force"]),
        "exceeds 1048576 bytes",
        "reject oversized embedded package metadata",
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
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
        &run(root, &["install", notarget.to_str().unwrap(), "--force"]),
        "missing required field `target`",
        "reject archive missing target",
    );

    let wrong_target = make_package_archive(
        out,
        "wrongtarget",
        "{format: 1, name: \"wrongtarget\", version: \"0.1.0\", target: \"wrong-target\", bins: {default: {wrongtarget: \"bin/wrongtarget\"}}}\n",
    );
    assert_failure_contains(
        &run(
            root,
            &["install", wrong_target.to_str().unwrap(), "--force"],
        ),
        "is not a supported triple",
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
        &run(
            root,
            &["install", bad_bin_path.to_str().unwrap(), "--force"],
        ),
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
        &run(root, &["install", bad_version.to_str().unwrap(), "--force"]),
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
        &run(root, &["install", bad_bins.to_str().unwrap(), "--force"]),
        "package field `bins` must be a record",
        "reject non-record bins",
    );
}
