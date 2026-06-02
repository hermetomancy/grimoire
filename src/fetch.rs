use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    model::Source,
    progress::{status, success},
};

/// A source artifact that has been fetched into the local cache and verified against its
/// declared checksum. `path` points at the cached file the build context can consume.
#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub path: PathBuf,
    pub url: String,
    pub sha256: String,
}

/// Fetches and verifies every declared source. Each artifact is downloaded (or copied,
/// for local/`file://` sources) into `cache_dir`, then checked against its `sha256` before
/// it is offered to the build. A mismatch is a hard failure — nothing is trusted unverified
/// (AGENTS.md §5.1). `base_dir` is the directory relative local source paths resolve against.
pub fn fetch_sources(
    sources: &BTreeMap<String, Source>,
    base_dir: &Path,
    cache_dir: &Path,
    quiet: bool,
) -> Result<BTreeMap<String, FetchedSource>> {
    let mut fetched = BTreeMap::new();
    for (name, source) in sources {
        let path = fetch_verified(
            &source.url,
            base_dir,
            &source.sha256,
            cache_dir,
            quiet,
            &format!("source `{name}`"),
        )?;
        fetched.insert(
            name.clone(),
            FetchedSource {
                path,
                url: source.url.clone(),
                sha256: source.sha256.clone(),
            },
        );
    }
    Ok(fetched)
}

/// Fetches `location` (an `http(s)` URL or a path relative to `base_dir`) into `cache_dir`,
/// keyed by its expected hash, and verifies it before returning the cached path. This is the
/// single download-and-trust gate shared by source artifacts and binary archives (§5.1).
pub fn fetch_verified(
    location: &str,
    base_dir: &Path,
    expected_sha256: &str,
    cache_dir: &Path,
    quiet: bool,
    label: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    let cached = cache_dir.join(cache_name(expected_sha256));

    // A cache hit still has to clear the same checksum gate before it is trusted.
    if cached.exists() && hash_matches(&cached, expected_sha256)? {
        return Ok(cached);
    }

    status(quiet, &format!("fetching {label} ({location})"));
    let staged = tempfile::Builder::new()
        .prefix("grimoire-fetch-")
        .tempfile_in(cache_dir)?;
    download_into(location, base_dir, staged.path())
        .with_context(|| format!("fetch {label} from {location}"))?;

    let actual = archive::archive_hash(staged.path())?;
    archive::verify_hash(&actual, expected_sha256)
        .with_context(|| format!("verify {label} ({location})"))?;

    staged
        .persist(&cached)
        .with_context(|| format!("cache {label} at {}", cached.display()))?;
    success(quiet, &format!("{label} verified"));
    Ok(cached)
}

fn download_into(url: &str, base_dir: &Path, destination: &Path) -> Result<()> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let response = ureq::get(url)
            .call()
            .with_context(|| format!("request {url}"))?;
        let mut reader = response.into_reader();
        let mut file = File::create(destination)?;
        io::copy(&mut reader, &mut file)?;
        return Ok(());
    }

    let local = local_source_path(url, base_dir);
    if !local.exists() {
        bail!("source `{url}` does not exist at {}", local.display());
    }
    fs::copy(&local, destination)
        .with_context(|| format!("copy local source {}", local.display()))?;
    Ok(())
}

fn local_source_path(url: &str, base_dir: &Path) -> PathBuf {
    let raw = url.strip_prefix("file://").unwrap_or(url);
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn cache_name(sha256: &str) -> String {
    let hex = sha256.strip_prefix("sha256:").unwrap_or(sha256);
    hex.trim().to_ascii_lowercase()
}

fn hash_matches(path: &Path, expected: &str) -> Result<bool> {
    Ok(archive::verify_hash(&archive::archive_hash(path)?, expected).is_ok())
}
