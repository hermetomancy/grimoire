//! `conflicts`/`replaces` metadata: mutual exclusion at install time, rename migration with
//! intent carry-over, and rename discovery by a bare `grm upgrade`.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

fn simple_rune(name: &str, extra_fields: &str) -> String {
    format!(
        "export const package = {{\n  name: '{name}'\n  version: '0.1.0'\n{extra_fields} \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{name}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' '{name}')\n}}\n"
    )
}

fn write_tome(tome: &std::path::Path, name: &str) {
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: '{name}'\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn conflicting_package_is_refused_until_the_conflict_leaves() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    write_tome(tome, "conftome");
    fs::write(
        tome.join("runes").join("alpha.rn"),
        simple_rune("alpha", ""),
    )
    .unwrap();
    fs::write(
        tome.join("runes").join("beta.rn"),
        simple_rune("beta", "  conflicts: ['alpha']\n"),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add conftome",
    );
    assert_success(&run(root, &["install", "alpha"]), "install alpha");

    let blocked = run(root, &["install", "beta"]);
    assert_failure_contains(
        &blocked,
        "conflicts with installed `alpha`",
        "conflicting install refused",
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("beta.nuon")
            .exists(),
        "the refused package must not be installed"
    );

    assert_success(&run(root, &["remove", "alpha"]), "remove alpha");
    assert_success(
        &run(root, &["install", "beta"]),
        "install beta after removal",
    );
}

#[test]
fn replacing_package_migrates_state_and_intent() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    write_tome(tome, "renametome");
    fs::write(
        tome.join("runes").join("oldname.rn"),
        simple_rune("oldname", ""),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add renametome",
    );
    assert_success(&run(root, &["install", "oldname"]), "install oldname");
    assert_success(&run(root, &["hold", "oldname"]), "hold oldname");

    // The catalog renames the package; an explicit install of the new name migrates.
    fs::write(
        tome.join("runes").join("newname.rn"),
        simple_rune("newname", "  replaces: ['oldname']\n"),
    )
    .unwrap();
    assert_success(&run(root, &["tome", "update", "renametome"]), "tome update");

    let install = run(root, &["install", "newname"]);
    assert_success(&install, "install newname");
    assert!(
        stdout(&install).contains("newname replaces oldname"),
        "the migration must be announced: {}",
        stdout(&install)
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("oldname.nuon")
            .exists(),
        "the replaced package must be removed in the same command"
    );
    let new_state = state_text(root, "newname");
    assert!(
        new_state.contains("requested: true") && new_state.contains("held: true"),
        "requested/held intent must carry over to the replacement: {new_state}"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("oldname")
            .exists(),
        "the replaced package's bin must leave the profile"
    );
}

#[test]
fn bare_upgrade_discovers_a_rename() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    write_tome(tome, "uprename");
    fs::write(
        tome.join("runes").join("oldname.rn"),
        simple_rune("oldname", ""),
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add uprename",
    );
    assert_success(&run(root, &["install", "oldname"]), "install oldname");

    fs::write(
        tome.join("runes").join("newname.rn"),
        simple_rune("newname", "  replaces: ['oldname']\n"),
    )
    .unwrap();
    assert_success(&run(root, &["tome", "update", "uprename"]), "tome update");

    // Dry run names the rename without acting on it.
    let dry = run(root, &["upgrade", "--dry-run"]);
    assert_success(&dry, "upgrade --dry-run");
    assert!(
        stdout(&dry).contains("oldname → newname (replaced)"),
        "dry run must show the pending rename: {}",
        stdout(&dry)
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("oldname.nuon")
            .exists(),
        "dry run must not migrate"
    );

    let upgrade = run(root, &["upgrade"]);
    assert_success(&upgrade, "bare upgrade");
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("oldname.nuon")
            .exists()
            && root
                .join("state")
                .join("packages")
                .join("newname.nuon")
                .exists(),
        "bare upgrade must perform the rename migration"
    );
    assert!(
        state_text(root, "newname").contains("requested: true"),
        "intent must survive the rename: {}",
        state_text(root, "newname")
    );
    assert_eq!(stdout(&run_shim(root, "newname")).trim(), "newname");
}
