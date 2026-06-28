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
    model::{PackageMetadata, PackageState},
    nu::runtime::EmbeddedNuRuntime,
    solve, tome,
    util::output::{self, Cell},
    util::paths,
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

    output::finish();
    if matches.is_empty() {
        // Data-less result: say so on a terminal; piped output stays empty for scripts.
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            output::line(&output::faint(&format!("no matches for `{query}`")));
        }
        return Ok(());
    }
    let rows = matches
        .iter()
        .map(|package| {
            vec![
                Cell::strong(&package.metadata.name),
                Cell::plain(&package.metadata.version),
                Cell::faint(&package.tome),
                Cell::plain(package.metadata.summary.as_deref().unwrap_or("")),
            ]
        })
        .collect();
    output::print_rows(rows);
    Ok(())
}

pub fn info(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to query");
    }

    let world = install::InstalledWorld::load_default()?;
    let available_packages = tome_packages()?;
    let mut first = true;

    for package in &args.packages {
        let installed = world.get(package);
        let available: Vec<_> = available_packages
            .iter()
            .filter(|p| p.metadata.name == *package)
            .collect();

        if installed.is_none() && available.is_empty() {
            bail!("package `{package}` was not found in installed state or configured tomes");
        }

        if !first {
            output::line(&output::faint("---"));
        }
        first = false;

        if let Some(state) = installed {
            print_installed(state);
        }

        for pkg in available {
            print_available(pkg);
        }
    }

    Ok(())
}

pub fn upgrade(args: UpgradeArgs) -> Result<()> {
    if !args.dry_run {
        tome::update_all_configured().context("update configured tomes before upgrade")?;
    }

    let world = install::InstalledWorld::load_default()?;
    let held: BTreeMap<String, bool> = world
        .iter()
        .map(|state| (state.name.clone(), state.held))
        .collect();
    // A bare `grm upgrade` covers the user's environment — the linked set — not the
    // store-only cache: nobody asked for a cached build dep, so nobody asked for a newer
    // one. Naming a store-only package explicitly still upgrades it.
    let linked = world.linked_immut();
    let installed: BTreeMap<String, Version> = world.installed_versions();

    let explicit = !args.packages.is_empty();
    let targets = if explicit {
        args.packages.clone()
    } else {
        installed
            .keys()
            .filter(|name| linked.contains(*name))
            .cloned()
            .collect::<Vec<_>>()
    };

    if targets.is_empty() {
        output::report("no installed packages");
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

    // Rename discovery (bare upgrades only): a package stops receiving versions under its
    // own name when the catalog superseded it via `replaces`. Installing the replacer
    // performs the migration — the realize path removes the replaced package and carries
    // its requested/held intent. Held packages are respected like any other bare target.
    let upgrading: std::collections::HashSet<&String> =
        to_upgrade.iter().map(|(name, _, _)| name).collect();
    let renames: Vec<(String, String)> = if explicit {
        Vec::new()
    } else {
        let replacements = catalog_replacements()?;
        targets
            .iter()
            .filter(|name| !held.get(*name).copied().unwrap_or(false))
            .filter(|name| !upgrading.contains(*name))
            .filter_map(|old| {
                replacements
                    .get(old)
                    .filter(|new| *new != old && !installed.contains_key(*new))
                    .map(|new| (old.clone(), new.clone()))
            })
            .collect()
    };

    if to_upgrade.is_empty() && renames.is_empty() {
        return Ok(());
    }

    if args.dry_run {
        print_dry_run_plan(&targets, &to_upgrade)?;
        for (old, new) in &renames {
            output::plan_item('~', &format!("{old} → {new} (replaced)"));
        }
        return Ok(());
    }

    let mut names: Vec<String> = to_upgrade.into_iter().map(|(name, _, _)| name).collect();
    for (old, new) in renames {
        output::report(&format!(
            "{} {}",
            output::accent(new.as_str()),
            output::faint(&format!("replaces {old}"))
        ));
        names.push(new);
    }
    let mut announce = format!(
        "upgrading {} package{}…",
        output::strong(&names.len().to_string()),
        if names.len() == 1 { "" } else { "s" }
    );
    // Say what the upgrade *implies*, not just what was asked: missing or drifted build
    // deps realize along the way, and an innocuous one-package upgrade can mean an llvm
    // rebuild. Best-effort — a failed estimate never blocks the upgrade.
    if let Ok(extra) = install::estimate_extra_realizations(&names)
        && !extra.is_empty()
    {
        let shown: Vec<&str> = extra.iter().take(6).map(String::as_str).collect();
        let ellipsis = if extra.len() > shown.len() {
            ", …"
        } else {
            ""
        };
        announce.push_str(&output::faint(&format!(
            " (+ {} build dep{} to realize: {}{ellipsis})",
            extra.len(),
            if extra.len() == 1 { "" } else { "s" },
            shown.join(", ")
        )));
    }
    output::note(&announce);
    install::upgrade_packages(&names)
}

/// The catalog's supersessions: replaced name → replacer, harvested from every tome's runes
/// and index entries. First writer wins on conflicting claims (tomes are iterated in
/// configuration order), matching candidate resolution.
fn catalog_replacements() -> Result<BTreeMap<String, String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for pkg in tome_packages()? {
        for old in &pkg.metadata.replaces {
            map.entry(old.clone())
                .or_insert_with(|| pkg.metadata.name.clone());
        }
    }
    let target = paths::target_triple();
    for tome in tome::load_tomes()? {
        let Some((_, index)) = tome::package_index(&tome)? else {
            continue;
        };
        for (_, entry) in index.entries {
            if entry.target != target {
                continue;
            }
            for old in entry.replaces {
                map.entry(old).or_insert_with(|| entry.name.clone());
            }
        }
    }
    Ok(map)
}

