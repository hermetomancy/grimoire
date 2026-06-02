use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::{
    archive::pack,
    cli::BuildArgs,
    fetch,
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    paths,
    progress::status,
    tome,
};

pub fn build(args: BuildArgs) -> Result<()> {
    let archive = build_package(&args.package, &args.output, args.quiet)?;
    println!("built {}", archive.display());
    Ok(())
}

pub fn build_package(package: &str, output: &Path, quiet: bool) -> Result<PathBuf> {
    status(quiet, &format!("resolving rune ({package})"));
    let rune = resolve_rune(package, quiet)?;

    let runtime = EmbeddedNuRuntime;
    let metadata = runtime
        .package_metadata(&rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))?;

    let rune_dir = rune.parent().unwrap_or_else(|| Path::new("."));
    let sources = fetch::fetch_sources(
        &metadata.sources,
        rune_dir,
        &paths::source_cache_dir()?,
        quiet,
    )
    .with_context(|| format!("fetch sources for {}", rune.display()))?;

    let temp = tempfile::tempdir()?;
    let work_dir = temp.path().join("work");
    let package_dir = temp.path().join("package");
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&package_dir)?;

    status(quiet, &format!("building ({})", rune.display()));
    runtime
        .build(&rune, &package_dir, &work_dir, &sources)
        .with_context(|| format!("build rune {}", rune.display()))?;

    pack::pack_built_rune(&rune, &package_dir, output, quiet)
}

pub fn resolve_rune(package: &str, quiet: bool) -> Result<PathBuf> {
    let path = PathBuf::from(package);
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("resolve rune path {}", path.display()));
    }

    if package.ends_with(".rn") {
        bail!("could not find rune `{package}`");
    }

    for tome in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&tome, quiet)?;
        let rune = cache_path.join("runes").join(format!("{package}.rn"));
        if rune.exists() {
            return rune
                .canonicalize()
                .with_context(|| format!("resolve rune path {}", rune.display()));
        }
    }

    let candidates = [
        PathBuf::from(format!("{package}.rn")),
        PathBuf::from("runes").join(format!("{package}.rn")),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("resolve rune path {}", candidate.display()));
        }
    }

    bail!("could not find rune `{package}`; pass a .rn path or a known package name")
}
