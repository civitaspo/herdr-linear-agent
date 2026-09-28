//! The coordinator's inbox: one JSON file per item under `runs/<KEY>/inbox/`.
//!
//! The ticker writes items (an issue edit, a reply, a worker's new group);
//! `context` shows the unhandled ones and records which ids it showed;
//! `inbox done` moves items to `inbox/done/`. `--all` moves only the items
//! the coordinator has been shown, so an item written after its last
//! `context` is never handled unseen.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::run::Run;

/// Handled items are kept this long.
const DONE_KEPT: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub summary: String,
    #[serde(default)]
    pub created: String,
    /// The run's inbox counter when the item was written; orders the items.
    #[serde(default)]
    pub seq: u64,
}

fn inbox_dir(run: &Run) -> PathBuf {
    run.dir.join("inbox")
}

fn done_dir(run: &Run) -> PathBuf {
    run.dir.join("inbox/done")
}

fn counter_path(run: &Run) -> PathBuf {
    run.state_dir().join("inbox-counter.json")
}

fn seen_path(run: &Run) -> PathBuf {
    run.state_dir().join("inbox-seen.json")
}

/// Ids name files, so one that could leave the folder or hide is refused.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || id.starts_with('.') || id.contains('/') || id.contains("..") {
        bail!("`{id}` is not an inbox item id");
    }
    Ok(())
}

/// Keeps an id part to characters that are safe in a file name.
fn id_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Writes an item and returns its id, `<time>-<kind>-<subject>-<n>`.
pub fn write(run: &Run, kind: &str, subject: &str, summary: &str) -> Result<String> {
    let _lock = run.lock()?;
    let seq = files::read_json::<u64>(&counter_path(run)).unwrap_or(0) + 1;
    files::write_json(&counter_path(run), &seq)?;
    let created = files::now();
    let stamp: String = created
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let item = Item {
        id: format!("{stamp}-{}-{}-{seq}", id_part(kind), id_part(subject)),
        kind: kind.to_string(),
        subject: subject.to_string(),
        summary: summary.to_string(),
        created,
        seq,
    };
    std::fs::create_dir_all(inbox_dir(run))?;
    files::write_json(&item_path(&inbox_dir(run), &item.id), &item)?;
    Ok(item.id)
}

fn item_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// The items in `dir` that parse, and the names of the files that do not.
fn read_items(dir: &Path) -> (Vec<Item>, Vec<String>) {
    let (mut items, mut unreadable) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (items, unreadable);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match files::read_json::<Item>(&entry.path()) {
            Some(item) if name == format!("{}.json", item.id) => items.push(item),
            _ => unreadable.push(name),
        }
    }
    items.sort_by(|a, b| (a.seq, &a.id).cmp(&(b.seq, &b.id)));
    (items, unreadable)
}

/// The items not yet done, oldest first, each summary on one line.
pub fn unhandled(run: &Run) -> Vec<Item> {
    let (mut items, _) = read_items(&inbox_dir(run));
    for item in &mut items {
        item.summary = item.summary.replace("\r\n", " ").replace('\n', " ");
    }
    items
}

/// Records the ids `context` showed the coordinator, replacing the last set:
/// a digest shows every unhandled item, so nothing shown earlier is lost.
pub fn mark_seen(run: &Run, ids: &[String]) -> Result<()> {
    let _lock = run.lock()?;
    files::write_json(&seen_path(run), ids)
}

/// The ids the last `context` showed.
pub fn seen(run: &Run) -> Vec<String> {
    files::read_json(&seen_path(run)).unwrap_or_default()
}

/// Moves the named items and, with `all`, every item the coordinator has
/// been shown, to `inbox/done/`. Returns how many moved.
pub fn done(run: &Run, ids: &[String], all: bool) -> Result<usize> {
    for id in ids {
        check_id(id)?;
    }
    let _lock = run.lock()?;
    let mut targets: Vec<String> = ids.to_vec();
    if all {
        targets.extend(seen(run).into_iter().filter(|id| check_id(id).is_ok()));
    }
    targets.sort();
    targets.dedup();
    std::fs::create_dir_all(done_dir(run))?;
    let mut moved = 0;
    for id in &targets {
        let from = item_path(&inbox_dir(run), id);
        match std::fs::rename(&from, item_path(&done_dir(run), id)) {
            Ok(()) => moved += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("could not move {}", from.display())),
        }
    }
    Ok(moved)
}