fn collect_upgrades(
    targets: &[String],
    installed: &BTreeMap<String, Version>,
    held: &BTreeMap<String, bool>,
    explicit: bool,
    dry_run: bool,
) -> Result<Vec<(String, Version, Version)>> {
    // A package at the newest version can still be pending a rebuild: its expected
    // content address drifted (rune edited, dependency re-addressed, build environment
    // changed). Upgrade is the convergence command, so drifted packages are selected for
    // re-realization at the same version — and never reported "up to date" while stale.
    let world = crate::install::InstalledWorld::load_default()?;
    let stale_info = crate::store::closure::stale_installed(&world);
    let drifted: std::collections::HashSet<String> =
        stale_info.iter().map(|stale| stale.name.clone()).collect();
    // When the drift comes from the build environment, every affected package shares the
    // same cause — say it once instead of per line.
    if let Some(diff) = stale_info.iter().find_map(|stale| stale.env_change.clone()) {
        output::note(&format!("build environment changed: {diff}"));
    }

    let mut to_upgrade: Vec<(String, Version, Version)> = Vec::new();
    let mut up_to_date: Vec<(String, Version)> = Vec::new();
    for name in targets {
        let Some(current) = installed.get(name) else {
            bail!("package `{name}` is not installed");
        };
        if !explicit && held.get(name).copied().unwrap_or(false) {
            output::warn(&format!(
                "{name} is held; skipping (use `grm unhold {name}` to allow)"
            ));
            continue;
        }
        match solve::newest_available(name)? {
            Some(newest) if newest > *current => {
                if !dry_run {
                    // A major bump deserves a persistent line the user can act on; routine
                    // bumps just feed the transient progress spinner.
                    if newest.major > current.major {
                        output::warn(&format!(
                            "{} {} — major version (next time: grm hold {name})",
                            output::strong(name),
                            output::strong(&format!("{current} → {newest}")),
                        ));
                    } else {
                        output::status(&format!("upgrading {name} {current} → {newest}"));
                    }
                }
                to_upgrade.push((name.clone(), current.clone(), newest));
            }
            _ if drifted.contains(name.as_str()) => {
                if !dry_run {
                    output::status(&format!("rebuilding {name} {current} (address drifted)"));
                }
                to_upgrade.push((name.clone(), current.clone(), current.clone()));
            }
            _ => up_to_date.push((name.clone(), current.clone())),
        }
    }
    // A handful of "up to date" lines is reassuring; a long wall of them is noise. Past a
    // threshold, collapse to a single count.
    if up_to_date.len() > 10 {
        output::report(&format!("{} packages are up to date", up_to_date.len()));
    } else {
        for (name, current) in &up_to_date {
            output::report(&format!(
                "{} {}",
                output::accent(name),
                output::faint(&format!("is up to date ({current})"))
            ));
        }
    }
    Ok(to_upgrade)
}

