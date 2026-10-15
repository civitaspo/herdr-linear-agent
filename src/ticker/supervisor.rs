//! Reconciles the one persistent supervisor pane from the ticker's snapshot.

use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use std::path::{Path, PathBuf};

use super::reconcile::{Deps, Reconciler, blocking};
use crate::agents;
use crate::config::{Config, Profile};
use crate::files;
use crate::herdr::{
    Agent, AgentStatus as HerdrStatus, Herdr, Pane, PaneId, Placed, Snapshot, WorkspaceId,
};
use crate::supervisor::{
    self, PendingReplacement, SupervisorRecord, SupervisorStatus, SupervisorStore,
};

const LABEL: &str = "herdr-linear-agent-supervisor";
const AGENT_NAME: &str = "herdr-linear-agent-supervisor";
const DRAIN: SignedDuration = SignedDuration::from_secs(120);
const WAKE_RETRY: SignedDuration = SignedDuration::from_secs(60);
const LAUNCH_RETRY: SignedDuration = SignedDuration::from_secs(30);
const MAX_WAKE_ATTEMPTS: u8 = 3;
const MAX_LAUNCH_ATTEMPTS: u8 = 3;

fn workspace_dir(store: &SupervisorStore) -> PathBuf {
    store.root().join("workspace")
}

fn set_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn timestamp_due(text: &str, now: Timestamp, delay: SignedDuration) -> bool {
    text.parse::<Timestamp>()
        .ok()
        .is_none_or(|then| now.duration_since(then) >= delay)
}

fn write_priming(root: &Path, config: &Config, record: &SupervisorRecord, bin: &str) -> Result<()> {
    let profile = config.profile(&record.profile)?;
    let instructions = profile
        .instructions
        .iter()
        .map(|layer| {
            format!(
                "\n## Instructions from `{}`\n\n{}\n",
                layer.profile,
                layer.text.trim()
            )
        })
        .collect::<String>();
    let body = format!(
        "# {LABEL}\n\n{}\n\nThe configured generation is `{}`. Follow this file and use only the local supervisor commands below.\n\n- `{bin} supervisor context --generation {}`\n- `{bin} supervisor memory search <query>`\n- `{bin} supervisor show <handle>`\n- `{bin} supervisor profiles show <id> --revision <revision>`\n- `{bin} supervisor decide --generation {} --decision-file <path>`\n{}",
        include_str!("../../assets/SUPERVISOR.md").trim(),
        record.generation,
        record.generation,
        record.generation,
        instructions,
    );
    files::write_atomic(&root.join("AGENTS.md"), body.as_bytes())?;
    let claude = root.join("CLAUDE.md");
    if std::fs::read_link(&claude).ok().as_deref() != Some(Path::new("AGENTS.md")) {
        let _ = std::fs::remove_file(&claude);
        std::os::unix::fs::symlink("AGENTS.md", &claude)?;
    }
    let settings = serde_json::json!({
        "permissions": { "allow": [
            format!("Bash({bin} supervisor context:*)"),
            format!("Bash({bin} supervisor memory search:*)"),
            format!("Bash({bin} supervisor show:*)"),
            format!("Bash({bin} supervisor profiles show:*)"),
            format!("Bash({bin} supervisor decide:*)")
        ] }
    });
    std::fs::create_dir_all(root.join(".claude"))?;
    files::write_json(&root.join(".claude/settings.local.json"), &settings)?;
    std::fs::create_dir_all(root.join(".cursor"))?;
    files::write_json(
        &root.join(".cursor/cli.json"),
        &serde_json::json!({"permissions": {"allow": [format!("Shell({bin})")], "deny": []}}),
    )?;
    Ok(())
}

fn manager_pane<'a>(
    snapshot: &'a Snapshot,
    record: &SupervisorRecord,
    cwd: &str,
) -> Option<&'a Pane> {
    let matches_cwd = |pane: &Pane| {
        pane.cwd.as_deref() == Some(cwd) || pane.foreground_cwd.as_deref() == Some(cwd)
    };
    let recorded = (!record.pane_id.is_empty())
        .then(|| snapshot.panes.get(&PaneId(record.pane_id.clone())))
        .flatten()
        .filter(|pane| pane.workspace.0 == record.workspace_id && matches_cwd(pane));
    recorded.or_else(|| {
        snapshot.panes.values().find(|pane| {
            (record.workspace_id.is_empty() || pane.workspace.0 == record.workspace_id)
                && pane.label.as_deref() == Some(LABEL)
                && matches_cwd(pane)
        })
    })
}

