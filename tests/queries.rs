//! Read-only commands: prefer, notes, files/owns/provides, doctor, completions.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn command_parsing() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            &format!("--output={}", out.display()),
            "--quiet",
        ],
    );
    assert_success(&build, "build supports --output=value");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    assert!(archive.exists(), "--output=value archive should exist");

    // `--ref=value` form. A local tome lets `add` read its manifest name offline.
    let add = run(root, &["tome", "add", "./tome-example", "--ref=stable"]);
    assert_success(&add, "tome add supports --ref=value");
    let state = fs::read_to_string(root.join("state").join("tomes").join("tome-example.nuon"))
        .expect("tome-example state");
    assert!(state.contains("ref: stable"), "--ref=value state: {state}");

    let remove = run(root, &["tome", "remove", "tome-example"]);
    assert_success(&remove, "remove tome-example tome");

    let extra = run(root, &["build", "./tome-example/runes/hello.rn", "extra"]);
    assert_failure_contains(
        &extra,
        "unexpected argument 'extra' found",
        "reject extra build argument",
    );

    let unknown = run(root, &["doctor", "--unknown"]);
    assert_failure_contains(
        &unknown,
        "unexpected argument '--unknown' found",
        "reject unknown option",
    );

    let missing = run(root, &["build", "hello", "--output", "--quiet"]);
    assert_failure_contains(
        &missing,
        "a value is required for '--output <OUTPUT>'",
        "reject missing option value",
    );

    let bool_value = run(root, &["install", "hello", "--quiet=true"]);
    assert_failure_contains(
        &bool_value,
        "unexpected value 'true' for '--quiet'",
        "reject bool option value",
    );
}

#[test]
fn doctor_reports_health_and_problems() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    // A clean, empty root is healthy.
    let empty = run(root, &["doctor"]);
    assert_success(&empty, "doctor on empty root");
    let empty_out = stdout(&empty);
    assert!(
        empty_out.contains("installed packages: 0"),
        "doctor counts packages: {empty_out}"
    );
    assert!(
        empty_out.contains("managed core userland: build-env not installed"),
        "doctor reports build-env not installed: {empty_out}"
    );
    assert!(
        empty_out.contains("health: ok"),
        "empty health: {empty_out}"
    );

    let build = run(
        root,
        &[
            "build",
            "./tome-example/runes/hello.rn",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build hello");
    let archive = out.join(format!("hello-0.1.0-{}.tar.zst", target_triple()));
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install built archive");

    let healthy = run(root, &["doctor"]);
    assert_success(&healthy, "doctor after install");
    let healthy_out = stdout(&healthy);
    assert!(
        healthy_out.contains("installed packages: 1"),
        "doctor counts installed package: {healthy_out}"
    );
    assert!(
        healthy_out.contains("health: ok"),
        "installed health: {healthy_out}"
    );

    // Corrupt the install: the package's files vanish but its recorded state remains.
    fs::remove_dir_all(installed_store_dir(root, "hello").expect("hello store dir")).unwrap();
    let degraded = run(root, &["doctor"]);
    assert_success(&degraded, "doctor on degraded install");
    assert!(
        stdout(&degraded).contains("problem(s) found"),
        "doctor reports problem count: {}",
        stdout(&degraded)
    );
    assert!(
        stderr(&degraded).contains("files are missing"),
        "doctor diagnoses missing files on stderr: {}",
        stderr(&degraded)
    );
}

#[test]
fn prefer_resolves_contested_bins_between_installed_packages() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // Both packages declare the bin `tool`; their shims print their own package name so the
    // test can observe which provider owns the contested bin in the active generation.
    let entries = vec![
        capability_archive_entry(&dist, "alpha", "tool", &triple, "cafef00dcafef00d-alf"),
        capability_archive_entry(&dist, "beta", "tool", &triple, "cafef00dcafef00d-bet"),
    ];
    write_dep_tome(tome, "prefcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add prefcore",
    );
    assert_success(&run(root, &["tome", "update", "prefcore"]), "tome update");

    assert_success(&run(root, &["install", "alpha"]), "install alpha");
    // Installing the second claimant must fail with an actionable pointer at `grm prefer`.
    assert_failure_contains(
        &run(root, &["install", "beta"]),
        "grm prefer tool",
        "contested bin without preference",
    );

    // Preferring a package that doesn't provide the capability is rejected with providers.
    assert_failure_contains(
        &run(root, &["pkg", "prefer", "tool", "nosuchpkg"]),
        "does not provide `tool`",
        "prefer a non-provider",
    );

    assert_success(
        &run(root, &["pkg", "prefer", "tool", "beta"]),
        "prefer beta",
    );
    assert_success(
        &run(root, &["install", "beta"]),
        "install beta after prefer",
    );
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "beta",
        "preferred package must own the contested bin"
    );

    // Switching the preference relinks the active generation without reinstalling.
    assert_success(
        &run(root, &["pkg", "prefer", "tool", "alpha"]),
        "prefer alpha",
    );
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "alpha",
        "switching the preference must flip the bin"
    );

    // The listing shows the recorded choice.
    let listing = run(root, &["pkg", "prefer"]);
    assert_success(&listing, "prefer listing");
    assert!(
        stdout(&listing).contains("tool\talpha"),
        "listing should show the preference: {}",
        stdout(&listing)
    );

    // Clearing the preference is refused while the bin is still contested; once only one
    // claimant remains it succeeds.
    assert_failure_contains(
        &run(root, &["pkg", "prefer", "--unset", "tool"]),
        "would leave it contested",
        "unset while contested",
    );
    assert_success(&run(root, &["remove", "beta"]), "remove beta");
    assert_success(
        &run(root, &["pkg", "prefer", "--unset", "tool"]),
        "unset after remove",
    );
    assert_eq!(
        stdout(&run_shim(root, "tool")).trim(),
        "alpha",
        "sole remaining claimant owns the bin"
    );
}

