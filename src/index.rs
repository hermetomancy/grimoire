//! Reading a tome's binary package index (`index.nuon`): the catalog of prebuilt archives a
//! package repository offers. The index is inert data parsed through the shared NUON layer and
//! is the trust root for binary installs — archives it lists are checksum-verified against it,
//! and (when the tome is signed) the index document itself is signature-verified before it is
//! parsed (see `src/signing.rs` and `tome::package_index`).

#[cfg(test)]
mod tests {
    use crate::{model::PackageIndex, nu::nuon_io};

    fn parse_index(contents: &str) -> anyhow::Result<PackageIndex> {
        PackageIndex::from_value(nuon_io::parse_nuon(contents)?)
    }

    #[test]
    fn reads_and_finds_entries() {
        let index = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"deadbeefdeadbeef\": { name: \"hello\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"hello-1.0.0-linux-x86_64-gnu.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", runtime_deps: [\"libc\"] }\n  }\n}\n",
        )
        .expect("parse index");
        assert_eq!(index.entries.len(), 1);

        let candidates = index.candidates("hello", "linux-x86_64-gnu");
        let (hash, entry) = candidates.first().expect("entry for current target");
        assert_eq!(*hash, "deadbeefdeadbeef");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(
            entry
                .runtime_deps
                .iter()
                .map(|dep| dep.name.as_str())
                .collect::<Vec<_>>(),
            vec!["libc"]
        );
        assert!(index.candidates("hello", "macos-aarch64-darwin").is_empty());
        assert!(index.candidates("missing", "linux-x86_64-gnu").is_empty());
    }

    #[test]
    fn rejects_unsafe_archive_path() {
        let err = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"deadbeefdeadbeef\": { name: \"evil\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"../escape.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n  }\n}\n",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("parent components"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn rejects_missing_entries_field() {
        assert!(parse_index("{ format: 2 }\n").is_err());
    }

    #[test]
    fn lookup_by_hash() {
        let index = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"aaa\": { name: \"a\", version: \"1.0.0\", target: \"t\", archive: \"a.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n    \"bbb\": { name: \"b\", version: \"1.0.0\", target: \"t\", archive: \"b.tar.zst\", archive_hash: \"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\" }\n  }\n}\n",
        )
        .expect("parse index");
        assert!(index.entries.contains_key("aaa"));
        assert!(index.entries.contains_key("bbb"));
        assert!(!index.entries.contains_key("ccc"));
    }
}
