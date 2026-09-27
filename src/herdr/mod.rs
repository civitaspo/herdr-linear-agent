//! Herdr over its unix socket: a request client and a task that mirrors the
//! session's panes and agents.

mod client;
#[cfg(test)]
mod fake;
mod watch;

pub use client::{Client, Subscription, session_socket};
pub use watch::watch;

use std::collections::BTreeMap;
use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The oldest Herdr this client was written against (protocol 22).
pub const MIN_VERSION: &str = "0.9.1";

/// Whether the first `X.Y.Z` of `version` is at least `minimum`'s, so
/// `0.9.2-preview.3` counts as 0.9.2. An unreadable version is not.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    fn triple(text: &str) -> Option<(u64, u64, u64)> {
        let end = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(text.len());
        let mut parts = text[..end].split('.').map(|p| p.parse::<u64>().ok());
        Some((parts.next()??, parts.next()??, parts.next()??))
    }
    match (triple(version), triple(minimum)) {
        (Some(version), Some(minimum)) => version >= minimum,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn parse(text: &str) -> Self {
        match text {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// Herdr's `agent`, for example `claude`.
    pub kind: String,
    pub name: Option<String>,
    pub status: AgentStatus,
    pub status_since: Timestamp,
    /// `agent_session.value`.
    pub session: Option<String>,
    /// Herdr's `state_labels` map as `status=label`, in key order.
    pub state_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub workspace: WorkspaceId,
    pub tab: String,
    pub cwd: String,
    pub foreground_cwd: Option<String>,
    pub agent: Option<Agent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrView {
    pub connected: bool,
    /// When `connected` last changed.
    pub since: Timestamp,
    pub version: Option<String>,
    pub protocol: Option<u32>,
    pub panes: BTreeMap<PaneId, Pane>,
}

/// A pushed event the mirror cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PaneCreated(Pane),
    PaneUpdated(Pane),
    /// A move can give the pane a new id.
    PaneMoved {
        previous: PaneId,
        pane: Pane,
    },
    PaneClosed(PaneId),
    PaneExited(PaneId),
    AgentDetected {
        pane: PaneId,
        agent: Option<String>,
        released: bool,
    },
    WorkspaceClosed(WorkspaceId),
    AgentStatusChanged {
        pane: PaneId,
        agent: Option<String>,
        status: AgentStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrError {
    /// The socket is missing, refuses connections, or the connection ended.
    Unreachable,
    /// A line that is not JSON or not of the expected shape.
    Protocol(String),
    /// Herdr answered with an error.
    Api {
        code: String,
        message: String,
    },
    Timeout,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable => f.write_str("the Herdr socket is unreachable"),
            Self::Protocol(detail) => write!(f, "unexpected answer from Herdr: {detail}"),
            Self::Api { code, message } => {
                write!(f, "Herdr refused the request ({code}): {message}")
            }
            Self::Timeout => f.write_str("Herdr did not answer in time"),
        }
    }
}

impl std::error::Error for HerdrError {}

/// The fields of Herdr's `PaneInfo` and `AgentInfo` this crate reads.
#[derive(Debug, Deserialize)]
struct RawPane {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    agent: Option<String>,
    agent_status: Option<String>,
    agent_session: Option<RawAgentSession>,
    #[serde(default)]
    state_labels: BTreeMap<String, String>,
    /// Only `AgentInfo` has a name.
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAgentSession {
    value: String,
}

impl RawPane {
    fn agent(&self, now: Timestamp) -> Option<Agent> {
        Some(Agent {
            kind: self.agent.clone()?,
            name: self.name.clone(),
            status: AgentStatus::parse(self.agent_status.as_deref().unwrap_or_default()),
            status_since: now,
            session: self.agent_session.as_ref().map(|s| s.value.clone()),
            state_labels: self
                .state_labels
                .iter()
                .map(|(status, label)| format!("{status}={label}"))
                .collect(),
        })
    }

    fn into_pane(self, agent: Option<Agent>) -> Pane {
        Pane {
            id: PaneId(self.pane_id),
            workspace: WorkspaceId(self.workspace_id),
            tab: self.tab_id,
            cwd: self.cwd.unwrap_or_default(),
            foreground_cwd: self.foreground_cwd,
            agent,
        }
    }

    fn pane(self, now: Timestamp) -> Pane {
        let agent = self.agent(now);
        self.into_pane(agent)
    }
}

fn field<T: serde::de::DeserializeOwned>(data: &Value, name: &str) -> Result<T, HerdrError> {
    serde_json::from_value(data.get(name).cloned().unwrap_or(Value::Null))
        .map_err(|e| HerdrError::Protocol(format!("`{name}`: {e}")))
}

/// Parses one pushed `{"event", "data"}` line; `None` for events the mirror
/// ignores.
fn parse_event(line: &Value, now: Timestamp) -> Result<Option<Event>, HerdrError> {
    let Some(name) = line.get("event").and_then(Value::as_str) else {
        return Ok(None);
    };
    let data = line.get("data").unwrap_or(&Value::Null);
    let pane = |key: &str| field::<RawPane>(data, key).map(|p| p.pane(now));
    let pane_id = || field::<PaneId>(data, "pane_id");
    // Herdr pushes global events with underscores and subscription events
    // dotted; accept either spelling for every name.
    let event = match name.replace('.', "_").as_str() {
        "pane_created" => Event::PaneCreated(pane("pane")?),
        "pane_updated" => Event::PaneUpdated(pane("pane")?),
        "pane_moved" => Event::PaneMoved {
            previous: field(data, "previous_pane_id")?,
            pane: pane("pane")?,
        },
        "pane_closed" => Event::PaneClosed(pane_id()?),
        "pane_exited" => Event::PaneExited(pane_id()?),
        "pane_agent_detected" => Event::AgentDetected {
            pane: pane_id()?,
            agent: field(data, "agent")?,
            released: field::<Option<bool>>(data, "released")?.unwrap_or(false),
        },
        "workspace_closed" => Event::WorkspaceClosed(field(data, "workspace_id")?),
        "pane_agent_status_changed" => Event::AgentStatusChanged {
            pane: pane_id()?,
            agent: field(data, "agent")?,
            status: AgentStatus::parse(&field::<String>(data, "agent_status")?),
        },
        _ => return Ok(None),
    };
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_by_their_first_triple() {
        assert!(version_at_least("0.9.1", MIN_VERSION));
        assert!(version_at_least("0.9.2-preview.3", MIN_VERSION));
        assert!(version_at_least("0.10.0", MIN_VERSION));
        assert!(!version_at_least("0.9.0", MIN_VERSION));
        assert!(!version_at_least("0.9", MIN_VERSION));
        assert!(!version_at_least("dev", MIN_VERSION));
    }

    #[test]
    fn events_parse_in_both_spellings_and_unknown_ones_are_ignored() {
        let now = Timestamp::UNIX_EPOCH;
        for name in ["pane_closed", "pane.closed"] {
            let line = json!({"event": name, "data": {"pane_id": "w1:p1", "workspace_id": "w1"}});
            assert_eq!(
                parse_event(&line, now).unwrap(),
                Some(Event::PaneClosed(PaneId("w1:p1".into())))
            );
        }
        let line = json!({"event": "pane.agent_status_changed", "data": {
            "pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude", "agent_status": "done"}});
        assert_eq!(
            parse_event(&line, now).unwrap(),
            Some(Event::AgentStatusChanged {
                pane: PaneId("w1:p1".into()),
                agent: Some("claude".into()),
                status: AgentStatus::Done,
            })
        );
        let line = json!({"event": "tab_focused", "data": {"tab_id": "w1:t1"}});
        assert_eq!(parse_event(&line, now).unwrap(), None);
    }

    #[test]
    fn a_pane_carries_its_agent_and_labels() {
        let raw: RawPane = serde_json::from_value(json!({
            "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "cwd": "/src",
            "agent": "claude", "agent_status": "napping", "name": "coordinator",
            "agent_session": {"agent": "claude", "kind": "id", "source": "x", "value": "s-1"},
            "state_labels": {"working": "Testing", "idle": "Waiting"}
        }))
        .unwrap();
        let pane = raw.pane(Timestamp::UNIX_EPOCH);
        let agent = pane.agent.unwrap();
        assert_eq!(agent.status, AgentStatus::Unknown);
        assert_eq!(agent.name.as_deref(), Some("coordinator"));
        assert_eq!(agent.session.as_deref(), Some("s-1"));
        assert_eq!(agent.state_labels, ["idle=Waiting", "working=Testing"]);
        assert_eq!(pane.tab, "w1:t1");
        assert_eq!(pane.foreground_cwd, None);
    }
}
