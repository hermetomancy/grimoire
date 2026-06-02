use anyhow::{Context, Result, bail};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    archive, build,
    cli::{InstallArgs, PackageArg},
    lock,
    model::{PackageMetadata, PackageState, validate_relative_package_path, validate_target},
    nu::{
        nuon_io,
        runtime::{EmbeddedNuRuntime, RuneRuntime},
    },
    paths,
    progress::{status, success},
    resolve,
};

/// The archive an install will consume, plus the expected hash supplied by whatever resolved
/// it (a tome index entry) and the runtime dependencies that must also be installed. The CLI
/// `--sha256` flag takes precedence over the resolved hash when present.
struct ResolvedInstall {
    archive: PathBuf,
    expected_hash: Option<String>,
    runtime_deps: Vec<String>,
}

/// Shared state for one top-level install and its dependency tree. `visiting` records names
/// currently on the install stack so a cycle (or diamond) terminates instead of recursing
/// forever. `installed` is the set of already-installed package names, read from disk once up
/// front and updated as packages land, so dependency checks never re-scan the state directory.
struct InstallScope {
    visiting: HashSet<String>,
    installed: HashSet<String>,
}

pub fn install(args: InstallArgs) -> Result<()> {
    let installed = installed_states()?
        .into_iter()
        .map(|state| state.name)
        .collect();
    let mut scope = InstallScope {
        visiting: HashSet::new(),
        installed,
    };
    install_inner(&args, &mut scope)
}

/// Installs `args.package` and its dependencies.
fn install_inner(args: &InstallArgs, scope: &mut InstallScope) -> Result<()> {
    if !scope.visiting.insert(args.package.clone()) {
        return Ok(());
    }

    let resolved = resolve_install_archive(args, scope)?;
    let mut runtime_deps = resolved.runtime_deps.clone();
    // A local archive reports no runtime deps from resolution; its embedded metadata is the
    // authority. `install_archive` returns those so cycle/idempotency guards still apply.
    let metadata = install_archive(args, resolved)?;
    scope.installed.insert(metadata.name);
    for dep in metadata.runtime_deps {
        if !runtime_deps.contains(&dep) {
            runtime_deps.push(dep);
        }
    }
    install_runtime_deps(&runtime_deps, args.quiet, scope)
}

/// The installed package's name and the runtime dependency names declared in its embedded
/// metadata, returned so the caller can record it as installed and pull its runtime deps in.
struct InstalledArchive {
    name: String,
    runtime_deps: Vec<String>,
}

/// Verifies, extracts, and promotes the resolved archive into the install root.
fn install_archive(args: &InstallArgs, resolved: ResolvedInstall) -> Result<InstalledArchive> {
    let archive_path = resolved.archive;
    if !archive_path.exists() {
        bail!(
            "package archive `{}` does not exist",
            archive_path.display()
        );
    }

    // Verify integrity before the archive is read or extracted. The expected hash comes from
    // `--sha256` if given, otherwise from the resolving tome index entry. A mismatch is fatal.
    let expected = args.sha256.clone().or(resolved.expected_hash);
    status(args.quiet, "hashing archive");
    let archive_hash = archive::archive_hash(&archive_path)?;
    if let Some(expected) = &expected {
        archive::verify_hash(&archive_hash, expected)
            .with_context(|| format!("verify archive {}", archive_path.display()))?;
        success(args.quiet, "archive hash verified");
    }

    status(
        args.quiet,
        &format!("validating archive paths ({})", archive_path.display()),
    );
    validate_archive_paths(&archive_path)?;

    status(args.quiet, "reading package metadata");
    let metadata = inspect_archive(&archive_path)?;
    validate_target(&metadata, &paths::target_triple())?;

    let root = paths::install_root()?;

    let transactions_dir = root.join("transactions");
    fs::create_dir_all(&transactions_dir)?;
    let transaction = tempfile::Builder::new()
        .prefix("grimoire-")
        .tempdir_in(&transactions_dir)?;
    let staging_dir = transaction.path().join("package");
    fs::create_dir_all(&staging_dir)?;

    status(
        args.quiet,
        &format!(
            "extracting into transaction ({})",
            transaction.path().display()
        ),
    );
    extract_archive(&archive_path, &staging_dir)?;

    status(args.quiet, "validating extracted files");
    validate_bins(&metadata, &staging_dir)?;

    let package_dir = root
        .join("packages")
        .join(&metadata.name)
        .join(&metadata.version);

    // Everything from here mutates shared install state. Stage each step against a
    // transaction so that a failure restores the previously installed version.
    let mut tx = Transaction::new();

    status(
        args.quiet,
        &format!("promoting package to ({})", package_dir.display()),
    );
    let replaced = promote_package(&mut tx, &staging_dir, &package_dir)?;

    status(args.quiet, "linking shims");
    link_bins(&mut tx, &metadata, &root, &package_dir)?;

    status(args.quiet, "writing package state");
    write_state(&mut tx, &root, &metadata, &archive_hash)?;

    tx.commit();
    if let Some(replaced) = replaced {
        let _ = fs::remove_dir_all(replaced);
    }

    lock::rebuild()?;

    success(
        args.quiet,
        &format!("installed {} {}", metadata.name, metadata.version),
    );
    println!(
        "installed {} {} into {}",
        metadata.name,
        metadata.version,
        root.display()
    );
    Ok(InstalledArchive {
        name: metadata.name,
        runtime_deps: metadata.deps.runtime,
    })
}

