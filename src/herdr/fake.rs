//! A stand-in Herdr server for tests that behaves like the measured one: one
//! response per request connection, then close; a subscription answers its
//! acknowledgement and then pushes events; a second subscribe on a
//! subscription connection closes it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::Client;

pub fn pane_json(pane: &str, cwd: &str) -> Value {
    let workspace = pane.split(':').next().unwrap();
    json!({
        "pane_id": pane, "terminal_id": format!("term-{pane}"), "workspace_id": workspace,
        "tab_id": format!("{workspace}:t1"), "focused": false, "cwd": cwd,
        "foreground_cwd": cwd, "agent_status": "unknown", "revision": 0
    })
}

pub fn agent_json(pane: &str, agent: &str, status: &str, name: Option<&str>) -> Value {
    let mut value = pane_json(pane, "/");
    value["agent"] = json!(agent);
    value["agent_status"] = json!(status);
    value["name"] = json!(name);
    value
}

#[derive(Default)]
struct State {
    snapshot: Value,
    errors: BTreeMap<String, (String, String)>,
    hold_snapshot: Option<oneshot::Receiver<()>>,
    subscribers: Vec<(Vec<Value>, mpsc::UnboundedSender<String>)>,
}

struct Shared {
    state: Mutex<State>,
    requests: watch::Sender<Vec<Value>>,
}

pub struct FakeHerdr {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    shared: Arc<Shared>,
    server: Option<JoinHandle<()>>,
}

impl FakeHerdr {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Self {
            socket: dir.path().join("herdr.sock"),
            _dir: dir,
            shared: Arc::new(Shared {
                state: Mutex::default(),
                requests: watch::Sender::new(Vec::new()),
            }),
            server: None,
        };
        fake.set_snapshot(Vec::new(), Vec::new());
        fake.bind().await;
        fake
    }

    pub fn client(&self) -> Client {
        Client::new(&self.socket)
    }

    pub fn set_snapshot(&self, panes: Vec<Value>, agents: Vec<Value>) {
        self.shared.state.lock().unwrap().snapshot = json!({
            "version": "0.9.1", "protocol": 22, "workspaces": [], "tabs": [],
            "layouts": [], "panes": panes, "agents": agents
        });
    }

    pub fn fail(&self, method: &str, code: &str, message: &str) {
        self.shared
            .state
            .lock()
            .unwrap()
            .errors
            .insert(method.into(), (code.into(), message.into()));
    }

    /// Holds the next snapshot answer until the returned sender fires.
    pub fn hold_snapshot(&self) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.shared.state.lock().unwrap().hold_snapshot = Some(rx);
        tx
    }

    /// Waits until a request matching `f` has arrived.
    pub async fn wait_request(&self, mut f: impl FnMut(&Value) -> bool) {
        let mut rx = self.shared.requests.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.wait_for(|requests| requests.iter().any(&mut f)),
        )
        .await
        .expect("the request never arrived")
        .unwrap();
    }

    /// Pushes an event to every subscription that asked for it.
    pub fn push(&self, event: &str, data: Value) {
        let line = format!("{}\n", json!({"event": event, "data": data}));
        let name = event.replace('.', "_");
        self.shared
            .state
            .lock()
            .unwrap()
            .subscribers
            .retain(|(subscriptions, tx)| {
                let wanted = subscriptions.iter().any(|s| {
                    s["type"]
                        .as_str()
                        .is_some_and(|t| t.replace('.', "_") == name)
                        && s.get("pane_id").is_none_or(|p| *p == data["pane_id"])
                });
                !wanted || tx.send(line.clone()).is_ok()
            });
    }

    /// Like `herdr server stop`: every connection ends and the socket goes away.
    pub async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
        self.shared.state.lock().unwrap().subscribers.clear();
        let _ = std::fs::remove_file(&self.socket);
    }

    pub async fn bind(&mut self) {
        let listener = UnixListener::bind(&self.socket).unwrap();
        self.server = Some(tokio::spawn(serve(listener, self.shared.clone())));
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

async fn serve(listener: UnixListener, shared: Arc<Shared>) {
    // Dropping the set when this task is aborted ends every connection.
    let mut connections = JoinSet::new();
    while let Ok((stream, _)) = listener.accept().await {
        connections.spawn(connection(stream, shared.clone()));
    }
}

async fn connection(stream: UnixStream, shared: Arc<Shared>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let request: Value = serde_json::from_str(&line).unwrap();
    let log = |request: &Value| shared.requests.send_modify(|r| r.push(request.clone()));
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default().to_string();
    if method == "events.subscribe" {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let subscriptions = request["params"]["subscriptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        shared
            .state
            .lock()
            .unwrap()
            .subscribers
            .push((subscriptions, tx));
        // Logged after registering, so a test that waits for it can push.
        log(&request);
        let ack = json!({"id": id, "result": {"type": "subscription_started"}});
        if write
            .write_all(format!("{ack}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                line = rx.recv() => match line {
                    Some(line) if write.write_all(line.as_bytes()).await.is_ok() => {}
                    _ => return,
                },
                // A second subscribe, or the client going away, ends it.
                _ = lines.next_line() => return,
            }
        }
    }
    log(&request);
    let response = respond(&shared, &id, &method, &request["params"]).await;
    let _ = write.write_all(format!("{response}\n").as_bytes()).await;
}

async fn respond(shared: &Shared, id: &Value, method: &str, params: &Value) -> Value {
    let error = |code: &str, message: String| json!({"id": id, "error": {"code": code, "message": message}});
    let hold = {
        let mut state = shared.state.lock().unwrap();
        if let Some((code, message)) = state.errors.get(method) {
            return error(code, message.clone());
        }
        if method == "session.snapshot" {
            state.hold_snapshot.take()
        } else {
            None
        }
    };
    if let Some(hold) = hold {
        let _ = hold.await;
    }
    let state = shared.state.lock().unwrap();
    let result = match method {
        "session.snapshot" => json!({"type": "session_snapshot", "snapshot": state.snapshot}),
        "agent.get" => {
            let target = &params["target"];
            match state.snapshot["agents"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["pane_id"] == *target))
            {
                Some(agent) => json!({"type": "agent_info", "agent": agent}),
                None => {
                    return error(
                        "agent_not_found",
                        format!("agent target {} not found", target.as_str().unwrap_or("")),
                    );
                }
            }
        }
        "notification.show" => {
            json!({"type": "notification_show", "shown": true, "reason": "shown"})
        }
        other => return error("unknown_method", format!("unknown method {other}")),
    };
    json!({"id": id, "result": result})
}
