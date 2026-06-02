use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod git;

use crate::{
    cli::{TomeAddArgs, TomeRemoveArgs, TomeUpdateArgs},
    index,
    model::{
        IndexEntry, TomeManifest, TomePackages, TomeState, validate_relative_package_path,
        validate_tome_name, validate_tome_ref, validate_tome_url,
    },
    nu::{
        nuon_io,
        runtime::{EmbeddedNuRuntime, RuneRuntime},
    },
    paths,
    progress::status,
};

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    validate_local_tome_source(&args.git_url)?;

    // The tome names itself: read the `name` from its manifest rather than asking the user
    // to repeat it on the command line. Remote sources are cloned to a temp dir to read it.
    let name = read_source_tome_name(&args.git_url, &args.ref_name)?;
    validate_tome_name(&name)?;
    validate_local_tome_if_available(&name, &args.git_url)?;

    let state_dir = tome_state_dir()?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("tome `{name}` already exists");
    }

    fs::create_dir_all(&state_dir)?;
    let state = TomeState {
        name: name.clone(),
        url: args.git_url,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        tome: None,
    };
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    println!("added tome {name}");
    Ok(())
}

/// Reads the `name` a tome declares for itself in its root `tome.rn`. Local sources are read
/// in place; a remote source is cloned into a temporary directory just long enough to read
/// its manifest.
fn read_source_tome_name(url: &str, ref_name: &str) -> Result<String> {
    let runtime = EmbeddedNuRuntime;
    if is_local_tome_source(url) {
        let manifest_path = Path::new(url).join("tome.rn");
        return Ok(runtime
            .tome_manifest(&manifest_path)
            .with_context(|| format!("read tome manifest {}", manifest_path.display()))?
            .name);
    }

    let temp = tempfile::tempdir().context("create temporary directory to read tome manifest")?;
    git::clone(url, ref_name, temp.path())
        .with_context(|| format!("clone tome `{url}` to read its manifest"))?;
    let manifest_path = temp.path().join("tome.rn");
    if !manifest_path.exists() {
        bail!("tome `{url}` is missing root tome.rn");
    }
    Ok(runtime
        .tome_manifest(&manifest_path)
        .with_context(|| format!("read tome manifest from {url}"))?
        .name)
}

pub fn list() -> Result<()> {
    for state in load_tomes()? {
        println!("{}\t{}\t{}", state.name, state.url, state.ref_name);
    }
    Ok(())
}

pub fn update(args: TomeUpdateArgs) -> Result<()> {
    let tomes = match args.name {
        Some(name) => {
            validate_tome_name(&name)?;
            vec![load_tome(&name)?]
        }
        None => load_tomes()?,
    };

    if tomes.is_empty() {
        bail!("no tomes are configured");
    }

    for tome in tomes {
        sync_tome_cache(&tome, args.quiet)?;
        println!("updated tome {}", tome.name);
    }
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    validate_tome_name(&args.name)?;

    let state_path = tome_state_dir()?.join(format!("{}.nuon", args.name));
    if !state_path.exists() {
        bail!("tome `{}` is not configured", args.name);
    }

    fs::remove_file(state_path)?;
    println!("removed tome {}", args.name);
    Ok(())
}

fn tome_state_dir() -> Result<std::path::PathBuf> {
    Ok(paths::install_root()?.join("state").join("tomes"))
}

pub fn load_tomes() -> Result<Vec<TomeState>> {
    let state_dir = tome_state_dir()?;
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
        states.push(
            TomeState::from_value(nuon_io::read_nuon(&path)?)
                .with_context(|| format!("read tome state {}", path.display()))?,
        );
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(states)
}

pub fn load_tome(name: &str) -> Result<TomeState> {
    let state_path = tome_state_dir()?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        bail!("tome `{name}` is not configured");
    }
    TomeState::from_value(nuon_io::read_nuon(&state_path)?)
        .with_context(|| format!("read tome state {}", state_path.display()))
}

pub fn ensure_tome_cache(tome: &TomeState, quiet: bool) -> Result<PathBuf> {
    let cache_path = tome_cache_path(&tome.name)?;
    if !cache_path.exists() {
        sync_tome_cache(tome, quiet)?;
    }
    Ok(cache_path)
}

