//! Tome and addendum lifecycle: add/update/remove, metadata patching, authoring scaffolds, news.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn tome_add_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // The tome names itself `core` in its manifest; `add` reads that name rather than
    // taking one on the command line.
    let tome = make_fake_tome();
    let tome_path = tome.path().to_str().unwrap();

    let add = run(root, &["tome", "add", tome_path, "--ref", "stable"]);
    assert_success(&add, "tome add core");

    let state_path = root.join("state").join("tomes").join("core.nuon");
    assert!(state_path.exists(), "tome state should exist");
    let state = fs::read_to_string(&state_path).unwrap();
    assert!(state.contains("name: core"), "state name: {state}");
    assert!(state.contains("ref: stable"), "state ref: {state}");

    let list = run(root, &["tome", "list"]);
    assert_success(&list, "tome list");
    let listed = stdout(&list);
    assert!(listed.contains("core"), "list includes name: {listed}");
    assert!(listed.contains("stable"), "list includes ref: {listed}");

    let duplicate = run(root, &["tome", "add", tome_path]);
    assert_failure_contains(&duplicate, "already exists", "reject duplicate tome");

    let remove = run(root, &["tome", "remove", "core"]);
    assert_success(&remove, "tome remove core");
    assert!(!state_path.exists(), "removed tome state should be gone");
}

#[test]
fn addendum_add_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: localpatches, patches: [] }\n",
    )
    .unwrap();

    let add = run(
        root,
        &[
            "addendum",
            "add",
            addendum_path.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add addendum");

    let state_path = root
        .join("state")
        .join("addendums")
        .join("localpatches.nuon");
    assert!(state_path.exists(), "addendum state should exist");
    let state = fs::read_to_string(&state_path).unwrap();
    assert!(state.contains("name: localpatches"), "state name: {state}");
    assert!(state.contains("ref: main"), "state ref: {state}");
    assert!(
        !state.contains("signer_pubkeys"),
        "unsigned addendum records no pin: {state}"
    );

    let list = run(root, &["addendum", "list"]);
    assert_success(&list, "addendum list");
    assert!(
        stdout(&list).contains("localpatches"),
        "list includes addendum: {}",
        stdout(&list)
    );

    let duplicate = run(root, &["addendum", "add", addendum_path.to_str().unwrap()]);
    assert_failure_contains(&duplicate, "already exists", "reject duplicate addendum");

    let remove = run(root, &["addendum", "remove", "localpatches"]);
    assert_success(&remove, "remove addendum");
    assert!(
        !state_path.exists(),
        "removed addendum state should be gone"
    );
}

#[test]
fn addendum_update_syncs_latest_changes() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let addendum = TempDir::new().unwrap();
    let addendum_path = addendum.path();
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [] }\n",
    )
    .unwrap();

    let add = run(
        root,
        &[
            "addendum",
            "add",
            addendum_path.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add, "add addendum");

    let update = run(root, &["addendum", "update", "updatable"]);
    assert_success(&update, "update addendum");

    // Modify the source to add a new patch.
    fs::write(
        addendum_path.join("addendum.nuon"),
        "{ name: updatable, patches: [{ package: testpkg, version: '0.2.0' }] }\n",
    )
    .unwrap();

    let update2 = run(root, &["addendum", "update", "updatable"]);
    assert_success(&update2, "update addendum after source change");

    let cached_manifest = root
        .join("cache")
        .join("addendums")
        .join("updatable")
        .join("addendum.nuon");
    let cached = fs::read_to_string(&cached_manifest).unwrap();
    assert!(
        cached.contains("testpkg"),
        "cached manifest should reflect updated patch: {cached}"
    );
    assert!(
        cached.contains("0.2.0"),
        "cached manifest should reflect updated version: {cached}"
    );
}

