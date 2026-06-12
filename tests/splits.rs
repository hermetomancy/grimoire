//! Split package groups: companion runes (`split_from`) carved out of one parent build.

mod support;

use std::fs;
use std::path::Path;

use support::*;
use tempfile::TempDir;

/// Scaffolds a tome with a split group: `core` (parent) lays out files for itself and for
/// the `extra` member, whose companion rune claims them by glob.
fn write_split_tome(tome: &Path) {
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'splittome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("core.rn"),
        "export const package = {\n  name: 'core'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  mkdir ($ctx.package_dir | path join 'share' 'extra')\n  \"#!/usr/bin/env sh\\nprintf 'core\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'core')\n  \"#!/usr/bin/env sh\\nprintf 'extra\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'extra')\n  'extra data' | save ($ctx.package_dir | path join 'share' 'extra' 'data.txt')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("extra.rn"),
        "export const package = {\n  name: 'extra'\n  version: '0.1.0'\n  split_from: 'core'\n  files: ['bin/extra*' 'share/extra/**']\n  deps: { runtime: ['core'] }\n}\n",
    )
    .unwrap();
}

fn archive_members(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).expect("open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("decode archive");
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .expect("read archive entries")
        .map(|entry| {
            entry
                .expect("read archive entry")
                .path()
                .expect("read entry path")
                .display()
                .to_string()
        })
        .collect()
}

#[test]
fn group_build_produces_one_archive_per_member() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    let out = TempDir::new().unwrap();
    let out = out.path();

    // Building either member builds the whole group once.
    let build = run(
        root,
        &[
            "build",
            tome.path().join("runes").join("core.rn").to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build split group via parent rune");

    let triple = target_triple();
    let core_archive = out.join(format!("core-0.1.0-{triple}.tar.zst"));
    let extra_archive = out.join(format!("extra-0.1.0-{triple}.tar.zst"));
    assert!(core_archive.exists(), "parent archive should exist");
    assert!(extra_archive.exists(), "member archive should exist");

    let extra_members = archive_members(&extra_archive);
    assert!(
        extra_members.iter().any(|m| m == "bin/extra"),
        "member archive carries its claimed bin: {extra_members:?}"
    );
    assert!(
        extra_members.iter().any(|m| m == "share/extra/data.txt"),
        "member archive carries its claimed share files: {extra_members:?}"
    );
    assert!(
        extra_members.iter().any(|m| m == ".grimoire/group/core.rn")
            && extra_members
                .iter()
                .any(|m| m == ".grimoire/group/extra.rn"),
        "member archive embeds the whole group's runes: {extra_members:?}"
    );

    let core_members = archive_members(&core_archive);
    assert!(
        core_members.iter().any(|m| m == "bin/core"),
        "parent keeps its own files: {core_members:?}"
    );
    assert!(
        !core_members.iter().any(|m| m.starts_with("bin/extra")),
        "claimed files must not remain in the parent: {core_members:?}"
    );
    assert!(
        !core_members.iter().any(|m| m.starts_with("share/extra")),
        "emptied directories must not linger in the parent: {core_members:?}"
    );
}

#[test]
fn installing_a_member_realizes_member_and_parent() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());

    let add = run(
        root,
        &[
            "tome",
            "add",
            tome.path().to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add split tome");

    let install = run(root, &["install", "extra"]);
    assert_success(&install, "install split member");

    assert_eq!(stdout(&run_shim(root, "extra")).trim(), "extra");
    assert!(
        root.join("state")
            .join("packages")
            .join("extra.nuon")
            .exists(),
        "member state should be recorded"
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("core.nuon")
            .exists(),
        "the parent is a runtime dep of the member and must be installed too"
    );
    assert!(store_has_package(root, "core") && store_has_package(root, "extra"));

    // The parent's store dir holds only the remainder.
    let core_dir = installed_store_dir(root, "core").expect("core store dir");
    assert!(core_dir.join("bin").join("core").exists());
    assert!(
        !core_dir.join("bin").join("extra").exists(),
        "claimed member files must not be in the parent's store dir"
    );
}

#[test]
fn tome_build_registers_every_member_and_index_rebuild_agrees() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    let tome_path = tome.path().to_str().unwrap();

    let build = run(root, &["tome", "build", "core", "--path", tome_path]);
    assert_success(&build, "tome build split parent");

    let index_path = tome.path().join("dist").join("index.nuon");
    let index = fs::read_to_string(&index_path).unwrap();
    assert!(
        index.contains("name: \"core\"") || index.contains("name: core"),
        "index should register the parent: {index}"
    );
    assert!(
        index.contains("name: \"extra\"") || index.contains("name: extra"),
        "index should register the split member: {index}"
    );

    // A second build is a no-op only when *all* members are present: the archives must not
    // be rewritten.
    let triple = target_triple();
    let core_archive = tome
        .path()
        .join("dist")
        .join(format!("core-0.1.0-{triple}.tar.zst"));
    let extra_archive = tome
        .path()
        .join("dist")
        .join(format!("extra-0.1.0-{triple}.tar.zst"));
    let mtime = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
    let (core_before, extra_before) = (mtime(&core_archive), mtime(&extra_archive));
    let again = run(root, &["tome", "build", "core", "--path", tome_path]);
    assert_success(&again, "tome build skip when all members built");
    assert_eq!(
        (mtime(&core_archive), mtime(&extra_archive)),
        (core_before, extra_before),
        "a fully-built group must be skipped, not rebuilt"
    );

    // Rebuilding the index from the archives alone must reproduce the same store hashes —
    // member addresses are recomputable from the embedded `.grimoire/group/` runes.
    fs::remove_file(&index_path).unwrap();
    let reindex = run(root, &["tome", "build", "--index", "--path", tome_path]);
    assert_success(&reindex, "rebuild index from archives");
    let rebuilt = fs::read_to_string(&index_path).unwrap();
    assert_eq!(
        index, rebuilt,
        "index rebuilt from archives must match the one written at build time"
    );
}

