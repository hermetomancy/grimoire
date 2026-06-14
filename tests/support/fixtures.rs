//! Construction of test artifacts: source/package archives in every supported compression
//! (including deliberately unsafe ones for the validation tests), minisign keypairs, and
//! fully signed tome layouts.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::{Compression, write::GzEncoder};
use tempfile::TempDir;
use xz2::write::XzEncoder;

type GzipFileEncoder = GzEncoder<fs::File>;
type XzFileEncoder = XzEncoder<fs::File>;

use super::*;

pub fn source_archive_is_extracted_into_build_context(
    archive_name: &str,
    archive_kind: TestArchiveKind,
) {
    let root = TempDir::new().unwrap();
    let root = root.path();
    let out = TempDir::new().unwrap();
    let out = out.path();
    let src = TempDir::new().unwrap();
    let src = src.path();

    let source_archive = src.join(archive_name);
    write_test_source_archive(&source_archive, archive_kind);
    let source_hash = sha256_file(&source_archive);

    let rune = src.join("extractor.rn");
    fs::write(
        &rune,
        format!(
            "export const package = {{\n  name: 'extractor'\n  version: '0.1.0'\n  sources: {{ main: {{ url: '{archive_name}', sha256: '{source_hash}' }} }}\n  bins: {{default: {{ extractor: 'bin/extractor' }}}}\n}}\n\nexport def build [ctx] {{\n  mkdir ($ctx.package_dir | path join 'bin')\n  let message = (open --raw ($ctx.sources.main.dir | path join 'payload' 'message.txt') | str trim)\n  $\"#!/usr/bin/env sh\\nprintf '($message)\\n'\\n\" | save ($ctx.package_dir | path join 'bin' 'extractor')\n}}\n"
        ),
    )
    .unwrap();

    let build = run(
        root,
        &[
            "build",
            rune.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert_success(&build, "build from extracted source archive");
    let archive = out.join(format!("extractor-0.1.0-{}.tar.zst", target_triple()));
    let install = run(root, &["install", archive.to_str().unwrap()]);
    assert_success(&install, "install extracted source package");

    let output = run_shim(root, "extractor");
    assert_success(&output, "run extractor");
    assert_eq!(stdout(&output).trim(), "hello from extracted source");
}

pub fn open_archive(path: &Path) -> tar::Builder<ZstdFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = zstd::stream::write::Encoder::new(file, 0).expect("zstd encoder");
    tar::Builder::new(encoder)
}

pub fn open_gzip_archive(path: &Path) -> tar::Builder<GzipFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = GzEncoder::new(file, Compression::default());
    tar::Builder::new(encoder)
}

pub fn open_xz_archive(path: &Path) -> tar::Builder<XzFileEncoder> {
    let file = fs::File::create(path).expect("create archive");
    let encoder = XzEncoder::new(file, 6);
    tar::Builder::new(encoder)
}

pub fn finish_archive(builder: tar::Builder<ZstdFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish zstd");
}

pub fn finish_gzip_archive(builder: tar::Builder<GzipFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip");
}

pub fn finish_xz_archive(builder: tar::Builder<XzFileEncoder>) {
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish xz");
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)] // all fixtures are tarballs; the prefix is meaningful
pub enum TestArchiveKind {
    TarGz,
    TarXz,
    TarZst,
}

pub fn write_test_source_archive(path: &Path, kind: TestArchiveKind) {
    match kind {
        TestArchiveKind::TarGz => {
            let mut builder = open_gzip_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_gzip_archive(builder);
        }
        TestArchiveKind::TarXz => {
            let mut builder = open_xz_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_xz_archive(builder);
        }
        TestArchiveKind::TarZst => {
            let mut builder = open_archive(path);
            append_file(
                &mut builder,
                "payload/message.txt",
                b"hello from extracted source\n",
                0o644,
            );
            finish_archive(builder);
        }
    }
}

pub fn write_nested_symlink_source_archive(path: &Path) {
    let mut builder = open_archive(path);
    // `payload -> sub` has an in-package target (so the target check passes), but the member
    // `payload/evil.txt` underneath it would extract through the link.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "payload", "sub")
        .expect("append symlink");
    append_file(&mut builder, "payload/evil.txt", b"x\n", 0o644);
    finish_archive(builder);
}

pub fn append_file<W: Write>(builder: &mut tar::Builder<W>, path: &str, data: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(tar::EntryType::Regular);
    builder
        .append_data(&mut header, path, data)
        .expect("append file");
}

pub fn make_package_archive(out: &Path, name: &str, package_nuon: &str) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("{name}-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        b"#!/usr/bin/env sh\nexit 0\n",
        0o755,
    );
    finish_archive(builder);
    archive
}

