//! A task that mirrors the session's panes and agents into a
//! `tokio::sync::watch` channel. It knows nothing about runs.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{Agent, AgentStatus, Client, Event, HerdrError, HerdrView, Pane, PaneId, Subscription};

const MIN_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

const GLOBAL_EVENTS: [&str; 9] = [
    "pane.created",
    "pane.updated",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
    "pane.moved",
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
];

/// Starts mirroring the session behind `client`. The task ends when every
/// receiver is dropped.
pub fn watch(client: Client) -> watch::Receiver<HerdrView> {
    let (tx, rx) = watch::channel(HerdrView {
        connected: false,
        since: Timestamp::now(),
        version: None,
        protocol: None,
        panes: BTreeMap::new(),
    });
    tokio::spawn(run(client, tx));
    rx
}

async fn run(client: Client, tx: watch::Sender<HerdrView>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let mut mirror = Mirror {
            client: &client,
            tx: &tx,
            connected: false,
            version: None,
            protocol: None,
            panes: BTreeMap::new(),
            looked_up: BTreeSet::new(),
            status: None,
            status_panes: BTreeSet::new(),
        };
        let _ = mirror.run().await;
        if mirror.connected {
            backoff = MIN_BACKOFF;
        }
        tx.send_if_modified(|view| {
            let changed = view.connected;
            if changed {
                view.connected = false;
                view.since = Timestamp::now();
            }
            changed
        });
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = tx.closed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct Mirror<'a> {
    client: &'a Client,
    tx: &'a watch::Sender<HerdrView>,
    connected: bool,
    version: Option<String>,
    protocol: Option<u32>,
    panes: BTreeMap<PaneId, Pane>,
    /// Panes whose current agent was already looked up for its name and session.
    looked_up: BTreeSet<PaneId>,
    status: Option<Subscription>,
    status_panes: BTreeSet<PaneId>,
}

fn closed() -> HerdrError {
    HerdrError::Unreachable
}

async fn next_status(status: &mut Option<Subscription>) -> Result<Event, HerdrError> {
    match status {
        Some(subscription) => subscription.next().await?.ok_or_else(closed),
        None => std::future::pending().await,
    }
}

