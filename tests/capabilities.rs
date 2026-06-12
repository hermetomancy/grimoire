//! Capability runtime dependencies and content addressing: the closure walker resolves
//! capability names to concrete providers the same way the solver does, deterministically.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

/// A tome with two awk providers (`gawk` and `mawk`, each exposing an `awk` bin) and an `app`
/// whose runtime dep is the *capability* name `awk`, not a package name.
fn setup_capability_tome(root: &std::path::Path) -> TempDir {
    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'captome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    for provider in ["gawk", "mawk"] {
        fs::write(
            runes.join(format!("{provider}.rn")),
            format!(
                "export const package = {{\n  name: '{provider}'\n  version: '0.1.0'\n  bins: {{ default: {{ {provider}: 'bin/{provider}', awk: 'bin/{provider}' }} }}\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{provider}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' '{provider}')\n}}\n"
            ),
        )
        .unwrap();
    }
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['awk'], build: {} }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "tome add captome");
    tome
}

#[test]
fn capability_dep_is_hashable_and_resolution_is_stable() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let _tome = setup_capability_tome(root);

    // Hashing a rune with a capability runtime dep works at all (used to error with
    // "no rune found for `awk`"), and repeats are identical.
    let first = store_hash(root, "app");
    let second = store_hash(root, "app");
    assert_eq!(first, second, "capability resolution must be deterministic");

    // With no preference and nothing installed, the fallback is the first provider by name:
    // gawk, not mawk.
    assert_success(&run(root, &["install", "app"]), "install app");
    assert!(
        root.join("state")
            .join("packages")
            .join("gawk.nuon")
            .exists(),
        "the solver must pick the first provider by name"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("mawk.nuon")
            .exists(),
        "the other provider must not be pulled in"
    );

    // The walker and the solver agree: reinstalling is a no-op, not a staleness rebuild loop.
    let again = run(root, &["install", "app"]);
    assert_success(&again, "reinstall app");
    assert!(
        stdout(&again).contains("already installed and up to date"),
        "walker and solver must compute the same address: {}",
        stdout(&again)
    );
}

#[test]
fn preference_changes_the_capability_providers_address() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let _tome = setup_capability_tome(root);

    let default_hash = store_hash(root, "app");
    assert_success(&run(root, &["prefer", "awk", "mawk"]), "prefer awk mawk");
    let preferred_hash = store_hash(root, "app");
    assert_ne!(
        default_hash, preferred_hash,
        "a different provider is different content, so a different address"
    );

    // The preference drives the install too: the resolved provider is mawk.
    assert_success(
        &run(root, &["install", "app"]),
        "install app with preference",
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("mawk.nuon")
            .exists(),
        "the preferred provider must be installed"
    );
}