#[test]
fn solver_resolves_capability_dependency_through_preference() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // `consumer` depends on the capability `tool`, provided by both alpha and beta. Without
    // a preference the pick is arbitrary; with one it must be the preferred provider.
    let entries = vec![
        capability_archive_entry(&dist, "alpha", "tool", &triple, "cafef00dcafef00d-alf"),
        capability_archive_entry(&dist, "beta", "tool", &triple, "cafef00dcafef00d-bet"),
        dep_archive_entry(
            &dist,
            "consumer",
            "0.1.0",
            &triple,
            "[\"tool\"]",
            "cafef00dcafef00d-con",
        ),
    ];
    write_dep_tome(tome, "capcore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add capcore",
    );
    assert_success(&run(root, &["tome", "update", "capcore"]), "tome update");

    assert_success(
        &run(root, &["pkg", "prefer", "tool", "beta"]),
        "prefer beta",
    );
    assert_success(&run(root, &["install", "consumer"]), "install consumer");
    assert!(
        root.join("state")
            .join("packages")
            .join("beta.nuon")
            .exists(),
        "preferred provider must be installed"
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("alpha.nuon")
            .exists(),
        "non-preferred provider must not be installed"
    );
}

#[test]
fn post_install_notes_surface_and_replay() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    // A binary archive whose embedded metadata carries static notes.
    let pkg = "notepkg";
    let archive_name = format!("{pkg}-0.1.0-{triple}.tar.zst");
    let package_nuon = format!(
        "{{format: 1, name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{pkg}: \"bin/{pkg}\"}}}}, deps: {{ runtime: [] }}, notes: [\"run notepkg --init once\"]}}\n",
        fake_store_basename_with_hash(pkg, "0.1.0", "cafef00dcafef00d-note")
    );
    fs::create_dir_all(&dist).unwrap();
    let archive_path = dist.join(&archive_name);
    let mut builder = open_archive(&archive_path);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{pkg}"),
        format!("#!/usr/bin/env sh\nprintf '{pkg}\\n'\n").as_bytes(),
        0o755,
    );
    finish_archive(builder);
    let hash = sha256_file(&archive_path);
    let entries = vec![format!(
        "    \"cafef00dcafef00d-note\": {{ name: \"{pkg}\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}"
    )];
    write_dep_tome(tome, "notecore", &entries);
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add notecore",
    );
    assert_success(&run(root, &["tome", "update", "notecore"]), "tome update");

    let install = run(root, &["install", pkg]);
    assert_success(&install, "install notepkg");
    let out = stdout(&install);
    assert!(
        out.contains("notes for notepkg:") && out.contains("run notepkg --init once"),
        "install should print the package notes after committing: {out}"
    );

    assert!(
        state_text(root, pkg).contains("run notepkg --init once"),
        "notes must persist in package state: {}",
        state_text(root, pkg)
    );
    let info = run(root, &["info", pkg]);
    assert_success(&info, "info notepkg");
    assert!(
        stdout(&info).contains("notes:") && stdout(&info).contains("run notepkg --init once"),
        "info should replay the notes: {}",
        stdout(&info)
    );
}