#[test]
fn addendum_staleness_warning_on_use() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = { name: 'staletome' packages: { repo: 'dist', format: 'local', index: 'index.nuon' } }\n",
    )
    .unwrap();
    fs::write(
        runes.join("stalepkg.rn"),
        "export const package = { name: 'stalepkg' version: '0.1.0' }\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  echo '#!/usr/bin/env sh' | save ($ctx.package_dir | path join 'bin' 'stalepkg')\n}\n",
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum = addendum.path();
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [] }\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "add tome",
    );
    assert_success(
        &run(
            root,
            &[
                "addendum",
                "add",
                addendum.to_str().unwrap(),
                "--ref",
                "main",
            ],
        ),
        "add addendum",
    );

    // First info call syncs the addendum.
    let first = run(root, &["info", "stalepkg"]);
    assert_success(&first, "info succeeds on first use");
    let first_text = format!("{}{}", stdout(&first), stderr(&first));
    assert!(
        !first_text.contains("re-syncing"),
        "fresh addendum should not re-sync: {first_text}"
    );

    // Modify the addendum source and simulate staleness by making the recorded commit diverge.
    fs::write(
        addendum.join("addendum.nuon"),
        "{ name: staleadd, patches: [{ package: stalepkg, version: '0.2.0' }] }\n",
    )
    .unwrap();
    let state_path = root.join("state").join("addendums").join("staleadd.nuon");
    let mut state = fs::read_to_string(&state_path).unwrap();
    state = state.replace('}', ", checked_commit: \"deadbeef\"}");
    fs::write(&state_path, state).unwrap();

    // Second info call detects the drifted cache and converges by re-syncing — and the
    // re-synced patch set is what gets applied, not the stale one.
    let second = run(root, &["info", "stalepkg"]);
    assert_success(&second, "info succeeds after re-sync");
    let second_text = format!("{}{}", stdout(&second), stderr(&second));
    assert!(
        second_text.contains("re-syncing"),
        "drifted addendum must announce the re-sync: {second_text}"
    );
    assert!(
        stdout(&second).contains("0.2.0"),
        "the re-synced addendum's patch must be applied: {}",
        stdout(&second)
    );
}

#[test]
fn tome_init_rune_authoring_flow() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Author a tome from scratch: scaffold the tome skeleton, then add a package rune to it.
    let workspace = TempDir::new().unwrap();
    let tome_dir = workspace.path().join("mytome");
    let tome_path = tome_dir.to_str().unwrap();

    let init = run(
        root,
        &[
            "tome",
            "init",
            "mytome",
            "--path",
            tome_path,
            "--description",
            "Authoring smoke test",
        ],
    );
    assert_success(&init, "tome init");
    assert!(tome_dir.join("tome.rn").exists(), "tome.rn scaffolded");
    assert!(tome_dir.join("runes").is_dir(), "runes/ scaffolded");
    assert!(tome_dir.join("dist").is_dir(), "dist/ scaffolded");
    assert!(
        fs::read_to_string(tome_dir.join(".gitignore"))
            .unwrap()
            .contains("/dist/"),
        ".gitignore ignores dist/"
    );

    let rune = run(root, &["tome", "rune", "widget", "--path", tome_path]);
    assert_success(&rune, "tome rune");
    assert!(
        tome_dir.join("runes").join("widget.rn").exists(),
        "widget rune scaffolded"
    );

    // The scaffolded tome is valid: it can be added and the rune builds and installs.
    let add = run(root, &["tome", "add", tome_path, "--ref", "main"]);
    assert_success(&add, "tome add authored");

    let install = run(root, &["install", "widget", "--from-source"]);
    assert_success(&install, "install authored widget");

    let widget = run_shim(root, "widget");
    assert_success(&widget, "run authored widget");
    assert_eq!(
        stdout(&widget).trim(),
        "widget is not implemented yet",
        "authored widget stub output"
    );
}

#[test]
fn addendum_patches_source_metadata_before_install() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'patchtome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let old_payload = runes.join("old.txt");
    let new_payload = runes.join("new.txt");
    fs::write(
        &old_payload,
        b"#!/usr/bin/env sh\nprintf 'old payload\\n'\n",
    )
    .unwrap();
    fs::write(
        &new_payload,
        b"#!/usr/bin/env sh\nprintf 'new payload\\n'\n",
    )
    .unwrap();
    let old_hash = sha256_file(&old_payload);
    let new_hash = sha256_file(&new_payload);

    fs::write(
        runes.join("patched.rn"),
        format!(
            "export const package = {{\n  name: 'patched'\n  version: '0.1.0'\n  summary: 'original summary'\n  sources: {{ main: {{ url: 'old.txt', sha256: '{old_hash}' }} }}\n  bins: {{default: {{ patched: 'bin/patched' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'patched')\n}}\n"
        ),
    )
    .unwrap();

    let addendum = TempDir::new().unwrap();
    let addendum = addendum.path();
    fs::write(
        addendum.join("addendum.nuon"),
        format!(
            "{{\n  name: patchset\n  patches: [\n    {{\n      tome: patchtome\n      package: patched\n      version: \"0.2.0\"\n      summary: \"patched summary\"\n      sources: {{ main: {{ url: \"new.txt\", sha256: \"{new_hash}\" }} }}\n    }}\n  ]\n}}\n"
        ),
    )
    .unwrap();

    let add_tome = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add_tome, "add patch tome");
    let add_patch = run(
        root,
        &[
            "addendum",
            "add",
            addendum.to_str().unwrap(),
            "--ref",
            "main",
        ],
    );
    assert_success(&add_patch, "add patch addendum");

    let info = run(root, &["info", "patched"]);
    assert_success(&info, "info patched package");
    let info_out = stdout(&info);
    assert!(
        info_out.contains("version: 0.2.0"),
        "info should show patched version: {info_out}"
    );
    assert!(
        info_out.contains("patched summary"),
        "info should show patched summary: {info_out}"
    );

    let install = run(root, &["install", "patched"]);
    assert_success(&install, "install patched package");
    let output = run_shim(root, "patched");
    assert_success(&output, "run patched package");
    assert_eq!(stdout(&output).trim(), "new payload");

    let state = fs::read_to_string(root.join("state").join("packages").join("patched.nuon"))
        .expect("patched package state");
    assert!(state.contains("version: \"0.2.0\""), "state: {state}");
    assert!(
        state.contains(&new_hash),
        "state records patched source hash: {state}"
    );

    let lock = fs::read_to_string(root.join("state").join("grimoire.lock.nuon"))
        .expect("lockfile after patched install");
    assert!(lock.contains("patchset"), "lock records addendum: {lock}");
}

