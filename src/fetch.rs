//! Fetching and verifying remote artifacts: source downloads and binary archives.
//!
//! Everything funnels through [`fetch_verified`], the single download-and-trust gate: an artifact
//! is fetched (over HTTP with timeouts and bounded retries, or copied for local/`file://` paths)
//! into a content-addressed cache and checked against its expected hash *before* it is returned
//! (AGENTS.md §10.1). [`http_get_index`] fetches an index document, treating a 404 as "no index".

use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use crate::{
    archive,
    model::Source,
    util::output::{status, success},
};

/// Total attempts for a single HTTP GET before giving up. The first try plus retries on
/// transient failures (transport errors and 5xx responses).
const HTTP_MAX_ATTEMPTS: u32 = 3;
/// Connection establishment budget; a host that never accepts the connection fails here rather
/// than hanging the install indefinitely.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-read/write budget once connected, generous enough for large archives on a slow link.
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(120);
/// Overall budget for fetching a package index — a few-KB document that should answer
/// immediately. An unreachable binhost fails the fetch in seconds instead of holding the
/// command for connect-timeout-times-retries; the budget covers DNS, connect, and body,
/// and retries only happen while it lasts.
const INDEX_TIMEOUT: Duration = Duration::from_secs(5);

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
/// (AGENTS.md §10.1). `base_dir` is the directory relative local source paths resolve against.
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
/// single download-and-trust gate shared by source artifacts and binary archives (§10.1).
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

/// Fetches a package index over `https`. Returns [`IndexFetch::Missing`] on a 404 so a tome
/// whose host has not published an index yet is treated as offering no binaries, mirroring a
/// missing local `index.nuon`. The index is the trust root: archives it lists are checksum-
/// verified against it, so the document itself is fetched over the transport without a hash —
/// which is exactly why plain `http` is refused (a MitM index controls every archive hash).
/// Loopback addresses are exempt for local development and tests.
/// The three outcomes of an index fetch a caller may treat differently: a document, a
/// definitive "not published" (404), or an unreachable host — which resolution degrades to
/// source-only with a loud warning, while policy violations (plain http) stay hard errors.
pub enum IndexFetch {
    Document(String),
    Missing,
    Unreachable(anyhow::Error),
}

pub fn http_get_index(url: &str) -> Result<IndexFetch> {
    require_https_for_index(url)?;
    status(&format!("fetching index ({url})"));
    match http_get_bounded(index_agent(), url, Some(INDEX_TIMEOUT)) {
        // The index agent refuses to follow redirects (see `index_agent`), so a 3xx arrives here
        // as a normal response rather than a chased download. Treat it as unreachable: an https
        // index that 30x-redirects to plain http would otherwise silently downgrade the trust
        // root. The tome must point at the canonical index URL directly.
        Ok(response) if (300..400).contains(&response.status()) => {
            Ok(IndexFetch::Unreachable(anyhow!(
                "index at {url} responded with a redirect (HTTP {}); point the tome at the \
                 canonical index URL directly — redirects are refused so an https index cannot \
                 be downgraded to http",
                response.status()
            )))
        }
        Ok(response) => match response.into_string() {
            Ok(text) => Ok(IndexFetch::Document(text)),
            Err(err) => Ok(IndexFetch::Unreachable(anyhow!(
                "read index body from {url}: {err}"
            ))),
        },
        Err(err) if matches!(*err, ureq::Error::Status(404, _)) => Ok(IndexFetch::Missing),
        Err(err) => Ok(IndexFetch::Unreachable(http_error(url, *err))),
    }
}

/// The index is fetched without a content hash, so its transport must be authenticated:
/// `https` only, with loopback hosts exempt (local binhosts and offline tests).
fn require_https_for_index(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = url.strip_prefix("http://") else {
        bail!("expected an http(s) URL, got `{url}`");
    };
    let authority = rest.split('/').next().unwrap_or("");
    // Userinfo (`user@host`) lets `http://[::1]@evil.com/index.nuon` present a loopback-looking
    // host while ureq actually connects to `evil.com`. Refuse any embedded credentials so the
    // loopback exemption below can only ever match the real connect host.
    if authority.contains('@') {
        bail!("refusing to fetch a package index from a URL with embedded credentials: `{url}`");
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Ok(());
    }
    bail!(
        "refusing to fetch a package index over plain http: `{url}`. The index is the trust \
         root for binary installs; serve it over https (plain http is permitted only for \
         loopback addresses)"
    )
}

/// The process-wide HTTP agent, with connect/read/write timeouts so no request can hang the
/// install. Shared so consecutive downloads reuse connections instead of re-handshaking
/// TCP/TLS per request. Used for checksum-verified downloads (sources, archives), where a
/// redirect is harmless because the bytes are hashed against the trusted index.
fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(HTTP_CONNECT_TIMEOUT)
            .timeout_read(HTTP_IO_TIMEOUT)
            .timeout_write(HTTP_IO_TIMEOUT)
            .build()
    })
}

