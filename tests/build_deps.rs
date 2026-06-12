//! Build-dependency resolution and the managed build environment.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn example_tome_build_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    // `hello` is a build dependency of `forge`: it must be installed before the build,
    // so the install of `forge` succeeds end to end.
    let install = run(root, &["install", "forge"]);
    assert_success(&install, "install forge");

    let forge = run_shim(root, "forge");
    assert_success(&forge, "run forge");
    assert_eq!(stdout(&forge).trim(), "forged by grimoire", "forge output");
}

#[test]
fn build_dependency_bins_are_on_build_path() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'pathtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add path tome");
    let install = run(root, &["install", "usespath"]);
    assert_success(&install, "install package using build dep PATH");

    let output = run_shim(root, "usespath");
    assert_success(&output, "run usespath");
    assert_eq!(stdout(&output).trim(), "from build dependency");
}

#[test]
fn build_dependency_bins_take_precedence_over_host_tools() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'prectome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("managedmake.rn"),
        "export const package = {\n  name: 'managedmake'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'managed make\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'make')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usesmake.rn"),
        "export const package = {\n  name: 'usesmake'\n  version: '0.1.0'\n  deps: { build: { default: ['managedmake'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let made = (make | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($made)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usesmake')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add precedence tome");
    let install = run(root, &["install", "usesmake"]);
    assert_success(&install, "install package using managed make");

    let output = run_shim(root, "usesmake");
    assert_success(&output, "run usesmake");
    assert_eq!(stdout(&output).trim(), "managed make");
}

#[test]
fn doctor_reports_managed_core_ready_after_minimal_core_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = make_fake_core_tome(&triple);
    let tome_path = tome.path();

    let add = run(
        root,
        &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add fake core tome");

    let packages = core_readiness_packages();
    for package in packages {
        let install = run(root, &["install", package]);
        assert_success(&install, &format!("install core package {package}"));
    }

    let doctor = run(root, &["doctor"]);
    assert_success(&doctor, "doctor after core readiness install");
    let out = stdout(&doctor);
    let expected = format!("managed core userland: ready ({n}/{n})", n = packages.len());
    assert!(
        out.contains(&expected),
        "doctor reports managed core readiness: {out}"
    );
}

#[test]
fn source_install_keeps_pulled_build_dependency_after_success() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `usespath` is a source rune that lists `stampdep` as a build dep and shells out to its
    // `stamp` binary during the build. Build dependencies are kept after a successful source
    // install so the managed build userland remains available for later builds.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'cleantome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add cleantome");

    assert_success(&run(root, &["install", "usespath"]), "install usespath");

    // The just-built package still works end to end.
    let output = run_shim(root, "usespath");
    assert_success(&output, "run usespath");
    assert_eq!(stdout(&output).trim(), "from build dependency");

    // stampdep remains installed — state and package dir — because it is part of the managed
    // build environment now. But it is *store-only*: nobody requested it and nothing
    // runtime-depends on it, so it must never surface in the profile.
    assert!(
        root.join("state")
            .join("packages")
            .join("stampdep.nuon")
            .exists(),
        "stampdep state should remain"
    );
    assert!(
        store_has_package(root, "stampdep"),
        "stampdep package dir should remain"
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("stamp")
            .exists(),
        "a build dep's bin must not leak into the profile"
    );

    // `usespath` itself stays installed; it is the explicit target, not a build dep.
    assert!(
        root.join("state")
            .join("packages")
            .join("usespath.nuon")
            .exists(),
        "usespath should remain installed"
    );

    // The built archive still lives in cache/builds/ for reproducible locked/source rebuilds.
    let builds = root.join("cache").join("builds");
    let cached: Vec<_> = fs::read_dir(&builds)
        .map(|iter| {
            iter.filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("stampdep-"))
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !cached.is_empty(),
        "stampdep's built archive should remain in cache/builds for future reuse"
    );
}

