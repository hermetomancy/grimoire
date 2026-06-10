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
    build,
    cli::{PackageArg, QueryArg, UpgradeArgs},
    install,
    model::{PackageMetadata, PackageState, parse_version_relaxed},
    nu::runtime::EmbeddedNuRuntime,
    paths, progress, solve, tome,
};

#[derive(Debug, Clone)]
pub(crate) struct TomePackage {
    tome: String,
    rune: PathBuf,
    pub(crate) metadata: PackageMetadata,
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
    if args.packages.is_empty() {
        bail!("specify at least one package to query");
    }

    let installed_states = install::installed_states()?;
    let available_packages = tome_packages()?;
    let mut first = true;

    for package in &args.packages {
        let installed = installed_states.iter().find(|state| state.name == *package);
        let available: Vec<_> = available_packages
            .iter()
            .filter(|p| p.metadata.name == *package)
            .collect();

        if installed.is_none() && available.is_empty() {
            bail!("package `{package}` was not found in installed state or configured tomes");
        }

        if !first {
            println!("\n---\n");
        }
        first = false;

        if let Some(state) = installed {
            print_installed(state);
        }

        for pkg in available {
            if installed.is_some() {
                println!();
            }
            print_available(pkg);
        }
    }

    Ok(())
}

pub fn upgrade(args: UpgradeArgs) -> Result<()> {
    if !args.dry_run {
        tome::update_all_configured().context("update configured tomes before upgrade")?;
    }

    let states = install::installed_states()?;
    let held: BTreeMap<String, bool> = states
        .iter()
        .map(|state| (state.name.clone(), state.held))
        .collect();
    let installed: BTreeMap<String, Version> = states
        .into_iter()
        .filter_map(|state| {
            parse_version_relaxed(&state.version)
                .ok()
                .map(|v| (state.name, v))
        })
        .collect();

    let explicit = !args.packages.is_empty();
    let targets = if explicit {
        args.packages.clone()
    } else {
        installed.keys().cloned().collect::<Vec<_>>()
    };

    if targets.is_empty() {
        progress::report("no installed packages");
        return Ok(());
    }

    // Asking to upgrade a held package by name is almost certainly a mistake; fail before
    // doing any resolver work so the user sees the friction and can `grm unhold` deliberately.
    if explicit {
        for name in &targets {
            if held.get(name).copied().unwrap_or(false) {
                bail!("`{name}` is held; run `grm unhold {name}` to allow upgrading it");
            }
        }
    }

    let to_upgrade = collect_upgrades(&targets, &installed, &held, explicit, args.dry_run)?;

    if to_upgrade.is_empty() {
        return Ok(());
    }

    if args.dry_run {
        print_upgrade_plan(&to_upgrade);
        return Ok(());
    }

    let names: Vec<String> = to_upgrade.into_iter().map(|(name, _, _)| name).collect();
    install::upgrade_packages(&names)
}

fn collect_upgrades(
    targets: &[String],
    installed: &BTreeMap<String, Version>,
    held: &BTreeMap<String, bool>,
    explicit: bool,
    dry_run: bool,
) -> Result<Vec<(String, Version, Version)>> {
    let mut to_upgrade: Vec<(String, Version, Version)> = Vec::new();
    for name in targets {
        let Some(current) = installed.get(name) else {
            bail!("package `{name}` is not installed");
        };
        if !explicit && held.get(name).copied().unwrap_or(false) {
            progress::report(&format!(
                "{name} is held; skipping (use `grm unhold {name}` to allow)"
            ));
            continue;
        }
        match solve::newest_available(name)? {
            Some(newest) if newest > *current => {
                if !dry_run {
                    progress::status(&format!("upgrading {name} {current} -> {newest}"));
                }
                to_upgrade.push((name.clone(), current.clone(), newest));
            }
            _ => progress::report(&format!("{name} is up to date ({current})")),
        }
    }
    Ok(to_upgrade)
}

fn print_upgrade_plan(to_upgrade: &[(String, Version, Version)]) {
    println!("plan:");
    for (name, current, newest) in to_upgrade {
        println!("  ~ {name} {current} -> {newest}");
    }
}

pub(crate) fn tome_packages() -> Result<Vec<TomePackage>> {
    let _runtime = EmbeddedNuRuntime;
    let mut packages = Vec::new();

    for state in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&state)
            .with_context(|| format!("sync tome `{}`", state.name))?;
        let runes_dir = cache_path.join("runes");
        if !runes_dir.exists() {
            continue;
        }

        for rune in rune_files(&runes_dir)? {
            let metadata = build::read_rune_metadata(&rune, Some(&state.name))?;
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
    if !state.notes.is_empty() {
        println!("  notes:");
        for note in &state.notes {
            println!("    {note}");
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
    let target = paths::target_triple();
    let bins = package.metadata.bins_for(&target);
    if !bins.is_empty() {
        println!("  bins:");
        for (name, path) in &bins {
            println!("    {name}: {path}");
        }
    }
    if !package.metadata.notes.is_empty() {
        println!("  notes:");
        for note in &package.metadata.notes {
            println!("    {note}");
        }
    }
}
