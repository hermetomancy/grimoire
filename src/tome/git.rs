use anyhow::{Context, Result, anyhow};
use std::{path::Path, sync::atomic::AtomicBool};

/// Clones `url` at `ref_name` into `dest` and checks out a work tree. Network access uses
/// `gix`; Grimoire never shells out to the `git` CLI (AGENTS.md §1a).
pub fn clone(url: &str, ref_name: &str, dest: &Path) -> Result<()> {
    let should_interrupt = AtomicBool::new(false);
    let mut prepare = gix::prepare_clone(url, dest)
        .with_context(|| format!("prepare clone {url}"))?
        .with_ref_name(Some(ref_name))
        .map_err(|err| anyhow!("invalid ref `{ref_name}`: {err}"))?;
    let (mut checkout, _) = prepare
        .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
        .with_context(|| format!("clone {url}"))?;
    checkout
        .main_worktree(gix::progress::Discard, &should_interrupt)
        .with_context(|| format!("check out {url} ref {ref_name}"))?;
    Ok(())
}

pub fn head_commit(path: &Path) -> Result<Option<String>> {
    // Local (copied) tome sources are not git repositories and have no HEAD.
    if !path.join(".git").exists() {
        return Ok(None);
    }

    let repo = gix::open(path).with_context(|| format!("open git repo {}", path.display()))?;
    let commit = repo
        .head_commit()
        .with_context(|| format!("read HEAD commit {}", path.display()))?;
    Ok(Some(commit.id.to_string()))
}
