//! Tomes: the git-backed catalogs of runes packages are installed from.
//!
//! This module adds, updates, lists, and removes tomes (cloning and pinning via [`git`]), reads
//! their manifests and package indexes, and authors/publishes them: `tome init`/`tome rune`
//! scaffold a new catalog, and `tome build` compiles runes into verified archives recorded in a
//! git-untracked `dist/index.nuon` served either from a local path or over HTTP.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) mod git;

use crate::{
    cli::{TomeAddArgs, TomeBuildArgs, TomeInitArgs, TomeRemoveArgs, TomeRuneArgs, TomeUpdateArgs},
    install,
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
    progress::{report, status},
    signing, sync_common,
};

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    sync_common::validate_local_source(&args.git_url, "tome.rn")?;

    // The tome names itself: read the `name` from its manifest rather than asking the user
    // to repeat it on the command line. Remote sources are cloned to a temp dir to read it.
    let name = read_source_tome_name(&args.git_url, &args.ref_name)?;
    validate_tome_name(&name)?;
    validate_local_tome_if_available(&name, &args.git_url)?;

    let state_dir = sync_common::state_dir("tomes")?;
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
        &rune_names,
        &mut catalog,
        args.all,
        args.bootstrap,
        args.target.as_deref(),
    )?;
    nuon_io::write_nuon(&index_path, &catalog.to_value())?;

    report(&format!("registered in {}", index_path.display()));
    report(&format!(
        "publish: upload the contents of {} to the location in packages.repo",
        dist_dir.display()
    ));
    Ok(())
}

