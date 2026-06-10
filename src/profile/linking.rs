//! Materializing a generation: linking declared bins (with `grm prefer` resolving
//! contested names) and share/ trees from store paths via CoW clone or hard link.

use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    model::PackageState, model::preferences::Preferences, util::paths, util::progress::report,
};

/// Subdirectories scanned for human-facing artifacts (man pages, completions, desktop files)
/// that are not explicitly declared as bins.
pub(crate) const PROFILE_SHARE_SUBDIRS: &[&str] = &[
    "share/man",
    "share/bash-completion/completions",
    "share/zsh/site-functions",
    "share/fish/vendor_completions.d",
    "share/applications",
];

/// For each bin name declared by more than one package, applies the user's `grm prefer`
/// choice: the preferred package keeps the bin, every other claimant gets it added to its
/// skip set. A contested bin with no applicable preference is an error naming the contenders,
/// so the failure is order-independent and actionable.
pub(crate) fn contested_bin_skips(
    states: &[PackageState],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut owners: BTreeMap<&str, Vec<&PackageState>> = BTreeMap::new();
    for state in states {
        for bin_name in state.bins.keys() {
            owners.entry(bin_name).or_default().push(state);
        }
    }
    let preferences = Preferences::load().unwrap_or_default();
    let mut skips: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (bin_name, claimants) in owners {
        if claimants.len() < 2 {
            continue;
        }
        let preferred = preferences
            .providers
            .get(bin_name)
            .and_then(|p| claimants.iter().find(|s| s.name == *p));
        let Some(winner) = preferred else {
            let names: Vec<&str> = claimants.iter().map(|s| s.name.as_str()).collect();
            bail!(
                "bin `{bin_name}` is provided by multiple installed packages ({}); \
                 run `grm prefer {bin_name} <package>` to choose which one provides it",
                names.join(", ")
            );
        };
        for state in &claimants {
            if state.name != winner.name {
                skips
                    .entry(state.name.clone())
                    .or_default()
                    .insert(bin_name.to_owned());
            }
        }
    }
    Ok(skips)
}

pub(crate) fn link_package_into_generation(
    state: &PackageState,
    gen_dir: &Path,
    skip_bins: &BTreeSet<String>,
) -> Result<()> {
    let store_path = PathBuf::from(&state.store_path);
    let store_root = paths::store_root()?;
    if !store_path.starts_with(&store_root) {
        bail!(
            "package `{}` store path `{}` is outside the store root `{}`; refusing to link",
            state.name,
            store_path.display(),
            store_root.display()
        );
    }

    // Link declared bins into the generation's bin/ directory.
    // The bin name in the profile is the key from `state.bins`; the source path is the value.
    for (bin_name, bin_path) in &state.bins {
        // This package lost the contested bin to a `grm prefer` choice; the winner links it.
        if skip_bins.contains(bin_name) {
            continue;
        }
        let src = store_path.join(bin_path);
        if !src.exists() {
            report(&format!(
                "warning: declared bin `{bin_name}` points to missing file `{}` in {}",
                bin_path,
                store_path.display()
            ));
            continue;
        }
        let dst = gen_dir.join("bin").join(bin_name);
        if dst.exists() {
            // Backstop only: contested declared bins are resolved order-independently by
            // `contested_bin_skips` before linking starts.
            bail!(
                "bin `{bin_name}` from `{}` collides with an earlier package in this generation. \
                 To fix: run `grm prefer {bin_name} <package>` to choose a provider, or remove \
                 the other package.",
                state.name
            );
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        clone_or_hard_link(&src, &dst)
            .with_context(|| format!("link {} -> {}", dst.display(), src.display()))?;
    }

    // Scan share/ subdirectories for human-facing artifacts (man pages, completions, etc.)
    for subdir in PROFILE_SHARE_SUBDIRS {
        let src = store_path.join(subdir);
        if !src.exists() {
            continue;
        }
        let dst = gen_dir.join(subdir);
        link_tree(&src, &dst)?;
    }
    Ok(())
}

/// Try a CoW clone (APFS `clonefile` on macOS, `FICLONE` on Linux), falling back to a hard link
/// when the filesystem or platform does not support it.
pub(crate) fn clone_or_hard_link(src: &Path, dst: &Path) -> Result<()> {
    if let Err(e) = try_cow_clone(src, dst) {
        if !is_cow_unsupported(&e) {
            return Err(e);
        }
    } else {
        return Ok(());
    }
    fs::hard_link(src, dst)
        .with_context(|| format!("hard link {} -> {}", dst.display(), src.display()))
}

/// Whether an error indicates that CoW cloning is unsupported on this filesystem.
pub(crate) fn is_cow_unsupported(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.root_cause().downcast_ref::<std::io::Error>() {
        matches!(
            io_err.raw_os_error(),
            Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) | Some(libc::EINVAL)
        )
    } else {
        false
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn try_cow_clone(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let src_c = std::ffi::CString::new(src.as_os_str().as_bytes())
        .with_context(|| format!("convert src path to C string: {}", src.display()))?;
    let dst_c = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .with_context(|| format!("convert dst path to C string: {}", dst.display()))?;
    // SAFETY: `clonefile` is a read-only CoW clone syscall. `src_c` and `dst_c` are valid
    // NUL-terminated CStrings that outlive the call, derived from existing paths.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn try_cow_clone(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let src_file =
        fs::File::open(src).with_context(|| format!("open src for reflink: {}", src.display()))?;
    let dst_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .with_context(|| format!("open dst for reflink: {}", dst.display()))?;

    let src_fd = src_file.as_raw_fd();
    let dst_fd = dst_file.as_raw_fd();
    // FICLONE = _IOW(0x94, 9, int)
    const FICLONE: libc::c_ulong = 0x40049409;

    // SAFETY: `ioctl(FICLONE)` is a reflink operation. `src_fd` and `dst_fd` are valid,
    // owned file descriptors that outlive the call. `FICLONE` is the correct ioctl constant
    // for Linux reflink (`_IOW(0x94, 9, int)`).
    let rc = unsafe { libc::ioctl(dst_fd, FICLONE, src_fd) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn try_cow_clone(_src: &Path, _dst: &Path) -> Result<()> {
    bail!("CoW cloning not supported on this platform")
}

/// Recursively hard-links files from `src` into `dst`, preserving directory structure.
pub(crate) fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        if path == src {
            continue;
        }
        let relative = path
            .strip_prefix(src)
            .with_context(|| format!("strip prefix from {}", path.display()))?;
        let target = dst.join(relative);

        let meta = entry.metadata()?;
        if meta.is_dir() {
            fs::create_dir_all(&target)?;
        } else if meta.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            // Remove any existing file so we don't fail on collision
            let _ = fs::remove_file(&target);
            clone_or_hard_link(path, &target)
                .with_context(|| format!("link {} -> {}", target.display(), path.display()))?;
        } else if meta.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let link_target = fs::read_link(path)?;
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(&link_target, &target).with_context(|| {
                format!("symlink {} -> {}", target.display(), link_target.display())
            })?;
        }
    }
    Ok(())
}
