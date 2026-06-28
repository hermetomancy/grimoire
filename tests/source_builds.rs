//! Building packages from source runes: extraction, build contracts, failure surfacing.

mod support;

use std::fs;

use support::*;
use tempfile::TempDir;

#[test]
fn build_respects_musl_target() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

    let rune_path = root.join("test.rn");
    fs::write(
        &rune_path,
        "export const package = { name: 'testpkg' version: '0.1.0' }\n\nexport def build [ctx] {\n  echo $ctx.target | save ($ctx.package_dir | path join 'target.txt')\n}\n",
    ).unwrap();

    let build = run(
        root,
        &[
            "build",
            rune_path.to_str().unwrap(),
            &format!("--output={}", out.display()),
            "--target",
            "linux-x86_64-musl",
            "--bootstrap",
        ],
    );
    assert_success(&build, "build with musl target");

    let archive = out.join("testpkg-0.1.0-linux-x86_64-musl.tar.zst");
    assert!(archive.exists(), "musl archive should exist: {archive:?}");

    let target_text = archive_member_text(&archive, "target.txt");
    assert_eq!(
        target_text.trim(),
        "linux-x86_64-musl",
        "build context target"
    );
}

#[test]
fn build_rejects_invalid_rune_targets() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let rune = root.join("badtarget.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'badtarget'\n  version: '0.1.0'\n  targets: ['linux']\n}\n\nexport def build [ctx] {}\n",
    )
    .unwrap();

    let build = run(root, &["build", rune.to_str().unwrap()]);
    assert_failure_contains(
        &build,
        "target `linux` is not a supported triple",
        "invalid rune target",
    );
}

#[test]
fn source_root_dry_run_refuses_linked_conflicts() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let target = target_triple();

    let archive = make_versioned_archive(
        &root
            .join("archives")
            .join(format!("oldpkg-0.1.0-{target}.tar.zst")),
        "oldpkg",
        "0.1.0",
        &target,
        "#!/usr/bin/env sh\nprintf 'old\\n'\n",
    );
    assert_success(
        &run(root, &["install", archive.to_str().unwrap(), "--force"]),
        "install conflicting package",
    );

    let rune = root.join("newpkg.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'newpkg'\n  version: '0.1.0'\n  conflicts: ['oldpkg']\n  sources: {}\n  deps: { build: {} runtime: [] }\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'new\\n'\" | save ($ctx.package_dir | path join 'bin' 'newpkg')\n}\n",
    )
    .unwrap();

    let dry_run = run(root, &["install", rune.to_str().unwrap(), "--dry-run"]);
    assert_failure_contains(
        &dry_run,
        "conflicts with installed `oldpkg`",
        "source dry-run conflict",
    );
}

#[test]
fn example_tome_checksummed_source() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    let add = run(root, &["tome", "add", "./tome-example", "--ref", "main"]);
    assert_success(&add, "tome add example");

    // `bundle` fetches and verifies a checksummed source before building from it.
    let install = run(root, &["install", "bundle"]);
    assert_success(&install, "install bundle");

    let bundle = run_shim(root, "bundle");
    assert_success(&bundle, "run bundle");
    assert_eq!(
        stdout(&bundle).trim(),
        "grimoire example payload",
        "bundle output reflects the verified source"
    );
}

#[test]
fn source_tar_zst_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.zst", TestArchiveKind::TarZst);
}

#[test]
fn source_tar_gz_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.gz", TestArchiveKind::TarGz);
}

#[test]
fn source_tar_xz_is_extracted_into_build_context() {
    source_archive_is_extracted_into_build_context("payload.tar.xz", TestArchiveKind::TarXz);
}

