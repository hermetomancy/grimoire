//! Tome news: maintainer announcements shipped as `news/*.md` files in the tome repository.
//!
//! Filenames must sort chronologically (the convention is a date prefix, e.g.
//! `2026-06-10-musl-rebuild.md`); the highest filename a user has seen is tracked in
//! `TomeState.last_seen_news`, so each item is shown exactly once — on the `grm tome update`
//! that first syncs it, or via `grm tome news`. The first sync after `grm tome add` marks the
//! existing backlog as seen without printing it.

use anyhow::{Context, Result};
use std::{fs, path::Path};

use crate::{
    catalog::sync_common,
    model::{Catalog, TomeState},
    nu::nuon_io,
    util::progress::report,
};

pub struct NewsItem {
    /// The filename, the sort key and seen-marker value.
    pub id: String,
    pub title: String,
    pub body: String,
}

/// Reads every `news/*.md` item from a tome cache, sorted by filename. A tome without a
/// `news/` directory has no items.
pub fn list_news(cache: &Path) -> Result<Vec<NewsItem>> {
    let news_dir = cache.join("news");
    if !news_dir.exists() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for entry in fs::read_dir(&news_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read news item {}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(id)
            .to_owned();
        let (title, body) = split_title(&text, &stem);
        items.push(NewsItem {
            id: id.to_owned(),
            title,
            body,
        });
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(items)
}

/// The items strictly newer than `marker` (all items when no marker is recorded yet).
pub fn unread<'a>(items: &'a [NewsItem], marker: Option<&str>) -> &'a [NewsItem] {
    let start = match marker {
        None => 0,
        Some(marker) => items.partition_point(|item| item.id.as_str() <= marker),
    };
    &items[start..]
}

/// Title = the first `# ` heading (stripped), falling back to the filename stem; body = the
/// remaining lines with the heading removed and surrounding blank lines trimmed.
fn split_title(text: &str, fallback: &str) -> (String, String) {
    let mut lines = text.lines();
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("# ") {
            let body: Vec<&str> = lines.collect();
            return (heading.trim().to_owned(), body.join("\n").trim().to_owned());
        }
        // First non-empty line is not a heading: the whole text is the body.
        break;
    }
    (fallback.to_owned(), text.trim().to_owned())
}

/// Surfaces news after a tome cache sync. On the first ever sync (`first_sync`) the existing
/// backlog is marked seen without printing — a fresh `grm tome add` should not dump history.
/// Later syncs print each unread item (body capped for the update flow) and advance the marker.
pub fn surface_after_sync(tome_name: &str, cache: &Path, first_sync: bool) -> Result<()> {
    let state = sync_common::load_catalog::<TomeState>(tome_name)?;
    let items = list_news(cache)?;
    let Some(newest) = items.last().map(|item| item.id.clone()) else {
        return Ok(());
    };
    let fresh = unread(&items, state.last_seen_news.as_deref());
    if fresh.is_empty() {
        return Ok(());
    }
    if !first_sync {
        for item in fresh {
            print_item(tome_name, item, NEWS_UPDATE_BODY_LINES);
        }
        crate::util::progress::note(&format!(
            "run `grm tome news {tome_name}` to read items in full"
        ));
    }
    advance_marker(state, newest)
}

/// Body lines shown per item in the update flow; `grm tome news` prints items in full.
const NEWS_UPDATE_BODY_LINES: usize = 10;

fn print_item(tome_name: &str, item: &NewsItem, cap: usize) {
    use crate::util::progress::{faint, note, strong};
    report(&format!(
        "{} {}",
        faint(&format!("news [{tome_name}]")),
        strong(&item.title)
    ));
    if item.body.is_empty() {
        return;
    }
    let lines: Vec<&str> = item.body.lines().collect();
    for line in lines.iter().take(cap) {
        note(&format!("  {line}"));
    }
    if lines.len() > cap {
        note(&format!("  … ({} more lines)", lines.len() - cap));
    }
}

fn advance_marker(mut state: TomeState, newest: String) -> Result<()> {
    state.last_seen_news = Some(newest);
    let state_path =
        sync_common::state_dir(TomeState::SUBDIR)?.join(format!("{}.nuon", state.name));
    nuon_io::write_nuon(&state_path, &state.to_value())
}

/// The `grm tome news` command: prints unread items in full and advances the marker, or with
/// `all` prints everything and leaves the marker untouched. Explicitly requested data, so it
/// prints with `println!` (visible under `--quiet`).
pub fn news_command(name: Option<String>, all: bool) -> Result<()> {
    let tomes = match name {
        Some(name) => vec![sync_common::load_catalog::<TomeState>(&name)?],
        None => sync_common::load_catalogs::<TomeState>()?,
    };
    let mut printed_any = false;
    for tome in tomes {
        let cache = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
        let items = list_news(&cache)?;
        let to_show = if all {
            &items[..]
        } else {
            unread(&items, tome.last_seen_news.as_deref())
        };
        for item in to_show {
            printed_any = true;
            println!("news [{}] {}", tome.name, item.title);
            if !item.body.is_empty() {
                for line in item.body.lines() {
                    println!("  {line}");
                }
            }
            println!();
        }
        if !all
            && let Some(newest) = items.last().map(|item| item.id.clone())
            && unread(&items, tome.last_seen_news.as_deref())
                .last()
                .is_some()
        {
            advance_marker(tome, newest)?;
        }
    }
    if !printed_any {
        report("no unread news");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> NewsItem {
        NewsItem {
            id: id.to_owned(),
            title: id.to_owned(),
            body: String::new(),
        }
    }

    #[test]
    fn unread_is_everything_without_marker() {
        let items = vec![item("2026-01-01-a.md"), item("2026-02-01-b.md")];
        assert_eq!(unread(&items, None).len(), 2);
    }

    #[test]
    fn unread_is_strictly_newer_than_marker() {
        let items = vec![
            item("2026-01-01-a.md"),
            item("2026-02-01-b.md"),
            item("2026-03-01-c.md"),
        ];
        let fresh = unread(&items, Some("2026-02-01-b.md"));
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, "2026-03-01-c.md");
        assert!(unread(&items, Some("2026-03-01-c.md")).is_empty());
    }

    #[test]
    fn split_title_uses_heading_and_trims_body() {
        let (title, body) = split_title("\n# Big change\n\nDetails here.\nMore.\n", "fallback");
        assert_eq!(title, "Big change");
        assert_eq!(body, "Details here.\nMore.");
    }

    #[test]
    fn split_title_falls_back_to_stem_without_heading() {
        let (title, body) = split_title("Just a body line.\n", "2026-01-01-a");
        assert_eq!(title, "2026-01-01-a");
        assert_eq!(body, "Just a body line.");
    }
}
