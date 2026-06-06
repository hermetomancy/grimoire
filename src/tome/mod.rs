//! Tomes: the git-backed catalogs of runes packages are installed from.
//!
//! This module adds, updates, lists, and removes tomes (cloning and pinning via [`git`]), reads
//! their manifests and package indexes, and authors/publishes them: `tome init`/`tome rune`
//! scaffold a new catalog, and `tome build` compiles runes into verified archives recorded in a
//! git-untracked `dist/index.nuon` served either from a local path or over HTTP.

use anyhow::{Context, Result, bail};
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
    signing,
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
        url: resolve_tome_source_url(&args.git_url)?,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        tome: None,
        signer_pubkey: None,
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
            catalog.packages.len(),
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
        PackageIndex::from_value(nuon_io::read_nuon(&index_path)?)?
    } else {
        PackageIndex {
            packages: Vec::new(),
        }
    };

    // Build each rune, upserting its entry as we go, then write the index once so a multi-rune
    // build records every package atomically rather than rewriting the file per rune.
    // In `--all` mode, each built package is also installed store-only so subsequent runes can
    // depend on it as a build dependency without requiring a pre-installed userland.
    let target = paths::target_triple();
    for name in &rune_names {
        let (entry, archive) = build_rune_into(root, name, &dist_dir, args.bootstrap)?;
        report(&format!(
            "built {} {} ({target}) into {}",
            entry.name,
            entry.version,
            archive.display()
        ));
        if args.all {
            install::install_store_only(&archive, None, None)
                .with_context(|| format!("store-only install of {}", entry.name))?;
        }
        catalog.upsert(entry);
    }
    nuon_io::write_nuon(&index_path, &catalog.to_value())?;

    report(&format!("registered in {}", index_path.display()));
    report(&format!(
        "publish: upload the contents of {} to the location in packages.repo",
        dist_dir.display()
    ));
    Ok(())
}

/// Builds the rune named `name` (`runes/<name>.rn`) into `dist_dir`, returning the index entry
/// describing the verified archive and the archive path. Shared by single-package and `--all`
/// builds so both register identical entries.
fn build_rune_into(
    root: &Path,
    name: &str,
    dist_dir: &Path,
    bootstrap: bool,
) -> Result<(IndexEntry, PathBuf)> {
    validate_package_name(name)?;
    let rune_path = root.join("runes").join(format!("{name}.rn"));
    if !rune_path.exists() {
        bail!("rune not found: {}", rune_path.display());
    }

    let result = crate::build::build_package(&rune_path.to_string_lossy(), dist_dir, bootstrap)?;
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
    let entry = IndexEntry {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: paths::target_triple(),
        archive: archive_file.to_owned(),
        archive_hash,
        store_hash: result.store_hash,
        runtime_deps: metadata.deps.runtime.clone(),
    };
    Ok((entry, result.archive))
}

/// Rebuilds the package index from every `.tar.zst` archive already present in `dist_dir`.
/// Each archive is inspected for its embedded metadata and rune so the index entry is identical
/// to what a fresh build would produce.
fn rebuild_index(dist_dir: &Path) -> Result<PackageIndex> {
    let mut entries = Vec::new();
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
            Ok(index_entry) => {
                report(&format!(
                    "indexed {} {} ({}) from {}",
                    index_entry.name, index_entry.version, index_entry.target, name
                ));
                entries.push(index_entry);
            }
            Err(e) => {
                report(&format!("warning: skipping {}: {e}", path.display()));
            }
        }
    }
    Ok(PackageIndex { packages: entries })
}

