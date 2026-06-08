//! Set up the fixed Grimoire store directory (`/grm` on POSIX systems).
//!
//! When `GRIMOIRE_ROOT` is set, the store lives under the install root and no system-wide setup
//! is needed. Otherwise this command creates the fixed store directory that makes baked absolute
//! paths portable across users and machines.

use anyhow::{Context, Result, bail};
use std::{env, fs, os::unix::ffi::OsStrExt, path::Path};

use crate::paths;

pub fn setup() -> Result<()> {
    if env::var_os("GRIMOIRE_ROOT").is_some() {
        let root = paths::install_root()?;
        println!(
            "GRIMOIRE_ROOT is set; using {} as the store root. No system-wide setup needed.",
            root.display()
        );
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    return setup_macos();

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    return setup_linux();
}

fn setup_posix(path: &Path) -> Result<()> {
    if path.exists() {
        if is_writable(path)? {
            println!("Grimoire store {} is already set up.", path.display());
            return Ok(());
        }
        if let Some((uid, gid)) = sudo_identity() {
            chown_path(path, uid, gid)?;
            println!("Made {} writable for the invoking user.", path.display());
            return Ok(());
        }
        bail!(
            "{} exists but is not writable. Run: sudo chown $(whoami): {}",
            path.display(),
            path.display()
        );
    }

    fs::create_dir_all(path)
        .with_context(|| format!("create {} (try running with sudo)", path.display()))?;

    if let Some((uid, gid)) = sudo_identity() {
        chown_path(path, uid, gid)?;
        println!(
            "Created {} and made it owned by the invoking user.",
            path.display()
        );
    } else {
        println!("Created {} (owned by root).", path.display());
        println!(
            "To make it user-writable, run: sudo chown $(whoami): {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn setup_linux() -> Result<()> {
    setup_posix(Path::new("/grm"))
}

#[cfg(target_os = "macos")]
fn setup_macos() -> Result<()> {
    let path = Path::new("/grm");

    if path.exists() {
        return setup_posix(path);
    }

    let synthetic = Path::new("/etc/synthetic.conf");
    let marker = "grm";

    let content = if synthetic.exists() {
        fs::read_to_string(synthetic).with_context(|| format!("read {}", synthetic.display()))?
    } else {
        String::new()
    };

    if content
        .lines()
        .any(|line| line.split_whitespace().next() == Some(marker))
    {
        bail!(
            "'{marker}' is already registered in {} but {} does not exist yet. \
             Reboot your Mac, then rerun `grm setup` if needed.",
            synthetic.display(),
            path.display()
        );
    }

    let mut new_content = content.clone();
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str("grm\n");

    let temp = synthetic.with_extension("grimoire-tmp");
    fs::write(&temp, new_content).with_context(|| format!("write temporary {}", temp.display()))?;
    fs::rename(&temp, synthetic)
        .with_context(|| format!("atomically update {}", synthetic.display()))?;

    println!("Added '{marker}' to {}.", synthetic.display());
    println!(
        "Reboot your Mac. After reboot, {} will exist.",
        path.display()
    );
    println!("Then rerun `grm setup` to adjust permissions, or run:");
    println!("  sudo chown $(whoami): {}", path.display());
    Ok(())
}

/// Best-effort check whether the current process can write into `dir`.
/// Returns `false` if `dir` is a symlink to prevent following it to an arbitrary target.
fn is_writable(dir: &Path) -> Result<bool> {
    if fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let probe = dir.join(".grimoire-write-test");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Returns the uid/gid of the user that invoked sudo, if available.
fn sudo_identity() -> Option<(u32, u32)> {
    let uid = env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
    let gid = env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
    Some((uid, gid))
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("invalid path {}", path.display()))?;
    // SAFETY: lchown is a POSIX syscall; c_path is a valid NUL-terminated string.
    let rc = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        bail!(
            "lchown {} to uid {uid} gid {gid}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_writable_detects_writable_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert!(is_writable(temp.path()).unwrap());
        assert!(!temp.path().join(".grimoire-write-test").exists());
    }

    #[test]
    fn is_writable_detects_non_writable_directory() {
        // A non-existent path is not writable.
        assert!(!is_writable(Path::new("/does/not/exist/.grimoire-test")).unwrap());
    }
}