fn resolve_install_archive(
    args: &InstallArgs,
    scope: &mut InstallScope,
) -> Result<ResolvedInstall> {
    if args.from_source || args.package.ends_with(".rn") {
        return source_install(&args.package, args.quiet, scope);
    }

    let package_path = PathBuf::from(&args.package);
    if package_path.exists() || args.package.ends_with(".tar.zst") {
        // A local archive carries its dependency list inside `.grimoire/package.nuon`, which
        // `install_archive` reads later; runtime deps are picked up from that metadata there.
        return Ok(ResolvedInstall {
            archive: package_path,
            expected_hash: None,
            runtime_deps: Vec::new(),
        });
    }

    // A bare package name: prefer a verified binary archive, fall back to a source build.
    if let Some(resolved) = resolve::resolve_binary(&args.package, args.quiet)? {
        return Ok(ResolvedInstall {
            archive: resolved.path,
            expected_hash: Some(resolved.entry.archive_hash),
            runtime_deps: resolved.entry.runtime_deps,
        });
    }

    source_install(&args.package, args.quiet, scope)
}

/// Builds `package` from source after installing its build dependencies. Build deps must be
/// present before the rune runs, so they are installed up front; runtime deps come back to the
/// caller to be installed alongside the package.
fn source_install(package: &str, quiet: bool, scope: &mut InstallScope) -> Result<ResolvedInstall> {
    let metadata = read_rune_metadata(package, quiet)?;
    install_build_deps(&metadata, quiet, scope)?;
    Ok(ResolvedInstall {
        archive: build_from_source(package, quiet)?,
        expected_hash: None,
        runtime_deps: metadata.deps.runtime.clone(),
    })
}

fn read_rune_metadata(package: &str, quiet: bool) -> Result<PackageMetadata> {
    let rune = build::resolve_rune(package, quiet)?;
    EmbeddedNuRuntime
        .package_metadata(&rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))
}

fn install_build_deps(
    metadata: &PackageMetadata,
    quiet: bool,
    scope: &mut InstallScope,
) -> Result<()> {
    for dep in metadata.deps.build_for(&paths::target_triple()) {
        install_dep(&dep, quiet, scope)
            .with_context(|| format!("install build dependency `{dep}`"))?;
    }
    Ok(())
}

fn install_runtime_deps(deps: &[String], quiet: bool, scope: &mut InstallScope) -> Result<()> {
    for dep in deps {
        install_dep(dep, quiet, scope)
            .with_context(|| format!("install runtime dependency `{dep}`"))?;
    }
    Ok(())
}

