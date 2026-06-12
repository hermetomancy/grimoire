//! Lockfile tracking, locked installs, dry runs, holds, upgrades, and the process lock.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

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

    // Things `clean` must leave alone, so we can assert it does not touch installed state:
    // a requested package's state record and its store directory. (`clean` reads installed
    // state to sweep unused dependencies, so the record has to be a real one.)
    let state_dir = root.join("state").join("packages");
    fs::create_dir_all(&state_dir).unwrap();
    let state_file = state_dir.join("keep.nuon");
    let store_dir = root
        .join("store")
        .join(fake_store_basename("keep", "0.1.0"));
    fs::write(
        &state_file,
        format!(
            "{{format: 1, name: \"keep\", version: \"0.1.0\", archive_hash: \"{}\", store_hash: \"cafef00dcafef00d\", store_path: \"{}\", requested: true}}\n",
            "0".repeat(64),
            store_dir.display()
        ),
    )
    .unwrap();
    let packages_file = store_dir.join("file");
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
        again_out.contains("nothing to clean"),
        "second clean reports nothing freed: {again_out}"
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
fn restore_reproduces_locked_set_on_a_fresh_root() {
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let dist = tome_path.join("dist");
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
    write_dep_tome(tome_path, "restcore", &entries);

    // Source machine: install app (lib pulled in as a dep), hold lib, save the lockfile.
    let source_root = TempDir::new().unwrap();
    let source_root = source_root.path();
    assert_success(
        &run(
            source_root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add (source)",
    );
    assert_success(
        &run(source_root, &["tome", "update", "restcore"]),
        "tome update (source)",
    );
    assert_success(&run(source_root, &["install", "app"]), "install app");
    assert_success(&run(source_root, &["hold", "lib"]), "hold lib");
    let lock_text =
        fs::read_to_string(source_root.join("state").join("grimoire.lock.nuon")).unwrap();
    assert!(
        lock_text.contains("requested")
            && lock_text.contains("held")
            && lock_text.contains("store_hash"),
        "lock must record restore metadata: {lock_text}"
    );
    let saved_lock = tome_path.join("saved.lock.nuon");
    fs::write(&saved_lock, &lock_text).unwrap();

    // Fresh machine: configure the same tome, then restore from the saved lock alone.
    let fresh_root = TempDir::new().unwrap();
    let fresh_root = fresh_root.path();
    assert_success(
        &run(
            fresh_root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add (fresh)",
    );
    assert_success(
        &run(fresh_root, &["tome", "update", "restcore"]),
        "tome update (fresh)",
    );
    let restore = run(
        fresh_root,
        &["restore", "--lockfile", saved_lock.to_str().unwrap()],
    );
    assert_success(&restore, "restore from saved lock");

    let list = stdout(&run(fresh_root, &["list"]));
    assert!(
        list.contains("app\t0.1.0") && list.contains("lib\t0.1.0"),
        "restore must reproduce the recorded packages: {list}"
    );
    let app_state = state_text(fresh_root, "app");
    assert!(
        app_state.contains("requested: true"),
        "app must be restored as requested: {app_state}"
    );
    let lib_state = state_text(fresh_root, "lib");
    assert!(
        lib_state.contains("requested: false") && lib_state.contains("held: true"),
        "lib must be restored as a held dependency: {lib_state}"
    );
    assert_eq!(stdout(&run_shim(fresh_root, "app")).trim(), "app-0.1.0");
}