fn named_agent<'a>(snapshot: &'a Snapshot, pane: &Pane) -> Option<&'a Agent> {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane == pane.id && agent.name.as_deref() == Some(AGENT_NAME))
}

fn placed(pane: &Pane, cwd: &str) -> Placed {
    Placed {
        workspace: pane.workspace.clone(),
        tab: pane.tab.clone(),
        pane: pane.id.clone(),
        cwd: cwd.into(),
        worktree_path: None,
    }
}

fn persist_placement(
    store: &SupervisorStore,
    placed: &Placed,
    now: &str,
) -> Result<SupervisorRecord> {
    store.update_record(|record| {
        record.workspace_id = placed.workspace.0.clone();
        record.tab_id = placed.tab.clone();
        record.pane_id = placed.pane.0.clone();
        record.cwd = placed.cwd.clone();
        record.status = SupervisorStatus::Starting;
        record.launch_attempts = 0;
        record.last_launch_at.clear();
        record.placement_attempts = 0;
        record.last_placement_at.clear();
        record.wake_attempts = 0;
        record.error.clear();
        let _ = now;
    })
}

fn start_args(profile: &Profile, record: &SupervisorRecord) -> (String, Vec<String>) {
    let mut args = agents::profile_args(profile);
    let session = if !record.agent_session.is_empty() {
        record.agent_session.clone()
    } else if profile.kind == "claude" {
        uuid::Uuid::new_v4().to_string()
    } else {
        String::new()
    };
    if profile.kind == "claude" && !session.is_empty() {
        args.extend(["--session-id".into(), session.clone()]);
    }
    (session, args)
}

fn replacement(
    enabled: bool,
    profile: &str,
    fingerprint: &str,
    herdr_session: &str,
    policy_revision: &str,
    now: Timestamp,
    reason: &str,
) -> PendingReplacement {
    PendingReplacement {
        enabled,
        profile: profile.into(),
        fingerprint: fingerprint.into(),
        herdr_session: herdr_session.into(),
        policy_revision: policy_revision.into(),
        deadline: now.checked_add(DRAIN).unwrap_or(now).to_string(),
        reason: reason.into(),
    }
}

fn same_replacement(a: &PendingReplacement, b: &PendingReplacement) -> bool {
    a.enabled == b.enabled
        && a.profile == b.profile
        && a.fingerprint == b.fingerprint
        && a.herdr_session == b.herdr_session
        && a.policy_revision == b.policy_revision
}

fn begin_replacement(
    store: &SupervisorStore,
    desired: PendingReplacement,
) -> Result<SupervisorRecord> {
    store.update_record(|record| {
        if record
            .pending_replacement
            .as_ref()
            .is_none_or(|active| !same_replacement(active, &desired))
        {
            record.pending_replacement = Some(desired.clone());
        }
        record.status = SupervisorStatus::Draining;
        record.wake_pending = false;
        record.error.clear();
    })
}

fn activate_replacement(
    store: &SupervisorStore,
    target: PendingReplacement,
) -> Result<SupervisorRecord> {
    store.update_record(|record| {
        record.profile = target.profile.clone();
        record.profile_fingerprint = target.fingerprint.clone();
        record.herdr_session = target.herdr_session.clone();
        record.policy_revision = target.policy_revision.clone();
        record.workspace_id.clear();
        record.tab_id.clear();
        record.pane_id.clear();
        record.cwd.clear();
        record.agent_session.clear();
        record.active_batch = None;
        record.pending_replacement = None;
        record.status = if target.enabled {
            SupervisorStatus::Starting
        } else {
            SupervisorStatus::Disabled
        };
        record.wake_pending = false;
        record.wake_attempts = 0;
        record.last_wake_at.clear();
        record.launch_attempts = 0;
        record.last_launch_at.clear();
        record.error.clear();
    })
}

