//! Split package groups: companion runes carved out of one parent build.
//!
//! A rune with `split_from: "<parent>"` is a *split member*: it declares no sources and no
//! `build` function, only `files` globs claiming a slice of the parent rune's build output.
//! The parent and its members form a [`SplitGroup`] — built in one pass, then partitioned
//! into one package per member, with the parent receiving every unclaimed file. Membership
//! is discovered by scanning the parent rune's directory, so a group always lives in a
//! single `runes/` dir (one tome).

use anyhow::{Context, Result, bail};
use nu_glob::{MatchOptions, Pattern};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::model::PackageMetadata;

/// One package of a split group: the parent or a split member.
pub struct GroupMember {
    pub name: String,
    pub rune: PathBuf,
    pub metadata: PackageMetadata,
}

/// A parent rune and the split members carved from its build output.
pub struct SplitGroup {
    pub parent: GroupMember,
    /// Split members sorted by name; the parent is not among them.
    pub splits: Vec<GroupMember>,
}

impl SplitGroup {
    /// Every member of the group, parent first, then splits sorted by name.
    pub fn members(&self) -> impl Iterator<Item = &GroupMember> {
        std::iter::once(&self.parent).chain(self.splits.iter())
    }

    /// The raw rune bytes of every member keyed by package name — the group's recipe
    /// identity, folded into the group store hash and embedded in every member archive.
    pub fn rune_bytes(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        self.members()
            .map(|member| {
                let bytes = fs::read(&member.rune)
                    .with_context(|| format!("read rune {}", member.rune.display()))?;
                Ok((member.name.clone(), bytes))
            })
            .collect()
    }
}

/// Resolves the split group `rune` belongs to: the rune itself may be the parent or any
/// member. Returns `None` when the rune is an ordinary standalone package. Group metadata
/// is read through [`crate::build::read_rune_metadata`] (signature verification), exactly
/// like a standalone build would read it.
pub fn group_for(rune: &Path) -> Result<Option<SplitGroup>> {
    let metadata = crate::build::read_rune_metadata(rune, tome_name(rune)?.as_deref())?;
    let runes_dir = rune.parent().unwrap_or_else(|| Path::new("."));

    let (parent_rune, parent_meta) = match &metadata.split_from {
        Some(parent_name) => {
            let parent_rune = runes_dir.join(format!("{parent_name}.rn"));
            if !parent_rune.exists() {
                bail!(
                    "split member `{}` names parent `{parent_name}`, but {} does not exist",
                    metadata.name,
                    parent_rune.display()
                );
            }
            let parent_meta = crate::build::read_rune_metadata(
                &parent_rune,
                tome_name(&parent_rune)?.as_deref(),
            )?;
            (parent_rune, parent_meta)
        }
        None => (rune.to_path_buf(), metadata),
    };

    if let Some(grandparent) = &parent_meta.split_from {
        bail!(
            "split parent `{}` is itself a split member of `{grandparent}`; groups do not chain",
            parent_meta.name
        );
    }

    let splits = scan_members(runes_dir, &parent_meta.name)?;
    if splits.is_empty() {
        return Ok(None);
    }
    if parent_meta.fixed_output {
        bail!(
            "package `{}` is fixed-output and cannot be a split parent",
            parent_meta.name
        );
    }
    for member in &splits {
        if member.metadata.version != parent_meta.version {
            bail!(
                "split member `{}` is version {} but parent `{}` is {}; group versions must match",
                member.name,
                member.metadata.version,
                parent_meta.name,
                parent_meta.version
            );
        }
    }

    Ok(Some(SplitGroup {
        parent: GroupMember {
            name: parent_meta.name.clone(),
            rune: parent_rune,
            metadata: parent_meta,
        },
        splits,
    }))
}

/// The split members declaring `split_from: <parent>` in `runes_dir`, sorted by name.
/// The scan uses the light metadata reader; chosen members are re-read through the
/// canonical verified path by way of being built or hashed later.
fn scan_members(runes_dir: &Path, parent: &str) -> Result<Vec<GroupMember>> {
    let mut members = Vec::new();
    let entries = match fs::read_dir(runes_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(members),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rn") {
            continue;
        }
        let metadata = match crate::nu::runtime::EmbeddedNuRuntime.package_metadata(&path) {
            Ok(metadata) => metadata,
            // An unparsable sibling is usually not this group's problem — whoever builds it
            // gets the real error. But a rune that *names* `split_from` was meant to be a
            // member: silently dropping it would build the group without it (and hand its
            // files to the parent), so surface its error here instead.
            Err(err) => {
                // If the file cannot even be read we cannot tell whether it was meant to be a
                // member, and silently dropping a member is the failure mode this guards against
                // — so surface the read error rather than swallowing it into "no match".
                let source = fs::read(&path)
                    .with_context(|| format!("read split member candidate {}", path.display()))?;
                let needle = b"split_from";
                if source.windows(needle.len()).any(|window| window == needle) {
                    return Err(err.context(format!(
                        "split member candidate {} is invalid",
                        path.display()
                    )));
                }
                continue;
            }
        };
        if metadata.split_from.as_deref() != Some(parent) {
            continue;
        }
        let source =
            fs::read(&path).with_context(|| format!("read split member {}", path.display()))?;
        if exports_build_function(&source) {
            bail!(
                "split member `{}` must not declare a `build` function; split members only claim files from `{parent}`",
                metadata.name
            );
        }
        let metadata = crate::build::read_rune_metadata(&path, tome_name(&path)?.as_deref())?;
        members.push(GroupMember {
            name: metadata.name.clone(),
            rune: path,
            metadata,
        });
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(members)
}