/// A prebuilt archive whose embedded store basename is `<store_hash>-<name>-<version>`, so it
/// installs as a valid substitute for a rune of the same content address.
pub fn make_prebuilt(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    store_hash: &str,
    bin_script: &str,
) -> PathBuf {
    make_prebuilt_with_deps(path, name, version, triple, store_hash, "[]", bin_script)
}

/// Like [`make_prebuilt`], but embedding `runtime_deps` (a NUON list, e.g. `["lib"]`) in the
/// archive's package.nuon. Real `grm tome build` archives always embed their deps; the
/// installed state record reads them from the archive, and the linked set / orphan sweep
/// read them from that state.
pub fn make_prebuilt_with_deps(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    store_hash: &str,
    runtime_deps: &str,
    bin_script: &str,
) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", store_path: \"{store_hash}-{name}-{version}\", bins: {{default: {{{name}: \"bin/{name}\"}}}}, deps: {{ runtime: {runtime_deps} }}}}\n"
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        bin_script.as_bytes(),
        0o755,
    );
    finish_archive(builder);
    path.to_path_buf()
}

/// Builds a complete `.tar.zst` package archive at `path` whose single bin is `bin_script`.
/// Used to stage a pre-built binary in a fake package repository.
pub fn make_indexed_archive(path: &Path, name: &str, triple: &str, bin_script: &str) -> PathBuf {
    make_versioned_archive(path, name, "0.1.0", triple, bin_script)
}

/// Like [`make_indexed_archive`] but with an explicit `version`, so a test can stage several
/// versions of the same package in one index.
pub fn make_versioned_archive(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    bin_script: &str,
) -> PathBuf {
    make_versioned_archive_with_hash(path, name, version, triple, bin_script, "cafef00dcafef00d")
}

pub fn make_versioned_archive_with_hash(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    bin_script: &str,
    store_hash: &str,
) -> PathBuf {
    make_versioned_archive_with_hash_and_deps(
        path, name, version, triple, bin_script, store_hash, "[]",
    )
}

/// Like [`make_versioned_archive_with_hash`], but embedding `runtime_deps` (a NUON list) in
/// the archive's package.nuon, like real built archives do.
pub fn make_versioned_archive_with_hash_and_deps(
    path: &Path,
    name: &str,
    version: &str,
    triple: &str,
    bin_script: &str,
    store_hash: &str,
    runtime_deps: &str,
) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut builder = open_archive(path);
    let package_nuon = format!(
        "{{format: 1, name: \"{name}\", version: \"{version}\", target: \"{triple}\", store_path: \"{}\", bins: {{default: {{{name}: \"bin/{name}\"}}}}, deps: {{ runtime: {runtime_deps} }}}}\n",
        fake_store_basename_with_hash(name, version, store_hash)
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        &format!("bin/{name}"),
        bin_script.as_bytes(),
        0o755,
    );
    finish_archive(builder);
    path.to_path_buf()
}

pub fn gen_keypair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().expect("generate keypair")
}

/// Writes a detached minisign signature for `data` at `path`, in the `.minisig` text form
/// `grm` verifies.
pub fn sign_to(path: &Path, data: &[u8], keypair: &minisign::KeyPair) {
    let signature = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        std::io::Cursor::new(data),
        Some("grimoire test"),
        Some("grimoire test"),
    )
    .expect("sign")
    .into_string();
    fs::write(path, signature).expect("write signature");
}

