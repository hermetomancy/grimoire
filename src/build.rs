//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    archive::pack,
    cli::BuildArgs,
    fetch::{self, FetchedSource},
    nu::runtime::{BuildEnv, EmbeddedNuRuntime, RuneRuntime},
    paths,
    progress::{report, status},
    tome,
};

pub fn build(args: BuildArgs) -> Result<()> {
    let archive = build_package(&args.package, &args.output)?;
    report(&format!("built {}", archive.display()));
    Ok(())
}

pub fn build_package(package: &str, output: &Path) -> Result<PathBuf> {
    build_package_with_env(package, output, &BuildEnv::default())
}

pub fn build_package_with_env(package: &str, output: &Path, env: &BuildEnv) -> Result<PathBuf> {
    status(&format!("resolving rune ({package})"));
    let rune = resolve_rune(package)?;

    let runtime = EmbeddedNuRuntime;
    let metadata = runtime
        .package_metadata(&rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))?;

    let rune_dir = rune.parent().unwrap_or_else(|| Path::new("."));
    let sources = fetch::fetch_sources(&metadata.sources, rune_dir, &paths::source_cache_dir()?)
        .with_context(|| format!("fetch sources for {}", rune.display()))?;

    let temp = tempfile::tempdir()?;
    let work_dir = temp.path().join("work");
    let package_dir = temp.path().join("package");
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&package_dir)?;
    let sources = prepare_sources(sources, &work_dir)?;

    status(&format!("building ({})", rune.display()));
    runtime
        .build(&rune, &package_dir, &work_dir, &sources, env)
        .with_context(|| format!("build rune {}", rune.display()))?;

    pack::pack_built_rune(&rune, &package_dir, output)
}

fn prepare_sources(
    sources: BTreeMap<String, FetchedSource>,
    work_dir: &Path,
) -> Result<BTreeMap<String, FetchedSource>> {
    let sources_dir = work_dir.join("sources");
    let mut prepared = BTreeMap::new();
    for (name, mut source) in sources {
        if source_should_extract(&source.url) {
            let destination = sources_dir.join(&name);
            std::fs::create_dir_all(&destination)?;
            extract_tar_zst(&source.path, &destination)
                .with_context(|| format!("extract source `{name}`"))?;
            source.extracted_dir = Some(destination);
        }
        prepared.insert(name, source);
    }
    Ok(prepared)
}

fn source_should_extract(url: &str) -> bool {
    let normalized = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    normalized.ends_with(".tar.zst") || normalized.ends_with(".tzst")
}

fn extract_tar_zst(path: &Path, destination: &Path) -> Result<()> {
    let file =
        File::open(path).with_context(|| format!("open source archive {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode zstd source archive {}", path.display()))?;
    let mut tar = tar::Archive::new(decoder);
    validate_tar_entries(&mut tar)?;

    let file =
        File::open(path).with_context(|| format!("open source archive {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("decode zstd source archive {}", path.display()))?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(destination)
        .with_context(|| format!("unpack source archive into {}", destination.display()))?;
    Ok(())
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
