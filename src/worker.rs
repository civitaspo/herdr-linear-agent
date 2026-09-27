//! Workers: one agent per repository in its own worktree. This module holds
//! the worker record, the names and the brief, and the rules that read a
//! worker's state from a Herdr snapshot. Nothing here talks to Herdr.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::files::{self, write_atomic};
use crate::herdr::{Agent, AgentStatus as HerdrStatus, Snapshot};
use crate::linear::api::{Activity, Content};
use crate::outbox::Op;
use crate::progress;
use crate::run::{AgentRecord, AgentStatus, Run, Status};

/// Workers write their brief and report under this folder of the worktree.
pub const BRIEF_FOLDER: &str = ".herdr-linear-agent";
pub const MAX_RESTARTS: u32 = 2;
/// A blocked agent needs a person after this long.
pub const BLOCKED_SECS: i64 = 30;
/// A launch whose status stays unknown this long is stuck on a dialog.
pub const LAUNCH_DIALOG_SECS: i64 = 60;
/// A `Waiting for you` self-report counts for this long.
pub const SELF_REPORT_SECS: i64 = 5 * 60;

/// `workers/<id>.toml`. Every field is defaulted, so records written by an
/// older build still load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Worker {
    pub id: String,
    pub title: String,
    pub repo: String,
    pub repo_path: String,
    pub branch: String,
    pub base: String,
    pub worktree_path: String,
    pub brief_dir: String,
    pub restarts: u32,
    /// The hash of the report the ticker last copied home.
    pub report_hash: String,
    /// The hash of the report the coordinator was told about.
    pub announced_report_hash: String,
    pub pr_url: String,
    /// The "lost its pane before a report" error was sent.
    pub gone_reported: bool,
    pub created: String,
    pub updated: String,
    pub agent: AgentRecord,
}

impl Worker {
    /// Whether the worker counts against `max_agents`.
    pub fn counts(&self) -> bool {
        matches!(self.agent.status, AgentStatus::Pending | AgentStatus::Open)
    }
}

/// `w` and a positive number without a leading zero.
pub fn validate_id(id: &str) -> Result<()> {
    let ok = id.strip_prefix('w').is_some_and(|n| {
        !n.is_empty() && !n.starts_with('0') && n.chars().all(|c| c.is_ascii_digit())
    });
    if !ok {
        bail!("`{id}` is not a worker id (expected w1, w2, ...)");
    }
    Ok(())
}

fn record_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.toml"))
}

pub fn task_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.task.md"))
}

pub fn home_report_path(run: &Run, id: &str) -> PathBuf {
    run.workers_dir().join(format!("{id}.md"))
}

fn read(path: &Path) -> Result<Worker> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} does not parse", path.display()))
}

fn write(run: &Run, worker: &Worker) -> Result<()> {
    let text = toml::to_string(worker).context("a worker record does not serialize")?;
    write_atomic(&record_path(run, &worker.id), text.as_bytes())
}

pub fn load(run: &Run, id: &str) -> Result<Worker> {
    validate_id(id)?;
    let path = record_path(run, id);
    if !path.is_file() {
        bail!("run {} has no worker {id}", run.key);
    }
    read(&path)
}

/// Every worker of the run, by id number.
pub fn list(run: &Run) -> Vec<Worker> {
    let Ok(entries) = std::fs::read_dir(run.workers_dir()) else {
        return Vec::new();
    };
    let mut workers: Vec<Worker> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".toml")?;
            validate_id(id).ok()?;
            read(&e.path()).ok()
        })
        .collect();
    workers.sort_by_key(|w| number(&w.id));
    workers
}

fn number(id: &str) -> u64 {
    id.trim_start_matches('w').parse().unwrap_or(0)
}

