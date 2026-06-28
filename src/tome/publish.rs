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
    archive::{
        self, BoundedReader, MAX_ARCHIVE_DECOMPRESSED_BYTES, MAX_ARCHIVE_MEMBERS,
        MAX_CAPTURED_MEMBER_BYTES,
    },
    cli::TomeBuildArgs,
    install,
    model::{IndexEntry, PackageIndex, validate_package_name},
    nu::{nuon_io, runtime::EmbeddedNuRuntime},
    util::output::{report, status},
    util::paths,
};

use super::output_lint;

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

    let hermetic = !args.bootstrap && !args.impure;

    if args.index {
        let catalog = rebuild_index_with_mode(&dist_dir, hermetic)?;
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
    let host_target = paths::target_triple();
    let current_target = args.target.as_deref().unwrap_or(&host_target);
    let rune_names = if args.all {
        let names = rune_names_ordered(root, current_target)?;
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
        hermetic,
        args.target.as_deref(),
        args.force,
        args.strict,
        &rune_names,
        &mut catalog,
    )?;

    crate::util::output::note(&format!("registered in {}", index_path.display()));
    crate::util::output::note(&format!(
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
    hermetic: bool,
    target: Option<&str>,
    force: bool,
    strict: bool,
    rune_names: &[String],
    catalog: &mut PackageIndex,
) -> Result<()> {
    let host_target = paths::target_triple();
    let current_target = target.unwrap_or(&host_target);
    let mut any_built = false;
    for name in rune_names {
        // A split group is one build with several outputs: skip only when *every* member is
        // already registered with its archive present.
        let output_names = group_output_names(root, name)?;
        let already_built = !force
            && output_names.iter().all(|output_name| {
                catalog
                    .entries
                    .values()
                    .find(|e| e.name == *output_name && e.target == current_target)
                    .is_some_and(|existing| {
                        dist_dir
                            .join(format!(
                                "{}-{}-{}.tar.zst",
                                existing.name, existing.version, existing.target
                            ))
                            .exists()
                    })
            });
        if already_built {
            status(&format!(
                "skipping {} (already built; pass --force to rebuild)",
                output_names.join(", ")
            ));
            continue;
        }
        for (store_hash, entry, archive) in
            build_rune_into(root, name, dist_dir, bootstrap, hermetic, target)?
        {
            output_lint::archive_purity(&archive, strict)
                .with_context(|| format!("purity lint for `{}`", entry.name))?;
            output_lint::archive_linkage(&archive, &entry, strict)
                .with_context(|| format!("linkage lint for `{}`", entry.name))?;
            report(&format!(
                "built {} {}",
                crate::util::output::accent(&format!("{} {}", entry.name, entry.version)),
                crate::util::output::faint(&format!(
                    "({}) into {}",
                    entry.target,
                    archive.display()
                ))
            ));
            if all {
                let mut world = install::InstalledWorld::load_default()?;
                install::install_store_only_for_target(
                    &mut world,
                    &archive,
                    None,
                    None,
                    install::InstallOrigin::TomeBuild,
                    &entry.target,
                )
                .with_context(|| format!("store-only install of {}", entry.name))?;
            }
            catalog.upsert(store_hash, entry);
            any_built = true;
        }
    }
    // Write the index once, atomically, after all runes built successfully.
    // If any rune failed, the previous index and dist/ remain untouched.
    if any_built {
        nuon_io::write_nuon(index_path, &catalog.to_value())
            .with_context(|| format!("update index {}", index_path.display()))?;
    }
    Ok(())
}

/// The package names a rune's build produces: the split group's members when `name` belongs
/// to one, otherwise just `name`. Used to decide whether a build can be skipped.
fn group_output_names(root: &Path, name: &str) -> Result<Vec<String>> {
    let rune_path = root.join("runes").join(format!("{name}.rn"));
    if !rune_path.exists() {
        return Ok(vec![name.to_owned()]);
    }
    match crate::build::split::group_for(&rune_path)? {
        Some(group) => Ok(group.members().map(|member| member.name.clone()).collect()),
        None => Ok(vec![name.to_owned()]),
    }
}

/// Builds the rune named `name` (`runes/<name>.rn`) into `dist_dir`, returning one
/// `(store_hash, entry, archive)` per produced package — one for a standalone rune, one per
/// member for a split group. Shared by single-package and `--all` builds so both register
/// identical entries.
pub(crate) fn build_rune_into(
    root: &Path,
    name: &str,
    dist_dir: &Path,
    bootstrap: bool,
    hermetic: bool,
    target: Option<&str>,
) -> Result<Vec<(String, IndexEntry, PathBuf)>> {
    validate_package_name(name)?;
    let rune_path = root.join("runes").join(format!("{name}.rn"));
    if !rune_path.exists() {
        bail!("rune not found: {}", rune_path.display());
    }

    let result = crate::build::build_package(
        &rune_path.to_string_lossy(),
        dist_dir,
        bootstrap,
        hermetic,
        target,
    )?;
    let resolved_target = target.map_or_else(paths::target_triple, |t| t.to_string());

    result
        .products()
        .map(|product| {
            let archive_hash = crate::archive::archive_hash(&product.archive)?;
            let archive_file = product
                .archive
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| {
                    format!(
                        "archive path has no file name: {}",
                        product.archive.display()
                    )
                })?;
            let metadata = &product.metadata;
            let entry = IndexEntry {
                name: metadata.name.clone(),
                version: metadata.version.clone(),
                target: resolved_target.clone(),
                archive: archive_file.to_owned(),
                archive_hash,
                runtime_deps: metadata.deps.runtime.clone(),
                provides: metadata.provides.clone(),
                libs: metadata.libs.clone(),
                conflicts: metadata.conflicts.clone(),
                replaces: metadata.replaces.clone(),
            };
            Ok((product.store_hash.clone(), entry, product.archive.clone()))
        })
        .collect()
}

/// Rebuilds the package index from every `.tar.zst` archive already present in `dist_dir`.
/// Each archive is inspected for its embedded metadata and rune so the index entry is identical
/// to what a fresh build would produce in the selected build mode.
pub(crate) fn rebuild_index_with_mode(dist_dir: &Path, hermetic: bool) -> Result<PackageIndex> {
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
        let (store_hash, index_entry) = read_archive_index_entry_with_mode(&path, hermetic)
            .with_context(|| format!("index archive {}", path.display()))?;
        report(&format!(
            "indexed {} {}",
            crate::util::output::accent(&format!("{} {}", index_entry.name, index_entry.version)),
            crate::util::output::faint(&format!("({}) from {}", index_entry.target, name))
        ));
        entries.insert(store_hash, index_entry);
    }
    Ok(PackageIndex { entries })
}

