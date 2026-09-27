//! Requests and subscriptions over the session socket. Herdr closes a request
//! connection after its one response, so every call opens its own.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::{Agent, Event, HerdrError, Pane, PaneId, RawPane, parse_event};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Client {
    pub socket: PathBuf,
}

/// `session.snapshot`, with each pane's agent taken from the `agents` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: String,
    pub protocol: u32,
    pub panes: BTreeMap<PaneId, Pane>,
}

/// An `events.subscribe` connection after its acknowledgement. Herdr closes
/// a connection that subscribes twice, so each subscription owns one.
pub struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    // Dropping the write half would half-close the connection.
    _write: OwnedWriteHalf,
}

fn request_line(method: &str, params: Value) -> String {
    let id = format!("hla-{}", uuid::Uuid::new_v4().simple());
    let mut line = json!({"id": id, "method": method, "params": params}).to_string();
    line.push('\n');
    line
}

fn io_error(error: std::io::Error) -> HerdrError {
    HerdrError::Protocol(error.to_string())
}

fn parse_response<R: DeserializeOwned>(line: &str) -> Result<R, HerdrError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))?;
    if let Some(error) = value.get("error") {
        let text = |key: &str| {
            error
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        return Err(HerdrError::Api {
            code: text("code"),
            message: text("message"),
        });
    }
    let result = value
        .get("result")
        .cloned()
        .ok_or_else(|| HerdrError::Protocol(format!("no result: {line}")))?;
    serde_json::from_value(result).map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))
}

async fn first_line(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Result<String, HerdrError> {
    lines
        .next_line()
        .await
        .map_err(io_error)?
        .ok_or(HerdrError::Unreachable)
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn open(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf), HerdrError> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| HerdrError::Unreachable)?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(request_line(method, params).as_bytes())
            .await
            .map_err(io_error)?;
        Ok((BufReader::new(read).lines(), write))
    }

    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<R, HerdrError> {
        tokio::time::timeout(TIMEOUT, async {
            let (mut lines, _write) = self.open(method, params).await?;
            parse_response(&first_line(&mut lines).await?)
        })
        .await
        .map_err(|_| HerdrError::Timeout)?
    }

    /// Opens `events.subscribe` on a new connection and waits for its
    /// acknowledgement, so no event pushed after this returns is missed.
    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<Subscription, HerdrError> {
        tokio::time::timeout(TIMEOUT, async {
            let params = json!({"subscriptions": subscriptions});
            let (mut lines, write) = self.open("events.subscribe", params).await?;
            let ack: Value = parse_response(&first_line(&mut lines).await?)?;
            if ack["type"] != "subscription_started" {
                return Err(HerdrError::Protocol(format!(
                    "not an acknowledgement: {ack}"
                )));
            }
            Ok(Subscription {
                lines,
                _write: write,
            })
        })
        .await
        .map_err(|_| HerdrError::Timeout)?
    }

    pub async fn snapshot(&self) -> Result<Snapshot, HerdrError> {
        #[derive(Deserialize)]
        struct Answer {
            snapshot: Raw,
        }
        #[derive(Deserialize)]
        struct Raw {
            version: String,
            protocol: u32,
            panes: Vec<RawPane>,
            agents: Vec<RawPane>,
        }
        let Answer { snapshot } = self.call("session.snapshot", json!({})).await?;
        let now = Timestamp::now();
        let mut agents: BTreeMap<String, Option<Agent>> = snapshot
            .agents
            .iter()
            .map(|a| (a.pane_id.clone(), a.agent(now)))
            .collect();
        let panes = snapshot
            .panes
            .into_iter()
            .map(|raw| {
                let agent = match agents.remove(&raw.pane_id) {
                    Some(agent) => agent,
                    None => raw.agent(now),
                };
                let pane = raw.into_pane(agent);
                (pane.id.clone(), pane)
            })
            .collect();
        Ok(Snapshot {
            version: snapshot.version,
            protocol: snapshot.protocol,
            panes,
        })
    }

    /// The agent in `pane`, looked up by pane id.
    pub async fn agent_get(&self, pane: &PaneId) -> Result<Option<Agent>, HerdrError> {
        #[derive(Deserialize)]
        struct Answer {
            agent: RawPane,
        }
        let Answer { agent } = self.call("agent.get", json!({"target": pane})).await?;
        Ok(agent.agent(Timestamp::now()))
    }

    pub async fn notification_show(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        self.call::<Value>("notification.show", json!({"title": title, "body": body}))
            .await
            .map(|_| ())
    }
}

