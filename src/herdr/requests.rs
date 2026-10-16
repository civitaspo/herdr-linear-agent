//! The Herdr requests the plugin makes, as a trait the ticker and the agent
//! subcommands are generic over, and its implementation on the socket client.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Client, HerdrError, Pane, PaneId, Snapshot, WorkspaceId};

/// Herdr waits this long for a started agent to become ready.
const AGENT_START_TIMEOUT_MS: u64 = 30_000;
/// Where a new workspace or tab placed its root pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub workspace: WorkspaceId,
    pub tab: String,
    pub pane: PaneId,
    /// The root pane's working directory.
    pub cwd: String,
}

pub trait Herdr: Send + Sync {
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, HerdrError>> + Send;

    /// A client for a previously recorded session socket, when the
    /// implementation can open one. The reconciler uses it only to close a
    /// managed workspace left in the old session after an explicit switch.
    fn for_socket(&self, _socket: &str) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }

    fn workspace_create(
        &self,
        cwd: &str,
        label: &str,
    ) -> impl Future<Output = Result<Placed, HerdrError>> + Send;

    fn tab_create(
        &self,
        workspace: &WorkspaceId,
        cwd: &str,
        label: &str,
    ) -> impl Future<Output = Result<Placed, HerdrError>> + Send;

    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane: &PaneId,
        args: &[String],
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn agent_prompt(
        &self,
        pane: &PaneId,
        text: &str,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn send_escape(&self, pane: &PaneId) -> impl Future<Output = Result<(), HerdrError>> + Send;

    /// A missing pane returns `pane_not_found`; callers may treat that as already closed.
    fn pane_close(&self, pane: &PaneId) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn agent_rename(
        &self,
        pane: &PaneId,
        name: &str,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn tab_rename(
        &self,
        tab: &str,
        label: &str,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn workspace_close(
        &self,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn workspace_focus(
        &self,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    fn notification_show(
        &self,
        title: &str,
        body: &str,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    /// Reports pane tokens under `source`; an empty `display_agent` leaves
    /// the pane's display name alone.
    fn report_metadata(
        &self,
        pane: &PaneId,
        source: &str,
        display_agent: &str,
        tokens: &[(String, String)],
        ttl_ms: u64,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    /// The caller's pane, with its current terminal.
    fn pane_current(
        &self,
        caller: &PaneId,
    ) -> impl Future<Output = Result<Pane, HerdrError>> + Send;
}

#[derive(Deserialize)]
struct PlacedAnswer {
    root_pane: Pane,
}

impl PlacedAnswer {
    fn placed(self, requested_cwd: &str) -> Placed {
        let pane = self.root_pane;
        let cwd = pane
            .cwd
            .or(pane.foreground_cwd)
            .unwrap_or_else(|| requested_cwd.to_string());
        Placed {
            workspace: pane.workspace,
            tab: pane.tab,
            pane: pane.id,
            cwd,
        }
    }
}

impl Client {
    pub async fn agent_start_with_timeout(
        &self,
        name: &str,
        kind: &str,
        pane: &PaneId,
        args: &[String],
        timeout: Duration,
    ) -> Result<(), HerdrError> {
        let params = json!({
            "name": name, "kind": kind, "pane_id": pane, "args": args,
            "timeout_ms": timeout.as_millis() as u64,
        });
        self.call_with_timeout::<Value>("agent.start", params, timeout + Duration::from_secs(5))
            .await
            .map(|_| ())
    }

    async fn ok(&self, method: &str, params: Value) -> Result<(), HerdrError> {
        self.call::<Value>(method, params).await.map(|_| ())
    }
}

impl Herdr for Client {
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, HerdrError>> + Send {
        Client::snapshot(self)
    }

    fn for_socket(&self, socket: &str) -> Option<Self> {
        Some(Client::new(socket))
    }

    async fn workspace_create(&self, cwd: &str, label: &str) -> Result<Placed, HerdrError> {
        let params = json!({"cwd": cwd, "label": label, "focus": false});
        let answer: PlacedAnswer = self.call("workspace.create", params).await?;
        Ok(answer.placed(cwd))
    }

    async fn tab_create(
        &self,
        workspace: &WorkspaceId,
        cwd: &str,
        label: &str,
    ) -> Result<Placed, HerdrError> {
        let params = json!({
            "workspace_id": workspace,
            "cwd": cwd,
            "label": label,
            "focus": false,
        });
        let answer: PlacedAnswer = self.call("tab.create", params).await?;
        Ok(answer.placed(cwd))
    }

    async fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane: &PaneId,
        args: &[String],
    ) -> Result<(), HerdrError> {
        self.agent_start_with_timeout(
            name,
            kind,
            pane,
            args,
            Duration::from_millis(AGENT_START_TIMEOUT_MS),
        )
        .await
    }

    async fn agent_prompt(&self, pane: &PaneId, text: &str) -> Result<(), HerdrError> {
        self.ok("agent.prompt", json!({"target": pane, "text": text}))
            .await
    }

    async fn send_escape(&self, pane: &PaneId) -> Result<(), HerdrError> {
        self.ok("agent.send_keys", json!({"target": pane, "keys": ["esc"]}))
            .await
    }

    async fn pane_close(&self, pane: &PaneId) -> Result<(), HerdrError> {
        self.ok("pane.close", json!({"pane_id": pane})).await
    }

    async fn agent_rename(&self, pane: &PaneId, name: &str) -> Result<(), HerdrError> {
        self.ok("agent.rename", json!({"target": pane, "name": name}))
            .await
    }

    async fn tab_rename(&self, tab: &str, label: &str) -> Result<(), HerdrError> {
        self.ok("tab.rename", json!({"tab_id": tab, "label": label}))
            .await
    }

    async fn workspace_close(&self, workspace: &WorkspaceId) -> Result<(), HerdrError> {
        self.ok("workspace.close", json!({"workspace_id": workspace}))
            .await
    }

    async fn workspace_focus(&self, workspace: &WorkspaceId) -> Result<(), HerdrError> {
        self.ok("workspace.focus", json!({"workspace_id": workspace}))
            .await
    }

    async fn notification_show(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        self.ok("notification.show", json!({"title": title, "body": body}))
            .await
    }

    async fn report_metadata(
        &self,
        pane: &PaneId,
        source: &str,
        display_agent: &str,
        tokens: &[(String, String)],
        ttl_ms: u64,
    ) -> Result<(), HerdrError> {
        let tokens: Map<String, Value> = tokens
            .iter()
            .map(|(name, value)| (name.clone(), json!(value)))
            .collect();
        let mut params = json!({
            "pane_id": pane, "source": source, "tokens": tokens, "ttl_ms": ttl_ms,
        });
        if !display_agent.is_empty() {
            params["display_agent"] = json!(display_agent);
        }
        self.ok("pane.report_metadata", params).await
    }

    async fn pane_current(&self, caller: &PaneId) -> Result<Pane, HerdrError> {
        #[derive(Deserialize)]
        struct Answer {
            pane: Pane,
        }
        let answer: Answer = self
            .call("pane.current", json!({"caller_pane_id": caller}))
            .await?;
        Ok(answer.pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdrServer, pane_json};

    fn last_request(fake: &FakeHerdrServer, method: &str) -> Value {
        fake.requests()
            .into_iter()
            .rev()
            .find(|r| r["method"] == method)
            .unwrap_or_else(|| panic!("no {method} request"))
    }

    #[tokio::test]
    async fn errors_carry_herdrs_code_and_starts_carry_their_arguments() {
        let fake = FakeHerdrServer::start().await;
        fake.fail("agent.start", "agent_not_ready", "blocked during startup");
        let client = fake.client();
        let error = client
            .agent_start(
                "x",
                "claude",
                &PaneId("w1:p1".into()),
                &["--model".into(), "opus".into()],
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            HerdrError::Api {
                code: "agent_not_ready".into(),
                message: "blocked during startup".into(),
            }
        );
        let params = &last_request(&fake, "agent.start")["params"];
        assert_eq!(
            *params,
            json!({"name": "x", "kind": "claude", "pane_id": "w1:p1",
                   "args": ["--model", "opus"], "timeout_ms": 30000})
        );
    }

    #[tokio::test]
    async fn workspace_placement_comes_from_the_root_pane() {
        let fake = FakeHerdrServer::start().await;
        fake.answer(
            "workspace.create",
            json!({"type": "workspace_created", "workspace": {}, "tab": {},
                   "root_pane": pane_json("w3:p1", "/state/runs/DATA-1")}),
        );
        let client = fake.client();
        let placed = client
            .workspace_create("/state/runs/DATA-1", "DATA-1 Fix")
            .await
            .unwrap();
        assert_eq!(
            placed,
            Placed {
                workspace: WorkspaceId("w3".into()),
                tab: "w3:t1".into(),
                pane: PaneId("w3:p1".into()),
                cwd: "/state/runs/DATA-1".into(),
            }
        );
        assert_eq!(
            last_request(&fake, "workspace.create")["params"],
            json!({"cwd": "/state/runs/DATA-1", "label": "DATA-1 Fix", "focus": false})
        );
    }

    #[tokio::test]
    async fn tab_operations_use_the_0_9_1_socket_shapes() {
        let fake = FakeHerdrServer::start().await;
        fake.answer(
            "tab.create",
            json!({"type": "tab_created", "tab": {},
                   "root_pane": pane_json("w3:p2", "/wt/api")}),
        );
        fake.answer("tab.rename", json!({"type": "tab_info", "tab": {}}));
        fake.answer("pane.close", json!({"type": "ok"}));
        let client = fake.client();
        let placed = client
            .tab_create(&WorkspaceId("w3".into()), "/wt/api", "api · w1")
            .await
            .unwrap();
        assert_eq!(placed.workspace, WorkspaceId("w3".into()));
        assert_eq!(placed.tab, "w3:t1");
        assert_eq!(placed.pane, PaneId("w3:p2".into()));
        assert_eq!(
            last_request(&fake, "tab.create")["params"],
            json!({"workspace_id": "w3", "cwd": "/wt/api", "label": "api · w1", "focus": false})
        );
        client.tab_rename("w3:t1", "coordinator").await.unwrap();
        assert_eq!(
            last_request(&fake, "tab.rename")["params"],
            json!({"tab_id": "w3:t1", "label": "coordinator"})
        );
        let pane = PaneId("w3:p2".into());
        client.pane_close(&pane).await.unwrap();
        assert_eq!(
            last_request(&fake, "pane.close")["params"],
            json!({"pane_id": "w3:p2"})
        );
    }

    #[tokio::test]
    async fn pane_close_preserves_herdr_not_found_errors() {
        let fake = FakeHerdrServer::start().await;
        fake.fail("pane.close", "pane_not_found", "pane w1:p9 not found");
        let error = fake
            .client()
            .pane_close(&PaneId("w1:p9".into()))
            .await
            .unwrap_err();
        assert!(matches!(error, HerdrError::Api { ref code, .. } if code == "pane_not_found"));
    }

    #[tokio::test]
    async fn metadata_and_keys_use_herdrs_parameter_names() {
        let fake = FakeHerdrServer::start().await;
        fake.answer("pane.report_metadata", json!({"type": "ok"}));
        fake.answer("agent.send_keys", json!({"type": "ok"}));
        fake.answer(
            "pane.current",
            json!({"type": "pane_current", "pane": pane_json("w1:p2", "/wt")}),
        );
        let client = fake.client();
        let pane = PaneId("w1:p2".into());
        client
            .report_metadata(
                &pane,
                "herdr-linear-agent",
                "",
                &[("herdr_linear_agent_activity".into(), "Testing".into())],
                300_000,
            )
            .await
            .unwrap();
        assert_eq!(
            last_request(&fake, "pane.report_metadata")["params"],
            json!({"pane_id": "w1:p2", "source": "herdr-linear-agent",
                   "tokens": {"herdr_linear_agent_activity": "Testing"}, "ttl_ms": 300000})
        );
        client.send_escape(&pane).await.unwrap();
        assert_eq!(
            last_request(&fake, "agent.send_keys")["params"],
            json!({"target": "w1:p2", "keys": ["esc"]})
        );
        let current = client.pane_current(&pane).await.unwrap();
        assert_eq!(current.terminal, "term-w1:p2");
        assert_eq!(
            last_request(&fake, "pane.current")["params"],
            json!({"caller_pane_id": "w1:p2"})
        );
    }
}