async fn close_for_session_switch<H: Herdr>(
    d: &Deps<'_, H>,
    store: &SupervisorStore,
    record: &SupervisorRecord,
    target: PendingReplacement,
) -> Result<()> {
    let fenced = if record.status == SupervisorStatus::Closing {
        record.clone()
    } else {
        let root = store.clone();
        blocking(move || {
            root.update_record(|record| {
                record.generation = record.generation.saturating_add(1);
                record.active_batch = None;
                record.status = SupervisorStatus::Closing;
                record.error.clear();
            })
        })
        .await?
    };
    if !fenced.workspace_id.is_empty() {
        let old = d
            .herdr
            .for_socket(&fenced.herdr_session)
            .context("this Herdr client cannot reach the supervisor's previous session")?;
        match old
            .workspace_close(&WorkspaceId(fenced.workspace_id.clone()))
            .await
        {
            Ok(()) => {}
            Err(crate::herdr::HerdrError::Api { code, .. }) if code == "workspace_not_found" => {}
            Err(error) => {
                let root = store.clone();
                let message = error.to_string();
                blocking(move || root.update_record(|record| record.error = message)).await?;
                return Ok(());
            }
        }
    }
    let root = store.clone();
    blocking(move || activate_replacement(&root, target)).await?;
    d.log
        .line("supervisor: previous Herdr session was closed; replacement is ready to start");
    Ok(())
}

async fn replace_same_session<H: Herdr>(
    d: &Deps<'_, H>,
    store: &SupervisorStore,
    record: &SupervisorRecord,
    snapshot: Option<&Snapshot>,
    now: Timestamp,
) -> Result<()> {
    let Some(target) = record.pending_replacement.clone() else {
        return Ok(());
    };
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    let full = snapshot.skipped == 0;
    let pane = if record.cwd.is_empty() {
        None
    } else {
        manager_pane(snapshot, record, &record.cwd)
    };
    let old_workspace_present = !record.workspace_id.is_empty()
        && snapshot
            .panes
            .values()
            .any(|pane| pane.workspace.0 == record.workspace_id);
    let old_manager = pane.and_then(|pane| named_agent(snapshot, pane).cloned());
    if full && !old_workspace_present {
        let root = store.clone();
        blocking(move || activate_replacement(&root, target)).await?;
        return Ok(());
    }
    let expired = target
        .deadline
        .parse::<Timestamp>()
        .is_ok_and(|deadline| now >= deadline);
    let idle = record.active_batch.is_none()
        && old_manager
            .as_ref()
            .is_none_or(|agent| agent.status.is_idle());
    if !expired && !idle {
        // The only work allowed to finish during drain is the already issued batch.
        if record.active_batch.is_some()
            && let (Some(pane), Some(agent)) = (pane, old_manager.as_ref())
            && agent.status.is_idle()
        {
            let prompt = format!(
                "[herdr-linear-agent supervisor] Finish the already issued batch using generation {}. Do not request a new batch while draining.",
                record.generation
            );
            let _ = d.herdr.agent_prompt(&pane.id, &prompt).await;
        }
        return Ok(());
    }

    let fenced = if record.status == SupervisorStatus::Closing {
        record.clone()
    } else {
        let root = store.clone();
        blocking(move || {
            root.update_record(|record| {
                record.generation = record.generation.saturating_add(1);
                record.active_batch = None;
                record.status = SupervisorStatus::Closing;
                record.error.clear();
            })
        })
        .await?
    };
    if expired && let Some(pane) = pane {
        let _ = d.herdr.send_escape(&pane.id).await;
    }
    if !fenced.workspace_id.is_empty()
        && let Err(error) = d
            .herdr
            .workspace_close(&WorkspaceId(fenced.workspace_id.clone()))
            .await
    {
        let root = store.clone();
        let message = error.to_string();
        blocking(move || root.update_record(|record| record.error = message)).await?;
        return Ok(());
    }
    // Keep the old identity until the next complete snapshot proves it is gone.
    d.log
        .line("supervisor: old pane closed; waiting for a complete snapshot before replacement");
    Ok(())
}

