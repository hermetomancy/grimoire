//! Archive integrity and safety primitives shared by installs and builds.
//!
//! Hashing and the [`verify_hash`] checkpoint enforce "verify before trust"; [`validate`]
//! rejects unsafe member paths before extraction (AGENTS.md §10.2–§10.3); [`unpack`]
//! extracts validated archives and sanitizes permissions; [`pack`] produces the `.tar.zst`
//! package archives source builds emit.

pub mod pack;
mod unpack;
mod validate;

pub(crate) use unpack::*;
pub use validate::*;

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

/// Hard-fails unless `actual` matches `expected`. This is the single checkpoint for the
/// "verify before trust" rule: callers verify an archive's integrity here *before* reading
/// or extracting it. Both sides accept an optional `sha256:` prefix and are compared
/// case-insensitively over the hex digest.
pub fn verify_hash(actual: &str, expected: &str) -> Result<()> {
    if normalize_hash(actual) == normalize_hash(expected) {
        return Ok(());
    }
    bail!("archive hash mismatch: expected `{expected}`, computed `{actual}`");
}

fn normalize_hash(hash: &str) -> String {
    hash.strip_prefix("sha256:")
        .unwrap_or(hash)
        .trim()
        .to_ascii_lowercase()
}

pub fn archive_hash(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_hash_accepts_matching_digest() {
        assert!(verify_hash("sha256:abc123", "sha256:abc123").is_ok());
    }

    #[test]
    fn verify_hash_ignores_prefix_and_case() {
        assert!(verify_hash("sha256:ABC123", "abc123").is_ok());
        assert!(verify_hash("deadBEEF", "sha256:deadbeef").is_ok());
    }

    #[test]
    fn verify_hash_rejects_mismatch() {
        let err = verify_hash("sha256:abc123", "sha256:def456").unwrap_err();
        assert!(err.to_string().contains("hash mismatch"));
    }
}