/// Reads an existing archive and produces the [`IndexEntry`] that would describe it.
fn read_archive_index_entry(path: &Path) -> Result<IndexEntry> {
    let file = fs::File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode archive {}", path.display()))?;
    let mut archive = tar::Archive::new(decoder);

    let mut metadata = None;
    let mut rune_bytes = None;

    for entry in archive.entries()? {
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

    Ok(IndexEntry {
        name: metadata.name,
        version: metadata.version,
        target,
        archive: archive_name,
        archive_hash,
        store_hash,
        runtime_deps: metadata.deps.runtime,
    })
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
        metadata_map.insert(name.clone(), metadata);
    }

    // Build adjacency list: dependent -> [its dependencies within this tome]
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for name in &names {
        in_degree.entry(name.clone()).or_insert(0);
    }
    for name in &names {
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
    let mut queue: Vec<String> = names
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
                let count = in_degree.get_mut(dep).expect("in_degree entry");
                *count -= 1;
                if *count == 0 {
                    queue.push(dep.clone());
                }
            }
        }
    }

    if ordered.len() != names.len() {
        let remaining: Vec<String> = names.into_iter().filter(|n| !ordered.contains(n)).collect();
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

    let state_path = tome_state_dir()?.join(format!("{}.nuon", args.name));
    if !state_path.exists() {
        bail!("tome `{}` is not configured", args.name);
    }

    fs::remove_file(state_path)?;
    report(&format!("removed tome {}", args.name));
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

pub fn ensure_tome_cache(tome: &TomeState) -> Result<PathBuf> {
    let cache_path = tome_cache_path(&tome.name)?;
    // Re-sync when the cache is missing *or* invalid: a sync that failed partway (e.g. after a
    // misconfigured URL) can leave a directory with no root `tome.rn`, which would otherwise be
    // trusted forever and fail every later read. Treating that as "not cached" lets it self-heal.
    if !cache_path.join("tome.rn").exists() {
        sync_tome_cache(tome)?;
    }
    Ok(cache_path)
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
    let cache_dir = tome_cache_dir()?;
    let cache_path = tome_cache_path(&tome.name)?;
    fs::create_dir_all(&cache_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("grimoire-tome-")
        .tempdir_in(&cache_dir)?;
    let staged = temp.path().join("cache");

    if is_local_tome_source(&tome.url) {
        let source = PathBuf::from(&tome.url)
            .canonicalize()
            .with_context(|| format!("resolve tome source {}", tome.url))?;
        status(&format!("copying local tome ({})", tome.name));
        copy_dir_all(&source, &staged)?;
    } else {
        sync_remote_tome_cache(tome, &staged)?;
    }

    validate_staged_tome_cache(tome, &staged)?;
    let to_commit = promote_tome_cache(tome, &staged, &cache_path)?;
    Ok(TomeSyncReport {
        name: tome.name.clone(),
        ref_name: tome.ref_name.clone(),
        from_commit: tome.checked_commit.clone(),
        to_commit,
    })
}

fn is_local_tome_source(url: &str) -> bool {
    let path = Path::new(url);
    path.is_dir() && path.join("tome.rn").exists()
}

/// Normalizes the URL stored for a tome. A local tome source is canonicalized to an absolute
/// path so later syncs — which run from whatever directory the user invokes a command in —
/// resolve to the same directory `add` read the manifest from, rather than re-interpreting a
/// relative path like `.` against the new working directory. Remote URLs pass through unchanged.
fn resolve_tome_source_url(url: &str) -> Result<String> {
    if !is_local_tome_source(url) {
        return Ok(url.to_owned());
    }
    let absolute = Path::new(url)
        .canonicalize()
        .with_context(|| format!("resolve local tome source {url}"))?;
    Ok(absolute.to_string_lossy().into_owned())
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
        signer_pubkey: None,
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
    let signer_pubkey = capture_signer(tome, cache_path, &manifest, state.signer_pubkey.clone())?;
    state.checked_ref = Some(tome.ref_name.clone());
    state.checked_commit = commit.clone();
    state.tome = Some(manifest);
    state.signer_pubkey = signer_pubkey;

    let state_path = tome_state_dir()?.join(format!("{}.nuon", tome.name));
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

    // Enforce the trust-on-first-use pin before the index is parsed or any archive is fetched:
    // the index is the trust root for binary installs, so a signed tome's index must verify
    // against the key pinned when it was added (`src/signing.rs`). An unsigned tome (no pin)
    // skips verification.
    enforce_pinned_signature(tome, &raw)?;

    parse_resolved_index(&cache, &packages, &raw.text).map(Some)
}

/// A package index document as published, together with its detached minisign signature when the
/// repository serves one. The signature, when present, is over the exact bytes in `text`.
struct RawIndex {
    text: String,
    signature: Option<String>,
}

/// Fetches a tome's raw index document and its `.minisig` signature (if published), without
/// parsing or trusting either yet. Handles both an `http(s)` package repo and a local/filesystem
/// one. `None` means the tome declares a package repo but has not published an index there.
fn load_raw_index(cache: &Path, packages: &TomePackages) -> Result<Option<RawIndex>> {
    if is_http_repo(&packages.repo) {
        let base = packages.repo.trim_end_matches('/');
        let index_url = format!("{base}/{}", packages.index);
        let Some(text) = crate::fetch::http_get_text(&index_url)? else {
            return Ok(None);
        };
        let signature =
            crate::fetch::http_get_text(&format!("{index_url}.{}", signing::SIGNATURE_EXTENSION))?;
        Ok(Some(RawIndex { text, signature }))
    } else {
        let root = packages_repo_root(cache, packages);
        let index_path = root.join(&packages.index);
        if !index_path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&index_path)
            .with_context(|| format!("read package index {}", index_path.display()))?;
        let sig_path = root.join(format!(
            "{}.{}",
            packages.index,
            signing::SIGNATURE_EXTENSION
        ));
        let signature = if sig_path.exists() {
            Some(
                fs::read_to_string(&sig_path)
                    .with_context(|| format!("read index signature {}", sig_path.display()))?,
            )
        } else {
            None
        };
        Ok(Some(RawIndex { text, signature }))
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
        for entry in &mut index.packages {
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

/// Enforces a tome's pinned signing key against a freshly loaded index (read path). An unsigned
/// tome — one with no pinned key — passes through. A signed tome must present a valid signature
/// over the index bytes; a missing or invalid signature is refused as possible tampering.
fn enforce_pinned_signature(tome: &TomeState, raw: &RawIndex) -> Result<()> {
    let Some(pinned) = &tome.signer_pubkey else {
        return Ok(());
    };
    let Some(signature) = &raw.signature else {
        bail!(
            "tome `{}` was added with a signed index but no `index` signature is published now; \
             refusing to use an unsigned index (possible tampering). If the publisher \
             intentionally stopped signing, remove and re-add the tome.",
            tome.name
        );
    };
    signing::verify(raw.text.as_bytes(), signature, pinned)
        .with_context(|| format!("verify package index signature for tome `{}`", tome.name))
}

/// Trust-on-first-use capture, run when a tome is synced (add/update). Given the key pinned so
/// far (`existing`) and the manifest just synced, returns the key to record going forward:
///
/// - first sync of a tome that advertises a signer → verify its index against that key and pin it;
/// - a later sync of an already-pinned tome → require the index still verifies against the pinned
///   key, and refuse a manifest that advertises a *different* key (rotation needs a deliberate
///   re-add);
/// - an unsigned tome → stays unpinned.
fn capture_signer(
    tome: &TomeState,
    cache: &Path,
    manifest: &TomeManifest,
    existing: Option<String>,
) -> Result<Option<String>> {
    let Some(packages) = &manifest.packages else {
        // No package repo at all: nothing to verify. Preserve any prior pin.
        return Ok(existing);
    };
    let advertised = packages.signer.clone();

    // Without a published index we cannot verify anything this sync; keep the prior pin so a
    // later sync (once an index exists) still enforces it.
    let Some(raw) = load_raw_index(cache, packages)? else {
        return Ok(existing);
    };

    match (existing, advertised) {
        (Some(pinned), advertised) => {
            if let Some(advertised) = &advertised {
                if !signing::keys_match(&pinned, advertised)? {
                    bail!(
                        "tome `{}` now advertises a different signing key than the one pinned on \
                         first use; refusing. Remove and re-add the tome to trust the new key.",
                        tome.name
                    );
                }
            }
            require_signed_index(tome, &raw, &pinned)?;
            Ok(Some(pinned))
        }
        (None, Some(advertised)) => {
            require_signed_index(tome, &raw, &advertised)?;
            report(&format!(
                "pinned signing key for tome `{}` (trust on first use)",
                tome.name
            ));
            Ok(Some(advertised))
        }
        (None, None) => Ok(None),
    }
}

/// Requires that `raw` carries a signature that verifies against `key`. Used by TOFU capture to
/// refuse pinning (or continuing to trust) a tome whose advertised signer does not actually sign
/// its index.
fn require_signed_index(tome: &TomeState, raw: &RawIndex, key: &str) -> Result<()> {
    let Some(signature) = &raw.signature else {
        bail!(
            "tome `{}` advertises a signing key but publishes no index signature; \
             expected `{}.{}` alongside the index.",
            tome.name,
            tome.tome
                .as_ref()
                .and_then(|t| t.packages.as_ref())
                .map(|p| p.index.as_str())
                .unwrap_or("index.nuon"),
            signing::SIGNATURE_EXTENSION
        );
    };
    signing::verify(raw.text.as_bytes(), signature, key)
        .with_context(|| format!("verify package index signature for tome `{}`", tome.name))
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

fn sync_remote_tome_cache(tome: &TomeState, cache_path: &Path) -> Result<()> {
    status(&format!("cloning tome ({})", tome.name));
    status(&format!(
        "checking out tome ({}) ref ({})",
        tome.name, tome.ref_name
    ));
    git::clone(&tome.url, &tome.ref_name, cache_path)
        .with_context(|| format!("could not sync tome `{}`", tome.name))
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

fn promote_tome_cache(
    tome: &TomeState,
    staged: &Path,
    cache_path: &Path,
) -> Result<Option<String>> {
    let backup = tome_cache_backup_path(cache_path)?;
    let had_previous = cache_path.exists();
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("remove stale tome cache backup {}", backup.display()))?;
    }
    if had_previous {
        fs::rename(cache_path, &backup)
            .with_context(|| format!("back up tome cache {}", cache_path.display()))?;
    }

    if let Err(err) = fs::rename(staged, cache_path)
        .with_context(|| format!("promote tome cache {}", cache_path.display()))
    {
        if had_previous {
            let _ = fs::rename(&backup, cache_path);
        }
        return Err(err);
    }

    let commit = match record_tome_sync_state(tome, cache_path) {
        Ok(commit) => commit,
        Err(err) => {
            let _ = fs::remove_dir_all(cache_path);
            if had_previous {
                let _ = fs::rename(&backup, cache_path);
            }
            return Err(err);
        }
    };

    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(commit)
}

fn tome_cache_backup_path(cache_path: &Path) -> Result<PathBuf> {
    let name = cache_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("tome cache path should have a name")?;
    Ok(cache_path.with_file_name(format!("{name}.grimoire-old")))
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

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    crate::fs_util::copy_dir_all(source, destination, "tome")
}

fn tome_cache_dir() -> Result<PathBuf> {
    Ok(paths::install_root()?.join("cache").join("tomes"))
}

/// Location of a tome's cached repository. Does not sync; callers that need the cache
/// populated should use [`ensure_tome_cache`].
pub fn tome_cache_path(name: &str) -> Result<PathBuf> {
    Ok(tome_cache_dir()?.join(name))
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
