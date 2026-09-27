//! The coordinator's side of a run: the priming files in the run folder, the
//! sheet (`skill`) and the digest it reads every turn (`context`).
//!
//! A coordinator is primed by `AGENTS.md` in its working directory, the run
//! folder. Claude Code reads `CLAUDE.md` (a link to `AGENTS.md`), Codex and
//! Cursor Agent read `AGENTS.md`. Nothing is installed as a skill: the sheet is
//! embedded in the binary, so it always matches the commands it describes.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;

use crate::config::Config;
use crate::files::{self, shell_quote, write_atomic};
use crate::herdr::Snapshot;
use crate::run::{AgentRecord, AgentStatus, Run, RunRecord};
use crate::worker::{self, Group};
use crate::{inbox, names};

/// The subcommands a coordinator may run without a permission prompt. The
/// plugin's own subcommands (`startup`, `action`, `ticker`) are left out.
pub const ALLOWED_SUBCOMMANDS: [&str; 8] = [
    "skill",
    "context",
    "inbox done",
    "plan",
    "say",
    "ask",
    "worker",
    "finish",
];

/// The binary's absolute path, quoted for a shell.
pub fn binary_command() -> Result<String> {
    Ok(shell_quote(&crate::paths::binary()?.to_string_lossy()))
}

pub fn sheet(bin: &str, key: &str) -> String {
    include_str!("../assets/COORDINATOR.md")
        .replace("{bin}", bin)
        .replace("{key}", key)
}

pub fn agents_md(bin: &str, record: &RunRecord) -> String {
    format!(
        "# herdr-linear-agent run {key}\n\n\
         If your working directory is this folder, you are the coordinator of the herdr-linear-agent run for the Linear issue {key} ({title}).\n\n\
         Run `{bin} skill {key}` now and follow the sheet it prints. Then run `{bin} context {key}` at the start of every turn.\n",
        key = record.identifier,
        title = record.title.replace('\n', " "),
    )
}

pub fn settings_local(bin: &str) -> serde_json::Value {
    let allow: Vec<String> = ALLOWED_SUBCOMMANDS
        .iter()
        .map(|sub| format!("Bash({bin} {sub}:*)"))
        .collect();
    serde_json::json!({ "permissions": { "allow": allow } })
}

/// Writes `AGENTS.md`, the `CLAUDE.md` link and the allow-list. Rewritten at
/// every launch, so an updated binary's path is what the coordinator sees.
pub fn write_priming(run: &Run, record: &RunRecord, bin: &str) -> Result<()> {
    write_atomic(
        &run.dir.join("AGENTS.md"),
        agents_md(bin, record).as_bytes(),
    )?;
    let claude = run.dir.join("CLAUDE.md");
    if std::fs::read_link(&claude).ok().as_deref() != Some(Path::new("AGENTS.md")) {
        let _ = std::fs::remove_file(&claude);
        std::os::unix::fs::symlink("AGENTS.md", &claude)?;
    }
    files::write_json(
        &run.dir.join(".claude/settings.local.json"),
        &settings_local(bin),
    )
}

/// The coordinator record before its workspace exists.
pub fn pending_record(record: &RunRecord, profile: &str, kind: &str) -> AgentRecord {
    AgentRecord {
        status: AgentStatus::Pending,
        profile: profile.to_string(),
        kind: kind.to_string(),
        agent_name: names::coordinator(&record.identifier, &record.issue_id),
        ..AgentRecord::default()
    }
}

pub fn workspace_label(record: &RunRecord) -> String {
    let title: String = record
        .title
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    format!("{} {title}", record.identifier)
}

pub fn launch_prompt(key: &str, resume: bool) -> String {
    if resume {
        format!(
            "[herdr-linear-agent ticker] You were restarted as the coordinator of {key}. Run context."
        )
    } else {
        format!("[herdr-linear-agent ticker] Start {key}. Follow AGENTS.md.")
    }
}

pub const NUDGE_REPLY: &str =
    "[herdr-linear-agent ticker] There is a new reply in Linear. Run context.";
