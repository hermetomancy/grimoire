use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::{
    cli::{PackageArg, QueryArg, UpgradeArgs},
    install,
    model::{PackageMetadata, PackageState},
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    tome,
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

    for package in tome_packages(true)? {
        let summary = package.metadata.summary.as_deref().unwrap_or("");
        if package.metadata.name.to_ascii_lowercase().contains(&query)
            || summary.to_ascii_lowercase().contains(&query)
        {
            matches.push(package);
        }
    }

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
    let available: Vec<_> = tome_packages(true)?
        .into_iter()
        .filter(|package| package.metadata.name == args.package)
        .collect();

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
    let packages = if args.packages.is_empty() {
        install::installed_states()?
            .into_iter()
            .map(|state| state.name)
            .collect::<Vec<_>>()
    } else {
        args.packages
    };

    if packages.is_empty() {
        println!("no installed packages");
        return Ok(());
    }

    for package in packages {
        if !args.quiet {
            eprintln!("grimoire: upgrading {package}");
        }
        install::install(crate::cli::InstallArgs {
            package,
            from_source: false,
            sha256: None,
            quiet: args.quiet,
        })?;
    }

    Ok(())
}

fn tome_packages(quiet: bool) -> Result<Vec<TomePackage>> {
    let runtime = EmbeddedNuRuntime;
    let mut packages = Vec::new();

    for state in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&state, quiet)
            .with_context(|| format!("sync tome `{}`", state.name))?;
        let runes_dir = cache_path.join("runes");
        if !runes_dir.exists() {
            continue;
        }

        for rune in rune_files(&runes_dir)? {
            let metadata = runtime
                .package_metadata(&rune)
                .with_context(|| format!("read rune metadata {}", rune.display()))?;
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