#[test]
fn source_build_supports_configure_make_install_contract() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    let sources = tome.join("sources");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(&sources).unwrap();
    let dist = tome.join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'realbuild'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    let source_archive = runes.join("realpkg-1.0.0.tar.zst");
    let mut builder = open_archive(&source_archive);
    append_file(
        &mut builder,
        "realpkg-1.0.0/message.txt",
        b"built from source\n",
        0o644,
    );
    append_file(
        &mut builder,
        "realpkg-1.0.0/configure",
        br#"#!/usr/bin/env sh
set -eu
prefix=
source_dir=${SOURCE_DIR:-.}
for arg in "$@"; do
  case "$arg" in
    --prefix=*) prefix=${arg#--prefix=} ;;
  esac
done
if [ -z "$prefix" ]; then
  echo "missing --prefix" >&2
  exit 2
fi
printf '%s\n' "$prefix" > configured-prefix.txt
{
  printf '%s\n' '#!/usr/bin/env sh'
  printf '%s\n' 'set -eu'
  printf '%s\n' "IFS= read -r message < '$source_dir/message.txt'"
  printf '%s\n' 'printf "%s\n" "$message" > built-message.txt'
} > build.sh
{
  printf '%s\n' '#!/usr/bin/env sh'
  printf '%s\n' 'set -eu'
  printf '%s\n' 'destdir=$1'
  printf '%s\n' 'IFS= read -r message < built-message.txt'
  printf '%s\n' 'IFS= read -r configured < configured-prefix.txt'
  printf '%s\n' '{'
  printf '%s\n' "  printf '%s\n' '#!/usr/bin/env sh'"
  printf '%s\n' "  printf \"printf '%%s\\\\n' '%s via %s'\\n\" \"\$message\" \"\$configured\""
  printf '%s\n' '} > "$destdir$configured/realpkg"'
} > install.sh
"#,
        0o755,
    );
    finish_archive(builder);
    let source_hash = sha256_file(&source_archive);

    let minimake_archive_name = format!("minimake-0.1.0-{}.tar.zst", target_triple());
    let minimake_archive = dist.join(&minimake_archive_name);
    let mut builder = open_archive(&minimake_archive);
    let minimake_metadata = format!(
        "{{format: 1, name: \"minimake\", version: \"0.1.0\", target: \"{}\", store_path: \"{}\", bins: {{default: {{make: \"bin/make\"}}}}}}\n",
        target_triple(),
        fake_store_basename("minimake", "0.1.0")
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        minimake_metadata.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        "bin/make",
        b"#!/usr/bin/env sh\nset -eu\ntarget=${1:-all}\ncase \"$target\" in\n  all) sh ./build.sh ;;\n  install) destdir=\"\"; for arg in \"$@\"; do case \"$arg\" in DESTDIR=*) destdir=${arg#DESTDIR=} ;; esac; done; if [ -z \"$destdir\" ]; then echo 'missing DESTDIR' >&2; exit 2; fi; sh ./install.sh \"$destdir\" ;;\n  *) echo \"unsupported target: $target\" >&2; exit 2 ;;\nesac\n",
        0o755,
    );
    finish_archive(builder);
    let minimake_hash = sha256_file(&minimake_archive);
    fs::write(
        dist.join("index.nuon"),
        format!(
            "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"minimake\", version: \"0.1.0\", target: \"{}\", archive: \"{minimake_archive_name}\", archive_hash: \"{minimake_hash}\", runtime_deps: []}}\n  }}\n}}\n",
            target_triple()
        ),
    )
    .unwrap();

    fs::write(
        runes.join("realpkg.rn"),
        format!(
            "export const package = {{\n  name: 'realpkg'\n  version: '1.0.0'\n  sources: {{ main: {{ url: 'realpkg-1.0.0.tar.zst', sha256: '{source_hash}' }} }}\n  deps: {{ build: {{ default: ['minimake'] }}, runtime: [] }}\n  bins: {{default: {{ realpkg: 'realpkg' }}}}\n}}\n\nexport def build [ctx] {{\n  let source_dir = ($ctx.sources.main.dir | path join 'realpkg-1.0.0')\n  let build_dir = ($ctx.package_dir | path join 'build')\n  let staged_prefix = ($ctx.package_dir | path join ($ctx.prefix | str replace -r '^/' ''))\n  mkdir $build_dir\n  mkdir $staged_prefix\n  let result = (sh -c $\"cd '($build_dir)' && SOURCE_DIR='($source_dir)' '($source_dir)/configure' --prefix='($ctx.prefix)' && make && make install DESTDIR='($ctx.package_dir)'\" | complete)\n  if $result.exit_code != 0 {{\n    error make {{ msg: $result.stderr }}\n  }}\n}}\n"
        ),
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add real build tome");
    let install = run(root, &["install", "realpkg"]);
    assert_success(&install, "install configure/make style source package");

    let built_archive = root
        .join("cache")
        .join("builds")
        .join(format!("realpkg-1.0.0-{}.tar.zst", target_triple()));
    let package_metadata = archive_member_text(&built_archive, ".grimoire/package.nuon");
    assert!(
        package_metadata.contains("store_path"),
        "built archive metadata should record its final store path: {package_metadata}"
    );
    assert!(
        package_metadata.contains("-realpkg-1.0.0")
            && !package_metadata.contains("packages/realpkg"),
        "store path should be the content-addressed store basename, not a packages/ dir: {package_metadata}"
    );

    let output = run_shim(root, "realpkg");
    assert_success(&output, "run realpkg");
    let line = stdout(&output);
    assert!(
        line.starts_with("built from source via "),
        "realpkg output should reflect configured source build: {line}"
    );
    assert!(
        line.contains("/store/") && line.trim_end().ends_with("-realpkg-1.0.0"),
        "ctx.prefix should point at the final store path, not the temporary staging dir: {line}"
    );
    assert!(
        !line.trim_end().ends_with("/package"),
        "ctx.prefix should not leak the temporary staging package dir: {line}"
    );
}