/// Gives the next id under the run lock. `check` sees every existing worker
/// and may refuse; `init` fills the new record before it is written.
pub fn allocate(
    run: &Run,
    check: impl FnOnce(&[Worker]) -> Result<()>,
    init: impl FnOnce(&mut Worker),
) -> Result<Worker> {
    let _lock = run.lock()?;
    let workers = list(run);
    check(&workers)?;
    let next = workers.iter().map(|w| number(&w.id)).max().unwrap_or(0) + 1;
    let now = files::now();
    let mut worker = Worker {
        id: format!("w{next}"),
        created: now.clone(),
        updated: now,
        ..Worker::default()
    };
    init(&mut worker);
    write(run, &worker)?;
    Ok(worker)
}

/// Read-modify-write of one record under the run lock; `created` is kept.
pub fn update(run: &Run, id: &str, change: impl FnOnce(&mut Worker)) -> Result<Worker> {
    let _lock = run.lock()?;
    let mut worker = load(run, id)?;
    let created = worker.created.clone();
    change(&mut worker);
    worker.id = id.to_string();
    worker.created = created;
    worker.updated = files::now();
    write(run, &worker)?;
    Ok(worker)
}

pub fn branch_name(key: &str, id: &str, title: &str) -> String {
    let slug = files::slugify(title);
    let key = key.to_lowercase();
    if slug.is_empty() {
        format!("herdr-linear-agent/{key}/{id}")
    } else {
        format!("herdr-linear-agent/{key}/{id}-{slug}")
    }
}

pub fn brief_dir(worktree: &str, key: &str, id: &str) -> String {
    format!(
        "{}/{BRIEF_FOLDER}/{key}-{id}",
        worktree.trim_end_matches('/')
    )
}

pub fn launch_prompt(key: &str, id: &str) -> String {
    format!("Read {BRIEF_FOLDER}/{key}-{id}/brief.md and do what it says.")
}

/// The `Start worker` action, queued when the worker's launch prompt is
/// delivered.
pub fn start_action(worker: &Worker) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Action {
            action: "Start worker".into(),
            parameter: format!("{} {}: {}", worker.id, worker.repo, worker.title),
            result: None,
        }),
    }
}

/// Adds the text as a follow-up to the worker's task file.
pub fn append_follow_up(run: &Run, id: &str, text: &str) -> Result<()> {
    let _lock = run.lock()?;
    let path = task_path(run, id);
    let mut task = std::fs::read_to_string(&path).unwrap_or_default();
    if !task.contains("\n## Follow-ups\n") {
        task = format!("{}\n\n## Follow-ups\n", task.trim_end());
    }
    task.push_str(&format!("\n### {}\n\n{}\n", files::now(), text.trim()));
    write_atomic(&path, task.as_bytes())
}

fn report_path(worker: &Worker) -> PathBuf {
    Path::new(&worker.brief_dir).join("report.md")
}

/// The report's bytes, never through a symbolic link.
fn read_report(worker: &Worker) -> Option<Vec<u8>> {
    let path = report_path(worker);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    std::fs::read(&path).ok()
}

pub fn report_hash(worker: &Worker) -> Option<String> {
    read_report(worker).map(|bytes| files::sha256_hex(&bytes))
}