#[test]
fn build_returned_notes_merge_with_static_notes() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // The rune declares a static note and its build returns a dynamic one; both must reach
    // the installed state and the post-install report.
    let rune_dir = TempDir::new().unwrap();
    let rune = rune_dir.path().join("dualnote.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'dualnote'\n  version: '0.1.0'\n  notes: ['static note']\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'dualnote\\\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'dualnote')\n  { bins: { default: { dualnote: 'bin/dualnote' } }, notes: ['dynamic note'] }\n}\n",
    )
    .unwrap();

    let install = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&install, "install dualnote from rune");
    let out = stdout(&install);
    assert!(
        out.contains("static note") && out.contains("dynamic note"),
        "both static and build-returned notes should print: {out}"
    );
    let state = state_text(root, "dualnote");
    assert!(
        state.contains("static note") && state.contains("dynamic note"),
        "both notes must persist in state: {state}"
    );
}

#[test]
fn files_owns_and_provides_resolve_package_contents() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let triple = target_triple();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let dist = tome.join("dist");

    let entries = vec![
        dep_archive_entry(
            &dist,
            "app",
            "0.1.0",
            &triple,
            "[\"lib\"]",
            "cafef00dcafef00d-app",
        ),
        dep_archive_entry(&dist, "lib", "0.1.0", &triple, "[]", "cafef00dcafef00d-lib"),
    ];
    write_dep_tome(tome, "filescore", &entries);
    // A source rune whose bin name differs from the package name: a capability (`ex`)
    // resolvable through `grm provides` while the package is still only available.
    fs::write(
        tome.join("runes").join("gex.rn"),
        "export const package = {\n  name: 'gex'\n  version: '0.3.0'\n  bins: {default: {ex: 'bin/ex'}}\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'ex\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'ex')\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add filescore",
    );
    assert_success(&run(root, &["tome", "update", "filescore"]), "tome update");
    assert_success(&run(root, &["install", "app"]), "install app");

    // files: relative paths of everything the package staged into the store.
    let files = run(root, &["pkg", "files", "app"]);
    assert_success(&files, "files app");
    let listed = stdout(&files);
    assert!(
        listed.contains("bin/app"),
        "files should list the package contents: {listed}"
    );
    assert!(
        !listed.contains(".grimoire/"),
        "files must not list grimoire's own store metadata (.grimoire/*): {listed}"
    );
    assert_failure_contains(
        &run(root, &["pkg", "files", "nosuchpkg"]),
        "is not installed",
        "files for unknown package",
    );

    // owns: a profile bin path (through the `current` symlink) and a raw store path.
    let profile_bin = root
        .join("profiles")
        .join("current")
        .join("bin")
        .join("app");
    let owns = run(root, &["pkg", "owns", profile_bin.to_str().unwrap()]);
    assert_success(&owns, "owns profile bin");
    assert!(
        stdout(&owns).contains("app\t0.1.0"),
        "profile bin should resolve to app: {}",
        stdout(&owns)
    );

    let store_file = installed_store_dir(root, "lib").unwrap().join("bin/lib");
    let owns_store = run(root, &["pkg", "owns", store_file.to_str().unwrap()]);
    assert_success(&owns_store, "owns store path");
    assert!(
        stdout(&owns_store).contains("lib\t0.1.0"),
        "store path should resolve to lib: {}",
        stdout(&owns_store)
    );

    let foreign = root.join("not-owned.txt");
    fs::write(&foreign, "hello").unwrap();
    assert_failure_contains(
        &run(root, &["pkg", "owns", foreign.to_str().unwrap()]),
        "is not owned by any installed package",
        "owns on a foreign path",
    );

    // provides: installed packages report `installed`, rune-only capabilities `available`.
    let provides_app = run(root, &["pkg", "provides", "app"]);
    assert_success(&provides_app, "provides app");
    assert!(
        stdout(&provides_app).contains("app\t0.1.0\tinstalled"),
        "installed package should be marked installed: {}",
        stdout(&provides_app)
    );

    let provides_ex = run(root, &["pkg", "provides", "ex"]);
    assert_success(&provides_ex, "provides ex");
    assert!(
        stdout(&provides_ex).contains("gex\t0.3.0\tavailable"),
        "capability from an uninstalled rune should be available: {}",
        stdout(&provides_ex)
    );

    assert_failure_contains(
        &run(root, &["pkg", "provides", "nothing-has-this"]),
        "nothing provides",
        "provides with no providers",
    );
}