pub const NUDGE_INBOX: &str = "[herdr-linear-agent ticker] There are new inbox items. Run context.";

/// A Herdr snapshot and what reading a worker's live state needs besides.
pub struct View<'a> {
    pub snapshot: &'a Snapshot,
    pub state_dir: &'a Path,
    pub socket: &'a str,
    pub now: Timestamp,
}

/// One worker line of the digest.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    pub id: String,
    pub title: String,
    pub repo: String,
    /// `Stopped`, `Starting`, or the group label.
    pub state: &'static str,
    pub pr_url: String,
}

/// The workers' groups, computed live from `view`; without one (Herdr is
/// unreachable) they come from each record's `last_group`.
pub fn worker_rows(run: &Run, view: Option<&View>) -> Vec<WorkerRow> {
    worker::list(run)
        .into_iter()
        .map(|w| {
            let group = match (w.agent.status, view) {
                (AgentStatus::Stopped, _) => None,
                (_, Some(v)) => Some(worker::group(
                    &w,
                    &worker::live_state(&w.agent, v.snapshot, v.now, v.state_dir, v.socket),
                )),
                (AgentStatus::Failed, None) => Some(Group::WaitingOnYou),
                (_, None) => Group::from_token(&w.agent.last_group),
            };
            let state = match (w.agent.status, group) {
                (AgentStatus::Stopped, _) => "Stopped",
                (_, Some(group)) => group.label(),
                (_, None) => "Starting",
            };
            WorkerRow {
                id: w.id,
                title: w.title,
                repo: w.repo,
                state,
                pr_url: w.pr_url,
            }
        })
        .collect()
}

fn read_or(path: &Path, missing: &str) -> String {
    std::fs::read_to_string(path)
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|_| missing.to_string())
}