#[test]
fn source_install_keeps_user_installed_build_dependency() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Same shape as the previous test, but the user installs `stampdep` explicitly first. Keeping
    // build deps after source installs means this should behave the same either way.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'keeptome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("stampdep.rn"),
        "export const package = {\n  name: 'stampdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'from build dependency\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("usespath.rn"),
        "export const package = {\n  name: 'usespath'\n  version: '0.1.0'\n  deps: { build: { default: ['stampdep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'usespath')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add keeptome");

    assert_success(
        &run(root, &["install", "stampdep"]),
        "install stampdep explicitly",
    );
    assert_success(&run(root, &["install", "usespath"]), "install usespath");

    assert!(
        root.join("state")
            .join("packages")
            .join("stampdep.nuon")
            .exists(),
        "explicit stampdep install must remain after the source build"
    );
}

#[test]
fn platform_conditional_build_deps_only_set_matching_prefix() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'prefixtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("matchdep.rn"),
        "export const package = {\n  name: 'matchdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'matchdep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'matchdep')\n}\n",
    )
    .unwrap();

    let other_os = if std::env::consts::OS == "linux" {
        "macos"
    } else {
        "linux"
    };
    fs::write(
        runes.join("skipdep.rn"),
        "export const package = {\n  name: 'skipdep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'skipdep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'skipdep')\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("consumer.rn"),
        format!(
            "export const package = {{\n  name: 'consumer'\n  version: '0.1.0'\n  deps: {{ build: {{ default: ['matchdep', 'skipdep[{}]'] }}, runtime: [] }}\n  bins: {{default: {{ consumer: 'bin/consumer' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  sh -c $\"env > '($ctx.package_dir | path join 'env.txt')'\"\n  \"#!/usr/bin/env sh\\nprintf 'consumer\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'consumer')\n}}\n",
            other_os
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add prefix tome");

    assert_success(&run(root, &["install", "matchdep"]), "install matchdep");

    let build = run(
        root,
        &[
            "build",
            runes.join("consumer.rn").to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build consumer");

    let archive = out.join(format!("consumer-0.1.0-{}.tar.zst", target_triple()));
    let env_text = archive_member_text(&archive, "env.txt");
    assert!(
        env_text.contains("MATCHDEP_PREFIX="),
        "MATCHDEP_PREFIX should be set for matching platform dep: {env_text}"
    );
    assert!(
        !env_text.contains("SKIPDEP_PREFIX="),
        "SKIPDEP_PREFIX should not be set for non-matching platform dep: {env_text}"
    );
}

#[test]
fn shared_build_dependency_realizes_once_across_overlapping_plans() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `top` build-depends on [mid, shared] and `mid` build-depends on [shared]. The plan for
    // top's build deps is resolved before mid's nested build installs `shared`, so by the time
    // the plan's own `shared` step executes it is stale — the package already landed. That
    // step must be reused, not rebuilt.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'sharedtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("shared.rn"),
        "export const package = {\n  name: 'shared'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'shared\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'shared')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("mid.rn"),
        "export const package = {\n  name: 'mid'\n  version: '0.1.0'\n  deps: { build: { default: ['shared'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'mid\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'mid')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("top.rn"),
        "export const package = {\n  name: 'top'\n  version: '0.1.0'\n  deps: { build: { default: ['mid' 'shared'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'top\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'top')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add shared-dep tome");

    let install = run(root, &["install", "top"]);
    assert_success(&install, "install top");
    let out = stdout(&install);
    assert_eq!(
        out.matches("shared 0.1.0 — built from source").count(),
        1,
        "the shared build dep must be built exactly once: {out}"
    );
    assert_eq!(
        out.matches("mid 0.1.0 — built from source").count(),
        1,
        "mid must be built exactly once: {out}"
    );
}

#[test]
fn changed_build_dep_rune_is_rebuilt_not_reused() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `stampdep` prints a stamp that consumers bake into their shims at build time. Editing
    // stampdep's rune without changing its version must re-realize it for the next consumer
    // build: reuse is by content address, not by name+version.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'staletome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let stampdep_rune = |stamp: &str| {
        format!(
            "export const package = {{\n  name: 'stampdep'\n  version: '0.1.0'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{stamp}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'stamp')\n}}\n"
        )
    };
    let consumer_rune = |name: &str| {
        format!(
            "export const package = {{\n  name: '{name}'\n  version: '0.1.0'\n  deps: {{ build: {{ default: ['stampdep'] }}, runtime: [] }}\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  let stamped = (stamp | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($stamped)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' '{name}')\n}}\n"
        )
    };
    fs::write(runes.join("stampdep.rn"), stampdep_rune("stamp one")).unwrap();
    fs::write(runes.join("usesone.rn"), consumer_rune("usesone")).unwrap();
    fs::write(runes.join("usestwo.rn"), consumer_rune("usestwo")).unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add stale tome");
    assert_success(&run(root, &["install", "usesone"]), "install usesone");
    assert_eq!(stdout(&run_shim(root, "usesone")).trim(), "stamp one");
    let hash_before = state_text(root, "stampdep");

    // Same version, different rune content: the dep's content address changes.
    fs::write(runes.join("stampdep.rn"), stampdep_rune("stamp two")).unwrap();
    assert_success(&run(root, &["tome", "update", "staletome"]), "tome update");

    let install = run(root, &["install", "usestwo"]);
    assert_success(&install, "install usestwo after stampdep rune change");
    assert!(
        stdout(&install).contains("stampdep 0.1.0"),
        "the drifted dep must be re-realized: {}",
        stdout(&install)
    );
    assert_eq!(
        stdout(&run_shim(root, "usestwo")).trim(),
        "stamp two",
        "usestwo must be built against the re-realized stampdep"
    );
    assert_ne!(
        state_text(root, "stampdep"),
        hash_before,
        "stampdep's state record must carry the new content address"
    );

    // An untouched rune keeps being reused: reinstalling the consumer is a no-op.
    let again = run(root, &["install", "usestwo"]);
    assert_success(&again, "reinstall usestwo");
    assert!(
        stdout(&again).contains("already installed and up to date"),
        "an unchanged graph must not rebuild: {}",
        stdout(&again)
    );
}

