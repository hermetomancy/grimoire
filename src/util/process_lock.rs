//! Process-wide install-root lock.
//!
//! Two `grm install`/`remove`/`clean`/`tome …`/`addendum …` runs against the same install root
//! mutate shared state (store/, bin/, state/, transactions/, the lockfile, tomes/, addendums/);
//! without coordination they can race and corrupt that state. Mutating CLI entry points acquire
//! an exclusive OS-level advisory lock on `<install root>/.grimoire-lock` before doing any work
//! and hold it for the entire command — released automatically when the file descriptor closes
//! at process exit, so a crash never leaves a stale lock on disk.

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};

use crate::util::paths;

const LOCK_FILE_NAME: &str = ".grimoire-lock";

/// RAII guard for the install-root lock. Dropping it releases the OS lock; the sentinel file
/// itself is intentionally left on disk so a third process can't race in between our unlock
/// and a delete and end up holding a lock on a freshly-recreated inode.
pub struct InstallRootGuard {
    _file: File,
}

/// Acquires the install-root lock or fails fast with a diagnostic naming the holder. The lock
/// is exclusive: at most one mutating `grm` process can hold it per install root. Read-only
/// commands (`list`, `search`, `info`, `doctor`) do not take it.
pub fn acquire() -> Result<InstallRootGuard> {
    let root = paths::install_root()?;
    fs::create_dir_all(&root).with_context(|| format!("create install root {}", root.display()))?;
    let path = root.join(LOCK_FILE_NAME);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open install-root lock {}", path.display()))?;

    // fs4 0.13: `try_lock_exclusive` returns `Ok(true)` on acquisition, `Ok(false)` on
    // contention, and `Err` only for OS-level failures (bad fd, EIO, …).
    let acquired = FileExt::try_lock_exclusive(&file).context("acquire install-root lock")?;
    if !acquired {
        let holder = fs::read_to_string(&path).unwrap_or_default();
        let holder = holder.trim();
        let suffix = if holder.is_empty() {
            String::new()
        } else {
            format!("\n  held by: {holder}")
        };
        bail!(
            "another `grm` process is mutating {}; retry once it finishes{}",
            root.display(),
            suffix
        );
    }

    // Stamp our identity into the file so a contending process can name us in its error.
    // Best-effort: we already hold the lock, so a write failure is not fatal.
    // Write through the already-opened fd to avoid a symlink TOCTOU race.
    let _ = (|| {
        file.rewind().ok()?;
        file.set_len(0).ok()?;
        file.write_all(format!("pid {}", std::process::id()).as_bytes())
            .ok()
    })();

    Ok(InstallRootGuard { _file: file })
}
