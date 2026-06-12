//! Realizing one package into the store: verify, validate, extract, promote, and record
//! its state and the lockfile — every step staged against a transaction.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    install::lock,
    model::{
        Dependency, PackageMetadata, PackageState, parse_version_relaxed,
        validate_relative_package_path, validate_sha256, validate_target,
    },
    nu::nuon_io,
    util::paths,
    util::progress::{faint, report, status, strong, success},
};

use super::*;

/// How a package reached the store, named in its result line so the user can tell a verified
/// prebuilt from a local source build at a glance.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InstallOrigin {
    /// A published substitute fetched from a binhost and verified against the signed index.
    Prebuilt,
    /// Built from its rune on this machine.
    Source,
    /// A local archive handed to `grm install` directly.
    LocalArchive,
    /// A build dependency cached store-only, never linked into the profile.
    BuildDep,
    /// A `grm tome build` product installed store-only for subsequent builds.
    TomeBuild,
}

impl InstallOrigin {
    fn describe(self) -> &'static str {
        match self {
            InstallOrigin::Prebuilt => "prebuilt, checksum verified",
            InstallOrigin::Source => "built from source",
            InstallOrigin::LocalArchive => "local archive",
            InstallOrigin::BuildDep => "build dependency, store-only",
            InstallOrigin::TomeBuild => "built, store-only",
        }
    }
}

/// The installed package's name, concrete version, and the runtime dependencies declared in its
/// embedded metadata, returned so the caller can record it as installed and resolve those deps.
pub(crate) struct InstalledArchive {
    pub name: String,
    pub version: Version,
    pub runtime_deps: Vec<Dependency>,
    /// Post-install notes from the embedded metadata, surfaced once the whole command lands.
    pub notes: Vec<String>,
}

/// Whether the exact install a plan step describes has already landed: recorded state matches
/// the step's name, version, and (when computed) content address, and the recorded store path
/// still exists on disk. Plans go stale — a deeper build-dependency recursion can realize a
/// package after the plan holding it was resolved — and re-realizing a stale step refetches or,
/// far worse, rebuilds it from source.
///
/// A store directory *without* a matching state record is deliberately not reused: state is
/// written only after hash verification inside a committed transaction, so an unrecorded
/// directory cannot be re-trusted (AGENTS.md §10.1) and the step realizes normally.
pub(crate) fn step_already_realized(step: &crate::solve::PlanStep) -> Result<bool> {
    let states = installed_states()?;
    let Some(state) = states.iter().find(|state| state.name == step.name) else {
        return Ok(false);
    };
    if state.version != step.version.to_string() {
        return Ok(false);
    }
    if let Some(hash) = step.store_hash.as_deref()
        && state.store_hash != hash
    {
        return Ok(false);
    }
    Ok(Path::new(&state.store_path).exists())
}

/// Verifies, extracts, and promotes the resolved archive into the install root, then rebuilds
/// the lockfile. `expected_hash` is the hash the archive must match before it is read (from
/// `--sha256` or a tome index entry); `None` skips the check, which only happens for a local
/// archive installed without `--sha256`.
pub(crate) fn install_archive(
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
    origin: InstallOrigin,
) -> Result<InstalledArchive> {
    let installed = install_store_only(archive_path, expected_hash, expected_store_hash, origin)?;
    let mut tx = Transaction::new();
    rebuild_lock(&mut tx)?;
    tx.commit();
    Ok(installed)
}

