//! Inbox items: events the ticker leaves in a run folder for the coordinator.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::run::Run;

pub const DONE_RETENTION_DAYS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub created: String,
    pub summary: String,
}

fn inbox_dir(run: &Run) -> PathBuf {
    run.dir.join("inbox")
}

fn parse(text: &str) -> Option<Item> {
    let rest = text.strip_prefix("+++\n")?;
    let front = rest.split_once("\n+++").map(|(front, _)| front)?;
    toml::from_str(front).ok()
}

/// File-name-safe form of a subject (a worker id, `issue`, `reply`).
fn safe_subject(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "item".to_string()
    } else {
        cleaned
    }
}

/// Writes one item. The id is `<UTC timestamp>-<kind>-<subject>-<n>`, where
/// `<n>` is a counter allocated under the run lock, so two events in one tick
/// never share a name.
pub fn write(run: &Run, kind: &str, subject: &str, summary: &str) -> Result<String> {
    let _lock = run.lock()?;
    let counter_path = run.state_dir().join("inbox-counter.json");
    let n: u64 = files::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    files::write_json(&counter_path, &n)?;
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    let id = format!("{stamp}-{kind}-{}-{n}", safe_subject(subject));
    let item = Item {
        id: id.clone(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        created: files::now(),
        // One line, no control characters: summaries are printed in the digest.
        summary: summary
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
    };
    let text = format!("+++\n{}+++\n", toml::to_string(&item)?);
    files::write_atomic(&inbox_dir(run).join(format!("{id}.md")), text.as_bytes())?;
    Ok(id)
}

/// Deletes handled items older than `DONE_RETENTION_DAYS`.
pub fn prune_done(run: &Run) {
    let Ok(entries) = std::fs::read_dir(inbox_dir(run).join("done")) else {
        return;
    };
    let limit = std::time::Duration::from_secs(DONE_RETENTION_DAYS * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Unhandled items, oldest first (ids start with a UTC timestamp).
pub fn unhandled(run: &Run) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(inbox_dir(run)) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| parse(&text))
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items
}

pub fn seen(run: &Run) -> BTreeSet<String> {
    files::read_json(&run.state_dir().join("inbox-seen.json")).unwrap_or_default()
}

/// Records that `context` showed these items, so they are nudged once only.
pub fn mark_seen(run: &Run, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let _lock = run.lock()?;
    let mut all = seen(run);
    all.extend(ids.iter().cloned());
    // Ids of items that no longer exist are dropped so the file stays small.
    let live: BTreeSet<String> = unhandled(run).into_iter().map(|i| i.id).collect();
    all.retain(|id| live.contains(id));
    files::write_json(&run.state_dir().join("inbox-seen.json"), &all)
}

/// An item id is also a file name, so it is checked before any path is built.
fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok || id.contains("..") {
        bail!("`{id}` is not an inbox item id");
    }
    Ok(())
}

/// Moves items to `inbox/done/`. Returns how many moved.
pub fn done(run: &Run, ids: &[String], all: bool) -> Result<usize> {
    let ids: Vec<String> = if all {
        unhandled(run).into_iter().map(|i| i.id).collect()
    } else {
        ids.to_vec()
    };
    for id in &ids {
        validate_id(id)?;
    }
    let _lock = run.lock()?;
    let dir = inbox_dir(run);
    let mut moved = 0;
    for id in &ids {
        let from = dir.join(format!("{id}.md"));
        if !from.is_file() {
            eprintln!("no unhandled item `{id}`");
            continue;
        }
        std::fs::rename(&from, dir.join("done").join(format!("{id}.md")))?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunRecord;

    fn run() -> (tempfile::TempDir, Run) {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        (dir, run)
    }

    #[test]
    fn items_are_written_listed_marked_seen_and_moved_to_done() {
        let (_dir, run) = run();
        let a = write(&run, "worker", "w1", "first").unwrap();
        let b = write(&run, "worker", "w1", "second\nline").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("-worker-w1-1"), "{a}");
        let items = unhandled(&run);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].summary, "second line");

        mark_seen(&run, std::slice::from_ref(&a)).unwrap();
        assert_eq!(seen(&run).len(), 1);
        assert_eq!(done(&run, &[a], false).unwrap(), 1);
        assert_eq!(unhandled(&run).len(), 1);
        assert_eq!(done(&run, &[], true).unwrap(), 1);
        assert!(unhandled(&run).is_empty());
    }

    #[test]
    fn hostile_ids_are_refused() {
        let (_dir, run) = run();
        for bad in ["../x", "a/b", "", ".hidden", "x..y"] {
            assert!(done(&run, &[bad.to_string()], false).is_err(), "{bad}");
        }
    }
}
