//! The read-only catalog queries: `search` (match packages across configured tomes by name and
//! summary) and `info` (show a single package's metadata, versions, and source). Both read tome
//! indexes and runes without installing anything.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    addendum,
    cli::{PackageArg, QueryArg, UpgradeArgs},
    install,
    model::{PackageMetadata, PackageState},
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    progress, solve, tome,
};

#[derive(Debug, Clone)]
struct TomePackage {
    tome: String,
    rune: PathBuf,
    metadata: PackageMetadata,
}

pub fn search(args: QueryArg) -> Result<()> {
    let query = args.query.to_ascii_lowercase();
    let mut matches = Vec::new();

    for package in tome_packages()? {
        let summary = package.metadata.summary.as_deref().unwrap_or("");
        if package.metadata.name.to_ascii_lowercase().contains(&query)
            || summary.to_ascii_lowercase().contains(&query)
        {
            matches.push(package);
        }
    }

    progress::finish();
    for package in matches {
        println!(
            "{}\t{}\t{}\t{}",
            package.metadata.name,
            package.metadata.version,
            package.tome,
            package.metadata.summary.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub fn info(args: PackageArg) -> Result<()> {
    let installed = install::installed_states()?
        .into_iter()
        .find(|state| state.name == args.package);
    let available: Vec<_> = tome_packages()?
        .into_iter()
        .filter(|package| package.metadata.name == args.package)
        .collect();

    progress::finish();
    if installed.is_none() && available.is_empty() {
        bail!(
            "package `{}` was not found in installed state or configured tomes",
            args.package
        );
    }

    if let Some(state) = &installed {
        print_installed(state);
    }

    for package in available {
        if installed.is_some() {
            println!();
        }
        print_available(&package);
    }

    Ok(())
}

pub fn upgrade(args: UpgradeArgs) -> Result<()> {
    let installed: BTreeMap<String, Version> = install::installed_states()?
        .into_iter()
        .filter_map(|state| Version::parse(&state.version).ok().map(|v| (state.name, v)))
        .collect();

    let targets = if args.packages.is_empty() {
        installed.keys().cloned().collect::<Vec<_>>()
    } else {
        args.packages.clone()
    };

    if targets.is_empty() {
        progress::report("no installed packages");
        return Ok(());
    }

    // Compare the installed version against the newest a tome offers and only reinstall when a
    // strictly newer release exists, so an up-to-date package is left untouched.
    let mut to_upgrade = Vec::new();
    for name in targets {
        let Some(current) = installed.get(&name) else {
            bail!("package `{name}` is not installed");
        };
        match solve::newest_available(&name)? {
            Some(newest) if newest > *current => {
                progress::status(&format!("upgrading {name} {current} -> {newest}"));
                to_upgrade.push(name);
            }
            _ => progress::report(&format!("{name} is up to date ({current})")),
        }
    }

    if to_upgrade.is_empty() {
        return Ok(());
    }
    install::upgrade_packages(&to_upgrade)
}

fn tome_packages() -> Result<Vec<TomePackage>> {
    let runtime = EmbeddedNuRuntime;
    let mut packages = Vec::new();

    for state in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&state)
            .with_context(|| format!("sync tome `{}`", state.name))?;
        let runes_dir = cache_path.join("runes");
        if !runes_dir.exists() {
            continue;
        }

        for rune in rune_files(&runes_dir)? {
            let mut metadata = runtime
                .package_metadata(&rune)
                .with_context(|| format!("read rune metadata {}", rune.display()))?;
            addendum::patched_package_metadata(&mut metadata, Some(&state.name), &rune)
                .with_context(|| format!("apply addendums to {}", rune.display()))?;
            packages.push(TomePackage {
                tome: state.name.clone(),
                rune,
                metadata,
            });
        }
    }

    packages.sort_by(|a, b| {
        a.metadata
            .name
            .cmp(&b.metadata.name)
            .then_with(|| a.tome.cmp(&b.tome))
    });
    Ok(packages)
}

fn rune_files(runes_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut runes = Vec::new();
    for entry in walkdir::WalkDir::new(runes_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("rn") {
            runes.push(path);
        }
    }
    Ok(runes)
}

fn print_installed(state: &PackageState) {
    println!("installed:");
    println!("  name: {}", state.name);
    println!("  version: {}", state.version);
    if let Some(target) = &state.target {
        println!("  target: {target}");
    }
    println!("  archive_hash: {}", state.archive_hash);
    if !state.bins.is_empty() {
        println!("  bins:");
        for (name, path) in &state.bins {
            println!("    {name}: {path}");
        }
    }
}

fn print_available(package: &TomePackage) {
    println!("available:");
    println!("  name: {}", package.metadata.name);
    println!("  version: {}", package.metadata.version);
    println!("  tome: {}", package.tome);
    println!("  rune: {}", package.rune.display());
    if let Some(summary) = &package.metadata.summary {
        println!("  summary: {summary}");
    }
    if !package.metadata.bins.is_empty() {
        println!("  bins:");
        for (name, path) in &package.metadata.bins {
            println!("    {name}: {path}");
        }
    }
}