/// Like [`install_archive`], but does **not** rebuild the lockfile or activate a generation.
/// Used by `grm tome build` to make built packages available as build dependencies for subsequent
/// builds without polluting the user's active profile.
pub(crate) fn install_store_only(
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
    origin: InstallOrigin,
) -> Result<InstalledArchive> {
    if !archive_path.exists() {
        bail!(
            "package archive `{}` does not exist",
            archive_path.display()
        );
    }
    if let Some(expected) = &expected_hash {
        validate_sha256(expected, "expected archive hash")?;
    }

    // Stage on the same filesystem as the store so `promote_package` can use an atomic rename.
    let store_root = paths::store_root()?;
    fs::create_dir_all(&store_root)?;
    let transaction = tempfile::Builder::new()
        .prefix("grimoire-")
        .tempdir_in(&store_root)?;
    let safe_archive = transaction.path().join("archive.tar.zst");

    // Verify integrity before the archive is read or extracted. A mismatch is fatal. The
    // hash is computed while staging the copy, and the embedded metadata is captured during
    // path validation, so the archive is read three times in total (stage, validate, extract)
    // instead of five.
    status("staging and hashing archive");
    let archive_hash = archive::copy_hashed(archive_path, &safe_archive)
        .with_context(|| format!("copy archive to transaction {}", safe_archive.display()))?;
    if let Some(expected) = &expected_hash {
        archive::verify_hash(&archive_hash, expected)
            .with_context(|| format!("verify archive {}", safe_archive.display()))?;
        success("archive hash verified");
    }

    status(&format!(
        "validating archive paths ({})",
        safe_archive.display()
    ));
    let metadata_text =
        archive::validate_archive_paths_capturing(&safe_archive, Some(".grimoire/package.nuon"))?
            .context("package archive is missing .grimoire/package.nuon")?;
    let metadata = PackageMetadata::from_value(nuon_io::parse_nuon(&metadata_text)?, true)?;
    validate_target(&metadata, &paths::target_triple())?;

    let root = paths::install_root()?;
    let (package_dir, store_hash) = resolve_store_dir(&metadata, expected_store_hash)?;

    let staging_dir = transaction.path().join("package");
    fs::create_dir_all(&staging_dir)?;

    status(&format!(
        "extracting into transaction ({})",
        transaction.path().display()
    ));
    archive::extract_archive(&safe_archive, &staging_dir)?;

    status("validating extracted files");
    validate_bins(&metadata, &paths::target_triple(), &staging_dir)?;

    // Everything from here mutates shared install state. Stage each step against a
    // transaction so that a failure restores the previously installed version.
    let mut tx = Transaction::new();

    status(&format!("promoting package to ({})", package_dir.display()));
    let replaced = promote_package(&mut tx, &staging_dir, &package_dir)?;

    status("writing package state");
    write_state(
        &mut tx,
        &root,
        &metadata,
        &paths::target_triple(),
        &archive_hash,
        &store_hash,
        &package_dir.to_string_lossy(),
    )?;

    tx.commit();
    if let Some(replaced) = replaced {
        let _ = fs::remove_dir_all(replaced);
    }

    report(&format!(
        "{} {}",
        strong(&format!("{} {}", metadata.name, metadata.version)),
        faint(&format!("— {}", origin.describe()))
    ));
    let version = parse_version_relaxed(&metadata.version)
        .with_context(|| format!("package version `{}` is not valid semver", metadata.version))?;
    let target = paths::target_triple();
    let runtime_deps: Vec<Dependency> = metadata
        .deps
        .runtime
        .into_iter()
        .filter(|d| d.matches_platform(&target))
        .collect();
    Ok(InstalledArchive {
        name: metadata.name,
        version,
        runtime_deps,
        notes: metadata.notes,
    })
}

pub(crate) fn inspect_archive(path: &Path) -> Result<PackageMetadata> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if normalize_archive_path(&entry.path()?) == ".grimoire/package.nuon" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return PackageMetadata::from_value(nuon_io::parse_nuon(&text)?, true);
        }
    }

    bail!("package archive is missing .grimoire/package.nuon");
}

/// Resolves the absolute store directory an archive installs into and its store hash.
///
/// Every archive records its content-addressed store basename (`<hash>-<name>-<version>`) in its
/// embedded `store_path`; the absolute location is `store_root()/<basename>`, derived locally so the
/// same archive lands in the same place on every host regardless of who built it. The hash is the
/// basename's leading component. When the caller knows the expected store hash — a tome index entry,
/// or a fresh source build — it is cross-checked so a tampered or mislabeled archive is refused.
pub(crate) fn resolve_store_dir(
    metadata: &PackageMetadata,
    expected_hash: Option<&str>,
) -> Result<(PathBuf, String)> {
    let Some(basename) = metadata.store_path.as_deref() else {
        bail!(
            "package `{}` metadata is missing its store_path basename",
            metadata.name
        );
    };
    validate_relative_package_path(basename, "metadata store_path")?;
    let suffix = format!("-{}-{}", metadata.name, metadata.version);
    let Some(hash) = basename.strip_suffix(&suffix) else {
        bail!(
            "package `{}` metadata store_path `{basename}` is not `<hash>-{}-{}`",
            metadata.name,
            metadata.name,
            metadata.version
        );
    };
    if hash.is_empty() || hash.contains('/') {
        bail!(
            "package `{}` metadata store_path `{basename}` has an invalid hash component",
            metadata.name
        );
    }
    if let Some(expected) = expected_hash
        && hash != expected
    {
        bail!(
            "package `{}` embeds store hash `{hash}` but its inputs hash to `{expected}`",
            metadata.name
        );
    }
    Ok((paths::store_root()?.join(basename), hash.to_string()))
}

/// Validates every archive member *before* extraction (AGENTS.md §10.2–§10.3): member paths must
/// stay inside the extraction root; symlinks are allowed only when their target also resolves
/// within the package (so a link can never point outside the install prefix), and no member may
/// be nested *under* a symlink (which would let extraction write through the link). Hard links
/// are still rejected outright. With these guarantees the subsequent `unpack` into a fresh
/// staging directory cannot be lured outside the destination.
pub(crate) fn validate_bins(
    metadata: &PackageMetadata,
    target: &str,
    package_dir: &Path,
) -> Result<()> {
    for (name, path) in metadata.bins_for(target) {
        validate_relative_package_path(&path, &format!("bin `{name}`"))?;
        let bin_path = package_dir.join(&path);
        if !bin_path.exists() {
            bail!("declared bin `{name}` points to missing file `{path}`");
        }
        make_executable(&bin_path)?;
    }
    Ok(())
}

