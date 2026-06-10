//! Binary installs: resolution from indexes, verification, dependency pulling.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

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