async fn create_or_adopt<H: Herdr>(
    d: &Deps<'_, H>,
    store: &SupervisorStore,
    mut record: SupervisorRecord,
    snapshot: &Snapshot,
    profile: &Profile,
    now: Timestamp,
) -> Result<SupervisorRecord> {
    let root = workspace_dir(store);
    set_private_dir(&root)?;
    let bin = crate::coordinator::binary_command()?;
    write_priming(&root, d.config, &record, &bin)?;
    let cwd = root.to_string_lossy().into_owned();
    let pane = manager_pane(snapshot, &record, &cwd).cloned();
    let pane = if let Some(pane) = pane {
        if record.workspace_id.is_empty() {
            let root = store.clone();
            let placed = placed(&pane, &cwd);
            record = blocking(move || persist_placement(&root, &placed, &files::now())).await?;
        }
        pane
    } else if !record.workspace_id.is_empty() {
        if snapshot.skipped != 0 {
            return Ok(record);
        }
        // A complete snapshot confirmed the previously recorded placement is gone.
        let root = store.clone();
        record = blocking(move || {
            root.update_record(|record| {
                record.workspace_id.clear();
                record.tab_id.clear();
                record.pane_id.clear();
                record.cwd.clear();
                record.agent_session.clear();
                record.status = SupervisorStatus::Starting;
            })
        })
        .await?;
        return Ok(record);
    } else {
        if snapshot.skipped != 0 {
            return Ok(record);
        }
        if record.placement_attempts >= MAX_LAUNCH_ATTEMPTS
            && !timestamp_due(&record.last_placement_at, now, LAUNCH_RETRY)
        {
            return Ok(record);
        }
        if record.placement_attempts >= MAX_LAUNCH_ATTEMPTS {
            let root = store.clone();
            record = blocking(move || {
                root.update_record(|record| {
                    record.status = SupervisorStatus::Blocked;
                    record.error = "supervisor placement failed repeatedly; reload or repair the Herdr session".into();
                })
            })
            .await?;
            return Ok(record);
        }
        let root = store.clone();
        blocking(move || {
            root.update_record(|record| {
                record.placement_attempts = record.placement_attempts.saturating_add(1);
                record.last_placement_at = now.to_string();
                record.status = SupervisorStatus::Starting;
            })
        })
        .await?;
        match d.herdr.workspace_create(&cwd, LABEL).await {
            Ok(placement) => {
                let root = store.clone();
                let persisted_placement = placement.clone();
                record =
                    blocking(move || persist_placement(&root, &persisted_placement, &files::now()))
                        .await?;
                let placed_pane = Pane {
                    id: placement.pane.clone(),
                    workspace: placement.workspace.clone(),
                    tab: placement.tab.clone(),
                    terminal: String::new(),
                    cwd: Some(cwd.clone()),
                    foreground_cwd: Some(cwd.clone()),
                    label: Some(LABEL.into()),
                };
                return start_if_empty(d, store, record, snapshot, &placed_pane, profile, now)
                    .await;
            }
            Err(error) => {
                let message = error.to_string();
                let root = store.clone();
                record =
                    blocking(move || root.update_record(|record| record.error = message)).await?;
                return Ok(record);
            }
        }
    };
    start_if_empty(d, store, record, snapshot, &pane, profile, now).await
}

