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

    // stampdep remains installed — state, package dir, and shim — because it is part of the
    // managed build environment now.
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
        root.join("profiles")
            .join("current")
            .join("bin")
            .join("stamp")
            .exists(),
        "stampdep shim should remain"
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
