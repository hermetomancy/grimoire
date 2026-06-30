//! Tome lifecycle, authoring scaffolds, disabled addenda, and news.

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
fn addendum_commands_are_disabled() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["addendum", "add", ".", "--ref", "main"]);
    assert_failure_contains(&add, "addenda are disabled", "addendum add is stubbed");
    assert!(
        !root.join("state").join("addendums").exists(),
        "disabled addendum command must not write state"
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

    // The index is fetched lazily at resolution time, not by `tome update`. An
    // unreachable binhost times out within its ~5s budget, warns loudly, and degrades to
    // source-only resolution instead of failing the command.
    let started = std::time::Instant::now();
    let install = run(root, &["install", "placeholder", "--dry-run"]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "index fetch against a hung binhost must give up within its budget, took {elapsed:?}"
    );
    assert_success(&install, "resolution degrades to source-only");
    let combined = format!("{}{}", stdout(&install), stderr(&install));
    assert!(
        combined.contains("binhost unreachable"),
        "the degrade must warn loudly: {combined}"
    );
    assert!(
        stdout(&install).contains("source rune"),
        "the plan should fall back to the source rune: {}",
        stdout(&install)
    );
}

#[test]
fn non_loopback_http_index_is_refused_before_fetch() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    fs::create_dir_all(tome_path.join("runes")).unwrap();
    fs::write(
        tome_path.join("runes").join("placeholder.rn"),
        "export const package = { name: 'placeholder' version: '0.1.0' }\n",
    )
    .unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'httppolicy'\n  packages: { repo: 'http://example.com/packages', format: 'http', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add httppolicy",
    );

    let install = run(root, &["install", "blockedpkg", "--dry-run"]);
    assert_failure_contains(
        &install,
        "refusing to fetch a package index over plain http",
        "non-loopback http index policy",
    );
    let combined = format!("{}{}", stdout(&install), stderr(&install));
    assert!(
        !combined.contains("binhost unreachable"),
        "plain-http policy errors must not be downgraded to a timeout/degrade warning: {combined}"
    );
}