impl Subscription {
    /// The next event the mirror cares about; `None` when Herdr closed the
    /// connection.
    pub async fn next(&mut self) -> Result<Option<Event>, HerdrError> {
        loop {
            // `next_line` is cancel safe, so this can sit in a `select!`.
            let Some(line) = self.lines.next_line().await.map_err(io_error)? else {
                return Ok(None);
            };
            let value: Value = serde_json::from_str(&line)
                .map_err(|e| HerdrError::Protocol(format!("{e}: {line}")))?;
            if value.get("error").is_some() {
                return parse_response::<Value>(&line).map(|_| None);
            }
            if let Some(event) = parse_event(&value, Timestamp::now())? {
                return Ok(Some(event));
            }
        }
    }
}

/// Asks `herdr session list --json` for a session's socket path.
pub async fn session_socket(herdr_bin: &str, session: Option<&str>) -> Result<PathBuf> {
    let output = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new(herdr_bin)
            .args(["session", "list", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("`herdr session list` did not finish in time")?
    .with_context(|| format!("could not run {herdr_bin}"))?;
    if !output.status.success() {
        bail!(
            "`herdr session list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    socket_in_list(&output.stdout, session)
}

fn socket_in_list(json: &[u8], session: Option<&str>) -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct List {
        sessions: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        #[serde(default)]
        default: bool,
        socket_path: PathBuf,
    }
    let list: List =
        serde_json::from_slice(json).context("`herdr session list --json` is not a list")?;
    list.sessions
        .into_iter()
        .find(|s| match session {
            Some(name) => s.name == name,
            None => s.default,
        })
        .map(|s| s.socket_path)
        .with_context(|| match session {
            Some(name) => format!("there is no Herdr session `{name}`"),
            None => "Herdr lists no default session".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::herdr::fake::FakeHerdr;

    #[test]
    fn a_session_socket_comes_from_the_session_list() {
        let list = br#"{"sessions":[
            {"default":true,"name":"default","running":true,"socket_path":"/h/herdr.sock"},
            {"default":false,"name":"work","running":true,"socket_path":"/h/sessions/work/herdr.sock"}]}"#;
        assert_eq!(
            socket_in_list(list, None).unwrap(),
            Path::new("/h/herdr.sock")
        );
        assert_eq!(
            socket_in_list(list, Some("work")).unwrap(),
            Path::new("/h/sessions/work/herdr.sock")
        );
        assert_eq!(
            socket_in_list(list, Some("other")).unwrap_err().to_string(),
            "there is no Herdr session `other`"
        );
    }

    #[tokio::test]
    async fn an_error_response_becomes_an_api_error() {
        let fake = FakeHerdr::start().await;
        fake.fail("session.snapshot", "server_busy", "try again later");
        let error = fake.client().snapshot().await.unwrap_err();
        assert_eq!(
            error,
            HerdrError::Api {
                code: "server_busy".into(),
                message: "try again later".into(),
            }
        );
        let error = fake
            .client()
            .agent_get(&PaneId("w9:p9".into()))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            HerdrError::Api {
                code: "agent_not_found".into(),
                message: "agent target w9:p9 not found".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_missing_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::new(dir.path().join("herdr.sock"));
        assert_eq!(
            client.notification_show("t", "b").await.unwrap_err(),
            HerdrError::Unreachable
        );
    }
}
