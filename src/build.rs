//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use xz2::read::XzDecoder;

use crate::{
    addendum, archive,
    archive::pack,
    cli::BuildArgs,
    fetch::{self, FetchedSource},
    install,
    nu::runtime::{BuildDirs, BuildEnv, EmbeddedNuRuntime, RuneRuntime},
    paths,
    progress::{report, status},
    tome, toolchain,
};

/// The result of a source build: the archive path and the computed store hash.
pub struct BuildResult {
    pub archive: PathBuf,
    pub store_hash: String,
}

pub fn build(args: BuildArgs) -> Result<()> {
    let result = build_package(&args.package, &args.output, args.bootstrap)?;
    report(&format!("built {}", result.archive.display()));
    Ok(())
}

pub fn build_package(package: &str, output: &Path, bootstrap: bool) -> Result<BuildResult> {
    let rune = resolve_rune(package)?;
    let store_hash = crate::closure::store_hash_for_rune(&rune)?;
    let env = if bootstrap {
        BuildEnv::bootstrap()
    } else {
        let metadata = EmbeddedNuRuntime.package_metadata(&rune)?;
        let build_deps = metadata.deps.build_for(&paths::target_triple());
        BuildEnv::managed(
            install::build_dep_bin_dirs(&build_deps)?,
            toolchain::source_build_host_tools()?,
        )
    };
    build_package_with_env(package, output, &env, &store_hash)
}

/// Builds `package` into an archive recorded under the already-computed `store_hash` (the package's
/// content address over its resolved dependency closure). The caller owns hash computation so the
/// installer can reuse the address it derived from the dependencies it actually installed.
pub fn build_package_with_env(
    package: &str,
    output: &Path,
    env: &BuildEnv,
    store_hash: &str,
) -> Result<BuildResult> {
    // A space in the install root breaks source builds: configure records the absolute paths of
    // build tools (MKDIR_P, INSTALL, ...) — which live under the root — and Makefiles use them
    // unquoted, so a path like `~/Library/Application Support/...` splits at the space. Fail early
    // with a clear message instead of a cryptic `make` error 30 seconds in.
    let root = paths::install_root()?;
    if root.to_string_lossy().contains(char::is_whitespace) {
        bail!(
            "install root `{}` contains whitespace, which breaks source builds; \
             set GRIMOIRE_ROOT to a path without spaces",
            root.display()
        );
    }

    let original_cwd = std::env::current_dir().context("read current working directory")?;
    status(&format!("resolving rune ({package})"));
    let rune = resolve_rune(package)?;

    let runtime = EmbeddedNuRuntime;
    let mut metadata = runtime
        .package_metadata(&rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))?;
    addendum::patched_package_metadata(&mut metadata, tome_name_for_rune(&rune)?.as_deref(), &rune)
        .with_context(|| format!("apply addendums to {}", rune.display()))?;

    let rune_dir = rune.parent().unwrap_or_else(|| Path::new("."));
    let sources = fetch::fetch_sources(&metadata.sources, rune_dir, &paths::source_cache_dir()?)
        .with_context(|| format!("fetch sources for {}", rune.display()))?;

    let final_prefix = paths::store_path(store_hash, &metadata.name, &metadata.version)?;

    let temp = tempfile::tempdir()?;
    let work_dir = temp.path().join("work");
    let package_dir = temp.path().join("package");
    let dirs = BuildDirs {
        package_dir: package_dir.clone(),
        final_prefix: final_prefix.clone(),
        work_dir: work_dir.clone(),
    };
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&package_dir)?;
    let sources = prepare_sources(sources, &work_dir)?;

    status(&format!(
        "building ({}) store={}",
        rune.display(),
        store_hash
    ));
    let build_result = runtime
        .build(&rune, &dirs, &sources, &metadata.build_flags, env)
        .with_context(|| format!("build rune {}", rune.display()));
    std::env::set_current_dir(&original_cwd)
        .with_context(|| format!("restore working directory {}", original_cwd.display()))?;
    build_result?;

    let archive = pack::pack_built_rune(
        &rune,
        &metadata,
        &package_dir,
        &final_prefix,
        store_hash,
        output,
    )?;
    drop(temp);
    Ok(BuildResult {
        archive,
        store_hash: store_hash.to_string(),
    })
}

pub(crate) fn tome_name_for_rune(rune: &Path) -> Result<Option<String>> {
    let rune = rune
        .canonicalize()
        .with_context(|| format!("resolve rune path {}", rune.display()))?;
    for tome in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&tome)?
            .canonicalize()
            .with_context(|| format!("resolve tome cache for `{}`", tome.name))?;
        let runes_dir = cache_path.join("runes");
        if rune.starts_with(&runes_dir) {
            return Ok(Some(tome.name));
        }
    }
    Ok(None)
}