#[test]
fn source_build_failure_surfaces_diagnostic_and_output_tail() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'brokentome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    // A build whose external command writes a recognizable error to stderr and exits non-zero —
    // as the build's *final* statement. This must abort the build (surfacing the exit code and the
    // output tail), not silently succeed and pack a broken archive. Regression test for a failing
    // trailing external being swallowed because the result was never drained / exit-checked.
    fs::write(
        runes.join("brokenpkg.rn"),
        "export const package = {\n  name: 'brokenpkg'\n  version: '1.0.0'\n  sources: {}\n  deps: { build: {}, runtime: [] }\n \n}\n\nexport def build [ctx] {\n  sh -c \"echo 'configure: error: no acceptable C compiler found in $PATH' >&2; exit 1\"\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add broken build tome");

    let install = run(root, &["install", "brokenpkg"]);
    // The Nushell diagnostic names the exit code instead of the opaque default message...
    assert_failure_contains(
        &install,
        "external command exited with code 1",
        "build failure reports the exit code",
    );
    // ...and the build's own stderr (the real cause) is carried up in the output tail.
    assert_failure_contains(
        &install,
        "no acceptable C compiler found",
        "build failure surfaces the underlying build output",
    );
}

#[test]
fn build_install_list_remove() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();

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
    assert!(archive.exists(), "built archive should exist");

    let install = run(root, &["install", archive.to_str().unwrap(), "--force"]);
    assert_success(&install, "install built archive");

    let state = fs::read_to_string(root.join("state").join("packages").join("hello.nuon")).unwrap();
    let expected = format!("archive_hash: \"{}\"", sha256_file(&archive));
    assert!(state.contains(&expected), "installed archive hash: {state}");

    let hello = run_shim(root, "hello");
    assert_success(&hello, "run installed hello");
    assert_eq!(
        stdout(&hello).trim(),
        "hello from grimoire",
        "installed shim output"
    );

    let list = run(root, &["list"]);
    assert_success(&list, "list installed packages");
    assert!(
        stdout(&list).contains("hello"),
        "list includes package name"
    );

    // `remove --dry-run` previews the removal without touching state.
    let dry = run(root, &["remove", "hello", "--dry-run"]);
    assert_success(&dry, "remove dry-run");
    assert!(
        stdout(&dry).contains("- hello"),
        "remove dry-run names the package: {}",
        stdout(&dry)
    );
    assert!(
        root.join("state")
            .join("packages")
            .join("hello.nuon")
            .exists(),
        "dry-run must not remove anything"
    );

    let remove = run(root, &["remove", "hello"]);
    assert_success(&remove, "remove installed package");
    assert!(
        !root
            .join("profiles")
            .join("current")
            .join("bin")
            .join("hello")
            .exists(),
        "removed shim should be gone"
    );
    assert!(
        store_has_package(root, "hello"),
        "removal is store-preserving: the store dir stays until `grm clean`"
    );

    // `clean --dry-run` reports what would be reclaimed without reclaiming it.
    let clean_dry = run(root, &["clean", "--dry-run", "--keep", "1"]);
    assert_success(&clean_dry, "clean dry-run");
    assert!(
        stdout(&clean_dry).contains("would reclaim")
            || stdout(&clean_dry).contains("nothing to clean"),
        "clean dry-run summarises: {}",
        stdout(&clean_dry)
    );
    assert!(
        store_has_package(root, "hello"),
        "clean dry-run must not delete store paths"
    );
}

