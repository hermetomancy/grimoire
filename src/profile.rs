//! Profile and generation management.
//!
//! A profile is the user-facing view into the store. Each generation is a real directory tree
//! containing hard links into store paths. The active generation is selected by a single symlink:
//! `profiles/current -> gen-N`.
//!
//! Because Grimoire binaries bake absolute store paths (RPATH, install_name, pkg-config prefix),
//! generations only need to surface executables and human-facing artifacts: `bin/`, `share/man/`,
//! shell completions, and desktop files. Everything else stays in the store and is found via baked
//! absolute paths.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    model::PackageState,
    nu::nuon_io,
    paths,
    progress::{report, status, success},
};

/// Subdirectories scanned for human-facing artifacts (man pages, completions, desktop files)
/// that are not explicitly declared as bins.
const PROFILE_SHARE_SUBDIRS: &[&str] = &[
    "share/man",
    "share/bash-completion/completions",
    "share/zsh/site-functions",
    "share/fish/vendor_completions.d",
    "share/applications",
];

/// Metadata for a single generation, stored as `gen.nuon` inside the generation directory.
#[derive(Debug, Clone)]
pub struct Generation {
    pub id: u64,
    pub created: u64,
    pub packages: Vec<String>,
    pub store_paths: Vec<String>,
}

/// The directory that holds the actual generation trees (hard links into the store).
pub fn profiles_dir() -> Result<PathBuf> {
    paths::profiles_dir()
}

/// The user-facing profile directory that holds the `current` symlink.
pub fn user_profiles_dir() -> Result<PathBuf> {
    paths::user_profiles_dir()
}

/// The symlink that points to the active generation directory.
pub fn current_profile_link() -> Result<PathBuf> {
    Ok(user_profiles_dir()?.join("current"))
}

/// The directory for a specific generation.
pub fn generation_dir(id: u64) -> Result<PathBuf> {
    Ok(profiles_dir()?.join(format!("gen-{id}")))
}

/// Returns the ID of the currently active generation, if one exists.
pub fn current_generation_id() -> Result<Option<u64>> {
    let link = current_profile_link()?;
    if !link.exists() {
        return Ok(None);
    }
    let target = fs::read_link(&link)
        .with_context(|| format!("read current profile link {}", link.display()))?;
    parse_generation_id(
        target
            .file_name()
            .and_then(|n| n.to_str())
            .context("current profile link target has no name")?,
    )
    .map(Some)
}

/// Creates a new generation from the given package states and atomically activates it.
pub fn rebuild_and_activate(states: &[PackageState]) -> Result<u64> {
    let id = create_generation(states)?;
    activate_generation(id)?;
    Ok(id)
}

/// Creates a new generation directory from the given package states and returns its ID.
///
/// The generation is built by hard-linking profile-relevant files (`bin/`, `share/man/`, etc.)
/// from each package's store path into the generation directory.
pub fn create_generation(states: &[PackageState]) -> Result<u64> {
    fs::create_dir_all(profiles_dir()?)?;

    let next_id = next_generation_id()?;
    let gen_dir = generation_dir(next_id)?;
    if gen_dir.exists() {
        fs::remove_dir_all(&gen_dir)?;
    }
    fs::create_dir_all(&gen_dir)?;

    status(&format!("building generation {next_id}"));

    for state in states {
        let store_path = PathBuf::from(&state.store_path);
        if !store_path.exists() {
            report(&format!(
                "warning: store path {} does not exist, skipping",
                store_path.display()
            ));
            continue;
        }
        link_package_into_generation(state, &gen_dir)?;
    }

    let generation = Generation {
        id: next_id,
        created: unix_now(),
        packages: states.iter().map(|s| s.name.clone()).collect(),
        store_paths: states.iter().map(|s| s.store_path.clone()).collect(),
    };
    write_generation_metadata(&gen_dir, &generation)?;

    let mut registry = read_registry().unwrap_or_default();
    registry.push(generation);
    if let Err(e) = write_registry(&registry) {
        report(&format!(
            "warning: could not write generations registry: {e}"
        ));
    }

    success(&format!("created generation {next_id}"));
    Ok(next_id)
}