#[test]
fn clean_reclaims_unused_build_dep_state_and_store_dirs() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'cleantome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("tool.rn"),
        "export const package = {\n  name: 'tool'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'tool\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tool')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { build: { default: ['tool'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add cleantome",
    );
    assert_success(&run(root, &["install", "app"]), "install app");
    assert!(
        store_has_package(root, "tool"),
        "the build dep is cached store-only after the install"
    );

    // Generations describe the linked environment only, so the cache is not pinned: one
    // clean removes the unused build dep's state *and* reclaims its store dir.
    let clean = run(root, &["clean", "-k", "1"]);
    assert_success(&clean, "clean");
    assert!(
        !store_has_package(root, "tool"),
        "clean must reclaim the unused build dep's store dir"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("tool.nuon")
            .exists(),
        "clean must sweep the unused build dep's state"
    );
    assert_eq!(
        stdout(&run_shim(root, "app")).trim(),
        "app",
        "the linked environment survives the clean"
    );
}

#[test]
fn store_only_packages_stay_out_of_lock_upgrade_and_profile() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'cachetome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let tooldep_rune = |version: &str| {
        format!(
            "export const package = {{\n  name: 'tooldep'\n  version: '{version}'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'tooldep {version}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tooldep')\n}}\n"
        )
    };
    fs::write(runes.join("tooldep.rn"), tooldep_rune("0.1.0")).unwrap();
    fs::write(
        runes.join("consumer.rn"),
        "export const package = {\n  name: 'consumer'\n  version: '0.1.0'\n  deps: { build: { default: ['tooldep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'consumer\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'consumer')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add cache tome");
    assert_success(&run(root, &["install", "consumer"]), "install consumer");

    // Store-only: cached for builds, invisible to the environment.
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("tooldep")
            .exists(),
        "build dep bin must not be linked into the profile"
    );
    // The lock must not carry a package *entry* for the build dep (consumer's own metadata
    // still names it in `build_deps`, which is fine — that is a recipe fact, not a pin).
    let lock = fs::read_to_string(root.join("state").join("grimoire.lock.nuon")).unwrap();
    assert!(
        !lock.contains("[tooldep,"),
        "build dep must not be recorded as a locked package: {lock}"
    );
    let list = stdout(&run(root, &["list", "--all"]));
    assert!(
        list.lines()
            .any(|l| l.contains("tooldep") && l.contains("store-only")),
        "list --all must mark the cached build dep store-only: {list}"
    );
    let default_list = stdout(&run(root, &["list"]));
    assert!(
        !default_list.contains("tooldep"),
        "default list shows only the linked environment, not store-only cache: {default_list}"
    );

    // A newer tooldep appears; a bare upgrade must leave the cache alone.
    fs::write(runes.join("tooldep.rn"), tooldep_rune("0.2.0")).unwrap();
    assert_success(&run(root, &["upgrade"]), "bare upgrade");
    assert!(
        state_text(root, "tooldep").contains("version: \"0.1.0\""),
        "bare upgrade must not touch store-only packages: {}",
        state_text(root, "tooldep")
    );

    // Naming it explicitly still upgrades it; it stays store-only.
    assert_success(&run(root, &["upgrade", "tooldep"]), "explicit upgrade");
    assert!(
        state_text(root, "tooldep").contains("version: \"0.2.0\""),
        "explicit upgrade must work: {}",
        state_text(root, "tooldep")
    );
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("tooldep")
            .exists(),
        "an upgraded build dep is still store-only"
    );

    // Requesting it by name promotes it into the environment — even with no install steps.
    let promote = run(root, &["install", "tooldep"]);
    assert_success(&promote, "promote tooldep");
    assert!(
        stdout(&promote).contains("marked as requested"),
        "promotion of an installed package must be announced: {}",
        stdout(&promote)
    );
    assert_eq!(
        stdout(&run_shim(root, "tooldep")).trim(),
        "tooldep 0.2.0",
        "a promoted package must surface in the new generation"
    );
}