#[test]
fn completions_and_man_render_from_cli_definition() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // Bash completion script for `grm` — the script ships the names of our subcommands so
    // a shell user can tab through them. Smoke check that a few of ours made it in.
    let bash = run(root, &["completions", "bash"]);
    assert_success(&bash, "completions bash");
    let bash_out = stdout(&bash);
    assert!(
        bash_out.contains("_grm") || bash_out.contains("_grm()"),
        "bash completion defines a function: {bash_out}"
    );
    for subcommand in ["install", "remove", "tome", "generation", "pkg"] {
        assert!(
            bash_out.contains(subcommand),
            "completion enumerates `{subcommand}`"
        );
    }

    // `man` writes one `.1` file per subcommand plus a `grm.1` root page.
    let out = TempDir::new().unwrap();
    let out = out.path().join("man");
    let man = run(root, &["man", "--output", out.to_str().unwrap()]);
    assert_success(&man, "man --output");
    let root_page = fs::read_to_string(out.join("grm.1")).expect("read grm.1");
    assert!(
        root_page.contains(".TH grm"),
        "grm.1 is a troff page with the expected title header: {root_page:.200}"
    );
    for sub in ["install", "remove", "clean", "pkg", "generation"] {
        let path = out.join(format!("grm-{sub}.1"));
        assert!(
            path.exists(),
            "man page for `{sub}` should exist at {path:?}"
        );
    }
}

#[test]
fn rune_outside_the_command_subset_fails_at_query_time() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `str join` is not in the rune command subset. With the bare core-language parser it
    // reads as an innocent external command and only explodes at build time — after build
    // deps were fetched and built. Const extraction now parses with the full rune command
    // context, so the violation surfaces at `grm info`.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'subsettome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("badpkg.rn"),
        "export const package = {\n  name: 'badpkg'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  let x = ([\"a\" \"b\"] | str join \"-\")\n  mkdir ($ctx.package_dir | path join 'bin')\n}\n",
    )
    .unwrap();

    // The violation surfaces at the earliest gate: adding the tome validates every rune
    // with the full command set, so the bad rune never enters the catalog at all.
    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert!(
        !add.status.success(),
        "tome add must reject a rune outside the command subset: {}",
        stdout(&add)
    );
    assert!(
        stderr(&add).contains("could not parse") && stderr(&add).contains("badpkg.rn"),
        "the failure must be a parse error naming the rune: {}",
        stderr(&add)
    );
}

