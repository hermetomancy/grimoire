//! Semantic activation: rollback restores package state and the lockfile from the
//! generation's snapshot, clean preserves the rollback target and reclaims unreferenced
//! store paths, and doctor flags divergence.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

/// Two single-package generations: installing `alpha` then `beta` so gen-1 = {alpha} and
/// gen-2 = {alpha, beta}, ready for rollback scenarios.
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
fn rollback_restores_package_state_and_lockfile() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    let beta_state = root.join("state").join("packages").join("beta.nuon");
    assert!(beta_state.exists(), "beta installed in gen 2");

    assert_success(&run(root, &["rollback"]), "rollback to gen 1");

    // State now describes the rolled-back generation, not the abandoned one.
    assert!(
        !beta_state.exists(),
        "rollback must drop beta from state/packages"
    );
    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("alpha") && !list.contains("beta"),
        "grm list must report the rolled-back set: {list}"
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
fn mutation_after_rollback_does_not_resurrect_rolled_back_packages() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);

    assert_success(&run(root, &["rollback"]), "rollback to gen 1");
    assert_success(&run(root, &["install", "gamma"]), "install gamma (gen 3)");

    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("alpha") && list.contains("gamma") && !list.contains("beta"),
        "the new generation must build on the rolled-back set: {list}"
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

    assert_success(&run(root, &["rollback"]), "rollback to gen 1");
    assert_success(&run(root, &["rollback", "2"]), "switch forward to gen 2");

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
fn clean_preserves_the_rollback_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    setup_two_generations(root);
    assert_success(&run(root, &["install", "gamma"]), "install gamma (gen 3)");

    assert_success(&run(root, &["clean", "--keep", "1"]), "clean --keep 1");
    let rollback = run(root, &["rollback"]);
    assert_success(&rollback, "rollback after aggressive clean");
    let list = stdout(&run(root, &["list"]));
    assert!(
        list.contains("beta") && !list.contains("gamma"),
        "rollback target (gen 2) must have survived clean: {list}"
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
    // retention window (`--keep 1` retains gen 5 plus its rollback target, gen 4).
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
    let current = stdout(&run(root, &["generations"]));
    assert!(current.contains("* gen-2"), "gen 2 active: {current}");
    assert_success(&run(root, &["rollback", "2"]), "re-activate to converge");
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
