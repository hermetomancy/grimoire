//! Publishing prebuilts: `tome build` compiles runes into `.tar.zst` archives and records
//! them in the tome's `dist/index.nuon`.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    cli::TomeBuildArgs,
    install,
    model::{IndexEntry, PackageIndex, validate_package_name},
    nu::{nuon_io, runtime::EmbeddedNuRuntime},
    paths,
    progress::{report, status},
};

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
pub(crate) fn build_runes(
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
pub(crate) fn build_rune_into(
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
pub(crate) fn rebuild_index(dist_dir: &Path) -> Result<PackageIndex> {
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
pub(crate) fn read_archive_index_entry(path: &Path) -> Result<(String, IndexEntry)> {
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
pub(crate) fn rune_names(root: &Path) -> Result<Vec<String>> {
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
pub(crate) fn rune_names_ordered(root: &Path) -> Result<Vec<String>> {
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
