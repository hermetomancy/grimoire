//! Tomes: the git-backed catalogs of runes packages are installed from.
//!
//! This module adds, updates, lists, and removes tomes (cloning and pinning via [`git`]), reads
//! their manifests and package indexes, and authors/publishes them: `tome init`/`tome rune`
//! scaffold a new catalog, and `tome build` compiles runes into verified archives recorded in a
//! git-untracked `dist/index.nuon` served either from a local path or over HTTP.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) mod git;
pub(crate) mod news;

use crate::{
    archive,
    cli::{TomeAddArgs, TomeBuildArgs, TomeInitArgs, TomeRemoveArgs, TomeRuneArgs, TomeUpdateArgs},
    install,
    model::{
        Catalog, IndexEntry, PackageIndex, TomeManifest, TomePackages, TomeState,
        validate_package_name, validate_package_version, validate_relative_package_path,
        validate_tome_name, validate_tome_ref, validate_tome_url,
    },
    nu::{nuon_io, runtime::EmbeddedNuRuntime},
    paths,
    progress::{report, status},
    signing, sync_common,
};

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    sync_common::validate_local_source(&args.git_url, "tome.rn")?;

    // The tome names itself: read the `name` from its manifest rather than asking the user
    // to repeat it on the command line. Remote sources are cloned to a temp dir to read it.
    let name = sync_common::read_source_catalog_name(
        &args.git_url,
        &args.ref_name,
        "tome.rn",
        |path| {
            let manifest_path = path.join("tome.rn");
            Ok(EmbeddedNuRuntime
                .tome_manifest(&manifest_path)
                .with_context(|| format!("read tome manifest {}", manifest_path.display()))?
                .name)
        },
        git::clone,
    )?;
    validate_tome_name(&name)?;
    validate_local_tome_if_available(&name, &args.git_url)?;

    let state_dir = sync_common::state_dir(TomeState::SUBDIR)?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("tome `{name}` already exists");
    }

    fs::create_dir_all(&state_dir)?;
    let state = TomeState {
        name: name.clone(),
        url: sync_common::resolve_source_url(&args.git_url, "tome.rn")?,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        tome: None,
        signer_pubkeys: args.signer,
        last_seen_news: None,
    };
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    report(&format!("added tome {name}"));
    Ok(())
}

/// Scaffolds a new tome: a self-naming `tome.rn` manifest, empty `runes/` and `sources/`
/// directories, a git-untracked `dist/` publish directory, and a `.gitignore` that keeps `dist/`
/// out of git. The git repository holds only runes and `tome.rn`; `grm tome build` writes built
/// archives and `index.nuon` into `dist/`, which the author uploads to the host in `packages.repo`.
pub fn init(args: TomeInitArgs) -> Result<()> {
    validate_tome_name(&args.name)?;

    let root = &args.path;
    let manifest_path = root.join("tome.rn");
    if manifest_path.exists() {
        bail!("{} already contains a tome.rn", root.display());
    }

    fs::create_dir_all(root.join("runes"))?;
    fs::create_dir_all(root.join("sources"))?;
    fs::create_dir_all(root.join("dist"))?;

    let description = args
        .description
        .unwrap_or_else(|| format!("{} tome", args.name));
    fs::write(
        &manifest_path,
        tome_manifest_template(&args.name, &description),
    )?;

    let gitignore_path = root.join(".gitignore");
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, "/dist/\n")?;
    }

    report(&format!("created tome {} in {}", args.name, root.display()));
    report(&format!(
        "next: add a package with `grm tome rune <name> --path {}`",
        root.display()
    ));
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
    report(&format!(
        "created rune {} in {}",
        args.name,
        rune_path.display()
    ));
    Ok(())
}

