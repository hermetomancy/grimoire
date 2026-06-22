//! Semantic activation: switch restores package state and the lockfile from the
//! generation's snapshot, clean preserves the switch-back target and reclaims unreferenced
//! store paths, and doctor flags divergence.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

/// Two single-package generations: installing `alpha` then `beta` so gen-1 = {alpha} and
/// gen-2 = {alpha, beta}, ready for switch scenarios.
fn setup_two_generations(root: &std::path::Path) {
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let dist = tome_path.join("dist");
    let entries = vec![
        dep_archive_entry(
            &dist,
            "alpha",
            "0.1.0",
            &triple,
            "[]",
            "cafef00dcafef00d-alf",
        ),
        dep_archive_entry(
            &dist,
            "beta",
            "0.1.0",
            &triple,
            "[]",
            "cafef00dcafef00d-bet",
        ),
        dep_archive_entry(
            &dist,
            "gamma",
            "0.1.0",
            &triple,
            "[]",
            "cafef00dcafef00d-gam",
        ),
    ];
    write_dep_tome(tome_path, "gencore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add gencore",
    );
    assert_success(&run(root, &["tome", "update", "gencore"]), "tome update");
    assert_success(&run(root, &["install", "alpha"]), "install alpha (gen 1)");
    assert_success(&run(root, &["install", "beta"]), "install beta (gen 2)");
    // The tome dir is a TempDir; keep it alive long enough for the installs above.
    // (Tome cache is already synced, so dropping it afterwards is fine.)
}

#[test]
fn switch_restores_package_state_and_lockfile() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    let beta_state = root.join("state").join("packages").join("beta.nuon");
    assert!(beta_state.exists(), "beta installed in gen 2");

    assert_success(&run(root, &["generation", "switch"]), "switch to gen 1");

    // State now describes the switched-to generation, not the abandoned one.
    assert!(
        !beta_state.exists(),
        "switch must drop beta from state/packages"
    );
    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("alpha") && !list.contains("beta"),
        "grm list must report the switched-to set: {list}"
    );
    let lock = fs::read_to_string(root.join("state").join("grimoire.lock.nuon")).unwrap();
    assert!(
        lock.contains("alpha") && !lock.contains("beta"),
        "lockfile must be rebuilt from the activated generation: {lock}"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("beta")
            .exists(),
        "beta's bin must leave the active profile"
    );
}

#[test]
fn mutation_after_switch_does_not_resurrect_dropped_packages() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    assert_success(&run(root, &["generation", "switch"]), "switch to gen 1");
    assert_success(&run(root, &["install", "gamma"]), "install gamma (gen 3)");

    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("alpha") && list.contains("gamma") && !list.contains("beta"),
        "the new generation must build on the switched-to set: {list}"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("beta")
            .exists(),
        "beta must not be resurrected into the new generation"
    );
}

#[test]
fn switch_forward_restores_the_newer_set() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    assert_success(&run(root, &["generation", "switch"]), "switch to gen 1");
    assert_success(&run(root, &["generation", "switch", "2"]), "switch forward to gen 2");

    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("alpha") && list.contains("beta"),
        "switching forward must restore the newer state: {list}"
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("beta.nuon")
            .exists(),
        "beta state must be restored by the forward switch"
    );
}

#[test]
fn clean_preserves_the_switch_back_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);
    assert_success(&run(root, &["install", "gamma"]), "install gamma (gen 3)");

    assert_success(&run(root, &["clean", "--keep", "1"]), "clean --keep 1");
    let switched = run(root, &["generation", "switch"]);
    assert_success(&switched, "switch after aggressive clean");
    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("beta") && !list.contains("gamma"),
        "switch-back target (gen 2) must have survived clean: {list}"
    );
}

#[test]
fn clean_reclaims_store_dirs_left_by_removal() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    // Removal is store-preserving: beta's store dir survives the remove (gen 3) ...
    assert_success(&run(root, &["remove", "beta"]), "remove beta (gen 3)");
    assert!(
        store_has_package(root, "beta"),
        "beta's store dir must survive its removal"
    );

    // ... and a couple more generations push every beta-referencing generation past the
    // retention window (`--keep 1` retains gen 5 plus its switch-back target, gen 4).
    assert_success(&run(root, &["install", "gamma"]), "install gamma (gen 4)");
    assert_success(&run(root, &["remove", "gamma"]), "remove gamma (gen 5)");
    assert_success(&run(root, &["clean", "--keep", "1"]), "clean --keep 1");

    assert!(
        !store_has_package(root, "beta"),
        "clean must collect the store dir no retained generation references"
    );
    assert!(
        store_has_package(root, "alpha"),
        "alpha is still installed and must survive clean"
    );
}

#[test]
fn doctor_flags_state_generation_divergence() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    // Manufacture the crash-window shape: state lost a package the active generation has.
    fs::remove_file(root.join("state").join("packages").join("beta.nuon")).unwrap();

    let doctor = run(root, &["doctor"]);
    assert_success(&doctor, "doctor runs");
    assert!(
        stderr(&doctor).contains("diverges from active generation"),
        "doctor must flag the divergence: {}",
        stderr(&doctor)
    );

    // Re-activating the current generation is the documented repair path.
    let current = stdout(&run(root, &["generation", "list"]));
    assert!(current.contains("* gen-2"), "gen 2 active: {current}");
    assert_success(&run(root, &["generation", "switch", "2"]), "re-activate to converge");
    assert!(
        root.join("state")
            .join("packages")
            .join("beta.nuon")
            .exists(),
        "re-activation must restore the missing state"
    );
    let doctor = run(root, &["doctor"]);
    assert!(
        !stderr(&doctor).contains("diverges from active generation"),
        "doctor must be clean after convergence: {}",
        stderr(&doctor)
    );
}

