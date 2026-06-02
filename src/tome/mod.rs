use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod git;

use crate::{
    cli::{TomeAddArgs, TomeBuildArgs, TomeInitArgs, TomeRemoveArgs, TomeRuneArgs, TomeUpdateArgs},
    index,
    model::{
        IndexEntry, PackageIndex, TomeManifest, TomePackages, TomeState, validate_package_name,
        validate_package_version, validate_relative_package_path, validate_tome_name,
        validate_tome_ref, validate_tome_url,
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

/// Scaffolds a new tome: a self-naming `tome.rn` manifest, empty `runes/` and `sources/`
/// directories, and an empty `index.nuon`. The result is ready for `grm tome rune` to add
/// package definitions, and for `grm tome add <path>` once it holds at least one rune.
pub fn init(args: TomeInitArgs) -> Result<()> {
    validate_tome_name(&args.name)?;

    let root = &args.path;
    let manifest_path = root.join("tome.rn");
    if manifest_path.exists() {
        bail!("{} already contains a tome.rn", root.display());
    }

    fs::create_dir_all(root.join("runes"))?;
    fs::create_dir_all(root.join("sources"))?;

    let description = args
        .description
        .unwrap_or_else(|| format!("{} tome", args.name));
    fs::write(
        &manifest_path,
        tome_manifest_template(&args.name, &description),
    )?;

    let index_path = root.join("index.nuon");
    if !index_path.exists() {
        fs::write(&index_path, "{\n  packages: []\n}\n")?;
    }

    println!("created tome {} in {}", args.name, root.display());
    println!(
        "next: add a package with `grm tome rune <name> --path {}`",
        root.display()
    );
    Ok(())
}

/// Scaffolds a starter rune (`runes/<name>.rn`) in an existing tome. The template is a valid,
/// buildable package definition with placeholders for the author to fill in.
pub fn rune(args: TomeRuneArgs) -> Result<()> {
    validate_package_name(&args.name)?;
    validate_package_version(&args.version)?;

    let root = &args.path;
    if !root.join("tome.rn").exists() {
        bail!("{} is not a tome (missing tome.rn)", root.display());
    }

    let runes_dir = root.join("runes");
    fs::create_dir_all(&runes_dir)?;
    let rune_path = runes_dir.join(format!("{}.rn", args.name));
    if rune_path.exists() {
        bail!("rune already exists: {}", rune_path.display());
    }

    fs::write(&rune_path, rune_template(&args.name, &args.version))?;
    println!("created rune {} in {}", args.name, rune_path.display());
    Ok(())
}

/// Builds a tome's rune into a `.tar.zst` under `packages/` and registers (or replaces) its
/// entry in the tome's package index, so the prebuilt archive can be published from the tome.
/// Only a local package repo (`packages.repo = "."`) is supported for now.
pub fn build(args: TomeBuildArgs) -> Result<()> {
    validate_package_name(&args.package)?;

    let root = &args.path;
    let manifest_path = root.join("tome.rn");
    if !manifest_path.exists() {
        bail!("{} is not a tome (missing tome.rn)", root.display());
    }

    let manifest = EmbeddedNuRuntime.tome_manifest(&manifest_path)?;
    let packages = manifest.packages.as_ref().with_context(|| {
        format!(
            "tome `{}` declares no `packages` index to publish into",
            manifest.name
        )
    })?;
    if packages.repo != "." {
        bail!(
            "publishing to external package repos is not supported yet (packages.repo must be \".\")"
        );
    }

    let rune_path = root.join("runes").join(format!("{}.rn", args.package));
    if !rune_path.exists() {
        bail!("rune not found: {}", rune_path.display());
    }

    let packages_dir = root.join("packages");
    let archive =
        crate::build::build_package(&rune_path.to_string_lossy(), &packages_dir, args.quiet)?;
    let archive_hash = crate::archive::archive_hash(&archive)?;
    let archive_file = archive
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("archive path has no file name: {}", archive.display()))?;

    let metadata = EmbeddedNuRuntime.package_metadata(&rune_path)?;
    let target = paths::target_triple();
    let entry = IndexEntry {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: target.clone(),
        archive: format!("packages/{archive_file}"),
        archive_hash,
        runtime_deps: metadata.deps.runtime.clone(),
    };

    let index_path = root.join(&packages.index);
    let mut catalog = if index_path.exists() {
        PackageIndex::from_value(nuon_io::read_nuon(&index_path)?)?
    } else {
        PackageIndex {
            packages: Vec::new(),
        }
    };
    catalog.upsert(entry);
    nuon_io::write_nuon(&index_path, &catalog.to_value())?;

    println!(
        "built {} {} ({target}) into {}",
        metadata.name,
        metadata.version,
        archive.display()
    );
    println!("registered in {}", index_path.display());
    Ok(())
}

fn tome_manifest_template(name: &str, description: &str) -> String {
    const TEMPLATE: &str = r#"export const tome = {
  name: "{NAME}"
  description: "{DESCRIPTION}"

  packages: {
    repo: "."
    format: "git"
    index: "index.nuon"
  }
}
"#;
    TEMPLATE
        .replace("{NAME}", name)
        .replace("{DESCRIPTION}", &escape_nu_string(description))
}

fn rune_template(name: &str, version: &str) -> String {
    const TEMPLATE: &str = r##"export const package = {
  name: "{NAME}"
  version: "{VERSION}"
  summary: "TODO: one-line summary of {NAME}"
  targets: ["linux-x86_64-gnu" "macos-aarch64-darwin" "windows-x86_64-gnu"]

  # Declare sources here; each is fetched and checksum-verified before `build` runs.
  # sources: {
  #   main: {
  #     url: "https://example.com/{NAME}-{VERSION}.tar.gz"
  #     sha256: "sha256:..."
  #   }
  # }
  sources: {}

  deps: {
    build: {}
    runtime: []
  }

  bins: { {NAME}: "bin/{NAME}" }
}

export def build [ctx] {
  # Assemble the package under `$ctx.package_dir`. Verified sources are available at
  # `$ctx.sources.<name>.path`. Replace this stub with the real build steps.
  let bin_dir = ($ctx.package_dir | path join "bin")
  mkdir $bin_dir
  "#!/usr/bin/env sh\nprintf '{NAME} is not implemented yet\n'\n" | save ($bin_dir | path join "{NAME}")
}
"##;
    TEMPLATE
        .replace("{NAME}", name)
        .replace("{VERSION}", version)
}

/// Escapes a value for embedding inside a double-quoted Nushell string in a generated `.rn`.
fn escape_nu_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
