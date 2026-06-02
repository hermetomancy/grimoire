//! Reading a tome's binary package index (`index.nuon`): the catalog of prebuilt archives a
//! package repository offers. The index is inert data parsed through the shared NUON layer and
//! is the trust root for binary installs — archives it lists are checksum-verified against it.

use anyhow::{Context, Result};
use std::path::Path;

use crate::{model::PackageIndex, nu::nuon_io};

/// Reads and validates a package repository index (`index.nuon`). The file is inert data,
/// read through the shared NUON layer (AGENTS.md §3).
pub fn read_index(path: &Path) -> Result<PackageIndex> {
    let value = nuon_io::read_nuon(path)
        .with_context(|| format!("read package index {}", path.display()))?;
    PackageIndex::from_value(value)
        .with_context(|| format!("parse package index {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_index(contents: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("index.nuon");
        fs::write(&path, contents).expect("write index");
        (dir, path)
    }

    #[test]
    fn reads_and_finds_entries() {
        let (_dir, path) = write_index(
            "{\n  packages: [\n    { name: \"hello\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"hello-1.0.0-linux-x86_64-gnu.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", runtime_deps: [\"libc\"] }\n  ]\n}\n",
        );
        let index = read_index(&path).expect("read index");
        assert_eq!(index.packages.len(), 1);

        let candidates = index.candidates("hello", "linux-x86_64-gnu");
        let entry = candidates.first().expect("entry for current target");
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
        let (_dir, path) = write_index(
            "{\n  packages: [\n    { name: \"evil\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"../escape.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n  ]\n}\n",
        );
        let err = read_index(&path).unwrap_err();
        assert!(
            err.to_string().contains("parse package index")
                || format!("{err:#}").contains("parent components"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn rejects_missing_packages_field() {
        let (_dir, path) = write_index("{ version: 1 }\n");
        assert!(read_index(&path).is_err());
    }
}
