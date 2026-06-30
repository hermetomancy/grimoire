//! Process-wide install-root and store locks.
//!
//! Two `grm install`/`remove`/`clean`/`tome …` runs mutate shared state; without
//! coordination they can race and corrupt it. A mutating command takes two advisory locks:
//!
//! - the **install-root lock** (`<install root>/.grimoire-lock`) serializes a single user's own
//!   per-user state (state/, bin/, transactions/, the lockfile, tomes/, generations);
//! - the **store lock** serializes mutations of the content-addressed store, which on the fixed
//!   `/grm/store` is *shared across users*. Without it, user B's `grm clean` could reclaim a store
//!   path only user A's generation references (GC reachability is per-user). It is a `flock` on the
//!   store directory itself, so no sentinel file lands in the store for GC to trip over.
//!
//! Both are held for the whole command and released when the fds close at process exit, so a crash
//! never leaves a stale lock. They are always acquired install-root-first, so two processes cannot
//! deadlock.

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};

use crate::util::paths;

const LOCK_FILE_NAME: &str = ".grimoire-lock";

/// RAII guard for the install-root and store locks. Dropping it releases both OS locks; the
/// install-root sentinel file is intentionally left on disk so a third process can't race in
/// between our unlock and a delete and end up holding a lock on a freshly-recreated inode.
pub struct InstallRootGuard {
    _file: File,
    /// The shared-store lock (a `flock` on the store directory). `None` only when the store does
    /// not exist yet, so first-run setup is not blocked.
    _store: Option<File>,
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

    // Then the shared-store lock. Order (install root first) is the same everywhere, so two
    // processes can never deadlock holding one another's lock.
    let store = acquire_store_lock()?;

    Ok(InstallRootGuard {
        _file: file,
        _store: store,
    })
}

/// Acquires an exclusive advisory lock on the content-addressed store so concurrent `grm`
/// processes — including different users on the fixed `/grm/store` — serialize store mutations and
/// GC. The lock is a `flock` on the store directory's own descriptor: no sentinel file is created
/// inside the store (which `grm clean` would otherwise see as an unreferenced entry). Returns
/// `None` when the store does not exist yet, so first-run installs and `grm setup` are not blocked.
fn acquire_store_lock() -> Result<Option<File>> {
    let store = paths::store_root()?;
    if !store.exists() {
        return Ok(None);
    }
    // Opening a directory read-only and `flock`-ing its descriptor is valid POSIX (the project is
    // POSIX-only, §11) and needs no write access to the store directory itself.
    let dir = File::open(&store).with_context(|| format!("open store {}", store.display()))?;
    let acquired = FileExt::try_lock_exclusive(&dir).context("acquire store lock")?;
    if !acquired {
        bail!(
            "another `grm` process is mutating the store {}; retry once it finishes",
            store.display()
        );
    }
    Ok(Some(dir))
}