async fn start_if_empty<H: Herdr>(
    d: &Deps<'_, H>,
    store: &SupervisorStore,
    mut record: SupervisorRecord,
    snapshot: &Snapshot,
    pane: &Pane,
    profile: &Profile,
    now: Timestamp,
) -> Result<SupervisorRecord> {
    if let Some(agent) = snapshot.agents.iter().find(|agent| agent.pane == pane.id) {
        if agent.name.as_deref() != Some(AGENT_NAME)
            || agent.kind.as_deref() != Some(profile.kind.as_str())
        {
            let root = store.clone();
            let pane_id = pane.id.0.clone();
            return blocking(move || {
                root.update_record(|record| {
                    record.status = SupervisorStatus::Blocked;
                    record.error = format!(
                        "the dedicated supervisor pane {pane_id} contains a different agent"
                    );
                })
            })
            .await;
        }
        let native_session = agent.session.clone().unwrap_or_default();
        let state = match agent.status {
            HerdrStatus::Working => SupervisorStatus::Busy,
            HerdrStatus::Blocked | HerdrStatus::Unknown => SupervisorStatus::Blocked,
            HerdrStatus::Idle | HerdrStatus::Done => SupervisorStatus::Idle,
        };
        let root = store.clone();
        record = blocking(move || {
            root.update_record(|record| {
                if !native_session.is_empty() {
                    record.agent_session = native_session;
                }
                if record.pending_replacement.is_none() {
                    record.status = state;
                }
            })
        })
        .await?;
        let pending = blocking({
            let root = store.clone();
            let scopes = supervisor::authorized_scopes(d.config);
            move || root.pending_report_count(&scopes)
        })
        .await?;
        if record.pending_replacement.is_some() {
            return Ok(record);
        }
        if pending > 0 || record.policy_wake_pending || record.active_batch.is_some() {
            if record.active_batch.is_none() && pending > 0 && record.last_wake_at.is_empty() {
                let root = store.clone();
                record = blocking(move || root.update_record(|record| record.wake_pending = true))
                    .await?;
            }
            let Some(idle_agent) = snapshot
                .agents
                .iter()
                .find(|agent| agent.pane == pane.id && agent.status.is_idle())
            else {
                return Ok(record);
            };
            let retry_due = record.wake_pending
                || record.last_wake_at.is_empty()
                || timestamp_due(&record.last_wake_at, now, WAKE_RETRY);
            if retry_due && record.wake_attempts < MAX_WAKE_ATTEMPTS {
                let prompt = format!(
                    "[herdr-linear-agent supervisor] Review pending work. Use generation {} and finish with one durable decision.",
                    record.generation
                );
                if let Err(error) = d.herdr.agent_prompt(&idle_agent.pane, &prompt).await
                    && !matches!(error, crate::herdr::HerdrError::OutcomeUnknown(_))
                {
                    let root = store.clone();
                    let message = error.to_string();
                    return blocking(move || root.update_record(|record| record.error = message))
                        .await;
                }
                let root = store.clone();
                record = blocking(move || {
                    root.update_record(|record| {
                        record.wake_pending = false;
                        record.wake_attempts = record.wake_attempts.saturating_add(1);
                        record.last_wake_at = now.to_string();
                        record.status = SupervisorStatus::Busy;
                        record.error.clear();
                    })
                })
                .await?;
            } else if record.wake_attempts >= MAX_WAKE_ATTEMPTS && retry_due {
                let root = store.clone();
                record = blocking(move || {
                    root.update_record(|record| {
                        record.status = SupervisorStatus::Blocked;
                        record.error =
                            "supervisor did not commit its issued work after three prompts".into();
                    })
                })
                .await?;
            }
        }
        return Ok(record);
    }
    if snapshot.skipped != 0 {
        return Ok(record);
    }
    let runs_dir = d.ctx.runs_dir();
    let active_agents = blocking(move || {
        Ok(crate::worker::agent_count(&crate::run::Run::list(
            &runs_dir,
        )))
    })
    .await?;
    if active_agents.saturating_add(1) > d.config.limits.max_agents as usize {
        let root = store.clone();
        return blocking(move || {
            root.update_record(|record| {
                record.status = SupervisorStatus::Starting;
                record.error = "waiting for a reserved supervisor agent slot".into();
            })
        })
        .await;
    }
    if record.launch_attempts >= MAX_LAUNCH_ATTEMPTS
        && !timestamp_due(&record.last_launch_at, now, LAUNCH_RETRY)
    {
        return Ok(record);
    }
    if record.launch_attempts >= MAX_LAUNCH_ATTEMPTS {
        let root = store.clone();
        return blocking(move || {
            root.update_record(|record| {
                record.status = SupervisorStatus::Blocked;
                record.error = "supervisor agent did not start after three attempts".into();
            })
        })
        .await;
    }
    let (session, args) = start_args(profile, &record);
    let root = store.clone();
    record = blocking(move || {
        root.update_record(|record| {
            record.agent_session = session;
            record.launch_attempts = record.launch_attempts.saturating_add(1);
            record.last_launch_at = now.to_string();
            record.status = SupervisorStatus::Starting;
            record.error.clear();
        })
    })
    .await?;
    if let Err(error) = d
        .herdr
        .agent_start(AGENT_NAME, &profile.kind, &pane.id, &args)
        .await
    {
        let root = store.clone();
        let message = error.to_string();
        record = blocking(move || root.update_record(|record| record.error = message)).await?;
    }
    Ok(record)
}