impl Mirror<'_> {
    /// One connected period: returns when the global subscription ends.
    async fn run(&mut self) -> Result<(), HerdrError> {
        let subscriptions: Vec<Value> = GLOBAL_EVENTS.iter().map(|t| json!({"type": t})).collect();
        let mut global = self.client.subscribe(subscriptions).await?;
        let snapshot = self.client.snapshot();
        tokio::pin!(snapshot);
        let mut buffered = Vec::new();
        let snapshot = loop {
            tokio::select! {
                snapshot = &mut snapshot => break snapshot?,
                event = global.next() => buffered.push(event?.ok_or_else(closed)?),
            }
        };
        self.version = Some(snapshot.version);
        self.protocol = Some(snapshot.protocol);
        self.panes = snapshot.panes;
        {
            let previous = self.tx.borrow();
            for (id, pane) in &mut self.panes {
                let old = previous.panes.get(id).and_then(|p| p.agent.as_ref());
                if let (Some(agent), Some(old)) = (pane.agent.as_mut(), old)
                    && agent.kind == old.kind
                    && agent.status == old.status
                {
                    agent.status_since = old.status_since;
                }
            }
        }
        self.looked_up = self
            .panes
            .iter()
            .filter(|(_, p)| p.agent.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        let now = Timestamp::now();
        for event in buffered {
            self.apply(event, now);
        }
        self.settle().await?;
        self.connected = true;
        self.publish();
        loop {
            let event = tokio::select! {
                event = global.next() => event?.ok_or_else(closed)?,
                event = next_status(&mut self.status) => event?,
                () = self.tx.closed() => return Ok(()),
            };
            self.apply(event, Timestamp::now());
            self.settle().await?;
            self.publish();
        }
    }

    /// Fills in unknown agent details and follows the statuses of the current
    /// pane set.
    async fn settle(&mut self) -> Result<(), HerdrError> {
        let panes: BTreeSet<PaneId> = self.panes.keys().cloned().collect();
        let mut added = BTreeSet::new();
        if panes != self.status_panes {
            let subscriptions: Vec<Value> = panes
                .iter()
                .map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p}))
                .collect();
            // The new subscription is acknowledged before the old one is
            // dropped, so no status change falls between them.
            self.status = if panes.is_empty() {
                None
            } else {
                Some(self.client.subscribe(subscriptions).await?)
            };
            added = panes.difference(&self.status_panes).cloned().collect();
            self.status_panes = panes;
        }
        // A status that changed before a pane's subscription was acknowledged
        // sent no event, so those agents are read once more.
        self.look_up_agents(&added).await;
        Ok(())
    }

    async fn look_up_agents(&mut self, refresh: &BTreeSet<PaneId>) {
        let panes = &self.panes;
        self.looked_up
            .retain(|id| panes.get(id).is_some_and(|p| p.agent.is_some()));
        let wanted: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(id, p)| {
                p.agent.as_ref().is_some_and(|a| {
                    refresh.contains(*id)
                        || (!self.looked_up.contains(*id)
                            && (a.name.is_none() || a.session.is_none()))
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in wanted {
            self.looked_up.insert(id.clone());
            let Ok(Some(found)) = self.client.agent_get(&id).await else {
                continue;
            };
            if let Some(agent) = self.panes.get_mut(&id).and_then(|p| p.agent.as_mut())
                && agent.kind == found.kind
            {
                agent.name = found.name.or(agent.name.take());
                agent.session = found.session.or(agent.session.take());
                agent.state_labels = found.state_labels;
                if agent.status != found.status {
                    agent.status = found.status;
                    agent.status_since = found.status_since;
                }
            }
        }
    }

    fn apply(&mut self, event: Event, now: Timestamp) {
        match event {
            Event::PaneCreated(pane) | Event::PaneUpdated(pane) => {
                let old = self.panes.remove(&pane.id);
                self.upsert(old, pane);
            }
            Event::PaneMoved { previous, pane } => {
                let old = self.panes.remove(&previous);
                self.looked_up.remove(&previous);
                self.upsert(old, pane);
            }
            Event::PaneClosed(id) | Event::PaneExited(id) => {
                self.panes.remove(&id);
            }
            Event::WorkspaceClosed(workspace) => {
                self.panes.retain(|_, p| p.workspace != workspace);
            }
            Event::AgentDetected {
                pane,
                agent,
                released,
            } => {
                let Some(p) = self.panes.get_mut(&pane) else {
                    return;
                };
                match agent.filter(|_| !released) {
                    None => p.agent = None,
                    Some(kind) => {
                        if p.agent.as_ref().is_none_or(|a| a.kind != kind) {
                            p.agent = Some(new_agent(kind, AgentStatus::Unknown, now));
                            self.looked_up.remove(&pane);
                        }
                    }
                }
            }
            Event::AgentStatusChanged {
                pane,
                agent,
                status,
            } => {
                let Some(p) = self.panes.get_mut(&pane) else {
                    return;
                };
                match p.agent.as_mut() {
                    Some(current) if agent.as_ref().is_none_or(|k| *k == current.kind) => {
                        if current.status != status {
                            current.status = status;
                            current.status_since = now;
                            // An agent reports its session some time after it starts.
                            if current.name.is_none() || current.session.is_none() {
                                self.looked_up.remove(&pane);
                            }
                        }
                    }
                    _ => {
                        if let Some(kind) = agent {
                            p.agent = Some(new_agent(kind, status, now));
                            self.looked_up.remove(&pane);
                        }
                    }
                }
            }
        }
    }

    /// Stores `pane`, keeping what the event does not carry (the agent name,
    /// and when the status last changed) from `old`.
    fn upsert(&mut self, old: Option<Pane>, mut pane: Pane) {
        let old = old.and_then(|p| p.agent);
        match (pane.agent.as_mut(), old) {
            (Some(agent), Some(old)) if agent.kind == old.kind => {
                agent.name = agent.name.take().or(old.name);
                agent.session = agent.session.take().or(old.session);
                if agent.status == old.status {
                    agent.status_since = old.status_since;
                }
            }
            _ => {
                self.looked_up.remove(&pane.id);
            }
        }
        self.panes.insert(pane.id.clone(), pane);
    }

    fn publish(&self) {
        self.tx.send_if_modified(|view| {
            let reconnected = view.connected != self.connected;
            if !reconnected
                && view.panes == self.panes
                && view.version == self.version
                && view.protocol == self.protocol
            {
                return false;
            }
            if reconnected {
                view.connected = self.connected;
                view.since = Timestamp::now();
            }
            view.version.clone_from(&self.version);
            view.protocol = self.protocol;
            view.panes.clone_from(&self.panes);
            true
        });
    }
}

fn new_agent(kind: String, status: AgentStatus, now: Timestamp) -> Agent {
    Agent {
        kind,
        name: None,
        status,
        status_since: now,
        session: None,
        state_labels: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdr, agent_json, pane_json};

    async fn wait(
        rx: &mut watch::Receiver<HerdrView>,
        what: &str,
        f: impl FnMut(&HerdrView) -> bool,
    ) -> HerdrView {
        let view = tokio::time::timeout(Duration::from_secs(1), rx.wait_for(f))
            .await
            .map(|view| view.unwrap().clone());
        view.unwrap_or_else(|_| panic!("timed out waiting for {what}: {:?}", *rx.borrow()))
    }

    fn id(text: &str) -> PaneId {
        PaneId(text.into())
    }

    #[tokio::test]
    async fn an_event_during_the_snapshot_is_applied_after_it() {
        let fake = FakeHerdr::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/old")], vec![]);
        let release = fake.hold_snapshot();
        let mut rx = watch(fake.client());
        fake.wait_request(|r| r["method"] == "session.snapshot")
            .await;
        fake.push("pane_updated", json!({"pane": pane_json("w1:p1", "/new")}));
        // Let the event reach the client before the snapshot answer does.
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        let view = wait(&mut rx, "connected", |v| v.connected).await;
        assert_eq!(view.panes[&id("w1:p1")].cwd, "/new");
        assert_eq!(view.version.as_deref(), Some("0.9.1"));
        assert_eq!(view.protocol, Some(22));
    }

    #[tokio::test]
    async fn a_new_pane_gets_a_status_subscription() {
        let fake = FakeHerdr::start().await;
        fake.set_snapshot(vec![pane_json("w1:p1", "/src")], vec![]);
        let mut rx = watch(fake.client());
        wait(&mut rx, "connected", |v| v.connected).await;
        fake.push("pane_created", json!({"pane": pane_json("w1:p2", "/src")}));
        fake.wait_request(|r| {
            r["method"] == "events.subscribe"
                && r["params"]["subscriptions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["pane_id"] == "w1:p2")
        })
        .await;
        // The fake registers a subscription before it acknowledges it, and
        // the client reads the ack before any event, so nothing is lost here.
        fake.push(
            "pane.agent_status_changed",
            json!({"pane_id": "w1:p2", "workspace_id": "w1", "agent": "claude", "agent_status": "working"}),
        );
        let view = wait(&mut rx, "a working agent", |v| {
            v.panes.get(&id("w1:p2")).is_some_and(|p| p.agent.is_some())
        })
        .await;
        let agent = view.panes[&id("w1:p2")].agent.clone().unwrap();
        assert_eq!(agent.kind, "claude");
        assert_eq!(agent.status, AgentStatus::Working);
    }

    #[tokio::test]
    async fn both_spellings_of_pane_closed_remove_the_pane() {
        let fake = FakeHerdr::start().await;
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p2", "/b")],
            vec![],
        );
        let mut rx = watch(fake.client());
        wait(&mut rx, "connected", |v| v.panes.len() == 2).await;
        fake.push(
            "pane_closed",
            json!({"pane_id": "w1:p1", "workspace_id": "w1"}),
        );
        let view = wait(&mut rx, "w1:p1 closed", |v| v.panes.len() == 1).await;
        assert_eq!(view.panes.keys().collect::<Vec<_>>(), [&id("w1:p2")]);
        fake.push(
            "pane.closed",
            json!({"pane_id": "w1:p2", "workspace_id": "w1"}),
        );
        let view = wait(&mut rx, "w1:p2 closed", |v| v.panes.is_empty()).await;
        assert!(view.connected);
    }

    #[tokio::test]
    async fn a_restart_disconnects_and_comes_back_with_the_new_snapshot() {
        let mut fake = FakeHerdr::start().await;
        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a")],
            vec![agent_json(
                "w1:p1",
                "claude",
                "working",
                Some("coordinator"),
            )],
        );
        let mut rx = watch(fake.client());
        let view = wait(&mut rx, "connected", |v| v.connected).await;
        let agent = view.panes[&id("w1:p1")].agent.clone().unwrap();
        assert_eq!(agent.name.as_deref(), Some("coordinator"));
        assert_eq!(agent.status, AgentStatus::Working);

        fake.stop().await;
        let view = wait(&mut rx, "disconnected", |v| !v.connected).await;
        assert_eq!(view.panes.keys().collect::<Vec<_>>(), [&id("w1:p1")]);
        assert!(view.panes[&id("w1:p1")].agent.is_some());

        fake.set_snapshot(
            vec![pane_json("w1:p1", "/a"), pane_json("w1:p3", "/c")],
            vec![agent_json("w1:p3", "codex", "idle", None)],
        );
        fake.bind().await;
        let view = wait(&mut rx, "reconnected", |v| v.connected).await;
        assert_eq!(
            view.panes.keys().collect::<Vec<_>>(),
            [&id("w1:p1"), &id("w1:p3")]
        );
        assert_eq!(view.panes[&id("w1:p1")].agent, None);
        let agent = view.panes[&id("w1:p3")].agent.clone().unwrap();
        assert_eq!(agent.kind, "codex");
        assert_eq!(agent.status, AgentStatus::Idle);
    }
}