#[test]
fn tome_news_surfaces_once_after_updates() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![dep_archive_entry(
        &dist,
        "newspkg",
        "0.1.0",
        &triple,
        "[]",
        "cafef00dcafef00d-nws",
    )];
    write_dep_tome(tome, "newscore", &entries);
    let news_dir = tome.join("news");
    fs::create_dir_all(&news_dir).unwrap();
    fs::write(
        news_dir.join("2026-01-01-alpha.md"),
        "# Alpha note\n\nbody-alpha\n",
    )
    .unwrap();

    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add newscore",
    );
    // First sync: the pre-existing backlog is marked seen silently, not dumped.
    let first = run(root, &["tome", "update", "newscore"]);
    assert_success(&first, "first tome update");
    assert!(
        !stdout(&first).contains("Alpha note"),
        "first sync must not dump the news backlog: {}",
        stdout(&first)
    );

    // A new item published after the add is printed exactly once.
    fs::write(
        news_dir.join("2026-06-10-beta.md"),
        "# Beta note\n\nbody-beta\n",
    )
    .unwrap();
    let second = run(root, &["tome", "update", "newscore"]);
    assert_success(&second, "second tome update");
    assert!(
        stdout(&second).contains("news [newscore] Beta note")
            && stdout(&second).contains("body-beta"),
        "new news item should print on update: {}",
        stdout(&second)
    );
    let third = run(root, &["tome", "update", "newscore"]);
    assert_success(&third, "third tome update");
    assert!(
        !stdout(&third).contains("Beta note"),
        "already-seen news must not repeat: {}",
        stdout(&third)
    );

    // `tome news --all` re-reads everything without disturbing the marker.
    let all = run(root, &["tome", "news", "newscore", "--all"]);
    assert_success(&all, "tome news --all");
    assert!(
        stdout(&all).contains("Alpha note") && stdout(&all).contains("Beta note"),
        "tome news --all should print every item: {}",
        stdout(&all)
    );
    let unread = run(root, &["tome", "news", "newscore"]);
    assert_success(&unread, "tome news");
    assert!(
        stdout(&unread).contains("no unread news"),
        "everything is seen: {}",
        stdout(&unread)
    );

    let state = fs::read_to_string(root.join("state").join("tomes").join("newscore.nuon")).unwrap();
    assert!(
        state.contains("2026-06-10-beta.md"),
        "seen marker must be recorded in tome state: {state}"
    );
}

/// An unreachable or hung binhost must fail the index fetch within its ~5s budget instead
/// of holding the command for connect-timeout-times-retries.
#[test]
fn hung_binhost_index_fetch_times_out_quickly() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    fs::create_dir_all(tome_path.join("runes")).unwrap();
    fs::write(
        tome_path.join("runes").join("placeholder.rn"),
        "export const package = {\n  name: 'placeholder'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n}\n",
    )
    .unwrap();
    let base = serve_black_hole();
    fs::write(
        tome_path.join("tome.rn"),
        format!(
            "export const tome = {{\n  name: 'hungtome'\n  packages: {{ repo: '{base}', format: 'http', index: 'index.nuon' }}\n}}\n"
        ),
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add hungtome",
    );

    // The index is fetched lazily at resolution time, not by `tome update`.
    let started = std::time::Instant::now();
    let install = run(root, &["install", "placeholder", "--dry-run"]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "index fetch against a hung binhost must fail within its budget, took {elapsed:?}"
    );
    assert!(
        !install.status.success(),
        "a hung binhost is an error, not a hang: {}",
        stdout(&install)
    );
    assert!(
        stderr(&install).contains(&format!("{base}/index.nuon")),
        "the error should name the index URL: {}",
        stderr(&install)
    );
}