#[test]
fn build_fetches_and_verifies_sources() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    // A local source artifact resolved relative to the rune directory; no network needed.
    let payload = src.join("payload.txt");
    fs::write(&payload, b"verified source payload\n").unwrap();
    let payload_hash = sha256_file(&payload);

    let rune = src.join("srctool.rn");
    let rune_src = format!(
        "export const package = {{\n  name: 'srctool'\n  version: '0.1.0'\n  sources: {{ main: {{ url: 'payload.txt', sha256: '{payload_hash}' }} }}\n  bins: {{default: {{ srctool: 'bin/srctool' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'srctool')\n}}\n"
    );
    fs::write(&rune, rune_src).unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build with verified source");
    let hex = payload_hash.strip_prefix("sha256:").unwrap();
    assert!(
        root.join("cache").join("sources").join(hex).exists(),
        "verified source should be cached by hash"
    );

    // A wrong checksum is a hard failure before the build runs.
    let bad_rune = src.join("badsrc.rn");
    let bad_src = "export const package = {\n  name: 'badsrc'\n  version: '0.1.0'\n  sources: { main: { url: 'payload.txt', sha256: 'sha256:0000000000000000000000000000000000000000000000000000000000000000' } }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  cp $ctx.sources.main.path ($ctx.package_dir | path join 'bin' 'badsrc')\n}\n";
    fs::write(&bad_rune, bad_src).unwrap();
    let bad = run(
        root,
        &[
            "build",
            bad_rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_failure_contains(&bad, "hash mismatch", "reject mismatched source checksum");
}

#[test]
fn direct_source_install_preserves_runtime_deps() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let src = TempDir::new().unwrap();
    let src = src.path();
    let runes = src.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        src.join("tome.rn"),
        "export const tome = {\n  name: 'directdeps'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();

    fs::write(
        runes.join("dep.rn"),
        "export const package = {\n  name: 'dep'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'dep\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'dep')\n}\n",
    )
    .unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['dep'], build: {} }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", src.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add direct deps tome");

    let install = run(root, &["install", runes.join("app.rn").to_str().unwrap()]);
    assert_success(&install, "install direct source app");
    assert!(
        root.join("state")
            .join("packages")
            .join("dep.nuon")
            .exists(),
        "runtime dep from embedded archive metadata should be installed"
    );
}