/// Atomically switches the active profile to the given generation.
pub fn activate_generation(id: u64) -> Result<()> {
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        bail!("generation {id} does not exist");
    }
    if current_generation_id()? == Some(id) {
        report(&format!("generation {id} is already active"));
        return Ok(());
    }
    let current = current_profile_link()?;
    let parent = current
        .parent()
        .context("current profile link should have a parent")?;
    fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(".current-{id}"));
    std::os::unix::fs::symlink(&gen_dir, &tmp)
        .with_context(|| format!("stage current symlink -> {}", gen_dir.display()))?;
    fs::rename(&tmp, &current).with_context(|| format!("activate generation {id}"))?;

    report(&format!("activated generation {id}"));
    Ok(())
}

/// Lists all retained generations, newest first.
///
/// Reads the canonical `state/generations.nuon` registry. Entries whose directories no longer
/// exist are pruned, and any generation directories on disk that are missing from the registry
/// are discovered and added. The registry is rewritten when it diverges from disk.
pub fn list_generations() -> Result<Vec<Generation>> {
    let mut generations = read_registry().unwrap_or_default();
    let mut changed = false;

    // Drop registry entries whose directories no longer exist.
    let before = generations.len();
    generations.retain(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false));
    if generations.len() != before {
        changed = true;
    }

    // Scan for generation directories on disk that are not yet in the registry.
    let dir = profiles_dir()?;
    if dir.exists() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "current" || !name.starts_with("gen-") {
                continue;
            }
            let gen_path = entry.path();
            if !gen_path.join("gen.nuon").exists() {
                continue;
            }
            let Ok(id) = parse_generation_id(name) else {
                continue;
            };
            if generations.iter().any(|g| g.id == id) {
                continue;
            }
            match read_generation_metadata(&gen_path) {
                Ok(g) => {
                    generations.push(g);
                    changed = true;
                }
                Err(e) => report(&format!(
                    "warning: could not read generation metadata {}: {e}",
                    gen_path.display()
                )),
            }
        }
    }

    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if changed {
        if let Err(e) = write_registry(&generations) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
    }

    Ok(generations)
}

/// Rolls back to the previous generation (the newest generation older than the current one).
/// Returns the ID of the generation rolled back to.
pub fn rollback() -> Result<u64> {
    let current = current_generation_id()?.context("no active generation to roll back from")?;
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    let previous = generations
        .into_iter()
        .find(|g| g.id < current)
        .context("no previous generation to roll back to")?;

    activate_generation(previous.id)?;
    report(&format!(
        "rolled back from generation {current} to {}",
        previous.id
    ));
    Ok(previous.id)
}

/// Garbage-collects unreferenced store paths and old generations.
///
/// Keeps the `keep` most recent generations (including the current one), deletes older
/// generations, then deletes any store path not referenced by a retained generation.
pub fn gc(keep: usize) -> Result<()> {
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if generations.is_empty() {
        report("no generations to collect");
        return Ok(());
    }

    let current = current_generation_id()?;
    let to_retain: BTreeSet<u64> = generations.iter().take(keep).map(|g| g.id).collect();

    let freed_generations = delete_old_generations(&generations, &to_retain, current)?;
    prune_registry()?;
    let referenced = collect_referenced_paths(&to_retain)?;
    let freed_stores = collect_unreferenced_stores(&referenced)?;

    report_gc_result(freed_stores, freed_generations);
    Ok(())
}

fn delete_old_generations(
    generations: &[Generation],
    to_retain: &BTreeSet<u64>,
    current: Option<u64>,
) -> Result<usize> {
    let mut freed = 0;
    for g in generations {
        if to_retain.contains(&g.id) {
            continue;
        }
        // Never delete the current generation unless explicitly told to
        if Some(g.id) == current {
            continue;
        }
        let dir = generation_dir(g.id)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
            freed += 1;
        }
    }
    if freed > 0 {
        report(&format!("removed {freed} old generation(s)"));
    }
    Ok(freed)
}