/// Copies the worker's report to `workers/<id>.md` and returns its text;
/// `None` when there is no report.
pub fn copy_report_home(run: &Run, worker: &Worker) -> Result<Option<String>> {
    let Some(bytes) = read_report(worker) else {
        return Ok(None);
    };
    write_atomic(&home_report_path(run, &worker.id), &bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The pull request of the report's first `PR:` line, when it names one.
pub fn pr_line(report: &str) -> Option<String> {
    let url = report
        .lines()
        .find_map(|l| l.trim().strip_prefix("PR:"))?
        .trim();
    let rest = url.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [owner, repo, "pull", number] = parts.as_slice() else {
        return None;
    };
    let name = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let valid = name(owner)
        && name(repo)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    valid.then(|| url.to_string())
}

pub struct BriefInput<'a> {
    pub issue_key: &'a str,
    pub issue_title: &'a str,
    pub issue_url: &'a str,
    pub worker: &'a Worker,
    pub task: &'a str,
    pub restart: bool,
    pub binary: &'a str,
}

pub fn compose_brief(input: &BriefInput) -> String {
    let w = input.worker;
    let mut text = format!(
        "# Worker brief\n\n\
         - Issue: {key} {title}\n\
         - URL: {url}\n\
         - Repository: {repo}\n\
         - Worktree: {worktree}\n\
         - Branch: {branch}\n\
         - Base: {base}\n\
         - Report: {report}\n\n",
        key = input.issue_key,
        title = input.issue_title.replace('\n', " "),
        url = input.issue_url,
        repo = w.repo,
        worktree = w.worktree_path,
        branch = w.branch,
        base = w.base,
        report = report_path(w).display(),
    );
    if input.restart {
        text.push_str(
            "This is a restart. A previous attempt already worked in this worktree: read the current state of the worktree and of the report before you go on.\n\n",
        );
    }
    text.push_str(include_str!("../assets/WORKER.md").trim_end());
    text.push_str(&format!(
        "\n\n## Progress\n\n\
         Tell the plugin how far you are whenever your activity changes:\n\n\
         ```sh\n{bin} report --percent N --activity 'Two to four words'\n```\n\n\
         Use `--unknown` instead of `--percent N` while the scope is unclear, and `--activity '{waiting}'` when you stop with a question.\n\n\
         ## Task\n\n{task}\n",
        bin = input.binary,
        waiting = progress::WAITING,
        task = input.task.trim(),
    ));
    text
}

/// Coordinators and workers of active runs that count against `max_agents`.
pub fn agent_count(runs: &[Run]) -> usize {
    runs.iter()
        .filter_map(|run| run.record().ok().map(|r| (run, r)))
        .filter(|(_, r)| r.status == Status::Active)
        .map(|(run, r)| {
            usize::from(matches!(
                r.coordinator.status,
                AgentStatus::Pending | AgentStatus::Open
            )) + list(run).iter().filter(|w| w.counts()).count()
        })
        .sum()
}

/// Herdr's agent in the recorded pane, when it is the recorded agent: pane,
/// working directory and kind agree, and the name does too or is empty (a
/// natively resumed agent loses its name).
pub fn find_agent<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    agents.iter().find(|a| {
        a.pane.0 == record.pane_id
            && same_place(record, a)
            && a.name
                .as_deref()
                .is_none_or(|n| n.is_empty() || n == record.agent_name)
    })
}

fn same_place(record: &AgentRecord, agent: &Agent) -> bool {
    agent.cwd.as_deref() == Some(record.cwd.as_str())
        && agent.kind.as_deref() == Some(record.kind.as_str())
}

/// The recorded agent in another pane: Herdr renumbered it.
fn find_moved<'a>(record: &AgentRecord, agents: &'a [Agent]) -> Option<&'a Agent> {
    if record.agent_name.is_empty() {
        return None;
    }
    agents.iter().find(|a| {
        a.pane.0 != record.pane_id
            && same_place(record, a)
            && a.name.as_deref() == Some(record.agent_name.as_str())
    })
}

/// What a snapshot shows of one recorded agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    /// Our agent is in its pane or moved, or its pane waits at a shell prompt.
    pub pane_exists: bool,
    /// `(workspace, tab, pane)` when Herdr moved our agent to another pane.
    pub moved_to: Option<(String, String, String)>,
    /// The agent found by the identity rule.
    pub agent: Option<Agent>,
    pub agent_state: Option<HerdrStatus>,
    /// Seconds in the current status, when it is the recorded one.
    pub state_secs: i64,
    /// The worker's own progress record, when it is recent.
    pub self_report: Option<progress::Record>,
}