#[test]
fn locked_source_install_rejects_rebuilt_hash_drift() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let rune = src.join("locksrc.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v1\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
    )
    .unwrap();

    let install = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&install, "initial source install");
    let lock_path = root.join("state").join("grimoire.lock.nuon");
    let locked = fs::read_to_string(&lock_path).expect("lockfile after source install");

    let remove = run(root, &["remove", "locksrc"]);
    assert_success(&remove, "remove source package");
    fs::write(&lock_path, locked).unwrap();

    fs::write(
        &rune,
        "export const package = {\n  name: 'locksrc'\n  version: '0.1.0'\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v2\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'locksrc')\n}\n",
    )
    .unwrap();

    let locked_install = run(root, &["install", rune.to_str().unwrap(), "--locked"]);
    assert_failure_contains(
        &locked_install,
        "hash mismatch",
        "locked source install rejects changed same-version source",
    );
    assert!(
        !root
            .join("state")
            .join("packages")
            .join("locksrc.nuon")
            .exists(),
        "failed locked source install should not write package state"
    );
}

#[test]
fn platform_filtered_sources_are_skipped_for_other_targets() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // The other-platform source points at a file that does not exist: the build can only
    // succeed if the platform filter prevents Grimoire from ever trying to fetch it.
    let other_os = if std::env::consts::OS == "linux" {
        "macos"
    } else {
        "linux"
    };
    let rune_dir = TempDir::new().unwrap();
    let rune = rune_dir.path().join("splitsrc.rn");
    fs::write(
        &rune,
        format!(
            "export const package = {{\n  name: 'splitsrc'\n  version: '0.1.0'\n  fixed_output: true\n  sources: {{\n    other: {{ url: 'does-not-exist.tar.zst', sha256: 'sha256:{}', platform: '{other_os}-*' }}\n  }}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'splitsrc\\\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'splitsrc')\n}}\n",
            "0".repeat(64)
        ),
    )
    .unwrap();

    let install = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&install, "install with a filtered-out source");
    assert_eq!(stdout(&run_shim(root, "splitsrc")).trim(), "splitsrc");
}

#[test]
fn changed_runtime_dep_rune_reinstalls_dep_and_dependent() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // `app` runtime-depends on `lib`. Editing lib's rune without changing its version gives
    // lib a new content address — and, because runtime deps fold into their dependents'
    // hashes, a new address for app too. The next install must re-realize both instead of
    // reusing them by version.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'drifttome'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let lib_rune = |payload: &str| {
        format!(
            "export const package = {{\n  name: 'lib'\n  version: '0.1.0'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{payload}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'lib')\n}}\n"
        )
    };
    fs::write(runes.join("lib.rn"), lib_rune("lib one")).unwrap();
    fs::write(
        runes.join("app.rn"),
        "export const package = {\n  name: 'app'\n  version: '0.1.0'\n  deps: { runtime: ['lib'], build: {} }\n \n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'app\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'app')\n}\n",
    )
    .unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add drift tome");
    assert_success(&run(root, &["install", "app"]), "install app");
    assert_eq!(stdout(&run_shim(root, "lib")).trim(), "lib one");
    let lib_state_before = state_text(root, "lib");

    fs::write(runes.join("lib.rn"), lib_rune("lib two")).unwrap();
    assert_success(&run(root, &["tome", "update", "drifttome"]), "tome update");

    let reinstall = run(root, &["install", "app"]);
    assert_success(&reinstall, "reinstall app after lib rune change");
    assert_eq!(
        stdout(&run_shim(root, "lib")).trim(),
        "lib two",
        "the active generation must surface the re-realized lib"
    );
    assert_ne!(
        state_text(root, "lib"),
        lib_state_before,
        "lib's state record must carry the new content address"
    );

    // With nothing drifted, the same install is a no-op again.
    let again = run(root, &["install", "app"]);
    assert_success(&again, "reinstall app with no drift");
    assert!(
        stdout(&again).contains("already installed and up to date"),
        "an unchanged graph must not reinstall: {}",
        stdout(&again)
    );
}

