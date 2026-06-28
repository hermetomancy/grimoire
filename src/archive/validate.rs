//! Pre-extraction safety validation: every archive member path, symlink target, and
//! entry type is checked before `unpack` can ever see it (AGENTS.md §10.2–§10.3).

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Take},
    path::{Component, Path, PathBuf},
};

use super::{
    BoundedReader, MAX_ARCHIVE_DECOMPRESSED_BYTES, MAX_ARCHIVE_MEMBERS, MAX_SOURCE_ARCHIVE_MEMBERS,
};

pub fn validate_archive_member_path(path: &Path) -> bool {
    !path.to_string_lossy().starts_with('/')
        && path
            .components()
            .all(|part| !matches!(part, std::path::Component::ParentDir))
}

/// Validates the target of a symlink archive member. The target is interpreted relative to the
/// directory that contains the link and must resolve to a path *within* the package root: absolute
/// targets and any `..` sequence that would climb above the root are rejected. This keeps
/// preserved symlinks self-contained and relocatable, and guarantees extraction can never be
/// lured outside the destination through a link (AGENTS.md §10.3).
pub fn validate_symlink_target(link: &Path, target: &Path) -> bool {
    let text = target.to_string_lossy();
    if text.is_empty() || text.starts_with('/') {
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
/// Rejects traversal, absolute paths, hard links,
/// escaping symlinks, and members nested under symlinks (AGENTS.md §10.2–§10.3).
pub fn validate_archive_paths(path: &Path) -> Result<()> {
    validate_archive_paths_capturing(path, None).map(|_| ())
}

/// Like [`validate_archive_paths`], but also returns the text of the member named `capture`
/// (when present) from the same pass, so callers that need embedded metadata do not have to
/// re-read the archive after validating it.
pub fn validate_archive_paths_capturing(
    path: &Path,
    capture: Option<&str>,
) -> Result<Option<String>> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let decoder = BoundedReader::new(
        decoder,
        MAX_ARCHIVE_DECOMPRESSED_BYTES,
        "archive decompressed stream",
    );
    let mut archive = tar::Archive::new(decoder);
    validate_tar_entries_capturing(&mut archive, capture, MAX_ARCHIVE_MEMBERS)
        .with_context(|| format!("validate archive {}", path.display()))
}

/// Generic tar entry validator for source extraction. Rejects traversal, absolute paths, hard
/// links, escaping symlinks, and members nested under symlinks. Source archives are pinned by the
/// rune's `sha256`, so they get the generous [`MAX_SOURCE_ARCHIVE_MEMBERS`] sanity bound rather than
/// the strict package-archive limit (the LLVM monorepo source alone ships ~185k members).
pub fn validate_tar_entries<R: Read>(tar: &mut tar::Archive<R>) -> Result<()> {
    validate_tar_entries_capturing(tar, None, MAX_SOURCE_ARCHIVE_MEMBERS).map(|_| ())
}

/// [`validate_tar_entries`] plus single-pass capture of the member named `capture`. `max_members`
/// is the caller's member-count ceiling — strict for untrusted package archives, generous for
/// checksum-pinned source archives.
pub fn validate_tar_entries_capturing<R: Read>(
    tar: &mut tar::Archive<R>,
    capture: Option<&str>,
    max_members: usize,
) -> Result<Option<String>> {
    let mut bad = Vec::new();
    let mut members: Vec<PathBuf> = Vec::new();
    let mut symlinks: BTreeSet<PathBuf> = BTreeSet::new();
    let mut captured = None;

    for entry in tar.entries()? {
        if members.len() >= max_members {
            bail!("archive contains more than {max_members} members");
        }
        let mut entry = entry?;
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
        if let Some(wanted) = capture {
            let normalized = member_path
                .strip_prefix(".")
                .unwrap_or(&member_path)
                .display()
                .to_string();
            if normalized == wanted {
                let mut text = String::new();
                let mut limited = entry.by_ref().take(super::MAX_CAPTURED_MEMBER_BYTES + 1);
                limited
                    .read_to_string(&mut text)
                    .with_context(|| format!("read archive member `{wanted}`"))?;
                reject_oversized_capture(&limited, wanted)?;
                captured = Some(text);
            }
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

    Ok(captured)
}

fn reject_oversized_capture<R>(reader: &Take<R>, wanted: &str) -> Result<()> {
    if reader.limit() == 0 {
        bail!(
            "archive member `{wanted}` exceeds {} bytes",
            super::MAX_CAPTURED_MEMBER_BYTES
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_member_paths_reject_traversal_and_absolute() {
        for path in [Path::new("../escape"), Path::new("/absolute")] {
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

    /// An uncompressed tar carrying `count` empty regular files, in memory.
    fn tar_with_members(count: usize) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for i in 0..count {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("file{i}"), std::io::empty())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn member_cap_is_enforced_at_the_caller_supplied_ceiling() {
        let bytes = tar_with_members(5);
        // Exactly at the ceiling is fine; one below it rejects the sixth-would-be member early.
        assert!(
            validate_tar_entries_capturing(&mut tar::Archive::new(&bytes[..]), None, 5).is_ok(),
            "five members under a ceiling of five must validate"
        );
        let err = validate_tar_entries_capturing(&mut tar::Archive::new(&bytes[..]), None, 4)
            .expect_err("five members under a ceiling of four must be rejected");
        assert!(
            err.to_string().contains("more than 4 members"),
            "the ceiling must be the one reported: {err}"
        );
    }

    const _: () = {
        // The LLVM monorepo source ships ~185k members; the source path must admit far more than the
        // strict package-archive cap, which would reject it.
        assert!(MAX_SOURCE_ARCHIVE_MEMBERS > MAX_ARCHIVE_MEMBERS);
        assert!(MAX_SOURCE_ARCHIVE_MEMBERS >= 185_000);
    };

    #[test]
    fn symlink_targets_reject_escaping_or_absolute() {
        for (link, target) in [
            ("bin/x", "/etc/passwd"),
            ("bin/x", "/tmp"),
            ("bin/x", "../../etc/passwd"),
            ("bin/x", "../../../root"),
            ("link", ".."),
            ("a/b/link", "../../../outside"),
            ("bin/x", ""),
        ] {
            assert!(
                !validate_symlink_target(Path::new(link), Path::new(target)),
                "symlink {link} -> {target} should be rejected"
            );
        }
    }
}
