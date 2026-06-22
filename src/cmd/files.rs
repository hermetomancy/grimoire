//! Read-only file-ownership queries: `files` (list what an installed package put in the store),
//! `owns` (map a file back to the package that installed it), and `provides` (which packages —
//! installed or available — supply a command or capability).

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    cli::{OwnsArgs, PackageArg, ProvidesArgs},
    cmd::query,
    install,
    model::PackageState,
    solve,
    util::output::{Cell, list_item, print_rows},
    util::paths,
};

/// Lists every file an installed package placed in the store, as paths relative to its store
/// directory. Explicitly requested data: prints under `--quiet` too.
pub fn files(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to list files for");
    }
    let world = install::InstalledWorld::load_default()?;
    for package in &args.packages {
        let Some(state) = world.get(package) else {
            bail!("package `{package}` is not installed");
        };
        let store = PathBuf::from(&state.store_path);
        for entry in walkdir::WalkDir::new(&store).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_dir() {
                continue;
            }
            let rel = entry.path().strip_prefix(&store)?;
            // Skip grimoire's own package metadata (.grimoire/package.nuon, .grimoire/rune.rn,
            // written into every store package by archive::pack) — not files the package installed.
            if rel.starts_with(".grimoire") {
                continue;
            }
            list_item(&rel.display().to_string());
        }
    }
    Ok(())
}

/// Resolves which installed package(s) own `path`. Accepts store paths and generation paths.
///
/// Every generation entry is an absolute symlink into the store, and `canonicalize` follows it
/// (through `profiles/current` and the leaf link) to its store target, so ownership reduces to a
/// store-path prefix match. For a `grm prefer`-contested bin this reports the provider actually
/// linked into the generation — the package the symlink targets — not every package that merely
/// declares the name.
pub fn owns(args: OwnsArgs) -> Result<()> {
    let path = fs::canonicalize(&args.path)
        .with_context(|| format!("path `{}` does not exist", args.path.display()))?;
    let world = install::InstalledWorld::load_default()?;
    let store_root = canonical_or_self(&paths::store_root()?);

    let owners: Vec<&PackageState> = if path.starts_with(&store_root) {
        world
            .iter()
            .filter(|state| path.starts_with(canonical_or_self(Path::new(&state.store_path))))
            .collect()
    } else {
        Vec::new()
    };

    if owners.is_empty() {
        bail!(
            "`{}` is not owned by any installed package",
            args.path.display()
        );
    }
    let rows = owners
        .iter()
        .map(|state| vec![Cell::strong(&state.name), Cell::plain(&state.version)])
        .collect();
    print_rows(rows);
    Ok(())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reports which packages provide `name` — as a literal package, a bin, or a declared
/// capability — across installed state, configured tome runes, and published indexes.
pub fn provides(args: ProvidesArgs) -> Result<()> {
    let providers = capability_providers_detailed(&args.name)?;
    if providers.is_empty() {
        bail!(
            "nothing provides `{}` in installed packages or configured tomes",
            args.name
        );
    }
    let rows = providers
        .into_iter()
        .map(|(package, (version, installed))| {
            vec![
                Cell::strong(package),
                Cell::plain(version),
                if installed {
                    Cell::plain("installed")
                } else {
                    Cell::faint("available")
                },
            ]
        })
        .collect();
    print_rows(rows);
    Ok(())
}

/// Every package providing `capability` — as a literal package, a bin, or a declared
/// capability — across installed state, configured tome runes, and published indexes. Maps
/// package name to `(version, installed)`; installed entries win over available ones. The
/// capability index carries no version, so index-only rows have an empty version unless the
/// package is also known elsewhere. Shared by `provides` and `prefer`'s validation.
pub(crate) fn capability_providers_detailed(
    capability: &str,
) -> Result<BTreeMap<String, (String, bool)>> {
    let target = paths::target_triple();
    let mut providers: BTreeMap<String, (String, bool)> = BTreeMap::new();

    for state in install::InstalledWorld::load_default()?.iter() {
        if state.name == *capability
            || state.bins.contains_key(capability)
            || state.provides.contains(&capability.to_owned())
        {
            providers.insert(state.name.clone(), (state.version.clone(), true));
        }
    }

    for package in query::tome_packages()? {
        let metadata = &package.metadata;
        if metadata.name == *capability
            || metadata.bins_for(&target).contains_key(capability)
            || metadata.provides.contains(&capability.to_owned())
        {
            providers
                .entry(metadata.name.clone())
                .or_insert((metadata.version.clone(), false));
        }
    }

    for provider in solve::capability_providers(capability)? {
        providers.entry(provider).or_insert((String::new(), false));
    }

    Ok(providers)
}