impl Reconciler {
    pub(super) async fn supervisor<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: Option<&Snapshot>,
        now: Timestamp,
    ) {
        if let Err(error) = self.supervisor_inner(d, snapshot, now).await {
            d.log.line(&format!("supervisor: {error:#}"));
        }
    }

    async fn supervisor_inner<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: Option<&Snapshot>,
        now: Timestamp,
    ) -> Result<()> {
        let store = SupervisorStore::new(d.ctx.supervisor_dir());
        if d.config.supervisor.is_none() && !store.root().join("current.json").exists() {
            return Ok(());
        }
        let policy = if d.config.supervisor.is_some() {
            supervisor::policy_revision(d.config)?
        } else {
            String::new()
        };
        let (enabled, profile_name, fingerprint) = match &d.config.supervisor {
            Some(settings) => {
                let profile = d.config.profile(&settings.profile)?;
                (
                    true,
                    settings.profile.clone(),
                    supervisor::profile_fingerprint(profile),
                )
            }
            None => (false, String::new(), String::new()),
        };
        if !store.root().join("current.json").exists() {
            let root = store.clone();
            let profile = profile_name.clone();
            let fingerprint = fingerprint.clone();
            let session = d.socket.to_string();
            let policy = policy.clone();
            blocking(move || root.initialize(&profile, &fingerprint, &session, &policy)).await?;
        }
        let mut record = blocking({
            let root = store.clone();
            move || root.record()
        })
        .await?;

        let session_changed = enabled && !d.socket.is_empty() && record.herdr_session != d.socket;
        let profile_changed = enabled
            && (record.profile != profile_name || record.profile_fingerprint != fingerprint);
        if !enabled && record.status != SupervisorStatus::Disabled {
            let desired = replacement(
                false,
                "",
                "",
                &record.herdr_session,
                &policy,
                now,
                "supervisor was disabled",
            );
            record = blocking({
                let root = store.clone();
                move || begin_replacement(&root, desired)
            })
            .await?;
        } else if enabled && (session_changed || profile_changed) {
            let reason = if session_changed {
                "configured Herdr session changed"
            } else {
                "supervisor profile changed"
            };
            let desired = replacement(
                true,
                &profile_name,
                &fingerprint,
                d.socket,
                &policy,
                now,
                reason,
            );
            record = blocking({
                let root = store.clone();
                move || begin_replacement(&root, desired)
            })
            .await?;
        }

        if enabled
            && record.pending_replacement.is_none()
            && record.status == SupervisorStatus::Disabled
        {
            let root = store.clone();
            let profile_name = profile_name.clone();
            let fingerprint = fingerprint.clone();
            let session = d.socket.to_string();
            let policy = policy.clone();
            record = blocking(move || {
                root.update_record(|record| {
                    record.profile = profile_name;
                    record.profile_fingerprint = fingerprint;
                    record.herdr_session = session;
                    record.policy_revision = policy;
                    record.status = SupervisorStatus::Starting;
                })
            })
            .await?;
        }

        if enabled && record.pending_replacement.is_none() && record.policy_revision != policy {
            let root = store.clone();
            let policy = policy.clone();
            record = blocking(move || {
                root.update_record(|record| {
                    record.generation = record.generation.saturating_add(1);
                    record.active_batch = None;
                    record.policy_revision = policy;
                    record.policy_wake_pending = true;
                    record.wake_pending = true;
                    record.wake_attempts = 0;
                    record.status = SupervisorStatus::Idle;
                    record.error.clear();
                })
            })
            .await?;
        }

        if let Some(target) = record.pending_replacement.clone() {
            if target.herdr_session != record.herdr_session {
                close_for_session_switch(d, &store, &record, target).await?;
                return Ok(());
            }
            replace_same_session(d, &store, &record, snapshot, now).await?;
            return Ok(());
        }
        if !enabled {
            if store.root().join("current.json").exists() {
                let root = store.clone();
                let _ = blocking(move || root.repair_reports(100)).await;
            }
            return Ok(());
        }

        let root = store.clone();
        let repair_pending = blocking(move || {
            root.repair_reports(100)
                .and_then(|more| Ok(more || root.record()?.report_repair_pending))
        })
        .await?;
        if let Some(snapshot) = snapshot {
            let profile = d.config.profile(&profile_name)?;
            let current = blocking({
                let root = store.clone();
                move || root.record()
            })
            .await?;
            let current = create_or_adopt(d, &store, current, snapshot, profile, now).await?;
            if repair_pending || current.status == SupervisorStatus::Blocked {
                return Ok(());
            }
        }
        Ok(())
    }
}