/// A dry-run plan shows the full resolved closure — not just the named targets but every
/// dependency the upgrade would pull in or rebuild — so it matches what a real run would do.
/// Mirrors the upgrade resolve (`estimate_extra_realizations`): drop the targets so they
/// re-resolve to the newest, keep the rest of the current world.
fn print_dry_run_plan(targets: &[String], to_upgrade: &[(String, Version, Version)]) -> Result<()> {
    let world = install::InstalledWorld::load_default()?;
    let installed_now = world.installed_versions();
    let mut installed = world.installed_versions_current()?;
    // Only the genuinely-changing roots re-resolve; unchanged targets stay reused so they don't
    // show up as spurious rebuilds. Their outdated/drifted deps still surface as steps via the
    // resolver's newest-candidate rule.
    for (name, _, _) in to_upgrade {
        installed.remove(name);
    }
    let deps: Vec<crate::model::Dependency> = targets
        .iter()
        .map(|name| crate::model::Dependency::any(name.clone()))
        .collect();
    let linked = world.linked_immut();
    let plan = solve::resolve(&deps, &installed, &linked, None)?;
    output::line("plan:");
    for step in &plan.steps {
        match installed_now.get(&step.name) {
            None => output::plan_item('+', &format!("{} {} (new)", step.name, step.version)),
            Some(current) if *current == step.version => output::plan_item(
                '~',
                &format!("{} {} (rebuild: address drifted)", step.name, step.version),
            ),
            Some(current) => {
                output::plan_item('~', &format!("{} {current} → {}", step.name, step.version))
            }
        }
    }
    Ok(())
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
    output::heading("installed");
    output::field("name", &output::strong(&state.name));
    output::field("version", &state.version);
    if let Some(upstream) = &state.upstream_version {
        output::field("upstream version", upstream);
    }
    if let Some(target) = &state.target {
        output::field("target", target);
    }
    output::field("archive hash", &state.archive_hash);
    if !state.bins.is_empty() {
        output::field("bins", &join_bins(state.bins.iter()));
    }
    if !state.notes.is_empty() {
        output::field("notes", &state.notes.join(" · "));
    }
}

fn print_available(package: &TomePackage) {
    output::heading("available");
    output::field("name", &output::strong(&package.metadata.name));
    output::field("version", &package.metadata.version);
    if let Some(upstream) = &package.metadata.upstream_version {
        output::field("upstream version", upstream);
    }
    output::field("tome", &package.tome);
    output::field("rune", &package.rune.display().to_string());
    if let Some(parent) = &package.metadata.split_from {
        output::field("split from", &output::strong(parent));
    }
    if let Some(summary) = &package.metadata.summary {
        output::field("summary", summary);
    }
    let target = paths::target_triple();
    let bins = package.metadata.bins_for(&target);
    if !bins.is_empty() {
        output::field("bins", &join_bins(bins.iter()));
    }
    if !package.metadata.notes.is_empty() {
        output::field("notes", &package.metadata.notes.join(" · "));
    }
}

/// `name → path, name → path` for a bins map, the one-line value of the `bins` info field.
fn join_bins<'a>(bins: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    bins.map(|(name, path)| format!("{name} → {path}"))
        .collect::<Vec<_>>()
        .join(", ")
}