/// Moves the staged package into its final location. If a previous version exists it is
/// renamed aside first; the backup path is returned so the caller can drop it once the
/// whole install commits. A registered rollback restores the previous version on failure.
pub(crate) fn promote_package(
    tx: &mut Transaction,
    staging_dir: &Path,
    package_dir: &Path,
) -> Result<Option<PathBuf>> {
    let parent = package_dir
        .parent()
        .context("package directory should have parent")?;
    fs::create_dir_all(parent)?;

    let backup = backup_path(package_dir)?;
    let had_previous = package_dir.exists();
    if had_previous {
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::rename(package_dir, &backup)
            .with_context(|| format!("back up existing package {}", package_dir.display()))?;
    }

    // Register the restore before promoting so a failed rename also rolls back.
    {
        let package_dir = package_dir.to_path_buf();
        let backup = backup.clone();
        tx.on_rollback(move || {
            let _ = fs::remove_dir_all(&package_dir);
            if had_previous {
                let _ = fs::rename(&backup, &package_dir);
            }
        });
    }

    fs::rename(staging_dir, package_dir)
        .with_context(|| format!("promote package to {}", package_dir.display()))?;

    Ok(had_previous.then_some(backup))
}

pub(crate) fn backup_path(package_dir: &Path) -> Result<PathBuf> {
    let name = package_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("package directory should have a name")?;
    Ok(package_dir.with_file_name(format!("{name}.grimoire-old")))
}

pub(crate) fn write_state(
    tx: &mut Transaction,
    root: &Path,
    metadata: &PackageMetadata,
    target: &str,
    archive_hash: &str,
    store_hash: &str,
    store_path: &str,
) -> Result<()> {
    let state_dir = root.join("state").join("packages");
    fs::create_dir_all(&state_dir)?;
    let state_path = state_dir.join(format!("{}.nuon", metadata.name));

    // Holds and install reasons are user intent that survives reinstalls and upgrades —
    // preserve them when we rewrite the state file. A brand-new package starts as a
    // dependency (`requested: false`); the install entry point promotes the roots the user
    // actually named. (Nothing else in the prior state is worth carrying forward; the
    // install we just performed is the authoritative source for everything else.)
    let (previous_held, previous_requested) = if state_path.exists() {
        PackageState::from_value(nuon_io::read_nuon(&state_path)?)
            .map(|prior| (prior.held, prior.requested))
            .unwrap_or((false, false))
    } else {
        (false, false)
    };

    let state = PackageState {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: metadata.target.clone(),
        archive_hash: archive_hash.to_owned(),
        store_hash: store_hash.to_owned(),
        store_path: store_path.to_owned(),
        bins: metadata.bins_for(target),
        runtime_deps: metadata
            .deps
            .runtime
            .iter()
            .filter(|d| d.matches_platform(&paths::target_triple()))
            .map(|dep| dep.name.clone())
            .collect(),
        build_deps: metadata
            .deps
            .build_for(&paths::target_triple())
            .iter()
            .map(|dep| dep.name.clone())
            .collect(),
        source_hashes: metadata
            .sources
            .iter()
            .map(|(name, source)| (name.clone(), source.sha256.clone()))
            .collect(),
        held: previous_held,
        requested: previous_requested,
        provides: metadata.provides.clone(),
        libs: metadata.libs.clone(),
        notes: metadata.notes.clone(),
    };

    // Capture the prior state so a later failure can restore it.
    let previous = if state_path.exists() {
        Some(fs::read(&state_path)?)
    } else {
        None
    };
    {
        let state_path = state_path.clone();
        tx.on_rollback(move || match &previous {
            Some(bytes) => {
                let _ = fs::write(&state_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&state_path);
            }
        });
    }

    nuon_io::write_nuon(&state_path, &state.to_value())
}

pub(crate) fn rebuild_lock(tx: &mut Transaction) -> Result<()> {
    let lock_path = lock::lock_path()?;
    let previous = if lock_path.exists() {
        Some(fs::read(&lock_path)?)
    } else {
        None
    };
    {
        let lock_path = lock_path.clone();
        tx.on_rollback(move || match &previous {
            Some(bytes) => {
                let _ = fs::write(&lock_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&lock_path);
            }
        });
    }
    lock::rebuild()
}

pub(crate) fn normalize_archive_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("./").unwrap_or(&text).to_owned()
}

pub(crate) fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