#[test]
fn doctor_ignores_store_only_bin_collisions() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'contesttome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // The build dep ships a bin named like the app's — rust-stage0 vs rust in miniature.
    fs::write(
        runes.join("seedtool.rn"),
        "export const package = {\n  name: 'seedtool'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'seed app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { build: { default: ['seedtool'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add contesttome",
    );
    assert_success(&run(root, &["install", "app"]), "install app");

    // seedtool sits store-only with a colliding `app` bin: cache, not environment —
    // doctor must not flag it and prefer must not list it as contested.
    let doctor = run(root, &["doctor"]);
    assert_success(&doctor, "doctor");
    assert!(
        !stderr(&doctor).contains("provided by multiple packages"),
        "store-only bins must not be flagged contested: {}",
        stderr(&doctor)
    );
    let prefer = run(root, &["prefer"]);
    assert!(
        !stdout(&prefer).contains("contested (no preference set):"),
        "store-only bins must not appear contested in prefer: {}",
        stdout(&prefer)
    );
}

/// The announce line says what an install *implies* before the first fetch: missing (or
/// drifted) build deps that will realize along the way.
#[test]
fn install_announces_implied_build_deps_up_front() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'announcetome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("tooldep.rn"),
        "export const package = {\n  name: 'tooldep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'tooldep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'tooldep')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { build: { default: ['tooldep'] }, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add announcetome",
    );

    let install = run(root, &["install", "app"]);
    assert_success(&install, "install app");
    let combined = format!("{}{}", stdout(&install), stderr(&install));
    assert!(
        combined.contains("build dep to realize: tooldep"),
        "install must announce the implied build dep: {combined}"
    );
}