#[test]
fn held_package_is_not_re_realized_for_rune_drift() {
    let root = TempDir::new().unwrap();
    let root = root.path();

    // A hold pins the installed bits, not just the version: a rune edit at the same version
    // must not re-realize a held package until it is released.
    let tome = TempDir::new().unwrap();
    let tome = tome.path();
    let runes = tome.join("runes");
    fs::create_dir_all(&runes).unwrap();
    fs::write(
        tome.join("tome.rn"),
        "export const tome = {\n  name: 'holdfast'\n  packages: { repo: 'dist', format: 'local', index: 'index.nuon' }\n}\n",
    )
    .unwrap();
    let pinned_rune = |payload: &str| {
        format!(
            "export const package = {{\n  name: 'pinned'\n  version: '0.1.0'\n \n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf '{payload}\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'pinned')\n}}\n"
        )
    };
    fs::write(runes.join("pinned.rn"), pinned_rune("payload one")).unwrap();

    let add = run(
        root,
        &["tome", "add", tome.to_str().unwrap(), "--ref", "main"],
    );
    assert_success(&add, "add holdfast tome");
    assert_success(&run(root, &["install", "pinned"]), "install pinned");
    assert_success(&run(root, &["pkg", "hold", "pinned"]), "hold pinned");
    let state_before = state_text(root, "pinned");

    fs::write(runes.join("pinned.rn"), pinned_rune("payload two")).unwrap();
    assert_success(&run(root, &["tome", "update", "holdfast"]), "tome update");

    let reinstall = run(root, &["install", "pinned"]);
    assert_success(&reinstall, "reinstall while held");
    assert!(
        !stdout(&reinstall).contains("no longer matches its rune"),
        "a held package must not be flagged for drift: {}",
        stdout(&reinstall)
    );
    assert_eq!(
        stdout(&run_shim(root, "pinned")).trim(),
        "payload one",
        "held bits must stay exactly as installed"
    );
    assert_eq!(
        state_text(root, "pinned"),
        state_before,
        "held state must be untouched by drift"
    );

    // Releasing the hold lets the pending drift apply on the next install.
    assert_success(&run(root, &["pkg", "unhold", "pinned"]), "unhold pinned");
    let after = run(root, &["install", "pinned"]);
    assert_success(&after, "reinstall after unhold");
    assert_eq!(
        stdout(&run_shim(root, "pinned")).trim(),
        "payload two",
        "the released package must be re-realized from the edited rune"
    );
}

#[test]
fn reinstall_after_remove_reuses_the_cached_build_archive() {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let rune = src.join("cachedsrc.rn");
    fs::write(
        &rune,
        "export const package = {\n  name: 'cachedsrc'\n  version: '0.1.0'\n  bins: {default: { cachedsrc: 'bin/cachedsrc' }}\n}\n\nexport def build [ctx] {\n  mkdir ($ctx.package_dir | path join 'bin')\n  \"#!/usr/bin/env sh\\nprintf 'v1\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'cachedsrc')\n}\n",
    )
    .unwrap();

    assert_success(
        &run(root, &["install", rune.to_str().unwrap()]),
        "initial source install",
    );
    assert_success(&run(root, &["remove", "cachedsrc"]), "remove cachedsrc");

    // Same inputs, same content address: the verified archive in cache/builds is reused, so
    // the reinstall never re-runs the build.
    let reinstall = run(root, &["install", rune.to_str().unwrap()]);
    assert_success(&reinstall, "reinstall from cached archive");
    assert!(
        stdout(&reinstall).contains("cached archive"),
        "the reinstall must come from the build cache: {}",
        stdout(&reinstall)
    );
    assert_eq!(stdout(&run_shim(root, "cachedsrc")).trim(), "v1");
}