/// Builds a tome's rune into a `.tar.zst` inside the tome's git-untracked publish directory
/// (`dist/`) and registers (or replaces) its entry in the publish directory's `index.nuon`. The
/// author uploads the whole `dist/` directory to the host named by `packages.repo`; the git
/// repository itself holds only runes and `tome.rn`.
pub fn build(args: TomeBuildArgs) -> Result<()> {
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

    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir)?;
    let index_path = dist_dir.join(&packages.index);

    if args.index {
        let catalog = rebuild_index(&dist_dir)?;
        nuon_io::write_nuon(&index_path, &catalog.to_value())?;
        report(&format!(
            "rebuilt index with {} package(s) in {}",
            catalog.entries.len(),
            index_path.display()
        ));
        return Ok(());
    }

    // Decide which runes to build: every rune in `runes/` for `--all`, otherwise the single
    // named package. clap already rejects passing both, so exactly one branch applies.
    let rune_names = if args.all {
        let names = rune_names_ordered(root)?;
        if names.is_empty() {
            bail!("tome `{}` has no runes to build", manifest.name);
        }
        names
    } else {
        let Some(package) = args.package.as_deref() else {
            bail!("specify a rune to build, or pass --all to build every rune");
        };
        vec![package.to_owned()]
    };

    let mut catalog = if index_path.exists() {
        PackageIndex::from_value(nuon_io::read_nuon(&index_path)?)
            .with_context(|| format!("parse package index {}", index_path.display()))?
    } else {
        PackageIndex {
            entries: std::collections::BTreeMap::new(),
        }
    };

    build_runes(
        root,
        &dist_dir,
        &index_path,
        args.all,
        args.bootstrap,
        args.target.as_deref(),
        args.force,
        &rune_names,
        &mut catalog,
    )?;

    report(&format!("registered in {}", index_path.display()));
    report(&format!(
        "publish: upload the contents of {} to the location in packages.repo",
        dist_dir.display()
    ));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_runes(
    root: &Path,
    dist_dir: &Path,
    index_path: &Path,
    all: bool,
    bootstrap: bool,
    target: Option<&str>,
    force: bool,
    rune_names: &[String],
    catalog: &mut PackageIndex,
) -> Result<()> {
    let host_target = paths::target_triple();
    let current_target = target.unwrap_or(&host_target);
    let mut any_built = false;
    for name in rune_names {
        if !force {
            if let Some(existing) = catalog
                .entries
                .values()
                .find(|e| e.name == *name && e.target == current_target)
            {
                let archive_path = dist_dir.join(format!(
                    "{}-{}-{}.tar.zst",
                    existing.name, existing.version, existing.target
                ));
                if archive_path.exists() {
                    status(&format!(
                        "skipping {} {} (already built; pass --force to rebuild)",
                        existing.name, existing.version
                    ));
                    continue;
                }
            }
        }
        let (store_hash, entry, archive) =
            build_rune_into(root, name, dist_dir, bootstrap, target)?;
        report(&format!(
            "built {} {} ({}) into {}",
            entry.name,
            entry.version,
            entry.target,
            archive.display()
        ));
        if all {
            install::install_store_only(&archive, None, None)
                .with_context(|| format!("store-only install of {}", entry.name))?;
        }
        catalog.upsert(store_hash, entry);
        any_built = true;
    }
    // Write the index once, atomically, after all runes built successfully.
    // If any rune failed, the previous index and dist/ remain untouched.
    if any_built {
        nuon_io::write_nuon(index_path, &catalog.to_value())
            .with_context(|| format!("update index {}", index_path.display()))?;
    }
    Ok(())
}

/// Builds the rune named `name` (`runes/<name>.rn`) into `dist_dir`, returning the store hash,
/// index entry describing the verified archive, and the archive path. Shared by single-package
/// and `--all` builds so both register identical entries.
fn build_rune_into(
    root: &Path,
    name: &str,
    dist_dir: &Path,
    bootstrap: bool,
    target: Option<&str>,
) -> Result<(String, IndexEntry, PathBuf)> {
    validate_package_name(name)?;
    let rune_path = root.join("runes").join(format!("{name}.rn"));
    if !rune_path.exists() {
        bail!("rune not found: {}", rune_path.display());
    }

    let result =
        crate::build::build_package(&rune_path.to_string_lossy(), dist_dir, bootstrap, target)?;
    let archive_hash = crate::archive::archive_hash(&result.archive)?;
    let archive_file = result
        .archive
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| {
            format!(
                "archive path has no file name: {}",
                result.archive.display()
            )
        })?;

    let metadata = EmbeddedNuRuntime.package_metadata(&rune_path)?;
    let resolved_target = target.map_or_else(paths::target_triple, |t| t.to_string());
    let entry = IndexEntry {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: resolved_target,
        archive: archive_file.to_owned(),
        archive_hash,
        runtime_deps: metadata.deps.runtime.clone(),
        provides: result.discovered_bins.keys().cloned().collect(),
        libs: result.libs.clone(),
    };
    Ok((result.store_hash, entry, result.archive))
}