/// An install whose generation build *refuses* (a contested bin with no preference) commits
/// its package transactions first, leaving state saying "installed" while the environment
/// still shows the old generation. The re-run must converge — retry the link, not report
/// "already installed and up to date" over the stale environment.
#[test]
fn failed_generation_link_is_retried_by_the_next_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'wedgetome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // Two packages both shipping an `awk` bin: linking them together contests the name.
    for provider in ["gawk", "mawk"] {
        fs::write(
            runes.join(format!("{provider}.rn")),
            format!(
                "export const package = {{\n  name: '{provider}'\n  version: '0.1.0'\n  bins: {{ default: {{ {provider}: 'bin/{provider}', awk: 'bin/{provider}' }} }}\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{provider}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' '{provider}')\n}}\n"
            ),
        )
        .unwrap();
    }
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add wedgetome",
    );

    assert_success(&run(root, &["install", "gawk"]), "install gawk (gen 1)");

    // Installing mawk commits its transaction, then the generation build refuses on the
    // contested `awk` bin — the command fails with state already saying mawk is installed.
    let wedge = run(root, &["install", "mawk"]);
    assert!(
        !wedge.status.success(),
        "install mawk should refuse on the contested bin"
    );
    assert!(
        stderr(&wedge).contains("provided by multiple installed packages"),
        "expected the contested-bin refusal: {}",
        stderr(&wedge)
    );

    // The wedge: a re-run resolves no steps. It must detect the stale generation and retry
    // the link (repeating the real error), not claim "already installed and up to date".
    let rerun = run(root, &["install", "mawk"]);
    assert!(
        !rerun.status.success(),
        "re-run must retry the failed generation link, not report success: {}",
        stdout(&rerun)
    );
    assert!(
        !stdout(&rerun).contains("already installed and up to date"),
        "re-run must not report up to date over a stale environment: {}",
        stdout(&rerun)
    );
    assert!(
        stderr(&rerun).contains("provided by multiple installed packages"),
        "re-run should repeat the contested-bin refusal: {}",
        stderr(&rerun)
    );

    // Settling the contest unwedges the same re-run: it relinks and the environment
    // finally contains mawk.
    assert_success(&run(root, &["pkg", "prefer", "awk", "gawk"]), "prefer awk gawk");
    assert_success(
        &run(root, &["install", "mawk"]),
        "install mawk after preference relinks",
    );
    let mawk = run_shim(root, "mawk");
    assert_success(&mawk, "run mawk from the relinked generation");
    assert_eq!(stdout(&mawk).trim(), "mawk", "mawk bin output");
    let awk = run_shim(root, "awk");
    assert_eq!(
        stdout(&awk).trim(),
        "gawk",
        "awk goes to the preferred provider"
    );
}

/// Generation bins are symlinks into the immutable store, so a binary's `@loader_path` /
/// `current_exe` resolves back to the store where `bin/` and `lib/` are siblings — what rust's
/// `rustc` needs to find `librustc_driver` and its sysroot. This holds whether or not the
/// package ships a `lib/`, and the symlinked tool still runs through the profile.
#[test]
fn generation_bins_are_symlinks_into_the_store() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'libtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // `libtool` installs a lib/ alongside its bin (the rust shape); `plaintool` ships only a
    // bin. Both must be symlinked uniformly.
    fs::write(
        runes.join("libtool.rn"),
        "export const package = {\n  name: 'libtool'\n  version: '0.1.0'\n  bins: { default: { libtool: 'bin/libtool' } }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  mkdir ($ctx.package_dir | path join 'lib')\n  \"#!/usr/bin/env sh\\nprintf 'libtool\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'libtool')\n  \"x\" | save ($ctx.package_dir | path join 'lib' 'libfoo.dylib')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("plaintool.rn"),
        "export const package = {\n  name: 'plaintool'\n  version: '0.1.0'\n  bins: { default: { plaintool: 'bin/plaintool' } }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'plaintool\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'plaintool')\n}\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add libtome",
    );
    assert_success(
        &run(root, &["install", "libtool", "plaintool"]),
        "install libtool + plaintool",
    );

    let gen_bin = |name: &str| root.join("profiles").join("current").join("bin").join(name);

    // Both bins are symlinks pointing into the store, regardless of whether the package ships
    // a lib/.
    for name in ["libtool", "plaintool"] {
        let bin = gen_bin(name);
        let meta = fs::symlink_metadata(&bin).expect("bin linked into generation");
        assert!(
            meta.file_type().is_symlink(),
            "{name}'s generation bin must be a symlink"
        );
        let target = fs::read_link(&bin).unwrap();
        assert!(
            target.starts_with(root.join("store")),
            "{name}'s symlink must point into the store, got {}",
            target.display()
        );
    }

    // The symlinked tool still runs through the profile.
    let out = run_shim(root, "libtool");
    assert_success(&out, "run libtool from the generation symlink");
    assert_eq!(stdout(&out).trim(), "libtool", "libtool output");
}