fn prune_registry() -> Result<()> {
    let mut registry = read_registry().unwrap_or_default();
    let before = registry.len();
    registry.retain(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false));
    if registry.len() != before {
        if let Err(e) = write_registry(&registry) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
    }
    Ok(())
}

fn collect_referenced_paths(to_retain: &BTreeSet<u64>) -> Result<BTreeSet<String>> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for id in to_retain {
        let dir = generation_dir(*id)?;
        let meta = dir.join("gen.nuon");
        if meta.exists() {
            if let Ok(g) = read_generation_metadata(&dir) {
                referenced.extend(g.store_paths);
            }
        }
    }
    Ok(referenced)
}

fn collect_unreferenced_stores(referenced: &BTreeSet<String>) -> Result<usize> {
    let store_root = paths::store_root()?;
    if !store_root.exists() {
        report("store root does not exist, nothing to collect");
        return Ok(0);
    }

    let mut freed = 0;
    for entry in fs::read_dir(&store_root)? {
        let entry = entry?;
        let path = entry.path();
        let path_str = path.display().to_string();
        if referenced.contains(&path_str) {
            continue;
        }
        let size = du(&path)?;
        fs::remove_dir_all(&path)?;
        report(&format!(
            "collected {} ({:.2} MiB)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            size as f64 / (1024.0 * 1024.0)
        ));
        freed += 1;
    }
    Ok(freed)
}

fn report_gc_result(freed_stores: usize, freed_generations: usize) {
    if freed_stores == 0 && freed_generations == 0 {
        report("nothing to collect");
    } else {
        report(&format!(
            "gc complete: removed {freed_stores} store path(s) and {freed_generations} generation(s)"
        ));
    }
}

