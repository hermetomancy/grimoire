//! `grm tome lint`: validate a local tome before committing or publishing it.
//!
//! Unlike [`super::verify::validate_tome_cache`] — which runs against the *synced remote* cache
//! during `tome update` and bails on the first error — this walks a local worktree and collects
//! *every* problem in one pass: a rune that fails to parse (a command outside the rune subset),
//! package metadata that fails validation (an unknown field, a fixed-output package with build
//! deps), a duplicate package name, or a malformed `tome.rn`/`packages` block. The author sees
//! the full list at once instead of fixing one, re-running, and hitting the next.

use anyhow::{Result, bail};
use std::{collections::BTreeMap, path::Path};

use crate::{
    cli::TomeLintArgs,
    model::validate_tome_name,
    nu::runtime::EmbeddedNuRuntime,
    util::output::{problem, report, warn},
};

use super::verify::{read_tome_manifest, validate_tome_packages};

pub fn lint(args: TomeLintArgs) -> Result<()> {
    let root = &args.path;
    if !root.join("tome.rn").exists() {
        bail!("{} is not a tome (missing tome.rn)", root.display());
    }

    let warnings = collect_warnings(root)?;
    let problems = collect_problems(root)?;
    for warning in &warnings {
        warn(warning);
    }
    if problems.is_empty() {
        let runes = root.join("runes");
        let count = rune_files(&runes).map(|v| v.len()).unwrap_or(0);
        report(&format!(
            "tome at {} is clean ({count} runes)",
            root.display()
        ));
        return Ok(());
    }

    for p in &problems {
        problem(p);
    }
    bail!("{} problem(s) found in {}", problems.len(), root.display());
}

/// Collects every lint problem in the tome rooted at `root`, as human-readable lines. An empty
/// vec means the tome is clean. Used by both `tome lint` (which prints them) and `tome sign`
/// (which refuses to sign when any exist). Returns `Err` only for an I/O failure reading the
/// tree, not for lint findings — those are the returned strings.
pub(super) fn collect_problems(root: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();

    match read_tome_manifest(root) {
        Ok(manifest) => {
            if let Err(e) = validate_tome_name(&manifest.name) {
                problems.push(format!("tome.rn: {e:#}"));
            }
            match &manifest.packages {
                Some(packages) => {
                    if let Err(e) = validate_tome_packages(packages) {
                        problems.push(format!("tome.rn: {e:#}"));
                    }
                }
                None => problems.push("tome.rn: missing required field `packages`".to_string()),
            }
        }
        Err(e) => problems.push(format!("tome.rn: {e:#}")),
    }

    let runes_dir = root.join("runes");
    if !runes_dir.is_dir() {
        problems.push("missing runes/ directory".to_string());
        return Ok(problems);
    }

    let runes = rune_files(&runes_dir)?;
    if runes.is_empty() {
        problems.push("runes/ contains no .rn definitions".to_string());
        return Ok(problems);
    }

    // Map each declared package name to the first rune file that declared it, so a collision
    // names both offenders.
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    for path in &runes {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        match EmbeddedNuRuntime.package_metadata(path) {
            Ok(meta) => {
                if let Some(first) = by_name.insert(meta.name.clone(), rel.clone()) {
                    problems.push(format!(
                        "duplicate package name `{}` in {first} and {rel}",
                        meta.name
                    ));
                }
            }
            Err(e) => problems.push(format!("{rel}: {e:#}")),
        }
    }

    Ok(problems)
}

fn collect_warnings(root: &Path) -> Result<Vec<String>> {
    let runes_dir = root.join("runes");
    if !runes_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut warnings = Vec::new();
    for path in rune_files(&runes_dir)? {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let source = std::fs::read(&path)?;
        if let Some(warning) = crate::nu::runtime::shell_wrapper_warning(&source, &rel) {
            warnings.push(warning);
        }
    }
    Ok(warnings)
}

/// The `.rn` files directly under `runes_dir`, sorted by name for stable output and a
/// deterministic manifest. Non-`.rn` files and subdirectories are ignored.
pub(super) fn rune_files(runes_dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !runes_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(runes_dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rn") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn clean_tome_has_no_problems() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("runes")).unwrap();
        write(
            &root.join("tome.rn"),
            &crate::tome::tome_manifest_template("scratch", "scratch tome"),
        );
        write(
            &root.join("runes/hello.rn"),
            &crate::tome::rune_template("hello", "1.0.0"),
        );
        assert!(collect_problems(root).unwrap().is_empty());
    }

    #[test]
    fn reports_parse_and_schema_problems_together() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("runes")).unwrap();
        write(
            &root.join("tome.rn"),
            &crate::tome::tome_manifest_template("scratch", "scratch tome"),
        );
        // A command outside the rune subset: a parse error.
        write(
            &root.join("runes/bad_parse.rn"),
            "export const package = { name: \"a\", version: \"1.0.0\", sources: {} }\n\
             export def build [ctx] { ls | str join \",\" }\n",
        );
        // A field that belongs under `meta`: a metadata validation error.
        write(
            &root.join("runes/bad_field.rn"),
            "export const package = { name: \"b\", version: \"1.0.0\", homepage: \"https://x\", sources: {} }\n",
        );
        let problems = collect_problems(root).unwrap();
        assert_eq!(
            problems.len(),
            2,
            "both problems reported at once: {problems:?}"
        );
        assert!(problems.iter().any(|p| p.contains("bad_parse.rn")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("bad_field.rn") && p.contains("homepage"))
        );
    }

    #[test]
    fn detects_duplicate_package_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("runes")).unwrap();
        write(
            &root.join("tome.rn"),
            &crate::tome::tome_manifest_template("scratch", "scratch tome"),
        );
        write(
            &root.join("runes/one.rn"),
            "export const package = { name: \"dup\", version: \"1.0.0\", sources: {} }\n",
        );
        write(
            &root.join("runes/two.rn"),
            "export const package = { name: \"dup\", version: \"2.0.0\", sources: {} }\n",
        );
        let problems = collect_problems(root).unwrap();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("duplicate package name `dup`")),
            "{problems:?}"
        );
    }

    #[test]
    fn shell_wrapper_is_warning_not_problem() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("runes")).unwrap();
        write(
            &root.join("tome.rn"),
            &crate::tome::tome_manifest_template("scratch", "scratch tome"),
        );
        write(
            &root.join("runes/shell.rn"),
            "export const package = { name: \"shell\", version: \"1.0.0\" }\n\
             export def build [ctx] { sh -c \"echo hi\" }\n",
        );
        assert!(collect_problems(root).unwrap().is_empty());
        let warnings = collect_warnings(root).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("sh -c"), "{warnings:?}");
    }
}