/// Reads an existing archive and produces the `(store_hash, IndexEntry)` that would describe it
/// in the selected build mode.
pub(crate) fn read_archive_index_entry_with_mode(
    path: &Path,
    hermetic: bool,
) -> Result<(String, IndexEntry)> {
    archive::validate_archive_paths(path)
        .with_context(|| format!("validate archive {}", path.display()))?;

    let file = fs::File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode archive {}", path.display()))?;
    let decoder = BoundedReader::new(
        decoder,
        MAX_ARCHIVE_DECOMPRESSED_BYTES,
        "archive decompressed stream",
    );
    let mut archive = tar::Archive::new(decoder);

    let mut metadata = None;
    let mut rune_bytes = None;
    let mut group_runes: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    for (members, entry) in archive
        .entries()
        .context("read archive entries")?
        .enumerate()
    {
        if members >= MAX_ARCHIVE_MEMBERS {
            bail!("archive contains more than {MAX_ARCHIVE_MEMBERS} members");
        }
        let entry = entry?;
        let path_str = entry.path()?.to_string_lossy().to_string();
        let normalized = path_str.strip_prefix("./").unwrap_or(&path_str);

        if normalized == ".grimoire/package.nuon" {
            let text = read_limited_text(entry, ".grimoire/package.nuon")?;
            metadata = Some(
                crate::model::PackageMetadata::from_value(nuon_io::parse_nuon(&text)?, true)
                    .with_context(|| format!("parse metadata in {}", path.display()))?,
            );
        } else if normalized == ".grimoire/rune.rn" {
            rune_bytes = Some(read_limited_bytes(entry, ".grimoire/rune.rn")?);
        } else if let Some(member) = normalized
            .strip_prefix(".grimoire/group/")
            .and_then(|rest| rest.strip_suffix(".rn"))
        {
            group_runes.insert(member.to_owned(), read_limited_bytes(entry, normalized)?);
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

    // Address against the target the archive was built for (recorded in its own metadata), not the
    // indexing host's, or a cross-target build is re-indexed under the wrong hash (AGENTS §9.8).
    let target = metadata
        .target
        .clone()
        .ok_or_else(|| anyhow::anyhow!("metadata in {} is missing target", path.display()))?;

    let store_hash = if group_runes.is_empty() {
        let build_env = crate::build::toolchain::store_build_env_id_for_target(hermetic, &target);
        crate::store::closure::store_hash_for_rune_bytes_with_target_and_env(
            &rune_bytes,
            &metadata,
            &target,
            &build_env,
        )
        .with_context(|| format!("compute store hash for {}", path.display()))?
    } else {
        // A split-group member: its address derives from the whole group, whose runes the
        // archive carries under `.grimoire/group/`.
        let group: Vec<(crate::model::PackageMetadata, Vec<u8>)> = group_runes
            .into_iter()
            .map(|(member, bytes)| {
                let member_metadata = EmbeddedNuRuntime
                    .package_metadata_from_bytes(&bytes, &format!("group rune `{member}`"))?;
                Ok((member_metadata, bytes))
            })
            .collect::<Result<_>>()?;
        let build_env = crate::build::toolchain::store_build_env_id_for_target(hermetic, &target);
        crate::store::closure::split_member_hashes_with_target_and_env(
            &group,
            &target,
            &build_env,
            &BTreeMap::new(),
        )
        .with_context(|| format!("compute split group hashes for {}", path.display()))?
        .get(&metadata.name)
        .cloned()
        .with_context(|| {
            format!(
                "archive {} names `{}`, which is not a member of its embedded group",
                path.display(),
                metadata.name
            )
        })?
    };
    let embedded_hash = crate::model::embedded_store_hash(&metadata)
        .with_context(|| format!("validate embedded store path in {}", path.display()))?;
    if embedded_hash != store_hash {
        bail!(
            "archive {} embeds store hash `{embedded_hash}` but its inputs hash to `{store_hash}`",
            path.display()
        );
    }

    let archive_hash = crate::archive::archive_hash(path)
        .with_context(|| format!("hash archive {}", path.display()))?;
    let archive_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("archive path has no file name: {}", path.display()))?
        .to_owned();

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
            conflicts: metadata.conflicts,
            replaces: metadata.replaces,
        },
    ))
}