fn exports_build_function(source: &[u8]) -> bool {
    use nu_parser::TokenContents;

    let (tokens, _) = nu_parser::lex(source, 0, &[], &[], true);
    let mut at_command_start = true;
    let mut saw_export = false;
    let mut saw_def = false;
    for token in tokens {
        match token.contents {
            TokenContents::Pipe
            | TokenContents::PipePipe
            | TokenContents::Semicolon
            | TokenContents::Eol => {
                at_command_start = true;
                saw_export = false;
                saw_def = false;
            }
            TokenContents::Item => {
                let word = std::str::from_utf8(
                    source
                        .get(token.span.start..token.span.end)
                        .unwrap_or_default(),
                )
                .unwrap_or_default();
                if at_command_start {
                    saw_export = word == "export";
                    saw_def = false;
                    at_command_start = false;
                    continue;
                }
                if saw_export && !saw_def {
                    if word == "def" || word == "def-env" {
                        saw_def = true;
                    } else if !word.starts_with('-') {
                        saw_export = false;
                    }
                    continue;
                }
                if saw_export && saw_def {
                    if word == "build" {
                        return true;
                    }
                    if !word.starts_with('-') {
                        saw_export = false;
                        saw_def = false;
                    }
                }
            }
            _ => {
                if at_command_start {
                    at_command_start = false;
                }
            }
        }
    }
    false
}

fn tome_name(rune: &Path) -> Result<Option<String>> {
    crate::build::tome_name_for_rune(rune)
}

/// Glob semantics for `files` claims: `*`/`?` stay within one path component, `**` crosses
/// directories — so `bin/clang*` cannot accidentally claim `bin/clang/foo`.
const GLOB_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
    recursive_match_hidden_dir: true,
};

/// Moves every file each split member claims out of `payload_dir` into
/// `staging_root/<member>/`, returning the per-member directories. The parent's share is
/// whatever remains in `payload_dir`. Hard errors: a file claimed by two members, a member
/// whose globs claim nothing, and a symlink whose target was claimed away from it.
pub fn partition_payload(
    payload_dir: &Path,
    splits: &[GroupMember],
    staging_root: &Path,
) -> Result<BTreeMap<String, PathBuf>> {
    let patterns = compile_patterns(splits)?;
    let entries = relative_entries(payload_dir)?;
    let pre_partition = relative_paths(payload_dir)?;

    let mut claims: BTreeMap<PathBuf, &str> = BTreeMap::new();
    let mut claimed_by: BTreeMap<&str, usize> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for rel in &entries {
        let mut claimants = patterns.iter().filter_map(|(name, globs)| {
            globs
                .iter()
                .any(|glob| glob.matches_path_with(rel, GLOB_OPTIONS))
                .then_some(name.as_str())
        });
        let Some(first) = claimants.next() else {
            continue;
        };
        let rest: Vec<&str> = claimants.collect();
        if !rest.is_empty() {
            conflicts.push(format!(
                "{} (claimed by {first} and {})",
                rel.display(),
                rest.join(", ")
            ));
            continue;
        }
        claims.insert(rel.clone(), first);
        *claimed_by.entry(first).or_default() += 1;
    }

    if !conflicts.is_empty() {
        bail!(
            "split members claim overlapping files; tighten their `files` globs:\n  {}",
            conflicts.join("\n  ")
        );
    }
    for (name, _) in &patterns {
        if !claimed_by.contains_key(name.as_str()) {
            bail!(
                "split member `{name}` claims no files from the build output; \
                 its `files` globs match nothing"
            );
        }
    }

    let mut dirs = BTreeMap::new();
    for (rel, member) in &claims {
        let source = payload_dir.join(rel);
        let dest_root = staging_root.join(member);
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create split staging dir {}", parent.display()))?;
        }
        fs::rename(&source, &dest)
            .with_context(|| format!("move {} into split `{member}`", rel.display()))?;
        dirs.entry((*member).to_owned()).or_insert(dest_root);
    }

    prune_empty_dirs(payload_dir)?;
    check_symlinks(payload_dir, &pre_partition, "the parent package")?;
    for (member, dir) in &dirs {
        check_symlinks(dir, &pre_partition, member)?;
    }
    Ok(dirs)
}

