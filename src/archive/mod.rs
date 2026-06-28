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

pub(crate) const MAX_ARCHIVE_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const MAX_ARCHIVE_MEMBERS: usize = 100_000;
/// Source archives carry far more members than any built package: the LLVM monorepo source ships
/// ~185k files. Their trust boundary is the rune's pinned `sha256`, not this count — the maintainer
/// already committed to that exact upstream tarball — so the cap here is only a sanity bound against
/// a pathological inode-exhausting source, set well above the largest real one rather than at the
/// strict package-archive limit.
pub(crate) const MAX_SOURCE_ARCHIVE_MEMBERS: usize = 500_000;
pub(crate) const MAX_CAPTURED_MEMBER_BYTES: u64 = 1024 * 1024;

pub(crate) struct BoundedReader<R> {
    inner: R,
    limit: u64,
    read: u64,
    label: &'static str,
}

impl<R> BoundedReader<R> {
    pub(crate) fn new(inner: R, limit: u64, label: &'static str) -> Self {
        Self {
            inner,
            limit,
            read: 0,
            label,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.read);
        if remaining == 0 {
            let mut byte = [0_u8; 1];
            return match self.inner.read(&mut byte)? {
                0 => Ok(0),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} exceeds {} bytes", self.label, self.limit),
                )),
            };
        }
        let max = remaining.min(buf.len() as u64) as usize;
        let read = self.inner.read(&mut buf[..max])?;
        self.read += read as u64;
        if self.read > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} exceeds {} bytes", self.label, self.limit),
            ));
        }
        Ok(read)
    }
}

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

/// Copies `src` to `dst` while hashing the stream, so staging an archive into a transaction
/// and computing its content hash is a single read instead of two.
pub fn copy_hashed(src: &Path, dst: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(src)?);
    let mut writer = File::create(dst)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        std::io::Write::write_all(&mut writer, &buf[..read])?;
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

pub fn archive_hash(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher)?;
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
