//! Publishing prebuilts with `grm tome build` and substitute matching.

mod support;

use std::fs;
use std::path::Path;

use support::*;
use tempfile::TempDir;

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

    // A target-unrestricted rune so every platform (including FreeBSD, where both
    // platform-specific runes are filtered out) has something to build.
    fs::write(
        tome_dir.join("runes").join("always.rn"),
        "export const package = {\n  name: 'always'\n  version: '0.1.0'\n  sources: {}\n  deps: { build: {} runtime: [] }\n \n}\n\nexport def build [ctx] {\n  let bin_dir = ($ctx.package_dir | path join 'bin')\n  mkdir $bin_dir\n  \"#!/usr/bin/env sh\\nprintf 'always\\n'\" | save ($bin_dir | path join 'always')\n}\n",
    )
    .unwrap();

    let build = run(root, &["tome", "build", "--all", "--path", tome_path]);
    assert_success(&build, "tome build --all with target filtering");

    let target = target_triple();
    let dist = tome_dir.join("dist");

    assert!(
        dist.join(format!("always-0.1.0-{target}.tar.zst")).exists(),
        "the target-unrestricted rune must build on every platform"
    );

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
fn tome_build_all_uses_requested_target_for_filtering_and_store_only_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("crosstarget");
    let tome_path = tome_dir.to_str().unwrap();
    assert_success(
        &run(root, &["tome", "init", "crosstarget", "--path", tome_path]),
        "tome init",
    );

    let requested = if target_triple().starts_with("macos-") {
        "linux-x86_64-musl"
    } else {
        "macos-aarch64-darwin"
    };
    fs::write(
        tome_dir.join("runes").join("onlyrequested.rn"),
        format!(
            "export const package = {{\n  name: 'onlyrequested'\n  version: '0.1.0'\n  targets: ['{requested}']\n  sources: {{}}\n  deps: {{ build: {{}} runtime: [] }}\n}}\n\nexport def build [ctx] {{\n  let bin_dir = ($ctx.package_dir | path join 'bin')\n  mkdir $bin_dir\n  \"#!/usr/bin/env sh\\nprintf 'onlyrequested\\n'\" | save ($bin_dir | path join 'onlyrequested')\n}}\n"
        ),
    )
    .unwrap();

    let build = run(
        root,
        &[
            "tome", "build", "--all", "--path", tome_path, "--target", requested,
        ],
    );
    assert_success(&build, "cross-target tome build --all");

    let archive_rel = format!("onlyrequested-0.1.0-{requested}.tar.zst");
    assert!(
        tome_dir.join("dist").join(&archive_rel).exists(),
        "requested-target archive should be built"
    );
    let index = fs::read_to_string(tome_dir.join("dist").join("index.nuon")).unwrap();
    assert!(
        index.contains(&archive_rel),
        "requested-target archive should be indexed: {index}"
    );
}

#[test]
fn tome_build_index_fails_on_bad_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("badindex");
    let tome_path = tome_dir.to_str().unwrap();
    assert_success(
        &run(root, &["tome", "init", "badindex", "--path", tome_path]),
        "tome init",
    );
    let dist = tome_dir.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        dist.join("broken-0.1.0-linux-x86_64-musl.tar.zst"),
        b"not zstd",
    )
    .unwrap();

    let rebuild = run(root, &["tome", "build", "--path", tome_path, "--index"]);
    assert_failure_contains(&rebuild, "index archive", "bad archive index rebuild");
}