/// Removes handled items older than 30 days, and moves files in `inbox/`
/// that are not items of this build (another build's format) to
/// `inbox/done/`, so the caller logs each of them once. Returns their names.
pub fn prune_done(run: &Run) -> Vec<String> {
    let Ok(_lock) = run.lock() else {
        return Vec::new();
    };
    let now = SystemTime::now();
    if let Ok(entries) = std::fs::read_dir(done_dir(run)) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > DONE_KEPT);
            if old && entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let (_, unreadable) = read_items(&inbox_dir(run));
    let _ = std::fs::create_dir_all(done_dir(run));
    unreadable
        .into_iter()
        .filter(|name| std::fs::rename(inbox_dir(run).join(name), done_dir(run).join(name)).is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunRecord;

    fn run() -> (tempfile::TempDir, Run) {
        let home = tempfile::tempdir().unwrap();
        let run = Run::create(
            home.path(),
            RunRecord {
                identifier: "DATA-7".into(),
                issue_id: "issue-7".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        (home, run)
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn items_are_listed_oldest_first_with_one_line_summaries() {
        let (_home, run) = run();
        let first = write(&run, "worker", "w1", "first line\nsecond line").unwrap();
        let second = write(&run, "reply", "reply", "A new reply.").unwrap();
        let third = write(&run, "worker", "w1", "again").unwrap();
        assert!(first.ends_with("-worker-w1-1"), "{first}");
        assert!(second.ends_with("-reply-reply-2"), "{second}");
        assert!(third.ends_with("-worker-w1-3"), "{third}");
        assert_ne!(first, third);

        let items = unhandled(&run);
        assert_eq!(ids(&items), [&first, &second, &third]);
        assert_eq!(items[0].summary, "first line second line");
        assert_eq!(items[0].kind, "worker");
        assert_eq!(items[0].subject, "w1");
        assert_eq!(items[1].summary, "A new reply.");
        assert!(run.dir.join(format!("inbox/{first}.json")).is_file());
    }

    #[test]
    fn done_all_moves_only_the_items_context_showed() {
        let (_home, run) = run();
        let shown = write(&run, "worker", "w1", "shown").unwrap();
        assert_eq!(done(&run, &[], true).unwrap(), 0, "nothing was shown yet");

        mark_seen(&run, std::slice::from_ref(&shown)).unwrap();
        assert_eq!(seen(&run), std::slice::from_ref(&shown));
        let later = write(&run, "worker", "w2", "written after context").unwrap();
        assert_eq!(done(&run, &[], true).unwrap(), 1);
        assert_eq!(ids(&unhandled(&run)), [&later]);
        assert!(run.dir.join(format!("inbox/done/{shown}.json")).is_file());

        assert_eq!(done(&run, &[], true).unwrap(), 0, "already moved");
        mark_seen(&run, std::slice::from_ref(&later)).unwrap();
        assert_eq!(
            seen(&run),
            std::slice::from_ref(&later),
            "the last context replaces the set"
        );
    }

    #[test]
    fn named_ids_move_whether_or_not_they_were_shown() {
        let (_home, run) = run();
        let a = write(&run, "issue", "issue", "edited").unwrap();
        let b = write(&run, "worker", "w1", "report").unwrap();
        let c = write(&run, "worker", "w2", "report").unwrap();
        mark_seen(&run, std::slice::from_ref(&a)).unwrap();
        assert_eq!(done(&run, std::slice::from_ref(&b), false).unwrap(), 1);
        assert_eq!(ids(&unhandled(&run)), [&a, &c]);
        assert_eq!(
            done(&run, &[c.clone(), "no-such-item".into()], true).unwrap(),
            2
        );
        assert!(unhandled(&run).is_empty());
    }

    #[test]
    fn ids_that_could_leave_the_folder_are_refused() {
        let (_home, run) = run();
        let kept = write(&run, "worker", "w1", "kept").unwrap();
        for bad in ["", "../run.json", "a/b", ".hidden", "x..y"] {
            let error = done(&run, &[kept.clone(), bad.into()], false).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("`{bad}` is not an inbox item id")
            );
        }
        assert_eq!(ids(&unhandled(&run)), [&kept], "nothing moved on a refusal");
    }

    #[test]
    fn pruning_drops_old_done_items_and_sets_aside_foreign_files() {
        let (_home, run) = run();
        let old = write(&run, "worker", "w1", "old").unwrap();
        let recent = write(&run, "worker", "w1", "recent").unwrap();
        done(&run, &[old.clone(), recent.clone()], false).unwrap();
        let old_path = run.dir.join(format!("inbox/done/{old}.json"));
        let month_ago = SystemTime::now() - Duration::from_secs(31 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(month_ago)
            .unwrap();
        let open = write(&run, "reply", "reply", "open").unwrap();
        std::fs::write(run.dir.join("inbox/legacy.md"), "an older format").unwrap();
        std::fs::write(run.dir.join("inbox/other.json"), r#"{"body":"x"}"#).unwrap();

        let mut set_aside = prune_done(&run);
        set_aside.sort();
        assert_eq!(set_aside, ["legacy.md", "other.json"]);
        assert!(!old_path.exists());
        assert!(run.dir.join(format!("inbox/done/{recent}.json")).exists());
        assert!(run.dir.join("inbox/done/legacy.md").exists());
        assert_eq!(ids(&unhandled(&run)), [&open]);
        assert!(
            prune_done(&run).is_empty(),
            "each foreign file is reported once"
        );
    }
}