pub fn live_state(
    record: &AgentRecord,
    snapshot: &Snapshot,
    now: Timestamp,
    state_dir: &Path,
    socket: &str,
) -> Live {
    let mut live = Live::default();
    let pane = if let Some(agent) = find_agent(record, &snapshot.agents) {
        live.agent = Some(agent.clone());
        Some(agent.pane.clone())
    } else if let Some(agent) = find_moved(record, &snapshot.agents) {
        live.agent = Some(agent.clone());
        if let Some(pane) = snapshot.panes.get(&agent.pane) {
            live.moved_to = Some((
                pane.workspace.0.clone(),
                pane.tab.clone(),
                pane.id.0.clone(),
            ));
        }
        Some(agent.pane.clone())
    } else {
        let id = crate::herdr::PaneId(record.pane_id.clone());
        let empty = !snapshot.agents.iter().any(|a| a.pane == id);
        (empty && snapshot.panes.contains_key(&id)).then_some(id)
    };
    let Some(pane) = pane else {
        return live;
    };
    live.pane_exists = true;
    live.agent_state = live.agent.as_ref().map(|a| a.status);
    if let Some(status) = live.agent_state
        && status.as_str() == record.last_state
    {
        live.state_secs = files::seconds_since(&record.last_state_change, now);
    }
    let terminal = snapshot
        .panes
        .get(&pane)
        .map_or("", |p| p.terminal.as_str());
    live.self_report = progress::self_report(state_dir, socket, &pane.0, terminal)
        .filter(|r| now.as_second() - r.reported_at < SELF_REPORT_SECS);
    live
}

/// Blocked for 30 s, or a launch stuck on a dialog for 60 s.
pub fn needs_person(record: &AgentRecord, live: &Live) -> bool {
    match live.agent_state {
        Some(HerdrStatus::Blocked) => live.state_secs >= BLOCKED_SECS,
        Some(HerdrStatus::Unknown) => {
            record.prompt_pending && live.state_secs >= LAUNCH_DIALOG_SECS
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    WaitingOnYou,
    Working,
    Idle,
    Reported,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "Waiting on you",
            Group::Working => "Working",
            Group::Idle => "Idle",
            Group::Reported => "Reported",
        }
    }

    /// The value stored in `last_group`.
    pub fn token(self) -> &'static str {
        match self {
            Group::WaitingOnYou => "waiting_on_you",
            Group::Working => "working",
            Group::Idle => "idle",
            Group::Reported => "reported",
        }
    }

    /// An unknown stored value reads as none, so the next group is a change.
    pub fn from_token(token: &str) -> Option<Group> {
        [
            Group::WaitingOnYou,
            Group::Working,
            Group::Idle,
            Group::Reported,
        ]
        .into_iter()
        .find(|g| g.token() == token)
    }
}