/// The agent for fetching index documents — the binary-install trust root, which is *not*
/// checksum-verified. It refuses to follow redirects (`redirects(0)`), so an `https` index that
/// 30x-redirects to plain `http` cannot silently downgrade the transport: with the limit at 0,
/// ureq returns the 3xx response unfollowed and `http_get_index` rejects it (AGENTS.md §10.6).
fn index_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(HTTP_CONNECT_TIMEOUT)
            .timeout_read(HTTP_IO_TIMEOUT)
            .timeout_write(HTTP_IO_TIMEOUT)
            .redirects(0)
            .build()
    })
}

/// GETs `url` with bounded retries: transport errors (DNS, connect, reset, timeout) and 5xx
/// responses are retried up to [`HTTP_MAX_ATTEMPTS`] with a short linear backoff, since they are
/// usually transient. A 4xx (including 404) returns immediately so callers can act on it.
fn http_get(agent: &ureq::Agent, url: &str) -> Result<ureq::Response, Box<ureq::Error>> {
    http_get_bounded(agent, url, None)
}

/// [`http_get`] with an optional overall wall-clock budget shared across all attempts. Each
/// attempt's request timeout is clamped to the remaining budget (covering DNS and connect,
/// which the agent's per-phase timeouts do not), and retries stop once the budget is spent.
fn http_get_bounded(
    agent: &ureq::Agent,
    url: &str,
    budget: Option<Duration>,
) -> Result<ureq::Response, Box<ureq::Error>> {
    let start = std::time::Instant::now();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut request = agent.get(url);
        if let Some(budget) = budget {
            let remaining = budget.saturating_sub(start.elapsed());
            request = request.timeout(remaining.max(Duration::from_millis(100)));
        }
        match request.call() {
            Ok(response) => return Ok(response),
            Err(err)
                if attempt < HTTP_MAX_ATTEMPTS
                    && is_retryable(&err)
                    && budget.is_none_or(|budget| start.elapsed() < budget) =>
            {
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

/// Fetches a small companion file (e.g. a detached `.minisig`) as text, over the same transport
/// the artifact it accompanies came from: an `http(s)` URL is downloaded, anything else is read
/// relative to `base_dir`. No content hash is required — a detached signature is verified against
/// the already-checksummed artifact it signs, so a tampered signature simply fails verification.
pub fn fetch_companion_text(location: &str, base_dir: &Path) -> Result<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        let response =
            http_get(http_agent(), location).map_err(|err| http_error(location, *err))?;
        response
            .into_string()
            .with_context(|| format!("read {location}"))
    } else {
        let path = local_source_path(location, base_dir);
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    }
}

fn download_into(url: &str, base_dir: &Path, destination: &Path) -> Result<()> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let response = http_get(http_agent(), url).map_err(|err| http_error(url, *err))?;
        let total: Option<u64> = response
            .header("Content-Length")
            .and_then(|v| v.parse().ok());
        let mut reader = response.into_reader();
        let mut file = File::create(destination)?;

        const CHUNK: usize = 64 * 1024;
        const REPORT_INTERVAL: u64 = 1024 * 1024; // update spinner every 1 MiB
        let mut buf = vec![0u8; CHUNK];
        let mut downloaded: u64 = 0;
        let mut since_report: u64 = 0;

        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            downloaded += n as u64;
            since_report += n as u64;

            if since_report >= REPORT_INTERVAL {
                since_report = 0;
                let progress = match total {
                    Some(t) => format!(
                        "{} / {} downloaded",
                        format_bytes(downloaded),
                        format_bytes(t)
                    ),
                    None => format!("{} downloaded", format_bytes(downloaded)),
                };
                status(&format!("fetching ({url}) — {progress}"));
            }
        }
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

fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if n == 0 {
        return "0 B".to_string();
    }
    let exp = (n as f64).log(1024.0).min(UNITS.len() as f64 - 1.0) as usize;
    let value = n as f64 / 1024f64.powi(exp as i32);
    if exp == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[exp])
    }
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

#[cfg(test)]
mod tests {
    use super::require_https_for_index;

    #[test]
    fn https_and_loopback_indexes_are_allowed() {
        for url in [
            "https://example.com/index.nuon",
            "http://127.0.0.1:8080/index.nuon",
            "http://localhost/index.nuon",
            "http://[::1]:8080/index.nuon",
        ] {
            assert!(require_https_for_index(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn plain_http_indexes_are_refused() {
        for url in [
            "http://example.com/index.nuon",
            "http://127.0.0.1.evil.com/index.nuon",
            "ftp://example.com/index.nuon",
        ] {
            assert!(require_https_for_index(url).is_err(), "{url}");
        }
    }

    #[test]
    fn userinfo_loopback_spoof_is_refused() {
        // Embedded credentials whose host *looks* like loopback but whose real connect host is
        // arbitrary must not slip through the loopback exemption (AGENTS.md §10.6).
        for url in [
            "http://[::1]@evil.com/index.nuon",
            "http://127.0.0.1@evil.com/index.nuon",
            "http://localhost@evil.com/index.nuon",
            "http://[::1]:8080@evil.com/index.nuon",
        ] {
            assert!(require_https_for_index(url).is_err(), "{url}");
        }
    }
}