fn prepare_sources(
    sources: BTreeMap<String, FetchedSource>,
    work_dir: &Path,
) -> Result<BTreeMap<String, FetchedSource>> {
    let sources_dir = work_dir.join("sources");
    let mut prepared = BTreeMap::new();
    for (name, mut source) in sources {
        if let Some(kind) = source_archive_kind(&source.url) {
            let destination = sources_dir.join(&name);
            std::fs::create_dir_all(&destination)?;
            extract_source_archive(&source.path, &destination, kind)
                .with_context(|| format!("extract source `{name}`"))?;
            source.extracted_dir = Some(destination);
        }
        prepared.insert(name, source);
    }
    Ok(prepared)
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)] // all source archives are tarballs; the prefix is meaningful
enum SourceArchiveKind {
    TarGz,
    TarXz,
    TarZst,
}

fn source_archive_kind(url: &str) -> Option<SourceArchiveKind> {
    let normalized = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if normalized.ends_with(".tar.zst") || normalized.ends_with(".tzst") {
        return Some(SourceArchiveKind::TarZst);
    }
    if normalized.ends_with(".tar.gz") || normalized.ends_with(".tgz") {
        return Some(SourceArchiveKind::TarGz);
    }
    if normalized.ends_with(".tar.xz") || normalized.ends_with(".txz") {
        return Some(SourceArchiveKind::TarXz);
    }
    None
}

fn extract_source_archive(path: &Path, destination: &Path, kind: SourceArchiveKind) -> Result<()> {
    let mut tar = tar::Archive::new(source_archive_reader(path, kind)?);
    validate_tar_entries(&mut tar)?;

    let mut tar = tar::Archive::new(source_archive_reader(path, kind)?);
    tar.unpack(destination)
        .with_context(|| format!("unpack source archive into {}", destination.display()))?;
    Ok(())
}

fn source_archive_reader(path: &Path, kind: SourceArchiveKind) -> Result<Box<dyn Read>> {
    let file =
        File::open(path).with_context(|| format!("open source archive {}", path.display()))?;
    match kind {
        SourceArchiveKind::TarGz => Ok(Box::new(GzDecoder::new(file))),
        SourceArchiveKind::TarXz => Ok(Box::new(XzDecoder::new(file))),
        SourceArchiveKind::TarZst => Ok(Box::new(
            zstd::stream::read::Decoder::new(file)
                .with_context(|| format!("decode zstd source archive {}", path.display()))?,
        )),
    }
}

fn validate_tar_entries<R: std::io::Read>(tar: &mut tar::Archive<R>) -> Result<()> {
    for entry in tar.entries()? {
        let entry = entry?;
        let member = entry.path()?.display().to_string();
        if !archive::validate_archive_member_path(&entry.path()?) {
            bail!("source archive contains unsafe path: {member}");
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() {
            bail!("source archive contains a symlink, which is not accepted yet: {member}");
        }
        if entry_type.is_hard_link() {
            bail!("source archive contains a hard link, which is not accepted yet: {member}");
        }
    }
    Ok(())
}

pub fn resolve_rune(package: &str) -> Result<PathBuf> {
    if let Some(rune) = find_rune(package)? {
        return Ok(rune);
    }
    if package.ends_with(".rn") {
        bail!("could not find rune `{package}`");
    }
    bail!("could not find rune `{package}`; pass a .rn path or a known package name")
}

/// Locates the rune for `package` without failing when none exists: an explicit `.rn` path,
/// then a `runes/<package>.rn` in any configured tome cache, then the same relative to the
/// current directory. Returns the canonical path, or `None` when nothing matches.
pub fn find_rune(package: &str) -> Result<Option<PathBuf>> {
    let path = PathBuf::from(package);
    if path.exists() {
        return Ok(Some(path.canonicalize().with_context(|| {
            format!("resolve rune path {}", path.display())
        })?));
    }

    if package.ends_with(".rn") {
        return Ok(None);
    }

    for tome in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&tome)?;
        let rune = cache_path.join("runes").join(format!("{package}.rn"));
        if rune.exists() {
            return Ok(Some(rune.canonicalize().with_context(|| {
                format!("resolve rune path {}", rune.display())
            })?));
        }
    }

    let candidates = [
        PathBuf::from(format!("{package}.rn")),
        PathBuf::from("runes").join(format!("{package}.rn")),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(Some(candidate.canonicalize().with_context(|| {
                format!("resolve rune path {}", candidate.display())
            })?));
        }
    }

    Ok(None)
}