/// Rebuilds the package index from every `.tar.zst` archive already present in `dist_dir`.
/// Each archive is inspected for its embedded metadata and rune so the index entry is identical
/// to what a fresh build would produce.
fn rebuild_index(dist_dir: &Path) -> Result<PackageIndex> {
    let mut entries = std::collections::BTreeMap::new();
    for entry in fs::read_dir(dist_dir)
        .with_context(|| format!("read dist directory {}", dist_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".tar.zst") {
            continue;
        }
        match read_archive_index_entry(&path) {
            Ok((store_hash, index_entry)) => {
                report(&format!(
                    "indexed {} {} ({}) from {}",
                    index_entry.name, index_entry.version, index_entry.target, name
                ));
                entries.insert(store_hash, index_entry);
            }
            Err(e) => {
                report(&format!("warning: skipping {}: {e}", path.display()));
            }
        }
    }
    Ok(PackageIndex { entries })
}

/// Reads an existing archive and produces the `(store_hash, IndexEntry)` that would describe it.
fn read_archive_index_entry(path: &Path) -> Result<(String, IndexEntry)> {
    archive::validate_archive_paths(path)
        .with_context(|| format!("validate archive {}", path.display()))?;

    let file = fs::File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode archive {}", path.display()))?;
    let mut archive = tar::Archive::new(decoder);

    let mut metadata = None;
    let mut rune_bytes = None;

    for entry in archive.entries().context("read archive entries")? {
        let mut entry = entry?;
        let path_str = entry.path()?.to_string_lossy().to_string();
        let normalized = path_str.strip_prefix("./").unwrap_or(&path_str);

        if normalized == ".grimoire/package.nuon" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            metadata = Some(
                crate::model::PackageMetadata::from_value(nuon_io::parse_nuon(&text)?, true)
                    .with_context(|| format!("parse metadata in {}", path.display()))?,
            );
        } else if normalized == ".grimoire/rune.rn" {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            rune_bytes = Some(bytes);
        }
    }

    let metadata = metadata.ok_or_else(|| {
        anyhow::anyhow!(
            "archive {} is missing .grimoire/package.nuon",
            path.display()
        )
    })?;
    let rune_bytes = rune_bytes.ok_or_else(|| {
        anyhow::anyhow!("archive {} is missing .grimoire/rune.rn", path.display())
    })?;

    let store_hash = crate::closure::store_hash_for_rune_bytes(&rune_bytes, &metadata)
        .with_context(|| format!("compute store hash for {}", path.display()))?;

    let archive_hash = crate::archive::archive_hash(path)
        .with_context(|| format!("hash archive {}", path.display()))?;
    let archive_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("archive path has no file name: {}", path.display()))?
        .to_owned();

    let target = metadata
        .target
        .ok_or_else(|| anyhow::anyhow!("metadata in {} is missing target", path.display()))?;

    Ok((
        store_hash,
        IndexEntry {
            name: metadata.name,
            version: metadata.version,
            target,
            archive: archive_name,
            archive_hash,
            runtime_deps: metadata.deps.runtime,
            provides: metadata.provides,
            libs: metadata.libs,
        },
    ))
}

