//! Fetching and verifying remote artifacts: source downloads and binary archives.
//!
//! Everything funnels through [`fetch_verified`], the single download-and-trust gate: an artifact
//! is fetched (over HTTP with timeouts and bounded retries, or copied for local/`file://` paths)
//! into a content-addressed cache and checked against its expected hash *before* it is returned
//! (AGENTS.md §5.1). [`http_get_text`] fetches an index document, treating a 404 as "no index".

use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use crate::{
    archive,
    model::Source,
    progress::{status, success},
};

/// Total attempts for a single HTTP GET before giving up. The first try plus retries on
/// transient failures (transport errors and 5xx responses).
const HTTP_MAX_ATTEMPTS: u32 = 3;
/// Connection establishment budget; a host that never accepts the connection fails here rather
/// than hanging the install indefinitely.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-read/write budget once connected, generous enough for large archives on a slow link.
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(120);

/// A source artifact that has been fetched into the local cache and verified against its
/// declared checksum. `path` points at the cached file the build context can consume.
#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub path: PathBuf,
    pub extracted_dir: Option<PathBuf>,
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
) -> Result<BTreeMap<String, FetchedSource>> {
    let mut fetched = BTreeMap::new();
    for (name, source) in sources {
        let path = fetch_verified(
            &source.url,
            base_dir,
            &source.sha256,
            cache_dir,
            &format!("source `{name}`"),
        )?;
        fetched.insert(
            name.clone(),
            FetchedSource {
                path,
                extracted_dir: None,
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
    label: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    let cached = cache_dir.join(cache_name(expected_sha256));

    // A cache hit still has to clear the same checksum gate before it is trusted.
    if cached.exists() && hash_matches(&cached, expected_sha256)? {
        return Ok(cached);
    }

    status(&format!("fetching {label} ({location})"));
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
    success(&format!("{label} verified"));
    Ok(cached)
}

/// Fetches a text document (a package index) over `http(s)`. Returns `None` on a 404 so a tome
/// whose host has not published an index yet is treated as offering no binaries, mirroring a
/// missing local `index.nuon`. The index is the trust root: archives it lists are checksum-
/// verified against it, so the document itself is fetched over the transport without a hash.
pub fn http_get_text(url: &str) -> Result<Option<String>> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("expected an http(s) URL, got `{url}`");
    }
    status(&format!("fetching index ({url})"));
    match http_get(&http_agent(), url) {
        Ok(response) => Ok(Some(
            response
                .into_string()
                .with_context(|| format!("read index body from {url}"))?,
        )),
        Err(err) if matches!(*err, ureq::Error::Status(404, _)) => Ok(None),
        Err(err) => Err(http_error(url, *err)),
    }
}

/// A reusable HTTP agent with connect/read/write timeouts so no request can hang the install.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(HTTP_CONNECT_TIMEOUT)
        .timeout_read(HTTP_IO_TIMEOUT)
        .timeout_write(HTTP_IO_TIMEOUT)
        .build()
}

/// GETs `url` with bounded retries: transport errors (DNS, connect, reset, timeout) and 5xx
/// responses are retried up to [`HTTP_MAX_ATTEMPTS`] with a short linear backoff, since they are
/// usually transient. A 4xx (including 404) returns immediately so callers can act on it.
fn http_get(agent: &ureq::Agent, url: &str) -> Result<ureq::Response, Box<ureq::Error>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match agent.get(url).call() {
            Ok(response) => return Ok(response),
            Err(err) if attempt < HTTP_MAX_ATTEMPTS && is_retryable(&err) => {
                thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
            }
            Err(err) => return Err(Box::new(err)),
        }
    }
}

/// Whether a failed request is worth retrying: transport-level failures and server-side (5xx)
/// errors are transient; client-side (4xx) errors are not and would only repeat.
fn is_retryable(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Transport(_) => true,
        ureq::Error::Status(code, _) => *code >= 500,
    }
}

/// Turns a `ureq` error into a clear, actionable message that names the URL and the cause:
/// the HTTP status (with reason) for a response error, or the transport detail otherwise.
fn http_error(url: &str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, response) => {
            let reason = response.status_text().to_owned();
            anyhow!("request {url} failed: HTTP {code} {reason}")
        }
        ureq::Error::Transport(transport) => anyhow!("request {url} failed: {transport}"),
    }
}

fn download_into(url: &str, base_dir: &Path, destination: &Path) -> Result<()> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let response = http_get(&http_agent(), url).map_err(|err| http_error(url, *err))?;
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
