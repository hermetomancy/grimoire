//! Extracting validated archives and normalising what lands on disk: setuid/setgid/sticky
//! bits are stripped so an archive can never smuggle in privilege-escalation modes.

use anyhow::{Context, Result};
use std::{fs, fs::File, path::Path};

/// Extracts a `.tar.zst` archive into `destination`. Callers must have validated the
/// archive first ([`super::validate_archive_paths`]); extraction itself then sanitizes
/// permissions on everything it wrote.
pub(crate) fn extract_archive(path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(destination)?;
    sanitize_permissions(destination)
        .with_context(|| format!("sanitize permissions in {}", destination.display()))?;
    Ok(())
}

/// Strips setuid, setgid, and sticky bits from every regular file under `dir`.
pub(crate) fn sanitize_permissions(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = metadata.permissions();
                let mode = perms.mode();
                if mode & 0o7000 != 0 {
                    perms.set_mode(mode & !0o7000);
                    fs::set_permissions(&path, perms)?;
                }
            }
        } else if metadata.is_dir() {
            sanitize_permissions(&path)?;
        }
    }
    Ok(())
}