fn sync_tome_cache(tome: &TomeState, quiet: bool) -> Result<()> {
    let cache_dir = tome_cache_dir()?;
    let cache_path = tome_cache_path(&tome.name)?;
    fs::create_dir_all(&cache_dir)?;

    if is_local_tome_source(&tome.url) {
        let source = PathBuf::from(&tome.url)
            .canonicalize()
            .with_context(|| format!("resolve tome source {}", tome.url))?;
        status(quiet, &format!("copying local tome ({})", tome.name));
        if cache_path.exists() {
            fs::remove_dir_all(&cache_path)?;
        }
        copy_dir_all(&source, &cache_path)?;
        record_tome_sync_state(tome, &cache_path)?;
        return Ok(());
    }

    sync_remote_tome_cache(tome, &cache_path, quiet)?;
    record_tome_sync_state(tome, &cache_path)
}

fn is_local_tome_source(url: &str) -> bool {
    let path = Path::new(url);
    path.is_dir() && path.join("tome.rn").exists()
}

fn validate_local_tome_source(url: &str) -> Result<()> {
    let path = Path::new(url);
    if path.exists() && path.is_dir() && !path.join("tome.rn").exists() {
        bail!(
            "local tome source is missing root tome.rn: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_local_tome_if_available(name: &str, url: &str) -> Result<()> {
    if !is_local_tome_source(url) {
        return Ok(());
    }

    let source = Path::new(url);
    let runtime = EmbeddedNuRuntime;
    let manifest = runtime
        .tome_manifest(&source.join("tome.rn"))
        .with_context(|| format!("validate local tome manifest {}", source.display()))?;
    let tome = TomeState {
        name: name.to_owned(),
        url: url.to_owned(),
        ref_name: "local-validation".to_owned(),
        checked_ref: None,
        checked_commit: None,
        tome: None,
    };
    validate_tome_cache(&tome, source, &manifest, &runtime)
}

fn record_tome_sync_state(tome: &TomeState, cache_path: &Path) -> Result<()> {
    let runtime = EmbeddedNuRuntime;
    let manifest_path = cache_path.join("tome.rn");
    if !manifest_path.exists() {
        bail!(
            "tome cache is missing root tome.rn: {}",
            cache_path.display()
        );
    }
    let manifest = runtime.tome_manifest(&manifest_path)?;
    validate_tome_cache(tome, cache_path, &manifest, &runtime)?;
    let commit = git::head_commit(cache_path)?;
    let mut state = load_tome(&tome.name).unwrap_or_else(|_| tome.clone());
    state.checked_ref = Some(tome.ref_name.clone());
    state.checked_commit = commit;
    state.tome = Some(manifest);

    let state_path = tome_state_dir()?.join(format!("{}.nuon", tome.name));
    nuon_io::write_nuon(&state_path, &state.to_value())
}

/// Finds the binary package index entry for `package`/`target` in this tome's package
/// repository, if one exists. Returns the package-repository root alongside the entry so a
/// relative archive location can be resolved against it. `None` means the tome offers no
/// matching binary (the caller should fall back to a source build).
pub fn package_index_entry(
    tome: &TomeState,
    package: &str,
    target: &str,
    quiet: bool,
) -> Result<Option<(PathBuf, IndexEntry)>> {
    let cache = ensure_tome_cache(tome, quiet)?;
    let manifest = EmbeddedNuRuntime.tome_manifest(&cache.join("tome.rn"))?;
    let Some(packages) = manifest.packages else {
        return Ok(None);
    };

    let root = packages_repo_root(tome, &cache, &packages, quiet)?;
    let index_path = root.join(&packages.index);
    if !index_path.exists() {
        return Ok(None);
    }

    let index = index::read_index(&index_path)?;
    Ok(index
        .find(package, target)
        .cloned()
        .map(|entry| (root, entry)))
}

/// Resolves the directory that holds a tome's package index and (relative) archives. `.`
/// means the package repo is the tome repo itself; a remote URL is cloned into the cache; a
/// local path is used directly.
fn packages_repo_root(
    tome: &TomeState,
    cache: &Path,
    packages: &TomePackages,
    quiet: bool,
) -> Result<PathBuf> {
    if packages.repo == "." {
        return Ok(cache.to_path_buf());
    }

    if is_remote_repo(&packages.repo) {
        let dest = packages_cache_dir()?.join(&tome.name);
        if !dest.exists() {
            status(quiet, &format!("cloning package repo ({})", tome.name));
            fs::create_dir_all(dest.parent().unwrap_or(&dest))?;
            git::clone(&packages.repo, &tome.ref_name, &dest)
                .with_context(|| format!("clone package repo for tome `{}`", tome.name))?;
        }
        return Ok(dest);
    }

    let path = Path::new(&packages.repo);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(cache.join(path))
    }
}

fn is_remote_repo(repo: &str) -> bool {
    repo.contains("://") || repo.starts_with("git@")
}

fn packages_cache_dir() -> Result<PathBuf> {
    Ok(paths::install_root()?.join("cache").join("packages"))
}

fn sync_remote_tome_cache(tome: &TomeState, cache_path: &Path, quiet: bool) -> Result<()> {
    if cache_path.exists() {
        status(quiet, &format!("fetching tome ({})", tome.name));
        fs::remove_dir_all(cache_path)
            .with_context(|| format!("replace tome cache {}", cache_path.display()))?;
    } else {
        status(quiet, &format!("cloning tome ({})", tome.name));
    }

    status(
        quiet,
        &format!("checking out tome ({}) ref ({})", tome.name, tome.ref_name),
    );
    git::clone(&tome.url, &tome.ref_name, cache_path)
        .with_context(|| format!("could not sync tome `{}`", tome.name))
}

fn validate_tome_cache(
    tome: &TomeState,
    cache_path: &Path,
    manifest: &TomeManifest,
    runtime: &EmbeddedNuRuntime,
) -> Result<()> {
    if manifest.name != tome.name {
        bail!(
            "tome manifest name `{}` does not match configured tome `{}`",
            manifest.name,
            tome.name
        );
    }

    let packages = manifest
        .packages
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tome manifest is missing required field `packages`"))?;
    validate_tome_packages(packages)?;

    let runes_dir = cache_path.join("runes");
    if !runes_dir.is_dir() {
        bail!(
            "tome cache is missing runes directory: {}",
            runes_dir.display()
        );
    }

    let mut rune_count = 0_usize;
    let mut package_names = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(&runes_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rn") {
            continue;
        }

        let metadata = runtime
            .package_metadata(&path)
            .with_context(|| format!("validate tome rune {}", path.display()))?;
        if !package_names.insert(metadata.name.clone()) {
            bail!(
                "tome contains duplicate package name `{}` in {}",
                metadata.name,
                path.display()
            );
        }
        rune_count += 1;
    }

    if rune_count == 0 {
        bail!(
            "tome cache contains no rune definitions in {}",
            runes_dir.display()
        );
    }

    Ok(())
}

fn validate_tome_packages(packages: &TomePackages) -> Result<()> {
    validate_tome_url(&packages.repo).context("validate tome packages.repo")?;
    if packages.format != "git" {
        bail!(
            "tome packages.format `{}` is not supported; expected `git`",
            packages.format
        );
    }
    validate_relative_package_path(&packages.index, "tome packages.index")?;
    Ok(())
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        if path == source {
            continue;
        }
        let relative = path
            .strip_prefix(source)
            .with_context(|| format!("strip source prefix from {}", path.display()))?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            bail!("tome source contains symlink {}", path.display());
        }
        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        }
    }
    Ok(())
}

fn tome_cache_dir() -> Result<PathBuf> {
    Ok(paths::install_root()?.join("cache").join("tomes"))
}

/// Location of a tome's cached repository. Does not sync; callers that need the cache
/// populated should use [`ensure_tome_cache`].
pub fn tome_cache_path(name: &str) -> Result<PathBuf> {
    Ok(tome_cache_dir()?.join(name))
}
