//! Drift detection for installed packages.
//!
//! Compares the store hashes recorded in installed package state against the hashes the current
//! runes would produce, so packages whose inputs changed are re-realized instead of silently
//! reused.

use crate::{build, build::toolchain, install::InstalledWorld};

/// Installed packages whose recorded address has drifted from the catalog: the rune currently
/// resolvable for the same name *and version* produces a different store hash — its content,
/// declared sources, build flags, dependency closures, or the host build environment changed
/// since the package was realized. Resolution must not reuse these by version; re-realizing
/// them is how a rune edit propagates to installs at all.
///
/// Conservative on both sides: a package whose rune cannot be resolved or hashed (installed
/// from a local archive, or whose deps only resolve as capabilities) is never reported — there
/// is nothing to rebuild it from; and a rune that moved to a *different* version is `grm
/// upgrade`'s business, not drift. One walker memoizes the closure walk across the whole set.
/// One drifted install: the package, the address it would re-realize to, and — when the
/// recorded build-environment identity differs from the current one — a human-readable
/// rendering of exactly which identity components moved. `env_change: None` means the
/// environment matches (the rune or a dependency changed instead) or the state predates
/// identity recording.
pub struct StaleInstall {
    pub name: String,
    pub expected: String,
    pub env_change: Option<String>,
}

pub fn stale_installed(world: &InstalledWorld) -> Vec<StaleInstall> {
    let hermetic = build::effective_source_build_hermetic(false, false).unwrap_or(true);
    stale_installed_with_mode(world, hermetic)
}

pub fn stale_installed_with_mode(world: &InstalledWorld, hermetic: bool) -> Vec<StaleInstall> {
    let Ok(mut walker) =
        super::Walker::with_target_and_mode(&crate::util::paths::target_triple(), hermetic)
    else {
        return Vec::new();
    };
    // Address split-member externals that are *held* against their pinned installed address,
    // matching the builder: a held dep is pinned to its installed bits, not its current rune, so
    // without this a split member depending on it would be flagged drifted forever (re-realizing it
    // reproduces the same address). Non-held externals stay on the canonical `of_name` walk, so a
    // genuine rune edit to a group's external dep is still detected and rebuilds the group.
    walker.resolved = world
        .iter()
        .filter(|state| state.held)
        .map(|state| (state.name.clone(), state.store_hash.clone()))
        .collect();
    let current_env =
        toolchain::store_build_env_id_for_target(hermetic, &crate::util::paths::target_triple());
    let mut stale = Vec::new();
    for state in world.iter() {
        // A hold pins the installed bits, not just the version: a held package is never
        // re-realized for drift. `grm unhold` lets the pending drift apply.
        if state.held {
            continue;
        }
        let Ok(Some(rune)) = build::find_rune(&state.name) else {
            continue;
        };
        let Ok(metadata) = walker.metadata(&rune) else {
            continue;
        };
        if metadata.version != state.version {
            continue;
        }
        let Ok(expected) = walker.of_rune(&state.name, &rune) else {
            continue;
        };
        if expected != state.store_hash {
            let env_change = match &state.build_env {
                Some(recorded) if recorded != &current_env => {
                    Some(diff_build_env(recorded, &current_env))
                }
                _ => None,
            };
            stale.push(StaleInstall {
                name: state.name.clone(),
                expected,
                env_change,
            });
        }
    }
    stale
}

/// Renders the difference between two build-environment identities ("tool:banner" lists)
/// as the changed components only: `ld: ld64-1167.5 → LLD 22.1.7`. Components present on
/// one side only render against `(none)`.
fn diff_build_env(recorded: &str, current: &str) -> String {
    let parse = |id: &str| -> std::collections::BTreeMap<String, String> {
        id.split(',')
            .filter_map(|part| {
                part.split_once(':')
                    .map(|(tool, banner)| (tool.to_owned(), banner.to_owned()))
            })
            .collect()
    };
    let old_parts = parse(recorded);
    let new_parts = parse(current);
    // Dedup by exact tool name. A `starts_with` check would let one tool's change mask another
    // whose name it is a prefix of (`ld` vs `ldd`, `as` vs `asm`), silently dropping a real
    // change from the explanation; a sorted set visits each tool once.
    let tools: std::collections::BTreeMap<&String, ()> = old_parts
        .keys()
        .chain(new_parts.keys())
        .map(|t| (t, ()))
        .collect();
    let mut changes = Vec::new();
    for tool in tools.into_keys() {
        let old = old_parts.get(tool).map(String::as_str).unwrap_or("(none)");
        let new = new_parts.get(tool).map(String::as_str).unwrap_or("(none)");
        if old != new {
            changes.push(format!("{tool}: {old} → {new}"));
        }
    }
    changes.join("; ")
}

#[cfg(test)]
mod tests {
    use super::diff_build_env;

    #[test]
    fn diff_renders_only_changed_identity_components() {
        let recorded = "as:llvm-as 22.1.7,cc:clang version 22.1.7,ld:ld64-1167.5,sdk:26.5";
        let current = "as:llvm-as 22.1.7,cc:clang version 22.1.7,ld:LLD 22.1.7,sdk:26.5";
        assert_eq!(
            diff_build_env(recorded, current),
            "ld: ld64-1167.5 → LLD 22.1.7"
        );
    }

    #[test]
    fn diff_renders_added_and_removed_components() {
        let diff = diff_build_env("cc:clang 22,lipo:llvm-lipo", "cc:clang 22,sdk:26.5");
        assert!(diff.contains("lipo: llvm-lipo → (none)"), "{diff}");
        assert!(diff.contains("sdk: (none) → 26.5"), "{diff}");
    }
}