fn build_runes(
    root: &Path,
    dist_dir: &Path,
    rune_names: &[String],
    catalog: &mut PackageIndex,
    all: bool,
    bootstrap: bool,
    target: Option<&str>,
) -> Result<()> {
    for name in rune_names {
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

    let temp_rune = tempfile::NamedTempFile::with_suffix(".rn")
        .with_context(|| "create temporary rune file")?;
    fs::write(temp_rune.path(), &rune_bytes).with_context(|| "write temporary rune file")?;
    let store_hash = crate::closure::store_hash_for_rune(temp_rune.path())
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
    let mut in_degree: HashMap<String, usize> = HashMap::new();
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
    let mut idx = 0;
    while idx < queue.len() {
        let name = queue[idx].clone();
        idx += 1;
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
  targets: ["linux-x86_64-gnu" "linux-x86_64-musl" "linux-aarch64-gnu" "linux-aarch64-musl" "macos-x86_64-darwin" "macos-aarch64-darwin" "freebsd-x86_64-unknown" "freebsd-aarch64-unknown"]

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
    if sync_common::is_local_source(url, "tome.rn") {
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
    // Note: the loop above prints requested data and uses plain `println!` so it survives `--quiet`.
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
        let sync = sync_tome_cache(&tome)?;
        report(&sync.report_line());
    }
    Ok(())
}

pub fn update_all_configured() -> Result<()> {
    for tome in load_tomes()? {
        let sync = sync_tome_cache(&tome)?;
        report(&sync.report_line());
    }
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    validate_tome_name(&args.name)?;

    let state_path = sync_common::state_dir("tomes")?.join(format!("{}.nuon", args.name));
    if !state_path.exists() {
        bail!("tome `{}` is not configured", args.name);
    }

    fs::remove_file(state_path)?;
    report(&format!("removed tome {}", args.name));
    Ok(())
}

pub fn load_tomes() -> Result<Vec<TomeState>> {
    let state_dir = sync_common::state_dir("tomes")?;
    let mut states = Vec::new();
    for path in sync_common::list_state_files(&state_dir)? {
        states.push(
            TomeState::from_value(nuon_io::read_nuon(&path)?)
                .with_context(|| format!("read tome state {}", path.display()))?,
        );
    }
    Ok(states)
}

pub fn load_tome(name: &str) -> Result<TomeState> {
    let state_path = sync_common::state_dir("tomes")?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        bail!("tome `{name}` is not configured");
    }
    TomeState::from_value(nuon_io::read_nuon(&state_path)?)
        .with_context(|| format!("read tome state {}", state_path.display()))
}

pub fn ensure_tome_cache(tome: &TomeState) -> Result<PathBuf> {
    let cache_path = sync_common::cache_path("tomes", &tome.name)?;
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
    for tome in load_tomes()? {
        let cache = sync_common::cache_path("tomes", &tome.name)?;
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

#[derive(Debug)]
struct TomeSyncReport {
    name: String,
    ref_name: String,
    from_commit: Option<String>,
    to_commit: Option<String>,
}

impl TomeSyncReport {
    fn report_line(&self) -> String {
        if self.from_commit.is_some()
            && self.to_commit.is_some()
            && self.from_commit == self.to_commit
        {
            return format!(
                "tome {} already at latest ({} {})",
                self.name,
                self.ref_name,
                short_commit(self.to_commit.as_deref())
            );
        }
        format!(
            "updated tome {} ({} {} -> {})",
            self.name,
            self.ref_name,
            short_commit(self.from_commit.as_deref()),
            short_commit(self.to_commit.as_deref())
        )
    }
}

fn short_commit(commit: Option<&str>) -> String {
    commit
        .map(|commit| commit.chars().take(7).collect())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn sync_tome_cache(tome: &TomeState) -> Result<TomeSyncReport> {
    let cache_dir = sync_common::cache_dir("tomes")?;
    let cache_path = sync_common::cache_path("tomes", &tome.name)?;
    fs::create_dir_all(&cache_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("grimoire-tome-")
        .tempdir_in(&cache_dir)?;
    let staged = temp.path().join("cache");

    if !sync_common::copy_local_source(&tome.name, &tome.url, &staged, "tome.rn")? {
        status(&format!("cloning tome ({})", tome.name));
        status(&format!(
            "checking out tome ({}) ref ({})",
            tome.name, tome.ref_name
        ));
        git::clone(&tome.url, &tome.ref_name, &staged)
            .with_context(|| format!("could not sync tome `{}`", tome.name))?;
    }

    validate_staged_tome_cache(tome, &staged)?;
    let to_commit = sync_common::promote_cache(&staged, &cache_path, || {
        record_tome_sync_state(tome, &cache_path)
    })?;
    Ok(TomeSyncReport {
        name: tome.name.clone(),
        ref_name: tome.ref_name.clone(),
        from_commit: tome.checked_commit.clone(),
        to_commit,
    })
}

fn validate_local_tome_if_available(name: &str, url: &str) -> Result<()> {
    if !sync_common::is_local_source(url, "tome.rn") {
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
        signer_pubkeys: Vec::new(),
    };
    validate_tome_cache(&tome, source, &manifest, &runtime)
}

fn record_tome_sync_state(tome: &TomeState, cache_path: &Path) -> Result<Option<String>> {
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
    // Trust-on-first-use: capture (or re-confirm) the signing key from this sync before the new
    // state is written, so the pin and the cached manifest always move together.
    let signer_pubkeys = sync_common::capture_signer(
        "tome",
        &tome.name,
        &manifest_path,
        &manifest.signers,
        &state.signer_pubkeys,
    )?;
    state.checked_ref = Some(tome.ref_name.clone());
    state.checked_commit = commit.clone();
    state.tome = Some(manifest);
    state.signer_pubkeys = signer_pubkeys;

    let state_path = sync_common::state_dir("tomes")?.join(format!("{}.nuon", tome.name));
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    Ok(commit)
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

fn validate_staged_tome_cache(tome: &TomeState, cache_path: &Path) -> Result<()> {
    let runtime = EmbeddedNuRuntime;
    let manifest_path = cache_path.join("tome.rn");
    if !manifest_path.exists() {
        bail!(
            "tome cache is missing root tome.rn: {}",
            cache_path.display()
        );
    }
    let manifest = runtime.tome_manifest(&manifest_path)?;
    validate_tome_cache(tome, cache_path, &manifest, &runtime)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("open file for hashing {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read file for hashing {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
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
        let actual_hex = sha256_file(&rune_path)
            .with_context(|| format!("hash rune {}", rune_path.display()))?;
        if actual_hex != expected_hex {
            bail!(
                "runes manifest hash mismatch for `{name}`: expected {expected}, got sha256:{actual_hex}"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_report_says_already_latest_when_commit_is_unchanged() {
        let report = TomeSyncReport {
            name: "core".to_owned(),
            ref_name: "main".to_owned(),
            from_commit: Some("abcdef1234567890".to_owned()),
            to_commit: Some("abcdef1234567890".to_owned()),
        };

        assert_eq!(
            report.report_line(),
            "tome core already at latest (main abcdef1)"
        );
    }

    #[test]
    fn sync_report_shows_commit_movement_when_commit_changes() {
        let report = TomeSyncReport {
            name: "core".to_owned(),
            ref_name: "main".to_owned(),
            from_commit: Some("abcdef1234567890".to_owned()),
            to_commit: Some("1234567890abcdef".to_owned()),
        };

        assert_eq!(
            report.report_line(),
            "updated tome core (main abcdef1 -> 1234567)"
        );
    }
}
