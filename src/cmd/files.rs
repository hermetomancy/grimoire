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
    util::paths,
};

/// Lists every file an installed package placed in the store, as paths relative to its store
/// directory. Explicitly requested data: prints under `--quiet` too.
pub fn files(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to list files for");
    }
    let states = install::installed_states()?;
    for package in &args.packages {
        let Some(state) = states.iter().find(|state| state.name == *package) else {
            bail!("package `{package}` is not installed");
        };
        let store = PathBuf::from(&state.store_path);
        for entry in walkdir::WalkDir::new(&store).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_dir() {
                continue;
            }
            let rel = entry.path().strip_prefix(&store)?;
            println!("{}", rel.display());
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
    let states = install::installed_states()?;
    let store_root = canonical_or_self(&paths::store_root()?);

    let owners: Vec<&PackageState> = if path.starts_with(&store_root) {
        states
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
    for state in owners {
        println!("{}\t{}", state.name, state.version);
    }
    Ok(())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reports which packages provide `name` — as a literal package, a bin, or a declared
/// capability — across installed state, configured tome runes, and published indexes.
pub fn provides(args: ProvidesArgs) -> Result<()> {
    let name = &args.name;
    let target = paths::target_triple();
    // package name -> (version, installed); installed entries win over available ones.
    let mut providers: BTreeMap<String, (String, bool)> = BTreeMap::new();

    for state in install::installed_states()? {
        if state.name == *name || state.bins.contains_key(name) || state.provides.contains(name) {
            providers.insert(state.name.clone(), (state.version.clone(), true));
        }
    }

    for package in query::tome_packages()? {
        let metadata = &package.metadata;
        if metadata.name == *name
            || metadata.bins_for(&target).contains_key(name)
            || metadata.provides.contains(name)
        {
            providers
                .entry(metadata.name.clone())
                .or_insert((metadata.version.clone(), false));
        }
    }

    // Index-published capabilities cover prebuilt-only packages whose runes are absent; the
    // capability index carries no version, so those rows print one only when known elsewhere.
    for provider in solve::capability_providers(name)? {
        providers.entry(provider).or_insert((String::new(), false));
    }

    if providers.is_empty() {
        bail!("nothing provides `{name}` in installed packages or configured tomes");
    }
    for (package, (version, installed)) in providers {
        println!(
            "{package}\t{version}\t{}",
            if installed { "installed" } else { "available" }
        );
    }
    Ok(())
}
