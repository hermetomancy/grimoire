use anyhow::Result;
use std::path::PathBuf;

use crate::{fetch, model::IndexEntry, paths, tome};

/// A binary archive resolved from a tome's package index, fetched and verified into the
/// local cache. `expected_hash` is the index's recorded hash, re-checked at install time.
pub struct ResolvedArchive {
    pub path: PathBuf,
    pub entry: IndexEntry,
}

/// Looks for a pre-built binary archive for `package` matching the current target across the
/// configured tomes, in name order. Returns `None` when no tome offers one, so the caller can
/// fall back to a source build. The archive is downloaded and checksum-verified before return.
pub fn resolve_binary(package: &str, quiet: bool) -> Result<Option<ResolvedArchive>> {
    let target = paths::target_triple();
    for tome_state in tome::load_tomes()? {
        let Some((root, entry)) = tome::package_index_entry(&tome_state, package, &target, quiet)?
        else {
            continue;
        };

        let path = fetch::fetch_verified(
            &entry.archive,
            &root,
            &entry.archive_hash,
            &paths::archive_cache_dir()?,
            quiet,
            &format!("archive `{}` {}", entry.name, entry.version),
        )?;
        return Ok(Some(ResolvedArchive { path, entry }));
    }
    Ok(None)
}
