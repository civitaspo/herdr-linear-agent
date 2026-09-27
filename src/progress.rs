//! `report`: a worker tells how far it got, from its own pane. The record is
//! kept under the state folder for the ticker's groups, and the activity is
//! shown in Herdr as a pane token.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::herdr::{self, Herdr, PaneId};
use crate::paths::Env;

/// The activity a worker reports when it waits on the coordinator.
pub const WAITING: &str = "Waiting for you";
/// The metadata source of every pane token the plugin reports.
pub const SOURCE: &str = "herdr-linear-agent";
pub const TOKEN_TTL_MS: u64 = 300_000;
const ACTIVITY_COLUMNS: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub activity: String,
    /// `None` for `--unknown`.
    pub percent: Option<u8>,
    /// Seconds since the Unix epoch.
    pub reported_at: i64,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.activity == WAITING
    }
}

fn is_wide(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Trimmed, without control or bidirectional-override characters, and cut
/// to `columns` display columns (a wide character counts two).
pub fn clean(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text
        .trim()
        .chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
    {
        let width = if is_wide(c) { 2 } else { 1 };
        if used + width > columns {
            break;
        }
        used += width;
        out.push(c);
    }
    out.trim_end().to_string()
}

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("progress")
}

fn path(state_dir: &Path, socket: &str, pane: &str) -> PathBuf {
    let pane: String = pane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let hash = files::sha256_hex(socket.as_bytes());
    dir(state_dir).join(format!("{pane}-{}.json", &hash[..16]))
}

pub fn save(state_dir: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(state_dir))?;
    files::write_json(&path(state_dir, &record.socket, &record.pane_id), record)
}

pub fn load(state_dir: &Path, socket: &str, pane: &str) -> Option<Record> {
    files::read_json::<Record>(&path(state_dir, socket, pane))
        .filter(|r| r.socket == socket && r.pane_id == pane)
}

/// The pane's record, unless it was written from another terminal than the
/// pane's current one (a new process in a reused pane id).
pub fn self_report(state_dir: &Path, socket: &str, pane: &str, terminal: &str) -> Option<Record> {
    load(state_dir, socket, pane).filter(|r| terminal.is_empty() || r.terminal_id == terminal)
}

/// Removes this socket's records of panes that no longer exist.
pub fn prune(state_dir: &Path, socket: &str, live: &[String]) {
    let Ok(entries) = std::fs::read_dir(dir(state_dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(record) = files::read_json::<Record>(&entry.path()) else {
            continue;
        };
        if record.socket == socket && !live.contains(&record.pane_id) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `report`: outside a Herdr pane it does nothing.
pub async fn report(env: &Env, percent: Option<u8>, activity: &str) -> Result<()> {
    if percent.is_some_and(|p| p > 100) {
        bail!("--percent is at most 100");
    }
    let var = |key| env.var(key).filter(|v| !v.is_empty());
    let (Some("1"), Some(pane), Some(socket)) = (
        var("HERDR_ENV"),
        var("HERDR_PANE_ID"),
        var("HERDR_SOCKET_PATH"),
    ) else {
        return Ok(());
    };
    let client = herdr::Client::new(socket);
    report_in_pane(
        &env.state_dir(),
        &client,
        socket,
        &PaneId(pane.into()),
        percent,
        activity,
        jiff::Timestamp::now().as_second(),
    )
    .await
}

pub async fn report_in_pane<H: Herdr>(
    state_dir: &Path,
    herdr: &H,
    socket: &str,
    caller: &PaneId,
    percent: Option<u8>,
    activity: &str,
    now: i64,
) -> Result<()> {
    let activity = clean(activity, ACTIVITY_COLUMNS);
    let pane = herdr
        .pane_current(caller)
        .await
        .context("Herdr does not know this pane")?;
    save(
        state_dir,
        &Record {
            socket: socket.into(),
            pane_id: pane.id.0.clone(),
            terminal_id: pane.terminal,
            activity: activity.clone(),
            percent,
            reported_at: now,
        },
    )?;
    // The record is what the ticker reads; a token that did not reach Herdr
    // only leaves the pane's label stale.
    let _ = herdr
        .report_metadata(
            &pane.id,
            SOURCE,
            "",
            &[("hla_activity".into(), activity)],
            TOKEN_TTL_MS,
        )
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;

    #[test]
    fn activity_is_cleaned_and_capped_at_forty_columns() {
        assert_eq!(clean("  Reading code\n", 40), "Reading code");
        assert_eq!(clean(&"x".repeat(60), 40).len(), 40);
        assert_eq!(clean("a\u{202e}b\u{7}c", 40), "abc");
        assert_eq!(clean("日本語テキスト", 6), "日本語");
    }

    #[test]
    fn records_round_trip_per_session_and_stale_terminals_are_ignored() {
        let state = tempfile::tempdir().unwrap();
        let record = Record {
            socket: "/a.sock".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "term_1".into(),
            activity: WAITING.into(),
            reported_at: 7,
            ..Record::default()
        };
        save(state.path(), &record).unwrap();
        let other = Record {
            socket: "/b.sock".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "term_9".into(),
            activity: "Testing".into(),
            reported_at: 8,
            ..Record::default()
        };
        save(state.path(), &other).unwrap();
        assert_eq!(load(state.path(), "/a.sock", "w1:p1").unwrap(), record);
        assert!(
            self_report(state.path(), "/a.sock", "w1:p1", "term_1")
                .unwrap()
                .waiting()
        );
        assert!(self_report(state.path(), "/a.sock", "w1:p1", "term_2").is_none());
        assert!(self_report(state.path(), "/a.sock", "w1:p1", "").is_some());
        prune(state.path(), "/a.sock", &["w1:p2".into()]);
        assert!(load(state.path(), "/a.sock", "w1:p1").is_none());
        assert!(load(state.path(), "/b.sock", "w1:p1").is_some());
    }

    #[tokio::test]
    async fn report_writes_the_record_and_a_token_inside_a_pane() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let herdr = FakeHerdr::new(home.path());
        let pane = herdr.add_pane("/wt");
        report_in_pane(&state, &herdr, "/s.sock", &pane, None, WAITING, 1)
            .await
            .unwrap();
        let record = load(&state, "/s.sock", &pane.0).unwrap();
        assert!(record.waiting());
        assert_eq!(record.terminal_id, "term-1");
        let tokens: Vec<_> = herdr
            .metadata()
            .into_iter()
            .filter(|m| m.tokens == [("hla_activity".into(), "Waiting for you".into())])
            .collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].source, SOURCE);

        let inside = Env::for_test(
            home.path(),
            &[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", "/s.sock"),
                ("XDG_STATE_HOME", state.to_str().unwrap()),
            ],
        );
        assert!(report(&inside, Some(101), "x").await.is_err());

        // Outside a pane nothing is written.
        let outside_home = tempfile::tempdir().unwrap();
        let outside = Env::for_test(outside_home.path(), &[]);
        report(&outside, Some(10), "Reading").await.unwrap();
        assert!(!outside.state_dir().exists());
    }
}
