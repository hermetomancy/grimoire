use super::*;

fn parse_package(contents: &str) -> Result<PackageMetadata> {
    PackageMetadata::from_value(crate::nu::nuon_io::parse_nuon(contents)?, false)
}

fn assert_parse_err(contents: &str, needle: &str) {
    let err = parse_package(contents).unwrap_err();
    assert!(
        format!("{err:#}").contains(needle),
        "unexpected error: {err:#}"
    );
}

#[test]
fn rejects_unknown_package_fields_but_allows_meta() {
    let metadata = parse_package(
        "{ name: 'pkg', version: '1.0.0', meta: { homepage: 'https://example.com', license: 'MIT' } }",
    )
    .expect("meta record is inert and allowed");
    assert_eq!(metadata.name, "pkg");

    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', licence: 'MIT' }",
        "unknown field `licence`",
    );
}

#[test]
fn authored_metadata_rejects_archive_only_fields_and_non_record_meta() {
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', target: 'linux-x86_64-musl' }",
        "unknown field `target`",
    );
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', store_path: 'deadbeef-pkg-1.0.0' }",
        "unknown field `store_path`",
    );
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', format: 1 }",
        "unknown field `format`",
    );
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', meta: 'MIT' }",
        "package field `meta` must be a record",
    );
}

#[test]
fn archive_metadata_accepts_archive_fields() {
    let value = crate::nu::nuon_io::parse_nuon(
        "{ format: 1, name: 'pkg', version: '1.0.0', target: 'linux-x86_64-musl', store_path: 'aaaaaaaaaaaaaaaa-pkg-1.0.0' }",
    )
    .unwrap();
    let metadata = PackageMetadata::from_value(value, true).unwrap();
    assert_eq!(metadata.target.as_deref(), Some("linux-x86_64-musl"));
    assert_eq!(
        metadata.store_path.as_deref(),
        Some("aaaaaaaaaaaaaaaa-pkg-1.0.0")
    );
}

#[test]
fn rejects_invalid_bins_target_key() {
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', bins: { linxu: { pkg: 'bin/pkg' } } }",
        "target key `linxu`",
    );
}

#[test]
fn fixed_output_requires_sources_and_no_build_deps() {
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', fixed_output: true }",
        "must declare at least one source",
    );
    assert_parse_err(
        "{ name: 'pkg', version: '1.0.0', fixed_output: true, sources: { main: { url: 'payload.tar.zst', sha256: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' } }, deps: { build: { default: ['cmake'] }, runtime: [] } }",
        "must not declare build deps",
    );
}

#[test]
fn sources_for_filters_by_platform_glob() {
    let mut sources = BTreeMap::new();
    sources.insert(
        "everywhere".to_owned(),
        Source {
            url: "https://example.com/all.tar.gz".to_owned(),
            sha256: "sha256:aaa".to_owned(),
            platform: None,
            host_libc: None,
        },
    );
    sources.insert(
        "mac-only".to_owned(),
        Source {
            url: "https://example.com/mac.tar.xz".to_owned(),
            sha256: "sha256:bbb".to_owned(),
            platform: Some("macos-*".to_owned()),
            host_libc: None,
        },
    );
    let metadata = PackageMetadata {
        name: "stage0".to_owned(),
        version: "1.0.0".to_owned(),
        target: None,
        store_path: None,
        targets: Vec::new(),
        fixed_output: true,
        build_only: false,
        summary: None,
        bins: BTreeMap::new(),
        sources,
        deps: Deps::default(),
        build_flags: BTreeMap::new(),
        provides: Vec::new(),
        libs: Vec::new(),
        notes: Vec::new(),
        upstream_version: None,
        conflicts: Vec::new(),
        replaces: Vec::new(),
        split_from: None,
        files: Vec::new(),
    };
    let mac = metadata.sources_for("macos-aarch64-darwin");
    assert!(mac.contains_key("everywhere") && mac.contains_key("mac-only"));
    let linux = metadata.sources_for("linux-x86_64-musl");
    assert!(linux.contains_key("everywhere") && !linux.contains_key("mac-only"));
}

#[test]
fn bins_for_merges_default_os_and_target() {
    let mut bins = BTreeMap::new();
    let mut default = BTreeMap::new();
    default.insert("sed".to_owned(), "bin/sed".to_owned());
    bins.insert("default".to_owned(), default);

    let mut linux = BTreeMap::new();
    linux.insert("awk".to_owned(), "bin/awk".to_owned());
    bins.insert("linux".to_owned(), linux);

    let mut musl = BTreeMap::new();
    musl.insert("tar".to_owned(), "bin/tar".to_owned());
    bins.insert("linux-x86_64-musl".to_owned(), musl);

    let meta = PackageMetadata {
        name: "multitool".to_owned(),
        version: "0.8.13".to_owned(),
        target: None,
        store_path: None,
        targets: vec![],
        fixed_output: false,
        build_only: false,
        summary: None,
        bins,
        sources: BTreeMap::new(),
        deps: Deps::default(),
        build_flags: BTreeMap::new(),
        provides: Vec::new(),
        libs: Vec::new(),
        notes: Vec::new(),
        upstream_version: None,
        conflicts: Vec::new(),
        replaces: Vec::new(),
        split_from: None,
        files: Vec::new(),
    };

    let resolved: Vec<_> = meta.bins_for("linux-x86_64-musl").into_keys().collect();
    assert_eq!(resolved, vec!["awk", "sed", "tar"]);

    let resolved: Vec<_> = meta.bins_for("linux-aarch64-musl").into_keys().collect();
    assert_eq!(resolved, vec!["awk", "sed"]);

    let resolved: Vec<_> = meta.bins_for("macos-aarch64-darwin").into_keys().collect();
    assert_eq!(resolved, vec!["sed"]);
}

#[test]
fn archive_metadata_writes_only_selected_sources() {
    let mut sources = BTreeMap::new();
    sources.insert(
        "linux".to_owned(),
        Source {
            url: "https://example.com/linux.tar.xz".to_owned(),
            sha256: "sha256:".to_owned() + &"a".repeat(64),
            platform: Some("linux-*".to_owned()),
            host_libc: Some(crate::util::paths::host_libc().to_owned()),
        },
    );
    sources.insert(
        "mac".to_owned(),
        Source {
            url: "https://example.com/mac.tar.xz".to_owned(),
            sha256: "sha256:".to_owned() + &"b".repeat(64),
            platform: Some("macos-*".to_owned()),
            host_libc: None,
        },
    );
    let meta = PackageMetadata {
        name: "pkg".to_owned(),
        version: "1.0.0".to_owned(),
        target: None,
        store_path: None,
        targets: Vec::new(),
        fixed_output: false,
        build_only: false,
        summary: None,
        bins: BTreeMap::new(),
        sources,
        deps: Deps::default(),
        build_flags: BTreeMap::new(),
        provides: Vec::new(),
        libs: Vec::new(),
        notes: Vec::new(),
        upstream_version: None,
        conflicts: Vec::new(),
        replaces: Vec::new(),
        split_from: None,
        files: Vec::new(),
    };

    let Value::Record { val, .. } = meta.archive_value("linux-x86_64-musl", None) else {
        panic!("archive metadata must be a record");
    };
    let Some(Value::Record { val: sources, .. }) = val.get("sources") else {
        panic!("archive metadata must contain source records");
    };
    assert!(sources.get("linux").is_some());
    assert!(sources.get("mac").is_none());
    let Some(Value::Record { val: linux, .. }) = sources.get("linux") else {
        panic!("linux source must be a record");
    };
    assert!(linux.get("host_libc").is_some());
}
