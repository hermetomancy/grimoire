//! Install-reason tracking and orphaned-dependency cleanup.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn remove_sweeps_orphaned_runtime_dependencies() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();

    // Two top-level packages, `app` and `other`, that both depend on the same `lib`. After
    // removing `app`, `lib` must stay because `other` still needs it; after removing `other`,
    // `lib` becomes truly unreferenced and the orphan sweep must take it out too.
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
        // record reads from the archive, and the orphan sweep reads from that state.
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
        !remove_app_out.contains("removed unused dependency lib"),
        "lib must not be swept while other still depends on it: {remove_app_out}"
    );
    assert!(lib_state.exists(), "lib should still be installed");

    // Second removal: nothing else references lib now, so it must be cascaded out.
    let remove_other = run(root, &["remove", "other"]);
    assert_success(&remove_other, "remove other");
    let remove_other_out = stdout(&remove_other);
    assert!(
        remove_other_out.contains("removed unused dependency lib"),
        "lib should be swept when no package depends on it: {remove_other_out}"
    );
    assert!(!lib_state.exists(), "lib state should be gone");
    assert!(
        store_has_package(root, "lib"),
        "removal is store-preserving: lib's store dir stays until `grm clean`"
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
    // the orphan sweep when its last dependent goes away.
    assert_success(&run(root, &["install", "lib"]), "explicit install lib");
    assert!(
        state_text(root, "lib").contains("requested: true"),
        "explicit install must promote the dep: {}",
        state_text(root, "lib")
    );
    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    assert!(
        !stdout(&remove_app).contains("removed unused dependency lib"),
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
fn held_dependency_survives_orphan_sweep() {
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
    assert_success(&run(root, &["pkg", "hold", "lib"]), "hold lib");

    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    assert!(
        !stdout(&remove_app).contains("removed unused dependency lib"),
        "held lib must not be swept: {}",
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
        stdout(&upgrade).contains("removed unused dependency lib"),
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
fn remove_demotes_a_still_required_package_instead_of_breaking_dependents() {
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
    write_dep_tome(tome, "guardcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add guardcore",
    );
    assert_success(&run(root, &["tome", "update", "guardcore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");
    // Promote lib so the demotion below is observable in its state record.
    assert_success(&run(root, &["install", "lib"]), "explicit install lib");
    assert!(
        state_text(root, "lib").contains("requested: true"),
        "lib promoted: {}",
        state_text(root, "lib")
    );

    // Removing a package something still depends on keeps it, demoted to a dependency.
    let remove_lib = run(root, &["remove", "lib"]);
    assert_success(&remove_lib, "remove a still-needed package");
    assert!(
        stdout(&remove_lib).contains("kept lib")
            && stdout(&remove_lib).contains("still required by app"),
        "demotion must name the dependents: {}",
        stdout(&remove_lib)
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("lib.nuon")
            .exists(),
        "lib must remain installed after the demoting removal"
    );
    assert!(
        state_text(root, "lib").contains("requested: false"),
        "lib demoted to dependency: {}",
        state_text(root, "lib")
    );

    // Removing the last dependent now sweeps the demoted package in the same transaction.
    let remove_app = run(root, &["remove", "app"]);
    assert_success(&remove_app, "remove app");
    assert!(
        stdout(&remove_app).contains("removed unused dependency lib"),
        "demoted lib leaves with its last dependent: {}",
        stdout(&remove_app)
    );
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
        "both packages gone after removing the last dependent"
    );
}

#[test]
fn remove_takes_a_dependent_and_its_dependency_together() {
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
    write_dep_tome(tome, "jointcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add jointcore",
    );
    assert_success(&run(root, &["tome", "update", "jointcore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");

    // The whole named set leaves together: lib's only dependent is in the removal set, so
    // lib is removed outright rather than demoted.
    let remove = run(root, &["remove", "app", "lib"]);
    assert_success(&remove, "remove the set together");
    assert!(
        !stdout(&remove).contains("kept lib"),
        "lib must not be demoted when its dependent leaves too: {}",
        stdout(&remove)
    );
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
        "both packages gone after joint removal"
    );
}
