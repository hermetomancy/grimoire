//! Realizing one package into the store: verify, validate, extract, promote, and record
//! its state and the lockfile — every step staged against a transaction.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    install::lock,
    model::{
        Dependency, PackageMetadata, PackageState, embedded_store_hash, parse_version_relaxed,
        validate_relative_package_path, validate_sha256, validate_target,
    },
    nu::nuon_io,
    util::output::{accent, faint, report, status, success},
    util::paths,
};

use super::world::InstalledWorld;
use super::*;

/// How a package reached the store, named in its result line so the user can tell a verified
/// prebuilt from a local source build at a glance.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InstallOrigin {
    /// A published substitute fetched from a binhost and verified against the signed index.
    Prebuilt,
    /// Built from its rune on this machine.
    Source,
    /// A previously built archive reused from `cache/builds`, content address verified.
    CachedBuild,
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
            InstallOrigin::CachedBuild => "built from source, cached archive",
            InstallOrigin::LocalArchive => "local archive",
            InstallOrigin::BuildDep => "build dependency, store-only",
            InstallOrigin::TomeBuild => "built, store-only",
        }
    }
}

/// Refuses to bring `name` into the linked set while a *linked* installed package conflicts
/// with it (symmetrically; packages `name` replaces are exempt). Store-only packages on
/// either side never conflict — they are cache, not environment. Shared by archive
/// realization and by requested-promotion, which links an already-present store-only
/// package without re-realizing it.
pub(crate) fn refuse_linked_conflicts(
    world: &InstalledWorld,
    name: &str,
    conflicts: &[String],
    replaces: &[String],
) -> Result<()> {
    let linked_names = world.linked_immut();
    if let Some(conflict) = world.iter().find(|state| {
        state.name != name
            && linked_names.contains(&state.name)
            && !replaces.contains(&state.name)
            && (conflicts.contains(&state.name) || state.conflicts.iter().any(|c| c == name))
    }) {
        bail!(
            "cannot install `{name}`: it conflicts with installed `{}`; remove `{}` first",
            conflict.name,
            conflict.name
        );
    }
    Ok(())
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
pub(crate) fn step_already_realized(
    world: &InstalledWorld,
    step: &crate::solve::PlanStep,
) -> Result<bool> {
    let Some(state) = world.get(&step.name) else {
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

/// Verifies, extracts, and promotes the resolved archive into the install root. The lockfile is
/// rebuilt once at the command boundary by the caller. `expected_hash` is the hash the archive
/// must match before it is read (from `--sha256` or a tome index entry); `None` skips the check,
/// which only happens for a local archive installed without `--sha256`.
pub(crate) fn install_archive(
    world: &mut InstalledWorld,
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
    expected_build_env: Option<&str>,
    origin: InstallOrigin,
) -> Result<InstalledArchive> {
    install_store_only(
        world,
        archive_path,
        expected_hash,
        expected_store_hash,
        expected_build_env,
        origin,
    )
}

/// Like [`install_archive`], but does **not** rebuild the lockfile or activate a generation.
/// Used by `grm tome build` to make built packages available as build dependencies for subsequent
/// builds without polluting the user's active profile.
pub(crate) fn install_store_only(
    world: &mut InstalledWorld,
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
    expected_build_env: Option<&str>,
    origin: InstallOrigin,
) -> Result<InstalledArchive> {
    let target = paths::target_triple();
    install_store_only_for_target(
        world,
        archive_path,
        expected_hash,
        expected_store_hash,
        expected_build_env,
        origin,
        &target,
    )
}

/// Like [`install_store_only`], but validates and records the archive against an explicit target.
/// Cross-target `grm tome build` uses this for store-only publishing outputs; linked installs still
/// use the host target via [`install_store_only`].
pub(crate) fn install_store_only_for_target(
    world: &mut InstalledWorld,
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
    expected_build_env: Option<&str>,
    origin: InstallOrigin,
    target: &str,
) -> Result<InstalledArchive> {
    if archive_path.is_dir() {
        // `File::open` on a directory succeeds, so without this guard the failure surfaced only
        // when staging read the "archive" and got `Is a directory (os error 21)` — blamed on the
        // transaction destination rather than the directory the caller passed in.
        bail!(
            "package archive `{}` is a directory, not a `.tar.zst` archive",
            archive_path.display()
        );
    }
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
    validate_target(&metadata, target)?;

    // Conflicts gate linked installs only — on *both* sides: store-only packages (build
    // deps, tome-build products) never enter the environment, so coexistence in the store
    // is fine whichever direction it happens. The canonical case is a package conflicting
    // with its own bootstrap seed (`rust` vs `rust-stage0`): the seed sits store-only as a
    // build dep and must not block the linked install of the package it just built. The
    // check is symmetric, and packages this one replaces are exempt — a replacer routinely
    // conflicts with what it supersedes, and migration removes it in the same transaction.
    let linked = matches!(
        origin,
        InstallOrigin::Prebuilt
            | InstallOrigin::Source
            | InstallOrigin::CachedBuild
            | InstallOrigin::LocalArchive
    );
    if linked {
        refuse_linked_conflicts(
            world,
            &metadata.name,
            &metadata.conflicts,
            &metadata.replaces,
        )?;
    }

    let (package_dir, store_hash) = resolve_store_dir(&metadata, expected_store_hash)?;

    let staging_dir = transaction.path().join("package");
    fs::create_dir_all(&staging_dir)?;

    status(&format!(
        "extracting into transaction ({})",
        transaction.path().display()
    ));
    archive::extract_archive(&safe_archive, &staging_dir)?;

    status("validating extracted files");
    validate_bins(&metadata, target, &staging_dir)?;

    // Everything from here mutates shared install state. Stage each step against a
    // transaction so that a failure restores the previously installed version.
    let mut tx = Transaction::new();

    status(&format!("promoting package to ({})", package_dir.display()));
    let replaced = promote_package(&mut tx, &staging_dir, &package_dir)?;

    status("writing package state");
    let state = build_package_state(
        &metadata,
        target,
        &archive_hash,
        &store_hash,
        &package_dir.to_string_lossy(),
        expected_build_env,
        world.get(&metadata.name),
    );
    world.insert(state);

    // Supersession: remove every installed package this one replaces, in the same command,
    // carrying its requested/held intent onto the replacement so a rename never silently
    // demotes or unpins anything. State mutations accumulate on the world and commit with the
    // package promotion in the transaction below. Store-only installs skip this — a cached
    // build dep must not mutate the user's environment.
    if linked {
        for old in &metadata.replaces {
            if old == &metadata.name {
                continue;
            }
            let Some(old_state) = world.get(old).cloned() else {
                continue;
            };
            report(&format!(
                "{} {}",
                accent(&metadata.name),
                faint(&format!("replaces {old}; removing {old}"))
            ));
            let removed = remove_one(world, old)?;
            if old_state.requested || removed.requested {
                set_requested(world, &metadata.name, true, false)?;
            }
            if old_state.held || removed.held {
                set_hold(world, &metadata.name, true, false)?;
            }
            sweep_orphans(world, removed.runtime_deps)?;
        }
    }

    world.commit(&mut tx)?;
    tx.commit();
    if let Some(replaced) = replaced {
        let _ = fs::remove_dir_all(replaced);
    }

    report(&format!(
        "{} {}",
        accent(&format!("{} {}", metadata.name, metadata.version)),
        faint(&format!("— {}", origin.describe()))
    ));
    let version = parse_version_relaxed(&metadata.version)
        .with_context(|| format!("package version `{}` is not valid semver", metadata.version))?;
    let runtime_deps: Vec<Dependency> = metadata
        .deps
        .runtime
        .into_iter()
        .filter(|d| d.matches_platform(target))
        .collect();
    Ok(InstalledArchive {
        name: metadata.name,
        version,
        runtime_deps,
        notes: metadata.notes,
    })
}

/// A previously built archive in `cache/builds` whose embedded content address matches
/// `store_hash`, if any. The reuse gets the same acceptance check a substitute gets —
/// `resolve_store_dir` cross-checks the embedded store basename against the expected hash at
/// install — so a cached archive is as trustworthy as a verified prebuilt. Any read or
/// mismatch problem is a miss (the build runs normally), never an error.
pub(crate) fn cached_build_archive(
    metadata: &PackageMetadata,
    store_hash: &str,
) -> Option<PathBuf> {
    let path = paths::build_output_dir().ok()?.join(format!(
        "{}-{}-{}.tar.zst",
        metadata.name,
        metadata.version,
        paths::target_triple()
    ));
    if !path.exists() {
        return None;
    }
    let embedded = inspect_archive(&path).ok()?;
    if embedded.name != metadata.name || embedded.version != metadata.version {
        return None;
    }
    let (_, embedded_hash) = resolve_store_dir(&embedded, None).ok()?;
    (embedded_hash == store_hash).then_some(path)
}

pub(crate) fn inspect_archive(path: &Path) -> Result<PackageMetadata> {
    let text = archive::validate_archive_paths_capturing(path, Some(".grimoire/package.nuon"))?
        .context("package archive is missing .grimoire/package.nuon")?;
    PackageMetadata::from_value(nuon_io::parse_nuon(&text)?, true)
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
    let hash = embedded_store_hash(metadata)?;
    if let Some(expected) = expected_hash
        && hash.as_str() != expected
    {
        bail!(
            "package `{}` embeds store hash `{hash}` but its inputs hash to `{expected}`",
            metadata.name
        );
    }
    let basename = metadata.store_path.as_deref().with_context(|| {
        format!(
            "package `{}` metadata is missing its store_path basename",
            metadata.name
        )
    })?;
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

/// Builds the [`PackageState`] that records a newly promoted package. Holds and install reasons
/// are user intent that survives reinstalls and upgrades — preserve them from `prior` when one
/// exists. A brand-new package starts as a dependency (`requested: false`); the install entry
/// point promotes the roots the user actually named.
pub(crate) fn build_package_state(
    metadata: &PackageMetadata,
    target: &str,
    archive_hash: &str,
    store_hash: &str,
    store_path: &str,
    build_env: Option<&str>,
    prior: Option<&PackageState>,
) -> PackageState {
    let (previous_held, previous_requested) = prior
        .map(|prior| (prior.held, prior.requested))
        .unwrap_or((false, false));

    PackageState {
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
            .filter(|d| d.matches_platform(target))
            .map(|dep| dep.name.clone())
            .collect(),
        build_deps: metadata
            .deps
            .build_for(target)
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
        upstream_version: metadata.upstream_version.clone(),
        conflicts: metadata.conflicts.clone(),
        replaces: metadata.replaces.clone(),
        build_env: build_env.map(str::to_owned),
        build_only: metadata.build_only,
    }
}

pub(crate) fn rebuild_lock(tx: &mut Transaction, world: &InstalledWorld) -> Result<()> {
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
                let _ = crate::util::fs_util::write_atomic(&lock_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&lock_path);
            }
        });
    }
    lock::rebuild(world)
}

pub(crate) fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_package_state;

    #[test]
    fn package_state_records_only_the_build_env_it_was_given() {
        let value = crate::nu::nuon_io::parse_nuon(
            r#"{ name: "pkg", version: "1.0.0", bins: { default: { pkg: "bin/pkg" } } }"#,
        )
        .unwrap();
        let metadata = crate::model::PackageMetadata::from_value(value, false).unwrap();

        let recorded = build_package_state(
            &metadata,
            "linux-x86_64-musl",
            "archive",
            "store",
            "/tmp/store/pkg",
            Some("path:ambient-posix"),
            None,
        );
        assert_eq!(recorded.build_env.as_deref(), Some("path:ambient-posix"));

        let unknown = build_package_state(
            &metadata,
            "linux-x86_64-musl",
            "archive",
            "store",
            "/tmp/store/pkg",
            None,
            None,
        );
        assert_eq!(unknown.build_env, None);
    }
}
