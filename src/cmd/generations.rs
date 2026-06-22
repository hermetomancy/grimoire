//! `grm generations`: the generation timeline — when each was created, what changed relative
//! to the one before it, and where `profiles/current` points.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::{profile, util::output::line, util::time_util};

/// How many per-package changes a generation line spells out before collapsing the rest
/// into a count, so a large upgrade does not wrap the listing into unreadability.
const CHANGES_SHOWN: usize = 4;

pub fn generations() -> Result<()> {
    // Newest first, matching `list_generations`; each entry diffs against the next-older one.
    let generations = profile::list_generations()?;
    let current = profile::current_generation_id()?;

    for (index, generation) in generations.iter().enumerate() {
        let marker = if current == Some(generation.id) {
            "*"
        } else {
            " "
        };
        let changes = match generations.get(index + 1) {
            Some(previous) => {
                diff_changes(&package_versions(generation), &package_versions(previous))
            }
            None => "initial".to_owned(),
        };
        line(&format!(
            "{} gen-{:<4} {}  {:>3} packages  {changes}",
            marker,
            generation.id,
            time_util::format_timestamp(generation.created),
            generation.packages.len(),
        ));
    }

    if let Some(id) = current {
        line(&format!("profiles/current → gen-{id}"));
    }
    Ok(())
}

/// Summarizes one generation's package map against its predecessor's: `+ added`, `- removed`,
/// and `~ name old → new` for version moves, capped at [`CHANGES_SHOWN`] entries. Empty
/// versions (generations without state snapshots) still diff by name; they just cannot show
/// version moves.
fn diff_changes(now: &BTreeMap<String, String>, was: &BTreeMap<String, String>) -> String {
    let mut changes = Vec::new();
    for (name, version) in now {
        match was.get(name) {
            None => changes.push(format!("+ {}", label(name, version))),
            Some(old) if old != version && !old.is_empty() && !version.is_empty() => {
                changes.push(format!("~ {name} {old} → {version}"))
            }
            Some(_) => {}
        }
    }
    for name in was.keys() {
        if !now.contains_key(name) {
            changes.push(format!("- {name}"));
        }
    }

    if changes.is_empty() {
        return "no changes".to_owned();
    }
    let hidden = changes.len().saturating_sub(CHANGES_SHOWN);
    changes.truncate(CHANGES_SHOWN);
    let mut text = changes.join(", ");
    if hidden > 0 {
        text.push_str(&format!(", … {hidden} more"));
    }
    text
}

/// Maps a generation's packages to their versions via its state snapshot. Generations from
/// before snapshots fall back to bare names (`gen.nuon` records names only), so their diffs
/// can still show additions and removals, just not version moves.
fn package_versions(generation: &profile::Generation) -> BTreeMap<String, String> {
    let snapshot = profile::generation_dir(generation.id)
        .ok()
        .and_then(|dir| profile::read_state_snapshot(&dir).ok().flatten());
    match snapshot {
        Some(states) => states
            .into_iter()
            .map(|state| (state.name, state.version))
            .collect(),
        None => generation
            .packages
            .iter()
            .map(|name| (name.clone(), String::new()))
            .collect(),
    }
}

fn label(name: &str, version: &str) -> String {
    if version.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packages(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, version)| (name.to_string(), version.to_string()))
            .collect()
    }

    #[test]
    fn additions_removals_and_version_moves_are_described() {
        let now = packages(&[("a", "1.0.0"), ("c", "2.0.0"), ("d", "1.1.0")]);
        let was = packages(&[("a", "1.0.0"), ("b", "0.9.0"), ("d", "1.0.0")]);
        assert_eq!(
            diff_changes(&now, &was),
            "+ c 2.0.0, ~ d 1.0.0 → 1.1.0, - b"
        );
    }

    #[test]
    fn snapshotless_generations_diff_by_name_only() {
        let now = packages(&[("a", ""), ("c", "")]);
        let was = packages(&[("a", ""), ("b", "")]);
        assert_eq!(diff_changes(&now, &was), "+ c, - b");
    }

    #[test]
    fn identical_sets_report_no_changes() {
        let set = packages(&[("a", "1.0.0")]);
        assert_eq!(diff_changes(&set, &set), "no changes");
    }

    #[test]
    fn long_diffs_collapse_into_a_count() {
        let now = packages(&[
            ("a", "1"),
            ("b", "1"),
            ("c", "1"),
            ("d", "1"),
            ("e", "1"),
            ("f", "1"),
        ]);
        let was = packages(&[]);
        let text = diff_changes(&now, &was);
        assert!(text.ends_with("… 2 more"), "{text}");
    }
}