/// Builds a local tome that publishes a single prebuilt `sgnpkg` archive with a detached
/// `archive.tar.zst.minisig` and a signed `runes-manifest.nuon`, declaring `keypair`'s public
/// key at the manifest's top-level `signers`. Re-running it with a different keypair rewrites
/// the declared signer and re-signs the artifacts, which is exactly the key-rotation scenario.
pub fn build_signed_tome(tome: &Path, name: &str, triple: &str, keypair: &minisign::KeyPair) {
    fs::create_dir_all(tome.join("dist")).unwrap();
    fs::create_dir_all(tome.join("runes")).unwrap();
    fs::write(
        tome.join("runes").join("dummy.rn"),
        "export const package = { name: 'dummy' version: '0.0.1' }\n",
    )
    .unwrap();

    // Generate runes-manifest.nuon listing all .rn files with their sha256 hashes.
    let mut runes_map = std::collections::BTreeMap::new();
    for entry in fs::read_dir(tome.join("runes")).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rn") {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap()
                .to_owned();
            let hash = sha256_file(&path);
            runes_map.insert(file_name, hash);
        }
    }
    let mut manifest_entries: Vec<String> = Vec::new();
    for (file_name, hash) in &runes_map {
        manifest_entries.push(format!("\"{file_name}\": \"{hash}\""));
    }
    let runes_manifest_body = format!(
        "{{ format: 1, runes: {{ {} }} }}\n",
        manifest_entries.join(", ")
    );
    fs::write(tome.join("runes-manifest.nuon"), &runes_manifest_body).unwrap();
    sign_to(
        &tome.join("runes-manifest.nuon.minisig"),
        runes_manifest_body.as_bytes(),
        keypair,
    );

    let pubkey = keypair.pk.to_base64();
    let manifest_body = format!(
        "export const tome = {{\n  name: '{name}'\n  signers: ['{pubkey}']\n  packages: {{ repo: 'dist', format: 'local', index: 'index.nuon' }}\n}}\n"
    );
    fs::write(tome.join("tome.rn"), &manifest_body).unwrap();
    sign_to(
        &tome.join("tome.rn.minisig"),
        manifest_body.as_bytes(),
        keypair,
    );

    let dist = tome.join("dist");
    let archive_name = format!("sgnpkg-0.1.0-{triple}.tar.zst");
    let archive = make_versioned_archive(
        &dist.join(&archive_name),
        "sgnpkg",
        "0.1.0",
        triple,
        "#!/usr/bin/env sh\nprintf 'signed\\n'\n",
    );
    let hash = sha256_file(&archive);
    let index_body = format!(
        "{{\n  format: 2,\n    entries: {{\n    \"cafef00dcafef00d\": {{ name: \"sgnpkg\", version: \"0.1.0\", target: \"{triple}\", archive: \"{archive_name}\", archive_hash: \"{hash}\", runtime_deps: []}}\n  }}\n}}\n"
    );
    fs::write(dist.join("index.nuon"), &index_body).unwrap();
    sign_to(
        &dist.join(format!("{archive_name}.minisig")),
        &fs::read(&archive).unwrap(),
        keypair,
    );
}

pub fn make_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("badlink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"badlink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{badlink: \"bin/badlink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/badlink", "/tmp")
        .expect("append symlink");
    finish_archive(builder);
    archive
}

pub fn make_nested_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("nested-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"nested\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    // `lib -> sub` has an in-package target (so the target check passes), but the member
    // `lib/evil` underneath it would extract through the link.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "lib", "sub")
        .expect("append symlink");
    append_file(&mut builder, "lib/evil", b"x\n", 0o644);
    finish_archive(builder);
    archive
}

pub fn make_bad_path_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("badpath-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);

    let data = b"unsafe\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    // Write a raw absolute name to bypass tar's relative-path normalization, producing the
    // unsafe member that install must reject.
    let gnu = header.as_gnu_mut().expect("gnu header");
    let name = b"/grimoire-absolute-bad";
    gnu.name[..name.len()].copy_from_slice(name);
    header.set_cksum();
    builder
        .append(&header, &data[..])
        .expect("append raw entry");

    finish_archive(builder);
    archive
}

pub fn make_hard_link_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("hardlink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"hardlink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{hardlink: \"bin/hardlink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(0o644);
    builder
        .append_link(&mut header, "bin/hardlink", "bin/real")
        .expect("append hard link");
    finish_archive(builder);
    archive
}

pub fn make_safe_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("safelink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"safelink\", version: \"0.1.0\", target: \"{}\", store_path: \"{}\", bins: {{default: {{safelink: \"bin/real\", alias: \"bin/alias\"}}}}}}\n",
        target_triple(),
        fake_store_basename("safelink", "0.1.0")
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );
    append_file(
        &mut builder,
        "bin/real",
        b"#!/usr/bin/env sh\nprintf 'real tool\n'",
        0o755,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/alias", "real")
        .expect("append safe symlink");
    finish_archive(builder);
    archive
}

pub fn make_unsafe_symlink_archive(out: &Path) -> PathBuf {
    fs::create_dir_all(out).unwrap();
    let archive = out.join(format!("unsafelink-0.1.0-{}.tar.zst", target_triple()));
    let mut builder = open_archive(&archive);
    let package_nuon = format!(
        "{{format: 1, name: \"unsafelink\", version: \"0.1.0\", target: \"{}\", bins: {{default: {{unsafelink: \"bin/unsafelink\"}}}}}}\n",
        target_triple()
    );
    append_file(
        &mut builder,
        ".grimoire/package.nuon",
        package_nuon.as_bytes(),
        0o644,
    );
    append_file(
        &mut builder,
        ".grimoire/rune.rn",
        b"export const package = {}\n",
        0o644,
    );

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, "bin/unsafelink", "../../etc/passwd")
        .expect("append unsafe symlink");
    finish_archive(builder);
    archive
}