fn read_limited_text<R: Read>(entry: tar::Entry<'_, R>, label: &str) -> Result<String> {
    let bytes = read_limited_bytes(entry, label)?;
    String::from_utf8(bytes).with_context(|| format!("archive member `{label}` is not utf-8"))
}

fn read_limited_bytes<R: Read>(mut entry: tar::Entry<'_, R>, label: &str) -> Result<Vec<u8>> {
    let mut limited = entry.by_ref().take(MAX_CAPTURED_MEMBER_BYTES + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .with_context(|| format!("read archive member `{label}`"))?;
    if bytes.len() as u64 > MAX_CAPTURED_MEMBER_BYTES {
        bail!("archive member `{label}` exceeds {MAX_CAPTURED_MEMBER_BYTES} bytes");
    }
    Ok(bytes)
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
pub(crate) fn rune_names_ordered(root: &Path, target: &str) -> Result<Vec<String>> {
    let names = rune_names(root)?;
    if names.is_empty() {
        return Ok(names);
    }

    let mut metadata_map: BTreeMap<String, crate::model::PackageMetadata> = BTreeMap::new();
    for name in &names {
        let rune_path = root.join("runes").join(format!("{name}.rn"));
        let metadata = EmbeddedNuRuntime
            .package_metadata(&rune_path)
            .with_context(|| format!("read metadata for {name}"))?;
        // Skip runes that explicitly declare targets and don't include the current one.
        if !metadata.targets.is_empty() && !metadata.targets.iter().any(|t| t == target) {
            continue;
        }
        metadata_map.insert(name.clone(), metadata);
    }

    // Split members are not built directly: their parent's build produces them. Coalesce
    // them under the parent — the parent is the buildable node, and a build dep on a member
    // (e.g. `rust` needing `clang`) becomes an edge to the member's parent.
    let mut alias: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped_members: Vec<String> = Vec::new();
    for (name, metadata) in &metadata_map {
        let Some(parent) = &metadata.split_from else {
            continue;
        };
        if metadata_map.contains_key(parent) {
            alias.insert(name.clone(), parent.clone());
        } else if root.join("runes").join(format!("{parent}.rn")).exists() {
            // The parent exists but was filtered out for this target; the member goes with it.
            skipped_members.push(name.clone());
        } else {
            bail!(
                "split member `{name}` names parent `{parent}`, which is not a rune in \
                 this tome"
            );
        }
    }
    for name in skipped_members {
        metadata_map.remove(&name);
    }
    let filtered_names: Vec<String> = metadata_map
        .keys()
        .filter(|name| !alias.contains_key(*name))
        .cloned()
        .collect();

    // Build adjacency list: dependent -> [its dependencies within this tome]
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
    for name in &filtered_names {
        in_degree.entry(name.clone()).or_insert(0);
    }
    for name in &filtered_names {
        let metadata = &metadata_map[name];
        let build_deps = metadata.deps.build_for(target);
        for dep in build_deps {
            let dep_node = alias.get(&dep.name).unwrap_or(&dep.name);
            if dep_node == name {
                continue; // A dep on a sibling split member is satisfied by this very build.
            }
            if metadata_map.contains_key(dep_node) && !alias.contains_key(dep_node) {
                adj.entry(dep_node.clone()).or_default().push(name.clone());
                *in_degree.entry(name.clone()).or_insert(0) += 1;
            }
        }
    }

    // Kahn's algorithm. The ready set is an ordered set, not a stack, so ties break
    // alphabetically for *every* node — not just the initial seeds — and the build order
    // is fully deterministic.
    let mut ready: std::collections::BTreeSet<String> = filtered_names
        .iter()
        .filter(|n| *in_degree.get(*n).unwrap_or(&0) == 0)
        .cloned()
        .collect();
    let mut ordered = Vec::new();
    while let Some(name) = ready.pop_first() {
        ordered.push(name.clone());
        if let Some(deps) = adj.get(&name) {
            for dep in deps {
                let Some(count) = in_degree.get_mut(dep) else {
                    bail!("missing in_degree entry for dependency `{dep}` in topological sort");
                };
                *count -= 1;
                if *count == 0 {
                    ready.insert(dep.clone());
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