/// Installs a dependency by name unless it is already installed. Resolution is name-based:
/// there is no version solver yet, so the latest archive a tome offers (or a source build) is
/// taken. The scope's `visiting` set guards against cycles across the whole dependency tree.
fn install_dep(name: &str, quiet: bool, scope: &mut InstallScope) -> Result<()> {
    if scope.installed.contains(name) {
        return Ok(());
    }
    let args = InstallArgs {
        package: name.to_owned(),
        from_source: false,
        sha256: None,
        quiet,
    };
    install_inner(&args, scope)
}

fn build_from_source(package: &str, quiet: bool) -> Result<PathBuf> {
    build::build_package(package, &paths::build_output_dir()?, quiet)
}

pub fn list() -> Result<()> {
    for state in installed_states()? {
        println!(
            "{}\t{}\t{}",
            state.name,
            state.version,
            state.target.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub fn installed_states() -> Result<Vec<PackageState>> {
    let state_dir = paths::install_root()?.join("state").join("packages");
    if !state_dir.exists() {
        return Ok(Vec::new());
    }

    let mut states = Vec::new();
    for entry in fs::read_dir(&state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
            continue;
        }
        let state = PackageState::from_value(nuon_io::read_nuon(&path)?)
            .with_context(|| format!("read package state {}", path.display()))?;
        states.push(state);
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(states)
}

pub fn remove(args: PackageArg) -> Result<()> {
    let root = paths::install_root()?;
    let state_path = root
        .join("state")
        .join("packages")
        .join(format!("{}.nuon", args.package));

    if !state_path.exists() {
        bail!("package `{}` is not installed", args.package);
    }

    let state = PackageState::from_value(nuon_io::read_nuon(&state_path)?)?;

    // Removal mutates the same shared install state as an install, so stage every step against
    // a transaction: a failure partway through restores the shims, package files, and state
    // record rather than leaving the package half-removed.
    let mut tx = Transaction::new();
    let bin_dir = root.join("bin");
    for bin in state.bins.keys() {
        let shim = shim_path(&bin_dir, bin);
        let previous = capture_shim(&shim)?;
        {
            let shim = shim.clone();
            tx.on_rollback(move || restore_shim(&shim, previous.as_ref()));
        }
        remove_if_exists(&shim)?;
    }

    // Move the package dir aside rather than deleting outright, so a later failure can restore
    // it; the backup is dropped only once the whole removal commits.
    let package_dir = root.join("packages").join(&state.name).join(&state.version);
    let backup = backup_path(&package_dir)?;
    let had_package = package_dir.exists();
    if had_package {
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::rename(&package_dir, &backup)
            .with_context(|| format!("move aside package {}", package_dir.display()))?;
        let package_dir = package_dir.clone();
        let backup = backup.clone();
        tx.on_rollback(move || {
            let _ = fs::rename(&backup, &package_dir);
        });
    }

    let state_bytes = fs::read(&state_path)?;
    {
        let state_path = state_path.clone();
        tx.on_rollback(move || {
            let _ = fs::write(&state_path, &state_bytes);
        });
    }
    fs::remove_file(&state_path)?;

    lock::rebuild()?;

    tx.commit();
    if had_package {
        let _ = fs::remove_dir_all(&backup);
    }
    println!("removed {}", args.package);
    Ok(())
}

fn inspect_archive(path: &Path) -> Result<PackageMetadata> {
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

/// Validates every archive member *before* extraction (AGENTS.md §5.2): member paths must
/// stay inside the extraction root, and symlinks/hard links are rejected outright (§5.3) so a
/// malicious link can never be followed during `unpack`.
fn validate_archive_paths(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    let mut bad = Vec::new();

    for entry in archive.entries()? {
        let entry = entry?;
        let member = entry.path()?.display().to_string();
        if !archive::validate_archive_member_path(&entry.path()?) {
            bad.push(member);
            continue;
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() {
            bail!("archive contains a symlink, which is not accepted yet: {member}");
        }
        if entry_type.is_hard_link() {
            bail!("archive contains a hard link, which is not accepted yet: {member}");
        }
    }

    if !bad.is_empty() {
        bail!("archive contains unsafe paths: {}", bad.join(", "));
    }

    Ok(())
}

fn extract_archive(path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(destination)?;
    Ok(())
}

fn validate_bins(metadata: &PackageMetadata, package_dir: &Path) -> Result<()> {
    for (name, path) in &metadata.bins {
        validate_relative_package_path(path, &format!("bin `{name}`"))?;
        let bin_path = package_dir.join(path);
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
fn promote_package(
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

fn backup_path(package_dir: &Path) -> Result<PathBuf> {
    let name = package_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("package directory should have a name")?;
    Ok(package_dir.with_file_name(format!("{name}.grimoire-old")))
}

fn link_bins(
    tx: &mut Transaction,
    metadata: &PackageMetadata,
    root: &Path,
    package_dir: &Path,
) -> Result<()> {
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir)?;

    for (name, path) in &metadata.bins {
        let source = package_dir.join(path).canonicalize()?;
        let shim = shim_path(&bin_dir, name);

        // Capture and register a restore of the prior shim before replacing it.
        let previous = capture_shim(&shim)?;
        {
            let shim = shim.clone();
            tx.on_rollback(move || restore_shim(&shim, previous.as_ref()));
        }

        remove_if_exists(&shim)?;
        write_shim(&shim, &source)?;
    }

    Ok(())
}

#[cfg(unix)]
fn shim_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(name)
}

#[cfg(windows)]
fn shim_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(format!("{name}.cmd"))
}

#[cfg(unix)]
fn write_shim(shim: &Path, source: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, shim)
        .with_context(|| format!("link shim {}", shim.display()))
}

#[cfg(windows)]
fn write_shim(shim: &Path, source: &Path) -> Result<()> {
    fs::write(
        shim,
        format!("@echo off\r\n\"{}\" %*\r\n", source.display()),
    )
    .with_context(|| format!("write shim {}", shim.display()))
}

enum PreviousShim {
    Symlink(PathBuf),
    File(Vec<u8>),
}

fn capture_shim(shim: &Path) -> Result<Option<PreviousShim>> {
    let Ok(metadata) = fs::symlink_metadata(shim) else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        Ok(Some(PreviousShim::Symlink(fs::read_link(shim)?)))
    } else {
        Ok(Some(PreviousShim::File(fs::read(shim)?)))
    }
}

fn restore_shim(shim: &Path, previous: Option<&PreviousShim>) {
    let _ = remove_if_exists(shim);
    match previous {
        None => {}
        Some(PreviousShim::File(bytes)) => {
            let _ = fs::write(shim, bytes);
        }
        Some(PreviousShim::Symlink(target)) => {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(target, shim);
            #[cfg(windows)]
            let _ = std::os::windows::fs::symlink_file(target, shim);
        }
    }
}

fn write_state(
    tx: &mut Transaction,
    root: &Path,
    metadata: &PackageMetadata,
    archive_hash: &str,
) -> Result<()> {
    let state = PackageState {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: metadata.target.clone(),
        archive_hash: archive_hash.to_owned(),
        bins: metadata.bins.clone(),
        runtime_deps: metadata.deps.runtime.clone(),
        build_deps: metadata.deps.build_for(&paths::target_triple()),
        source_hashes: metadata
            .sources
            .iter()
            .map(|(name, source)| (name.clone(), source.sha256.clone()))
            .collect(),
    };
    let state_dir = root.join("state").join("packages");
    fs::create_dir_all(&state_dir)?;
    let state_path = state_dir.join(format!("{}.nuon", metadata.name));

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

/// Best-effort, RAII-style rollback for a multi-step install. Rollback actions run in
/// reverse registration order when the transaction is dropped without [`commit`]ting,
/// e.g. when an install step returns an error via `?`.
#[derive(Default)]
struct Transaction {
    rollbacks: Vec<Box<dyn FnOnce()>>,
    committed: bool,
}

impl Transaction {
    fn new() -> Self {
        Self::default()
    }

    fn on_rollback(&mut self, action: impl FnOnce() + 'static) {
        self.rollbacks.push(Box::new(action));
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        while let Some(rollback) = self.rollbacks.pop() {
            rollback();
        }
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn normalize_archive_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("./").unwrap_or(&text).to_owned()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
