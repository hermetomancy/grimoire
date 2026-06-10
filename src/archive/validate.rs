//! Pre-extraction safety validation: every archive member path, symlink target, and
//! entry type is checked before `unpack` can ever see it (AGENTS.md §10.2–§10.3).

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub fn validate_archive_member_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    !text.starts_with('/')
        && !text.starts_with('\\')
        && !crate::model::looks_windows_absolute(&text)
        && !text.contains('\\')
        && path
            .components()
            .all(|part| !matches!(part, std::path::Component::ParentDir))
}

/// Validates the target of a symlink archive member. The target is interpreted relative to the
/// directory that contains the link and must resolve to a path *within* the package root: absolute
/// targets, Windows-style paths, and any `..` sequence that would climb above the root are
/// rejected. This keeps preserved symlinks self-contained and relocatable, and guarantees
/// extraction can never be lured outside the destination through a link (AGENTS.md §10.3).
pub fn validate_symlink_target(link: &Path, target: &Path) -> bool {
    let text = target.to_string_lossy();
    if text.is_empty()
        || text.starts_with('/')
        || text.starts_with('\\')
        || text.contains('\\')
        || crate::model::looks_windows_absolute(&text)
    {
        return false;
    }

    // Track how deep the resolved target sits below the package root. We seed the depth with the
    // link's own parent directory (the target is relative to it) and require it never to underflow.
    let mut depth: usize = 0;
    let seed = link.parent().into_iter().flat_map(Path::components);
    for part in seed.chain(target.components()) {
        match part {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(less) => depth = less,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Validates every member path in a `.tar.zst` archive before extraction.
/// Rejects traversal, absolute paths, Windows-style paths, hard links,
/// escaping symlinks, and members nested under symlinks (AGENTS.md §10.2–§10.3).
pub fn validate_archive_paths(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    validate_tar_entries(&mut archive)
        .with_context(|| format!("validate archive {}", path.display()))
}

/// Generic tar entry validator shared by archive installs and source extraction.
/// Rejects traversal, absolute paths, Windows-style paths, hard links,
/// escaping symlinks, and members nested under symlinks.
pub fn validate_tar_entries<R: Read>(tar: &mut tar::Archive<R>) -> Result<()> {
    let mut bad = Vec::new();
    let mut members: Vec<PathBuf> = Vec::new();
    let mut symlinks: BTreeSet<PathBuf> = BTreeSet::new();

    for entry in tar.entries()? {
        let entry = entry?;
        let member_path = entry.path()?.into_owned();
        let member = member_path.display().to_string();
        if !validate_archive_member_path(&member_path) {
            bad.push(member);
            continue;
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_hard_link() {
            bail!("archive contains a hard link, which is not accepted yet: {member}");
        }
        if entry_type.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| anyhow::anyhow!("archive symlink `{member}` is missing a target"))?;
            if !validate_symlink_target(&member_path, &target) {
                bail!(
                    "archive symlink `{member}` has a target that escapes the package: {}",
                    target.display()
                );
            }
            symlinks.insert(member_path.clone());
        }
        members.push(member_path);
    }

    if !bad.is_empty() {
        bail!("archive contains unsafe paths: {}", bad.join(", "));
    }

    // A member nested under a symlink would be extracted *through* that link; reject it so the
    // validated targets are the only paths `unpack` can ever follow.
    if !symlinks.is_empty() {
        for member in &members {
            if let Some(ancestor) = member
                .ancestors()
                .skip(1)
                .find(|ancestor| symlinks.contains(*ancestor))
            {
                bail!(
                    "archive member `{}` is nested under symlink `{}`",
                    member.display(),
                    ancestor.display()
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_member_paths_reject_cross_platform_escape_forms() {
        for path in [
            Path::new("../escape"),
            Path::new("/absolute"),
            Path::new("\\absolute"),
            Path::new("C:/absolute"),
            Path::new("C:\\absolute"),
            Path::new("dir\\file"),
        ] {
            assert!(
                !validate_archive_member_path(path),
                "archive path should be rejected: {}",
                path.display()
            );
        }
    }

    #[test]
    fn symlink_targets_accept_within_package() {
        // Sibling, nested, and `..` hops that stay inside the package root are all fine,
        // including the versioned shared-library aliases the core tome needs.
        for (link, target) in [
            ("bin/awk", "myln"),
            ("bin/sh", "myln"),
            ("lib/libintl.dylib", "libintl.8.dylib"),
            ("lib/foo/libbar.so", "libbar.so.1"),
            ("bin/tool", "../libexec/tool"),
            ("share/a/b/link", "../../c/file"),
        ] {
            assert!(
                validate_symlink_target(Path::new(link), Path::new(target)),
                "symlink {link} -> {target} should be accepted"
            );
        }
    }

    #[test]
    fn symlink_targets_reject_escaping_or_absolute() {
        for (link, target) in [
            ("bin/x", "/etc/passwd"),
            ("bin/x", "/tmp"),
            ("bin/x", "../../etc/passwd"),
            ("bin/x", "../../../root"),
            ("link", ".."),
            ("a/b/link", "../../../outside"),
            ("bin/x", "C:\\Windows"),
            ("bin/x", "dir\\file"),
            ("bin/x", ""),
        ] {
            assert!(
                !validate_symlink_target(Path::new(link), Path::new(target)),
                "symlink {link} -> {target} should be rejected"
            );
        }
    }
}