/// Deletes a specific generation by ID.
///
/// Fails if the generation does not exist or if it is the currently active generation.
/// Removes the generation directory and syncs the registry.
pub fn delete_generation(id: u64) -> Result<()> {
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        bail!("generation {id} does not exist");
    }
    if current_generation_id()? == Some(id) {
        bail!(
            "cannot delete generation {id}: it is the currently active generation; \
             switch to another generation first"
        );
    }
    fs::remove_dir_all(&gen_dir)
        .with_context(|| format!("remove generation directory {}", gen_dir.display()))?;

    let mut registry = read_registry().unwrap_or_default();
    let before = registry.len();
    registry.retain(|g| g.id != id);
    if registry.len() != before {
        if let Err(e) = write_registry(&registry) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
    }

    report(&format!("deleted generation {id}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn next_generation_id() -> Result<u64> {
    let mut max = 0u64;
    let dir = profiles_dir()?;
    if !dir.exists() {
        return Ok(1);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "current" {
            continue;
        }
        if let Ok(id) = parse_generation_id(name) {
            max = max.max(id);
        }
    }
    Ok(max + 1)
}

fn parse_generation_id(name: &str) -> Result<u64> {
    let id_str = name
        .strip_prefix("gen-")
        .with_context(|| format!("generation name `{name}` is not `gen-<id>`"))?;
    id_str
        .parse::<u64>()
        .with_context(|| format!("generation id `{id_str}` is not a number"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn link_package_into_generation(state: &PackageState, gen_dir: &Path) -> Result<()> {
    let store_path = PathBuf::from(&state.store_path);

    // Link declared bins into the generation's bin/ directory.
    // The bin name in the profile is the key from `state.bins`; the source path is the value.
    for (bin_name, bin_path) in &state.bins {
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
            bail!(
                "bin `{bin_name}` from `{}` collides with an earlier package in this generation. \
                 To fix: remove or upgrade the other package, or adjust its binaries to avoid overlap.",
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
fn clone_or_hard_link(src: &Path, dst: &Path) -> Result<()> {
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
fn is_cow_unsupported(err: &anyhow::Error) -> bool {
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
fn try_cow_clone(src: &Path, dst: &Path) -> Result<()> {
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
fn try_cow_clone(src: &Path, dst: &Path) -> Result<()> {
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
fn try_cow_clone(_src: &Path, _dst: &Path) -> Result<()> {
    bail!("CoW cloning not supported on this platform")
}

/// Recursively hard-links files from `src` into `dst`, preserving directory structure.
fn link_tree(src: &Path, dst: &Path) -> Result<()> {
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

fn write_generation_metadata(gen_dir: &Path, generation: &Generation) -> Result<()> {
    let mut record = nu_protocol::Record::new();
    record.push(
        "id",
        nu_protocol::Value::int(generation.id as i64, nu_protocol::Span::unknown()),
    );
    record.push(
        "created",
        nu_protocol::Value::int(generation.created as i64, nu_protocol::Span::unknown()),
    );
    record.push(
        "packages",
        crate::model::string_list_value(&generation.packages),
    );
    record.push(
        "store_paths",
        crate::model::string_list_value(&generation.store_paths),
    );
    let value = nu_protocol::Value::record(record, nu_protocol::Span::unknown());
    let path = gen_dir.join("gen.nuon");
    nuon_io::write_nuon(&path, &value)
}

fn read_generation_metadata(gen_dir: &Path) -> Result<Generation> {
    let path = gen_dir.join("gen.nuon");
    let value = nuon_io::read_nuon(&path)?;
    let record = crate::model::expect_record(value, "generation metadata")?;
    let id = crate::model::required_field_i64(&record, "generation metadata", "id")? as u64;
    let created = crate::model::optional_i64(&record, "created")?.unwrap_or(0) as u64;
    let packages = crate::model::optional_string_list(&record, "packages")?;
    let store_paths = crate::model::optional_string_list(&record, "store_paths")?;
    Ok(Generation {
        id,
        created,
        packages,
        store_paths,
    })
}

fn generations_registry_path() -> Result<PathBuf> {
    Ok(paths::install_root()?
        .join("state")
        .join("generations.nuon"))
}

fn read_registry() -> Result<Vec<Generation>> {
    let path = generations_registry_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value = nuon_io::read_nuon(&path)?;
    let nu_protocol::Value::List { vals, .. } = value else {
        bail!("generations registry is not a list");
    };
    let mut generations = Vec::with_capacity(vals.len());
    for val in vals {
        let record = crate::model::expect_record(val, "generation registry entry")?;
        let id =
            crate::model::required_field_i64(&record, "generation registry entry", "id")? as u64;
        let created = crate::model::optional_i64(&record, "created")?.unwrap_or(0) as u64;
        let packages = crate::model::optional_string_list(&record, "packages")?;
        let store_paths = crate::model::optional_string_list(&record, "store_paths")?;
        generations.push(Generation {
            id,
            created,
            packages,
            store_paths,
        });
    }
    Ok(generations)
}

fn write_registry(generations: &[Generation]) -> Result<()> {
    let path = generations_registry_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let span = nu_protocol::Span::unknown();
    let items: Vec<nu_protocol::Value> = generations
        .iter()
        .map(|g| {
            let mut record = nu_protocol::Record::new();
            record.push("id", nu_protocol::Value::int(g.id as i64, span));
            record.push("created", nu_protocol::Value::int(g.created as i64, span));
            record.push("packages", crate::model::string_list_value(&g.packages));
            record.push(
                "store_paths",
                crate::model::string_list_value(&g.store_paths),
            );
            nu_protocol::Value::record(record, span)
        })
        .collect();
    let value = nu_protocol::Value::list(items, span);
    nuon_io::write_nuon(&path, &value)
}

/// Rough disk usage of a directory in bytes (follows hard links, so it may overcount).
fn du(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}
