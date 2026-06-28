//! Upgrade behavior for content-address drift.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

/// A dry-run upgrade shows the full closure, not just the named target: a transitive dependency
/// with a newer version surfaces as its own plan line, alongside the dependent's rebuild.
#[test]
fn dry_run_upgrade_names_pulled_in_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'pull'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let lib_rune = |version: &str| {
        format!(
            "export const package = {{\n  name: 'lib'\n  version: '{version}'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'lib\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'lib')\n}}\n"
        )
    };
    let app_rune = "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['lib'], build: {} }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n";
    fs::write(runes.join("lib.rn"), lib_rune("1.0.0")).unwrap();
    fs::write(runes.join("app.rn"), app_rune).unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add pull",
    );
    assert_success(&run(root, &["install", "app"]), "install app");

    fs::write(runes.join("lib.rn"), lib_rune("1.1.0")).unwrap();
    assert_success(&run(root, &["tome", "update", "pull"]), "tome update");

    let dry = run(root, &["upgrade", "app", "--dry-run"]);
    assert_success(&dry, "upgrade app --dry-run");
    let out = stdout(&dry);
    assert!(
        out.contains("1.0.0 → 1.1.0"),
        "dry-run names the pulled-in dep bump: {out}"
    );
    assert!(
        out.contains("rebuild: address drifted"),
        "dry-run names the dependent rebuild: {out}"
    );
}

#[test]
fn upgrade_rebuilds_a_drifted_package_instead_of_claiming_up_to_date() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'updrift'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let rune = |payload: &str| {
        format!(
            "export const package = {{\n  name: 'tool'\n  version: '0.1.0'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{payload}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tool')\n}}\n"
        )
    };
    fs::write(runes.join("tool.rn"), rune("first")).unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add updrift",
    );
    assert_success(&run(root, &["install", "tool"]), "install tool");
    assert_eq!(stdout(&run_shim(root, "tool")).trim(), "first");

    fs::write(runes.join("tool.rn"), rune("second")).unwrap();
    assert_success(&run(root, &["tome", "update", "updrift"]), "tome update");

    let dry = run(root, &["upgrade", "tool", "--dry-run"]);
    assert_success(&dry, "upgrade --dry-run with drift");
    assert!(
        stdout(&dry).contains("rebuild: address drifted"),
        "dry-run must name the pending rebuild: {}",
        stdout(&dry)
    );
    assert!(
        !stdout(&dry).contains("is up to date"),
        "a drifted package must not be reported up to date: {}",
        stdout(&dry)
    );

    let upgrade = run(root, &["upgrade", "tool"]);
    assert_success(&upgrade, "upgrade converges the drift");
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "second",
        "the rebuilt package must be live in the profile"
    );

    let again = run(root, &["upgrade", "tool", "--dry-run"]);
    assert_success(&again, "upgrade --dry-run after convergence");
    assert!(
        !stdout(&again).contains("address drifted"),
        "no drift after convergence: {}",
        stdout(&again)
    );
}
