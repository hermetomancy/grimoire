//! Resolver-level conflict backtracking: the solver picks a compatible version/provider instead of
//! emitting a plan that is refused later.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use support::*;
use tempfile::TempDir;

/// Builds a binary-only archive whose package.nuon carries optional runtime deps and
/// `conflicts`/`replaces` metadata.
#[allow(clippy::too_many_arguments)]
fn make_archive(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    store_hash: &str,
    runtime_deps: &str,
    conflicts: &[&str],
    replaces: &[&str],
) -> PathBuf {
    let conflicts = if conflicts.is_empty() {
        String::new()
    } else {
        format!(
            ", conflicts: [{}]",
            conflicts
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let replaces = if replaces.is_empty() {
        String::new()
    } else {
        format!(
            ", replaces: [{}]",
            replaces
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", store_path: \"{store_hash}-{name}-{version}\", bins: {{default: {{{name}: \"bin/{name}\"}}}}, deps: {{ runtime: {runtime_deps} }}"
    ) + &conflicts
        + &replaces
        + "}\n";
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        format!("#!/usr/bin/env sh\nprintf '{name}-{version}\\n'\n").as_bytes(),
        0o755,
    );
    finish_archive(builder);
    path.to_path_buf()
}

#[allow(clippy::too_many_arguments)]
fn archive_entry(
    dist: &Path,
    name: &str,
    version: &str,
    triple: &str,
    store_hash: &str,
    runtime_deps: &str,
    conflicts: &[&str],
    replaces: &[&str],
) -> String {
    let archive_name = format!("{name}-{version}-{triple}.tar.zst");
    let archive = make_archive(
        &dist.join(&archive_name),
        name,
        version,
        triple,
        store_hash,
        runtime_deps,
        conflicts,
        replaces,
    );
    let hash = sha256_file(&archive);
    format!(
        "    \"{store_hash}\": {{ name: \"{name}\", version: \"{version}\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: {runtime_deps}}}"
    )
}

fn setup_tome(tome: &Path) {
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'backtracktome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
}

#[test]
fn resolver_backtracks_to_compatible_version() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let triple = target_triple();
    let dist = tome.join("dist");

    setup_tome(tome);

    let legacy_entry =
        dep_archive_entry(&dist, "legacy", "1.0.0", &triple, "[]", "cafe0001cafe0001");
    let app1_entry = dep_archive_entry(&dist, "app", "1.0.0", &triple, "[]", "cafe0002cafe0002");
    let app2_entry = archive_entry(
        &dist,
        "app",
        "2.0.0",
        &triple,
        "cafe0003cafe0003",
        "[]",
        &["legacy"],
        &[],
    );
    let suite_entry = archive_entry(
        &dist,
        "suite",
        "1.0.0",
        &triple,
        "cafe0004cafe0004",
        "[{name: \"app\", version: \">=1.0.0,<2.0.0\"}]",
        &[],
        &[],
    );

    write_dep_index(tome, &[legacy_entry, app1_entry, app2_entry, suite_entry]);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add backtracktome",
    );

    assert_success(&run(root, &["install", "legacy"]), "install legacy");
    let legacy_shim = run_shim(root, "legacy");
    assert_success(&legacy_shim, "run legacy");
    assert_eq!(stdout(&legacy_shim).trim(), "legacy-1.0.0");

    // suite requires app in [1.0.0, 2.0.0); app 2.0.0 conflicts with installed legacy, so the
    // resolver must backtrack to app 1.0.0 instead of producing a refused plan.
    let install = run(root, &["install", "suite"]);
    assert_success(
        &install,
        "install suite backtracks app to compatible version",
    );

    let suite_shim = run_shim(root, "suite");
    assert_success(&suite_shim, "run suite");
    assert_eq!(stdout(&suite_shim).trim(), "suite-1.0.0");

    let app_shim = run_shim(root, "app");
    assert_success(&app_shim, "run app");
    assert_eq!(
        stdout(&app_shim).trim(),
        "app-1.0.0",
        "resolver should have picked the non-conflicting app 1.0.0"
    );
}

#[test]
fn replaces_allows_coexistence_at_resolution_time() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let triple = target_triple();
    let dist = tome.join("dist");

    setup_tome(tome);

    let legacy_entry =
        dep_archive_entry(&dist, "legacy", "1.0.0", &triple, "[]", "cafe0001cafe0001");
    let app2_entry = archive_entry(
        &dist,
        "app",
        "2.0.0",
        &triple,
        "cafe0003cafe0003",
        "[]",
        &["legacy"],
        &["legacy"],
    );
    let suite_entry = archive_entry(
        &dist,
        "suite",
        "1.0.0",
        &triple,
        "cafe0004cafe0004",
        "[{name: \"app\", version: \">=2.0.0\"}]",
        &[],
        &[],
    );

    write_dep_index(tome, &[legacy_entry, app2_entry, suite_entry]);

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add replacetome",
    );
    assert_success(&run(root, &["install", "legacy"]), "install legacy");

    // suite requires app >=2.0.0. app 2.0.0 conflicts with legacy but also replaces it, so the
    // resolver should allow the plan and the install should migrate legacy to app.
    let install = run(root, &["install", "suite"]);
    assert_success(&install, "install suite replaces legacy");

    let suite_shim = run_shim(root, "suite");
    assert_success(&suite_shim, "run suite");
    assert_eq!(stdout(&suite_shim).trim(), "suite-1.0.0");

    let app_shim = run_shim(root, "app");
    assert_success(&app_shim, "run app");
    assert_eq!(stdout(&app_shim).trim(), "app-2.0.0");
}