/// The first row that holds, in order.
pub fn group(worker: &Worker, live: &Live) -> Group {
    let reported = !worker.report_hash.is_empty();
    let asked = live
        .self_report
        .as_ref()
        .is_some_and(progress::Record::waiting)
        && live.agent_state != Some(HerdrStatus::Working);
    if worker.agent.status == AgentStatus::Failed {
        Group::WaitingOnYou
    } else if !live.pane_exists {
        if reported {
            Group::Reported
        } else {
            Group::WaitingOnYou
        }
    } else if needs_person(&worker.agent, live) || asked {
        Group::WaitingOnYou
    } else if !live.agent_state.is_some_and(HerdrStatus::is_idle) {
        Group::Working
    } else if reported {
        Group::Reported
    } else {
        Group::Idle
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::herdr::{Pane, PaneId, WorkspaceId};
    use crate::run::RunRecord;

    fn now() -> Timestamp {
        "2026-09-25T12:00:00Z".parse().unwrap()
    }

    fn ago(secs: i64) -> String {
        (now() - jiff::SignedDuration::from_secs(secs)).to_string()
    }

    fn open_worker() -> Worker {
        Worker {
            id: "w1".into(),
            agent: AgentRecord {
                status: AgentStatus::Open,
                kind: "claude".into(),
                agent_name: "data-1-w1".into(),
                pane_id: "w2:p1".into(),
                cwd: "/wt".into(),
                last_state: "idle".into(),
                last_state_change: ago(45),
                ..AgentRecord::default()
            },
            ..Worker::default()
        }
    }

    fn live(state: Option<&str>, secs: i64) -> Live {
        Live {
            pane_exists: true,
            agent_state: state.map(HerdrStatus::parse),
            state_secs: secs,
            ..Live::default()
        }
    }

    #[test]
    fn groups_follow_the_rows_in_order() {
        let w = open_worker();
        let failed = Worker {
            agent: AgentRecord {
                status: AgentStatus::Failed,
                ..w.agent.clone()
            },
            ..w.clone()
        };
        assert_eq!(
            group(&failed, &live(Some("working"), 0)),
            Group::WaitingOnYou
        );
        assert_eq!(
            group(&w, &Live::default()),
            Group::WaitingOnYou,
            "pane gone without a report"
        );
        assert_eq!(group(&w, &live(Some("blocked"), 30)), Group::WaitingOnYou);
        assert_eq!(
            group(&w, &live(Some("blocked"), 29)),
            Group::Working,
            "a quickly answered prompt never shows"
        );
        let pending = Worker {
            agent: AgentRecord {
                prompt_pending: true,
                ..w.agent.clone()
            },
            ..w.clone()
        };
        assert_eq!(
            group(&pending, &live(Some("unknown"), 60)),
            Group::WaitingOnYou,
            "a launch stuck on a dialog"
        );
        assert_eq!(group(&pending, &live(None, 0)), Group::Working);

        let reported = Worker {
            report_hash: "h".into(),
            ..w.clone()
        };
        assert_eq!(group(&reported, &live(Some("idle"), 0)), Group::Reported);
        assert_eq!(group(&reported, &live(Some("done"), 0)), Group::Reported);
        assert_eq!(group(&reported, &live(Some("working"), 0)), Group::Working);
        assert_eq!(
            group(&reported, &Live::default()),
            Group::Reported,
            "a closed pane after the report is finished work"
        );
        assert_eq!(group(&w, &live(Some("idle"), 0)), Group::Idle);

        let waiting = progress::Record {
            activity: progress::WAITING.into(),
            reported_at: 1,
            ..Default::default()
        };
        let asked = Live {
            self_report: Some(waiting.clone()),
            ..live(Some("idle"), 0)
        };
        assert_eq!(group(&reported, &asked), Group::WaitingOnYou);
        let still_working = Live {
            self_report: Some(waiting),
            ..live(Some("working"), 0)
        };
        assert_eq!(group(&reported, &still_working), Group::Working);

        for g in [
            Group::WaitingOnYou,
            Group::Working,
            Group::Idle,
            Group::Reported,
        ] {
            assert_eq!(Group::from_token(g.token()), Some(g));
        }
        assert_eq!(Group::from_token("waiting"), None);
    }

    fn agent(pane: &str, name: &str, cwd: &str, kind: &str) -> Agent {
        Agent {
            pane: PaneId(pane.into()),
            kind: Some(kind.into()),
            name: Some(name.into()),
            status: HerdrStatus::Idle,
            session: None,
            cwd: Some(cwd.into()),
            foreground_cwd: None,
            terminal: format!("term-{pane}"),
            interactive_ready: true,
            launch_pending: false,
            state_change_seq: 1,
            state_labels: BTreeMap::new(),
        }
    }

    fn snapshot(agents: Vec<Agent>) -> Snapshot {
        let panes = agents
            .iter()
            .map(|a| {
                let workspace = a.pane.0.split(':').next().unwrap().to_string();
                let pane = Pane {
                    id: a.pane.clone(),
                    tab: format!("{workspace}:t1"),
                    workspace: WorkspaceId(workspace),
                    terminal: a.terminal.clone(),
                    cwd: a.cwd.clone(),
                    foreground_cwd: None,
                    label: None,
                };
                (a.pane.clone(), pane)
            })
            .collect();
        Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            panes,
            agents,
            skipped: 0,
        }
    }

    #[test]
    fn identity_is_pane_cwd_kind_and_name() {
        let w = open_worker();
        let rec = &w.agent;
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/wt", "claude")]).is_some());
        assert!(
            find_agent(rec, &[agent("w2:p1", "", "/wt", "claude")]).is_some(),
            "natively resumed, unnamed"
        );
        assert!(find_agent(rec, &[agent("w2:p1", "other", "/wt", "claude")]).is_none());
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/other", "claude")]).is_none());
        assert!(find_agent(rec, &[agent("w2:p1", "data-1-w1", "/wt", "codex")]).is_none());
        let state = Path::new("/nonexistent");
        // Renumbered after a restart: found by name, cwd and kind.
        let moved = live_state(
            rec,
            &snapshot(vec![agent("w9:p3", "data-1-w1", "/wt", "claude")]),
            now(),
            state,
            "/s",
        );
        assert!(moved.pane_exists);
        assert_eq!(moved.moved_to.unwrap().2, "w9:p3");
        // Someone else's agent in our pane: treated as gone.
        let foreign = live_state(
            rec,
            &snapshot(vec![agent("w2:p1", "other", "/wt", "claude")]),
            now(),
            state,
            "/s",
        );
        assert!(!foreign.pane_exists);
        let ours = live_state(
            rec,
            &snapshot(vec![agent("w2:p1", "data-1-w1", "/wt", "claude")]),
            now(),
            state,
            "/s",
        );
        assert_eq!(ours.state_secs, 45);
        assert_eq!(ours.agent_state, Some(HerdrStatus::Idle));
        // The placed pane at its shell prompt, before the agent starts.
        let mut shell = snapshot(vec![agent("w2:p1", "data-1-w1", "/wt", "claude")]);
        shell.agents.clear();
        let waiting = live_state(rec, &shell, now(), state, "/s");
        assert!(waiting.pane_exists && waiting.agent_state.is_none());
    }

    #[test]
    fn a_self_report_counts_only_while_it_is_recent() {
        let state = tempfile::tempdir().unwrap();
        let w = open_worker();
        let record = progress::Record {
            socket: "/s".into(),
            pane_id: "w2:p1".into(),
            terminal_id: "term-w2:p1".into(),
            activity: progress::WAITING.into(),
            reported_at: now().as_second() - 60,
            ..progress::Record::default()
        };
        progress::save(state.path(), &record).unwrap();
        let view = snapshot(vec![agent("w2:p1", "data-1-w1", "/wt", "claude")]);
        let fresh = live_state(&w.agent, &view, now(), state.path(), "/s");
        assert_eq!(fresh.self_report, Some(record));
        let later = now() + jiff::SignedDuration::from_secs(SELF_REPORT_SECS);
        assert_eq!(
            live_state(&w.agent, &view, later, state.path(), "/s").self_report,
            None
        );
    }

    #[test]
    fn ids_branches_briefs_and_pr_lines() {
        assert!(validate_id("w1").is_ok() && validate_id("w12").is_ok());
        for bad in ["", "w", "w0", "w01", "x1", "../w1", "w1a"] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
        assert_eq!(
            branch_name("DATA-12", "w1", "Fix the $(login) bug!"),
            "herdr-linear-agent/data-12/w1-fix-the-login-bug"
        );
        assert_eq!(
            branch_name("DATA-12", "w2", "???"),
            "herdr-linear-agent/data-12/w2"
        );
        assert_eq!(
            brief_dir("/wt/", "DATA-12", "w1"),
            "/wt/.herdr-linear-agent/DATA-12-w1"
        );
        assert_eq!(
            launch_prompt("DATA-12", "w1"),
            "Read .herdr-linear-agent/DATA-12-w1/brief.md and do what it says."
        );

        assert_eq!(
            pr_line("PR: https://github.com/o/r/pull/12\n## Report"),
            Some("https://github.com/o/r/pull/12".into())
        );
        for bad in [
            "## Report",
            "PR: https://evil.example/o/r/pull/1",
            "PR: https://github.com/o/r/issues/1",
            "PR: https://github.com/o/r/pull/1x",
        ] {
            assert_eq!(pr_line(bad), None, "{bad}");
        }

        let w = Worker {
            id: "w1".into(),
            repo: "api".into(),
            branch: "b".into(),
            base: "main".into(),
            worktree_path: "/wt".into(),
            brief_dir: "/wt/.herdr-linear-agent/DATA-12-w1".into(),
            ..Worker::default()
        };
        let brief = compose_brief(&BriefInput {
            issue_key: "DATA-12",
            issue_title: "Title",
            issue_url: "https://linear.app/x",
            worker: &w,
            task: "Do it.",
            restart: true,
            binary: "/bin/hla",
        });
        let pos = |needle: &str| {
            brief
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(pos("# Worker brief") < pos("previous attempt"));
        assert!(pos("previous attempt") < pos("/bin/hla report --percent N"));
        assert!(pos("/bin/hla report") < pos("Do it."));
        assert!(brief.contains("/wt/.herdr-linear-agent/DATA-12-w1/report.md"));
    }

    #[test]
    fn allocation_follow_ups_and_report_copies() {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let a = allocate(&run, |_| Ok(()), |w| w.repo = "api".into()).unwrap();
        let b = allocate(&run, |_| Ok(()), |_| {}).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("w1", "w2"));
        assert!(
            allocate(
                &run,
                |workers| if workers.len() >= 2 {
                    bail!("full")
                } else {
                    Ok(())
                },
                |_| {}
            )
            .is_err()
        );
        update(&run, "w1", |w| w.title = "T".into()).unwrap();
        assert_eq!(load(&run, "w1").unwrap().repo, "api");

        std::fs::write(task_path(&run, "w1"), "The task.").unwrap();
        append_follow_up(&run, "w1", "Also do Y.\n").unwrap();
        append_follow_up(&run, "w1", "And Z.").unwrap();
        let text = std::fs::read_to_string(task_path(&run, "w1")).unwrap();
        assert!(
            text.starts_with("The task.\n\n## Follow-ups\n\n### 20"),
            "{text}"
        );
        assert_eq!(text.matches("## Follow-ups").count(), 1);

        let work = tempfile::tempdir().unwrap();
        let brief = work.path().join(".herdr-linear-agent/DATA-1-w1");
        std::fs::create_dir_all(&brief).unwrap();
        let w = Worker {
            brief_dir: brief.to_string_lossy().into_owned(),
            ..load(&run, "w1").unwrap()
        };
        assert_eq!(copy_report_home(&run, &w).unwrap(), None);
        std::fs::write(
            brief.join("report.md"),
            "PR: https://github.com/o/r/pull/1\n",
        )
        .unwrap();
        assert!(copy_report_home(&run, &w).unwrap().is_some());
        assert!(home_report_path(&run, "w1").is_file());
        // A symbolic link is never read.
        std::fs::remove_file(brief.join("report.md")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", brief.join("report.md")).unwrap();
        assert_eq!(report_hash(&w), None);
    }

    #[test]
    fn a_record_of_an_older_build_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        std::fs::write(
            run.workers_dir().join("w1.toml"),
            "id = \"w1\"\nrepo = \"api\"\n\n[agent]\nstatus = \"open\"\nlast_group = \"working\"\n",
        )
        .unwrap();
        let w = load(&run, "w1").unwrap();
        assert_eq!(
            (w.repo.as_str(), w.agent.status),
            ("api", AgentStatus::Open)
        );
        assert_eq!(list(&run).len(), 1);
    }
}