/// The rune base names (without the `.rn` extension) in a tome's `runes/` directory, sorted for
/// deterministic build order. Returns an empty list when there is no `runes/` directory.
fn rune_names(root: &Path) -> Result<Vec<String>> {
    let runes_dir = root.join("runes");
    if !runes_dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&runes_dir)
        .with_context(|| format!("read runes directory {}", runes_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rn") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Returns rune names in dependency order: a rune's build dependencies appear before the rune
/// itself. Cycles within the tome are reported as errors.
fn rune_names_ordered(root: &Path) -> Result<Vec<String>> {
    let names = rune_names(root)?;
    if names.is_empty() {
        return Ok(names);
    }

    let target = paths::target_triple();
    let mut metadata_map: BTreeMap<String, crate::model::PackageMetadata> = BTreeMap::new();
    for name in &names {
        let rune_path = root.join("runes").join(format!("{name}.rn"));
        let metadata = EmbeddedNuRuntime
            .package_metadata(&rune_path)
            .with_context(|| format!("read metadata for {name}"))?;
        // Skip runes that explicitly declare targets and don't include the current one.
        if !metadata.targets.is_empty() && !metadata.targets.contains(&target) {
            continue;
        }
        metadata_map.insert(name.clone(), metadata);
    }

    let filtered_names: Vec<String> = metadata_map.keys().cloned().collect();

    // Build adjacency list: dependent -> [its dependencies within this tome]
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
    for name in &filtered_names {
        in_degree.entry(name.clone()).or_insert(0);
    }
    for name in &filtered_names {
        let metadata = &metadata_map[name];
        let build_deps = metadata.deps.build_for(&target);
        for dep in build_deps {
            if metadata_map.contains_key(&dep.name) {
                adj.entry(dep.name.clone()).or_default().push(name.clone());
                *in_degree.entry(name.clone()).or_insert(0) += 1;
            }
        }
    }

    // Kahn's algorithm
    let mut queue: Vec<String> = filtered_names
        .iter()
        .filter(|n| *in_degree.get(*n).unwrap_or(&0) == 0)
        .cloned()
        .collect();
    queue.sort(); // Deterministic tie-break for seeds
    let mut ordered = Vec::new();
    while let Some(name) = queue.pop() {
        ordered.push(name.clone());
        if let Some(deps) = adj.get(&name) {
            for dep in deps {
                let Some(count) = in_degree.get_mut(dep) else {
                    bail!("missing in_degree entry for dependency `{dep}` in topological sort");
                };
                *count -= 1;
                if *count == 0 {
                    queue.push(dep.clone());
                }
            }
        }
    }

    if ordered.len() != filtered_names.len() {
        let remaining: Vec<String> = filtered_names
            .into_iter()
            .filter(|n| !ordered.contains(n))
            .collect();
        bail!(
            "build dependency cycle detected among runes: {}",
            remaining.join(", ")
        );
    }

    Ok(ordered)
}

fn tome_manifest_template(name: &str, description: &str) -> String {
    const TEMPLATE: &str = r#"export const tome = {
  name: "{NAME}"
  description: "{DESCRIPTION}"

  # `grm tome build` writes archives and index.nuon into the git-untracked dist/ directory.
  # Upload dist/ to a webserver and point `repo` at the base URL that serves it. For local
  # testing `repo` may instead be an absolute path to the dist/ directory.
  packages: {
    repo: "https://example.com/{NAME}"
    format: "http"
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
}

export def build [ctx] {
  # Assemble the package under `$ctx.package_dir`. Verified sources are available at
  # `$ctx.sources.<name>.path`. Replace this stub with the real build steps.
  let bin_dir = ($ctx.package_dir | path join "bin")
  mkdir $bin_dir
  "#!/usr/bin/env sh\nprintf '{NAME} is not implemented yet\n'" | save ($bin_dir | path join "{NAME}")
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

pub fn list() -> Result<()> {
    sync_common::list_catalogs::<TomeState>()
}

pub fn update(args: TomeUpdateArgs) -> Result<()> {
    let tomes = match args.name {
        Some(name) => {
            validate_tome_name(&name)?;
            vec![sync_common::load_catalog::<TomeState>(&name)?]
        }
        None => sync_common::load_catalogs::<TomeState>()?,
    };

    if tomes.is_empty() {
        bail!("no tomes are configured");
    }

    for tome in tomes {
        let line = sync_tome_cache(&tome)?;
        report(&line);
    }
    Ok(())
}

pub fn update_all_configured() -> Result<()> {
    for tome in sync_common::load_catalogs::<TomeState>()? {
        let line = sync_tome_cache(&tome)?;
        report(&line);
    }
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    sync_common::remove_catalog::<TomeState>(&args.name, false)
}

pub fn load_tomes() -> Result<Vec<TomeState>> {
    sync_common::load_catalogs::<TomeState>()
}

pub fn ensure_tome_cache(tome: &TomeState) -> Result<PathBuf> {
    let cache_path = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
    // Re-sync when the cache is missing *or* invalid: a sync that failed partway (e.g. after a
    // misconfigured URL) can leave a directory with no root `tome.rn`, which would otherwise be
    // trusted forever and fail every later read. Treating that as "not cached" lets it self-heal.
    if !cache_path.join("tome.rn").exists() {
        sync_tome_cache(tome)?;
    }
    Ok(cache_path)
}

/// Finds the tome whose cache directory contains `path`. Returns `None` when the path is not
/// inside any configured tome cache (e.g. a local `.rn` file passed directly on the CLI).
pub fn find_tome_for_path(path: &std::path::Path) -> Result<Option<TomeState>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    for tome in sync_common::load_catalogs::<TomeState>()? {
        let cache = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
        let cache = if cache.exists() {
            cache
                .canonicalize()
                .with_context(|| format!("canonicalize tome cache {}", cache.display()))?
        } else {
            cache
        };
        if canonical.starts_with(&cache) {
            return Ok(Some(tome));
        }
    }
    Ok(None)
}

/// Verifies a rune's detached signature (`package.rn.minisig`) against the tome's pinned signers.
/// Returns `Ok` when the rune is not inside any tome cache, when the tome has no pinned signers,
/// or when the signature verifies against one of the pinned keys.
pub fn verify_rune(rune: &std::path::Path) -> Result<()> {
    let Some(tome) = find_tome_for_path(rune)? else {
        return Ok(());
    };
    if tome.signer_pubkeys.is_empty() {
        return Ok(());
    }
    signing::verify_detached(rune, &tome.signer_pubkeys)
        .with_context(|| format!("verify rune signature for {}", rune.display()))
}

/// Verifies an archive's detached signature (`archive.tar.zst.minisig`) against the tome's pinned
/// signers. Returns `Ok` when the tome has no pinned signers or when the signature verifies.
pub fn verify_archive(archive: &std::path::Path, tome: &TomeState) -> Result<()> {
    if tome.signer_pubkeys.is_empty() {
        return Ok(());
    }
    signing::verify_detached(archive, &tome.signer_pubkeys)
        .with_context(|| format!("verify archive signature for {}", archive.display()))
}

fn sync_report_line(name: &str, ref_name: &str, from: Option<&str>, to: Option<&str>) -> String {
    if from.is_some() && to.is_some() && from == to {
        format!(
            "tome {name} already at latest ({ref_name} {})",
            short_commit(to)
        )
    } else {
        format!(
            "updated tome {name} ({ref_name} {} -> {})",
            short_commit(from),
            short_commit(to)
        )
    }
}

fn short_commit(commit: Option<&str>) -> String {
    commit
        .map(|commit| commit.chars().take(7).collect())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn sync_tome_cache(tome: &TomeState) -> Result<String> {
    let from_commit = tome.checked_commit.clone();
    let to_commit = sync_common::sync_catalog_cache(
        tome,
        "tome.rn",
        git::clone,
        |tome, cache_path| {
            let manifest = read_tome_manifest(cache_path)?;
            validate_tome_cache(tome, cache_path, &manifest, &EmbeddedNuRuntime)
        },
        |tome, cache_path| {
            let manifest = read_tome_manifest(cache_path)?;
            let commit = git::head_commit(cache_path)?;
            sync_common::record_catalog_sync_state::<TomeState>(
                tome,
                cache_path,
                "tome.rn",
                &manifest,
                commit.as_deref(),
            )?;
            Ok(commit)
        },
    )?;
    // The very first sync after `tome add` marks the existing news backlog as seen without
    // printing it; later syncs surface only what arrived since. `checked_ref` is recorded on
    // every successful sync (commits are absent for local tomes), so its absence in the state
    // we were called with means this sync was the first. News problems must not fail the sync
    // itself — the cache and state are already committed.
    let first_sync = tome.checked_ref.is_none();
    let cache_path = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
    if let Err(e) = news::surface_after_sync(&tome.name, &cache_path, first_sync) {
        report(&format!("warning: could not surface tome news: {e}"));
    }
    Ok(sync_report_line(
        &tome.name,
        &tome.ref_name,
        from_commit.as_deref(),
        to_commit.as_deref(),
    ))
}

fn validate_local_tome_if_available(name: &str, url: &str) -> Result<()> {
    if !sync_common::is_local_source(url, "tome.rn") {
        return Ok(());
    }

    let source = Path::new(url);
    let manifest = EmbeddedNuRuntime
        .tome_manifest(&source.join("tome.rn"))
        .with_context(|| format!("validate local tome manifest {}", source.display()))?;
    let tome = TomeState {
        name: name.to_owned(),
        url: url.to_owned(),
        ref_name: "local-validation".to_owned(),
        checked_ref: None,
        checked_commit: None,
        tome: None,
        signer_pubkeys: Vec::new(),
        last_seen_news: None,
    };
    validate_tome_cache(&tome, source, &manifest, &EmbeddedNuRuntime)
}

/// Loads a tome's binary package index along with the package-repository root that its
/// (relative) archive locations resolve against. `None` means the tome declares no package
/// index or has not published one yet, so callers fall back to a source build.
pub fn package_index(tome: &TomeState) -> Result<Option<(PathBuf, crate::model::PackageIndex)>> {
    let cache = ensure_tome_cache(tome)?;
    let manifest = EmbeddedNuRuntime.tome_manifest(&cache.join("tome.rn"))?;
    let Some(packages) = manifest.packages else {
        return Ok(None);
    };

    let Some(raw) = load_raw_index(&cache, &packages)? else {
        return Ok(None);
    };

    parse_resolved_index(&cache, &packages, &raw).map(Some)
}

/// Loads a tome's raw `index.nuon` text. Returns `None` when the tome has no package repo or
/// the index file does not exist yet.
fn load_raw_index(cache: &Path, packages: &TomePackages) -> Result<Option<String>> {
    if is_http_repo(&packages.repo) {
        let base = packages.repo.trim_end_matches('/');
        let index_url = format!("{base}/{}", packages.index);
        crate::fetch::http_get_text(&index_url)
    } else {
        let root = packages_repo_root(cache, packages);
        let index_path = root.join(&packages.index);
        if !index_path.exists() {
            return Ok(None);
        }
        fs::read_to_string(&index_path)
            .with_context(|| format!("read package index {}", index_path.display()))
            .map(Some)
    }
}

/// Parses a verified index document and resolves each entry's archive location. For an `http(s)`
/// repo, relative archive paths are rewritten to absolute `{repo}/{archive}` URLs; for a local
/// repo, the returned root is the directory archive paths resolve against. The index is parsed
/// from the already-verified `text` rather than re-read from disk, so verification and parsing
/// see identical bytes.
fn parse_resolved_index(
    cache: &Path,
    packages: &TomePackages,
    text: &str,
) -> Result<(PathBuf, crate::model::PackageIndex)> {
    let mut index = crate::model::PackageIndex::from_value(nuon_io::parse_nuon(text)?)
        .context("parse package index")?;
    if is_http_repo(&packages.repo) {
        let base = packages.repo.trim_end_matches('/');
        for entry in index.entries.values_mut() {
            if !is_http_repo(&entry.archive) {
                entry.archive = format!("{base}/{}", entry.archive);
            }
        }
        // Archives now carry absolute URLs, so the base directory is unused at fetch time.
        Ok((PathBuf::new(), index))
    } else {
        Ok((packages_repo_root(cache, packages), index))
    }
}

/// Resolves the local directory that holds a tome's package index and (relative) archives — the
/// publish directory an author built and the host serves. An absolute path is used directly; a
/// relative path resolves against the tome cache.
fn packages_repo_root(cache: &Path, packages: &TomePackages) -> PathBuf {
    let path = Path::new(&packages.repo);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cache.join(path)
    }
}

fn is_http_repo(repo: &str) -> bool {
    repo.starts_with("http://") || repo.starts_with("https://")
}

fn read_tome_manifest(path: &Path) -> Result<TomeManifest> {
    let manifest_path = path.join("tome.rn");
    if !manifest_path.exists() {
        bail!("tome cache is missing root tome.rn: {}", path.display());
    }
    EmbeddedNuRuntime
        .tome_manifest(&manifest_path)
        .with_context(|| format!("read tome manifest {}", manifest_path.display()))
}

fn verify_runes_manifest(cache: &Path, pubkeys: &[String]) -> Result<()> {
    let runes = read_runes_manifest(cache, pubkeys)?;
    let runes_dir = cache.join("runes");
    verify_rune_hashes(&runes, &runes_dir)?;
    check_for_extra_runes(&runes, &runes_dir)?;
    Ok(())
}

fn read_runes_manifest(cache: &Path, pubkeys: &[String]) -> Result<nu_protocol::Record> {
    let manifest_path = cache.join("runes-manifest.nuon");
    if !manifest_path.exists() {
        bail!("runes-manifest.nuon is missing");
    }

    signing::verify_detached(&manifest_path, pubkeys)
        .with_context(|| "verify runes-manifest.nuon signature")?;

    let value = nuon_io::read_nuon(&manifest_path)?;
    let record = crate::model::expect_record(value, "runes manifest")?;

    let format = crate::model::optional_i64(&record, "format")?.unwrap_or(0);
    if format != 1 {
        bail!("unsupported runes manifest format {format}; expected 1");
    }

    match record.get("runes") {
        Some(nu_protocol::Value::Record { val, .. }) => Ok(val.clone().into_owned()),
        Some(_) => bail!("runes manifest field `runes` must be a record"),
        None => bail!("runes manifest is missing required field `runes`"),
    }
}

fn verify_rune_hashes(runes: &nu_protocol::Record, runes_dir: &Path) -> Result<()> {
    for (name, hash_value) in runes.iter() {
        let expected = match hash_value {
            nu_protocol::Value::String { val, .. } => val.as_str(),
            _ => bail!("runes manifest entry `{name}` must be a string"),
        };
        let expected_hex = expected.strip_prefix("sha256:").unwrap_or(expected);
        let rune_path = runes_dir.join(name);
        if !rune_path.exists() {
            bail!("runes manifest lists `{name}` but it does not exist in runes/");
        }
        let actual_hash = crate::archive::archive_hash(&rune_path)
            .with_context(|| format!("hash rune {}", rune_path.display()))?;
        let actual_hex = actual_hash.strip_prefix("sha256:").unwrap_or(&actual_hash);
        if actual_hex != expected_hex {
            bail!(
                "runes manifest hash mismatch for `{name}`: expected {expected}, got {actual_hash}"
            );
        }
    }
    Ok(())
}

fn check_for_extra_runes(runes: &nu_protocol::Record, runes_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(runes_dir)
        .with_context(|| format!("read runes directory {}", runes_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rn") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("rune path has invalid file name: {}", path.display()))?;
        if runes.get(name).is_none() {
            bail!("extra rune file `{name}` found in runes/ but not listed in runes-manifest.nuon");
        }
    }
    Ok(())
}

fn validate_tome_cache(
    tome: &TomeState,
    cache_path: &Path,
    manifest: &TomeManifest,
    runtime: &EmbeddedNuRuntime,
) -> Result<()> {
    sync_common::validate_catalog_identity::<TomeState>(tome, manifest)?;

    let packages = manifest
        .packages
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tome manifest is missing required field `packages`"))?;
    validate_tome_packages(packages)?;

    // Every package is defined by a rune (even an x-bin package carries a fetch-and-verify rune),
    // so a tome must ship a `runes/` directory with at least one definition, and no two may collide
    // on package name.
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

    if !manifest.signers.is_empty() {
        let pubkeys = if tome.signer_pubkeys.is_empty() {
            &manifest.signers
        } else {
            &tome.signer_pubkeys
        };
        verify_runes_manifest(cache_path, pubkeys)?;
    }

    Ok(())
}

fn validate_tome_packages(packages: &TomePackages) -> Result<()> {
    validate_tome_url(&packages.repo).context("validate tome packages.repo")?;
    let is_http = is_http_repo(&packages.repo);
    match packages.format.as_str() {
        "http" if is_http => {}
        "local" if !is_http => {}
        "http" => bail!("tome packages.format `http` requires an http(s) packages.repo URL"),
        "local" => bail!("tome packages.format `local` requires a filesystem packages.repo path"),
        other => {
            bail!("tome packages.format `{other}` is not supported; expected `http` or `local`")
        }
    }
    validate_relative_package_path(&packages.index, "tome packages.index")?;
    Ok(())
}