fn compile_patterns(splits: &[GroupMember]) -> Result<Vec<(String, Vec<Pattern>)>> {
    splits
        .iter()
        .map(|member| {
            let globs = member
                .metadata
                .files
                .iter()
                .map(|pattern| {
                    Pattern::new(pattern).map_err(|err| {
                        anyhow::anyhow!(
                            "split member `{}` files glob `{pattern}` is invalid: {err}",
                            member.name
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((member.name.clone(), globs))
        })
        .collect()
}

/// Every file and symlink under `root` as a sorted list of relative paths. Directories are not
/// claimable; they materialize on each side as needed.
fn relative_entries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("strip payload prefix from {}", entry.path().display()))?
            .to_path_buf();
        entries.push(rel);
    }
    Ok(entries)
}

/// Every path that existed before partitioning, including directories. Symlink validation uses
/// this broader set because a relative symlink to a directory can dangle after that directory's
/// contents are claimed by another member.
fn relative_paths(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut entries = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        if entry.path() == root {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("strip payload prefix from {}", entry.path().display()))?
            .to_path_buf();
        entries.insert(rel);
    }
    Ok(entries)
}

/// Removes directories left empty after claimed files moved out, bottom-up, so the parent
/// archive does not carry husks like an emptied `lib/cmake/clang/`.
fn prune_empty_dirs(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).contents_first(true) {
        let entry = entry?;
        if !entry.file_type().is_dir() || entry.path() == root {
            continue;
        }
        let is_empty = entry
            .path()
            .read_dir()
            .with_context(|| format!("read dir {}", entry.path().display()))?
            .next()
            .is_none();
        if is_empty {
            fs::remove_dir(entry.path())
                .with_context(|| format!("remove empty dir {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Rejects a partition that separated a relative symlink from its target: the link's
/// lexically resolved destination existed in the pre-partition payload but is no longer
/// reachable in the package that owns the link.
fn check_symlinks(root: &Path, pre_partition: &BTreeSet<PathBuf>, owner: &str) -> Result<()> {
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_symlink() {
            continue;
        }
        let target = fs::read_link(entry.path())
            .with_context(|| format!("read symlink {}", entry.path().display()))?;
        if target.is_absolute() {
            // Absolute targets are rejected later by archive packing; not a partition concern.
            continue;
        }
        let rel_link = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("strip prefix from {}", entry.path().display()))?;
        let Some(resolved) = lexical_resolve(rel_link, &target) else {
            continue; // Escapes the package root; packing rejects it with a better message.
        };
        if root.join(&resolved).symlink_metadata().is_ok() {
            continue;
        }
        if pre_partition.contains(&resolved) {
            bail!(
                "split partition broke symlink {} -> {} in {owner}: its target was claimed \
                 by another member; adjust the `files` globs to keep them together",
                rel_link.display(),
                target.display()
            );
        }
    }
    Ok(())
}

/// Warns when a split member's files embed the parent's absolute store prefix. The whole
/// group configures against the parent's prefix, so a path baked into a member's files
/// points at the parent's *remainder* — usually a latent bug (members must locate shared
/// resources relative to their own binaries). Warn-only: cmake export files and the like
/// legitimately reference the parent.
pub fn warn_parent_prefix_leaks(member: &str, dir: &Path, parent_prefix: &Path) -> Result<()> {
    let needle = parent_prefix.to_string_lossy().into_owned().into_bytes();
    let mut hits: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        if bytes
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
        {
            hits.push(
                entry
                    .path()
                    .strip_prefix(dir)
                    .unwrap_or(entry.path())
                    .display()
                    .to_string(),
            );
            if hits.len() >= 3 {
                break;
            }
        }
    }
    if !hits.is_empty() {
        crate::util::output::warn(&format!(
            "split member {member} bakes the parent's store prefix into {} — those paths \
             point at the parent's remainder, not this package",
            hits.join(", ")
        ));
    }
    Ok(())
}

/// Resolves `target` relative to the directory containing `link`, purely lexically.
/// Returns `None` when the result escapes the package root.
fn lexical_resolve(link: &Path, target: &Path) -> Option<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = link
        .parent()
        .map(|dir| dir.components().map(|c| c.as_os_str().to_owned()).collect())
        .unwrap_or_default();
    for component in target.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Deps, PackageMetadata};
    use std::collections::BTreeMap as Map;

    fn split_member(name: &str, files: &[&str]) -> GroupMember {
        GroupMember {
            name: name.to_owned(),
            rune: PathBuf::from(format!("{name}.rn")),
            metadata: PackageMetadata {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                target: None,
                store_path: None,
                targets: Vec::new(),
                fixed_output: false,
                build_only: false,
                summary: None,
                bins: Map::new(),
                sources: Map::new(),
                deps: Deps::default(),
                build_flags: Map::new(),
                provides: Vec::new(),
                libs: Vec::new(),
                notes: Vec::new(),
                upstream_version: None,
                conflicts: Vec::new(),
                replaces: Vec::new(),
                split_from: Some("core".to_owned()),
                files: files.iter().map(|f| (*f).to_owned()).collect(),
            },
        }
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn partition_moves_claims_and_prunes_emptied_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/core", "core");
        write(&payload, "bin/extra", "extra");
        write(&payload, "share/extra/data.txt", "data");

        let members = [split_member("extra", &["bin/extra*", "share/extra/**"])];
        let dirs = partition_payload(&payload, &members, &staging).unwrap();

        let extra_dir = dirs.get("extra").expect("extra staging dir");
        assert!(extra_dir.join("bin/extra").exists());
        assert!(extra_dir.join("share/extra/data.txt").exists());
        assert!(payload.join("bin/core").exists(), "remainder stays");
        assert!(!payload.join("bin/extra").exists(), "claims move out");
        assert!(
            !payload.join("share").exists(),
            "directories emptied by claims are pruned from the parent"
        );
    }

    #[test]
    fn star_does_not_cross_path_components() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/extra", "extra");
        write(&payload, "bin/extra-dir/tool", "tool");

        let members = [split_member("extra", &["bin/extra*"])];
        let dirs = partition_payload(&payload, &members, &staging).unwrap();
        assert!(dirs["extra"].join("bin/extra").exists());
        assert!(
            payload.join("bin/extra-dir/tool").exists(),
            "`*` must not match through `/`"
        );
    }

    #[test]
    fn overlapping_claims_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/extra", "extra");

        let members = [
            split_member("extra", &["bin/extra*"]),
            split_member("grabby", &["bin/**"]),
        ];
        let err = partition_payload(&payload, &members, &staging).unwrap_err();
        assert!(
            err.to_string().contains("overlapping"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn member_with_no_claims_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/core", "core");

        let members = [split_member("hollow", &["lib/nothing/**"])];
        let err = partition_payload(&payload, &members, &staging).unwrap_err();
        assert!(
            err.to_string().contains("claims no files"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn partition_that_separates_a_symlink_from_its_target_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/extra", "extra");
        std::os::unix::fs::symlink("extra", payload.join("bin").join("extra-alias")).unwrap();

        // The member claims the target but not the alias pointing at it.
        let members = [split_member("extra", &["bin/extra"])];
        let err = partition_payload(&payload, &members, &staging).unwrap_err();
        assert!(
            format!("{err:#}").contains("broke symlink"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn partition_that_separates_a_symlink_from_its_directory_target_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "lib/extra/data.txt", "data");
        fs::create_dir_all(payload.join("share")).unwrap();
        std::os::unix::fs::symlink("../lib/extra", payload.join("share").join("extra")).unwrap();

        // The member claims the directory contents; the parent keeps the symlink to that directory.
        let members = [split_member("extra", &["lib/extra/**"])];
        let err = partition_payload(&payload, &members, &staging).unwrap_err();
        assert!(
            format!("{err:#}").contains("broke symlink"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_claimed_together_with_its_target_is_fine() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let staging = temp.path().join("staging");
        write(&payload, "bin/extra", "extra");
        std::os::unix::fs::symlink("extra", payload.join("bin").join("extra-alias")).unwrap();

        let members = [split_member("extra", &["bin/extra*"])];
        let dirs = partition_payload(&payload, &members, &staging).unwrap();
        let link = dirs["extra"].join("bin/extra-alias");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(dirs["extra"].join("bin/extra").exists());
    }

    #[test]
    fn split_member_build_export_is_detected() {
        assert!(exports_build_function(
            b"export const package = { name: extra }\nexport def build [ctx] {}\n"
        ));
        assert!(exports_build_function(
            b"export def --env build [ctx] { cd $ctx.package_dir }\n"
        ));
        assert!(!exports_build_function(
            b"# export def build [] {}\nexport const package = { name: extra }\n"
        ));
        assert!(!exports_build_function(
            b"export const package = { name: extra, summary: 'export def build [] {}' }\n"
        ));
    }
}