#[test]
fn rune_metadata_cache_populates_and_self_heals() {
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
    fs::write(
        runes.join("cachedpkg.rn"),
        "export const package = {\n  name: 'cachedpkg'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add cachetome",
    );

    let info = run(root, &["info", "cachedpkg"]);
    assert_success(&info, "info populates the cache");
    let cache_root = root.join("cache").join("rune-meta");
    let entries: Vec<_> = walk_files(&cache_root);
    assert!(
        !entries.is_empty(),
        "rune-meta cache must be populated after a metadata read"
    );

    // Corrupt every cache entry: reads must fall back to a fresh parse and overwrite.
    for entry in &entries {
        fs::write(entry, b"not nuon at all").unwrap();
    }
    let again = run(root, &["info", "cachedpkg"]);
    assert_success(&again, "info self-heals a corrupt cache");
    assert!(
        stdout(&again).contains("cachedpkg"),
        "metadata must still be correct: {}",
        stdout(&again)
    );
}

fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn upstream_version_is_displayed_by_info() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'uptome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // openssh-style: upstream `9.9p1` normalized to semver `9.9.1`.
    fs::write(
        runes.join("ssh.rn"),
        "export const package = {\n  name: 'ssh'\n  version: '9.9.1'\n  upstream_version: '9.9p1'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n}\n",
    )
    .unwrap();
    assert_success(
        &run(
            root,
            &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add uptome",
    );

    let info = run(root, &["info", "ssh"]);
    assert_success(&info, "info ssh");
    assert!(
        stdout(&info).contains("version: 9.9.1")
            && stdout(&info).contains("upstream version: 9.9p1"),
        "info must show both the ordered and the upstream version: {}",
        stdout(&info)
    );
}

/// `grm owns` on a `grm prefer`-contested bin reports the provider actually linked into the
/// generation — the package the profile symlink targets — not every package that declares the
/// name. The bin is an absolute symlink into the store, so `canonicalize` resolves it to the
/// winner's store path.
#[test]
fn owns_reports_only_the_linked_provider_for_a_contested_bin() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let tome = TempDir::new().unwrap();
    let tome_path = tome.path();
    let runes = tome_path.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome_path.join("tome.rn"),
        "export const tome = {\n  name: 'awktome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    // gawk and mawk both declare an `awk` bin; `grm prefer` settles the contest.
    for provider in ["gawk", "mawk"] {
        fs::write(
            runes.join(format!("{provider}.rn")),
            format!(
                "export const package = {{\n  name: '{provider}'\n  version: '0.1.0'\n  bins: {{ default: {{ {provider}: 'bin/{provider}', awk: 'bin/{provider}' }} }}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{provider}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' '{provider}')\n}}\n"
            ),
        )
        .unwrap();
    }
    assert_success(
        &run(
            root,
            &["tome", "add", tome_path.to_str().unwrap(), "--ref", "main"],
        ),
        "tome add awktome",
    );
    assert_success(
        &run(root, &["pkg", "prefer", "awk", "gawk"]),
        "prefer awk gawk",
    );
    assert_success(
        &run(root, &["install", "gawk", "mawk"]),
        "install gawk + mawk",
    );

    let awk_bin = root
        .join("profiles")
        .join("current")
        .join("bin")
        .join("awk");
    let owns = run(root, &["pkg", "owns", awk_bin.to_str().unwrap()]);
    assert_success(&owns, "owns contested awk bin");
    let out = stdout(&owns);
    assert!(
        out.contains("gawk"),
        "owns must report the linked provider gawk: {out}"
    );
    assert!(
        !out.contains("mawk"),
        "owns must not report the losing declarant mawk: {out}"
    );
}