#[test]
fn member_install_resolves_parents_build_deps() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let runes = tome.path().join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(tome.path().join("dist")).unwrap();
    fs::write(
        tome.path().join("tome.rn"),
        "export const tome = {\n  name: 'splitdeps'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("tool.rn"),
        "export const package = {\n  name: 'tool'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'stamped\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamptool')\n}\n",
    )
    .unwrap();
    // The parent's build invokes `stamptool` — only on PATH if the parent's build deps are
    // installed and wired up.
    fs::write(
        runes.join("core.rn"),
        "export const package = {\n  name: 'core'\n  version: '0.1.0'\n  deps: { build: { default: ['tool'] }, runtime: [] }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  mkdir ($ctx.package_dir | path join 'share')\n  let result = (stamptool | complete)\n  $result.stdout | save ($ctx.package_dir | path join 'share' 'stamp.txt')\n  \"#!/usr/bin/env sh\\nprintf 'core\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'core')\n  \"#!/usr/bin/env sh\\nprintf 'extra\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'extra')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("extra.rn"),
        "export const package = {\n  name: 'extra'\n  version: '0.1.0'\n  split_from: 'core'\n  files: ['bin/extra*']\n  deps: { runtime: ['core'] }\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &[
            "tome",
            "add",
            tome.path().to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add splitdeps tome");

    // Installing the *member* must install the parent's build deps (the member declares
    // none of its own) and run the group build with them on PATH.
    let install = run(root, &["install", "extra"]);
    assert_success(&install, "install member whose parent needs a build dep");

    let core_dir = installed_store_dir(root, "core").expect("core store dir");
    let stamp = fs::read_to_string(core_dir.join("share").join("stamp.txt")).unwrap();
    assert_eq!(
        stamp.trim(),
        "stamped",
        "the parent build must have run with its own build deps available"
    );
}

#[test]
fn store_hash_accepts_member_rune_paths() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    let rune = |name: &str| {
        tome.path()
            .join("runes")
            .join(format!("{name}.rn"))
            .display()
            .to_string()
    };

    // `grm store-hash <file.rn>` addresses by path, not package name — the group lookup
    // must still resolve the member. Regression test: this used to fail for group members.
    let core = store_hash(root, &rune("core"));
    let extra = store_hash(root, &rune("extra"));
    assert_ne!(core, extra, "members derive distinct addresses");
    assert_eq!(
        extra,
        store_hash(root, &rune("extra")),
        "path-invoked member hash is stable"
    );
}

#[test]
fn overlapping_member_claims_fail_the_build() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    // A second member claiming the same files as `extra`.
    fs::write(
        tome.path().join("runes").join("grabby.rn"),
        "export const package = {\n  name: 'grabby'\n  version: '0.1.0'\n  split_from: 'core'\n  files: ['bin/extra*']\n}\n",
    )
    .unwrap();
    let out = TempDir::new().unwrap();

    let build = run(
        root,
        &[
            "build",
            tome.path().join("runes").join("core.rn").to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &build,
        "overlapping",
        "two members claiming the same file is a hard error",
    );
}

#[test]
fn member_claiming_nothing_fails_the_build() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    fs::write(
        tome.path().join("runes").join("hollow.rn"),
        "export const package = {\n  name: 'hollow'\n  version: '0.1.0'\n  split_from: 'core'\n  files: ['lib/nonexistent/**']\n}\n",
    )
    .unwrap();
    let out = TempDir::new().unwrap();

    let build = run(
        root,
        &[
            "build",
            tome.path().join("runes").join("core.rn").to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &build,
        "claims no files",
        "a member whose globs match nothing is a hard error",
    );
}

#[test]
fn version_skew_between_member_and_parent_fails() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    fs::write(
        tome.path().join("runes").join("extra.rn"),
        "export const package = {\n  name: 'extra'\n  version: '0.2.0'\n  split_from: 'core'\n  files: ['bin/extra*']\n}\n",
    )
    .unwrap();
    let out = TempDir::new().unwrap();

    let build = run(
        root,
        &[
            "build",
            tome.path().join("runes").join("core.rn").to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &build,
        "versions must match",
        "a member at a different version than its parent is rejected",
    );
}

#[test]
fn split_member_declaring_sources_is_rejected() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    write_split_tome(tome.path());
    fs::write(
        tome.path().join("runes").join("extra.rn"),
        format!(
            "export const package = {{\n  name: 'extra'\n  version: '0.1.0'\n  split_from: 'core'\n  files: ['bin/extra*']\n  sources: {{ main: {{ url: 'x.tar.zst', sha256: 'sha256:{}' }} }}\n}}\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    let out = TempDir::new().unwrap();

    let build = run(
        root,
        &[
            "build",
            tome.path().join("runes").join("core.rn").to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ],
    );
    assert_failure_contains(
        &build,
        "must not declare `sources`",
        "a split member with its own sources is rejected at parse time",
    );
}