/// What the coordinator reads at the start of every turn, and the inbox ids
/// it shows.
pub fn digest(
    run: &Run,
    config: &Config,
    bin: &str,
    rows: &[WorkerRow],
) -> Result<(String, Vec<String>)> {
    let record = run.record()?;
    let mut text = format!(
        "# Run {} ({:?}{})\n\n## Issue\n\n{}\n\n## Conversation\n\n{}\n\n## Repositories\n\n",
        run.key,
        record.status,
        if record.finished { ", finished" } else { "" },
        read_or(&run.issue_md(), "(issue.md is not written yet)"),
        read_or(&run.conversation_md(), "(no replies yet)"),
    );
    if config.repositories.is_empty() {
        text.push_str("(none)\n");
    }
    for (name, repo) in &config.repositories {
        let _ = write!(
            text,
            "- {name}: {} (base {})",
            repo.path.display(),
            repo.base
        );
        if !repo.description.is_empty() {
            let _ = write!(text, " \u{2014} {}", repo.description);
        }
        text.push('\n');
    }
    text.push_str("\n## Worker profiles\n\n");
    for name in &config.routing.workers {
        let Ok(profile) = config.profile(name) else {
            continue;
        };
        let _ = write!(text, "- {name}: {}", profile.kind);
        if let Some(model) = &profile.model {
            let _ = write!(text, " {model}");
        }
        if let Some(effort) = &profile.effort {
            let _ = write!(text, ", effort {effort}");
        }
        if !profile.description.is_empty() {
            let _ = write!(text, " \u{2014} {}", profile.description);
        }
        text.push('\n');
    }
    text.push_str("\n## Workers\n\n");
    if rows.is_empty() {
        let _ = writeln!(
            text,
            "(none yet; start one with `{bin} worker start {} --repo <name> ...`)",
            run.key
        );
    }
    for row in rows {
        let _ = write!(
            text,
            "- {} [{}] {} (repo {})",
            row.id, row.state, row.title, row.repo
        );
        if !row.pr_url.is_empty() {
            let _ = write!(text, ", PR {}", row.pr_url);
        }
        text.push('\n');
    }
    text.push_str("\n## Inbox\n\n");
    let items = inbox::unhandled(run);
    if items.is_empty() {
        text.push_str("(empty)\n");
    }
    for item in &items {
        let _ = writeln!(
            text,
            "- {} [{}] {}: {}",
            item.id, item.kind, item.subject, item.summary
        );
    }
    if !items.is_empty() {
        let _ = writeln!(
            text,
            "\nWhen you have handled them, run `{bin} inbox done {} --all` (or name the ids).",
            run.key
        );
    }
    Ok((text, items.into_iter().map(|i| i.id).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::SAMPLE;

    #[test]
    fn priming_names_the_binary_and_the_allow_list_leaves_out_plugin_commands() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            identifier: "DATA-1".into(),
            title: "Fix\nlogin".into(),
            ..RunRecord::default()
        };
        let run = Run::create(dir.path(), record.clone()).unwrap();
        write_priming(&run, &record, "/bin/hla").unwrap();
        write_priming(&run, &record, "/bin/hla").unwrap();
        let agents = std::fs::read_to_string(run.dir.join("AGENTS.md")).unwrap();
        assert!(agents.contains("Run `/bin/hla skill DATA-1`"));
        assert!(agents.contains("(Fix login)"));
        assert_eq!(
            std::fs::read_link(run.dir.join("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        let settings: serde_json::Value =
            files::read_json(&run.dir.join(".claude/settings.local.json")).unwrap();
        let allow: Vec<&str> = settings["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(allow.contains(&"Bash(/bin/hla context:*)"));
        assert!(allow.contains(&"Bash(/bin/hla inbox done:*)"));
        assert!(
            !allow
                .iter()
                .any(|a| a.contains("ticker") || a.contains("action") || a.contains("startup"))
        );
        assert!(sheet("/bin/hla", "DATA-1").contains("`/bin/hla worker start DATA-1 --repo"));
    }

    #[test]
    fn the_digest_shows_the_catalog_profiles_workers_and_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let record = RunRecord {
            identifier: "DATA-1".into(),
            title: "First".into(),
            url: "https://linear.app/x".into(),
            ..RunRecord::default()
        };
        let run = Run::create(dir.path(), record).unwrap();
        std::fs::write(run.issue_md(), "# DATA-1 First\n\nThe body.").unwrap();
        run.append_conversation("2026-09-25T00:00:00Z", "user-1", "Use the api repo.")
            .unwrap();
        worker::allocate(
            &run,
            |_| Ok(()),
            |w| {
                w.title = "API change".into();
                w.repo = "api".into();
                w.agent.status = AgentStatus::Open;
                w.agent.last_group = "reported".into();
            },
        )
        .unwrap();
        let id = inbox::write(&run, "reply", "reply", "A new reply is in conversation.md").unwrap();
        let config = Config::parse(SAMPLE).unwrap();
        let rows = worker_rows(&run, None);
        let (text, shown) = digest(&run, &config, "/bin/hla", &rows).unwrap();
        for needle in [
            "The body.",
            "Use the api repo.",
            "- api: /src/api (base main) \u{2014} The API server",
            "- deep: codex gpt-6-sol, effort xhigh",
            "- w1 [Reported] API change",
            "[reply] reply:",
        ] {
            assert!(text.contains(needle), "missing {needle}:\n{text}");
        }
        assert!(
            !text.contains("coordinator-light"),
            "only worker profiles are offered"
        );
        assert_eq!(shown, [id]);
    }

    #[test]
    fn rows_are_live_with_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        worker::allocate(
            &run,
            |_| Ok(()),
            |w| {
                w.agent.status = AgentStatus::Open;
                w.agent.pane_id = "w2:p1".into();
                w.agent.last_group = "working".into();
            },
        )
        .unwrap();
        let snapshot = Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            panes: Default::default(),
            agents: Vec::new(),
            skipped: 0,
        };
        let view = View {
            snapshot: &snapshot,
            state_dir: dir.path(),
            socket: "/s",
            now: Timestamp::now(),
        };
        assert_eq!(worker_rows(&run, None)[0].state, "Working");
        assert_eq!(
            worker_rows(&run, Some(&view))[0].state,
            "Waiting on you",
            "its pane is gone and it wrote no report"
        );
    }
}
