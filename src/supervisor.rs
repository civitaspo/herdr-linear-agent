//! Durable postmortem reports, supervisor decisions and portable knowledge.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, Profile};
use crate::postmortem::{SavedPostmortem, Stage};
use crate::run::{Run, RunRecord};
use crate::supervisor_index;

const MAX_REPORT_BATCH: usize = 20;
const MAX_TOPIC_BYTES: usize = 16_000;
const MAX_CONTEXT_BYTES: usize = 32_000;
const MAX_DECISION_BYTES: usize = 64_000;
const MAX_CHECKPOINT_BYTES: usize = 8_000;
const MAX_FIELD_CHARS: usize = 4_000;
const MAX_TOPIC_CHANGES: usize = 5;

/// A compact observation derived by the plugin from one completed attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorReport {
    pub schema_version: u32,
    pub id: String,
    pub source_attempt: String,
    /// SHA-256 of the saved attempt payload with this report field omitted.
    pub source_hash: String,
    pub run_key: String,
    pub issue_id: String,
    pub workspace: String,
    pub team: String,
    /// Empty only when a legacy outcome did not save its routing attribution.
    pub routing: String,
    #[serde(default)]
    pub attribution_complete: bool,
    /// Basename of a legacy outcome file when the UUID attempt format was not
    /// used. It is validated under the workspace-qualified run directory.
    #[serde(default)]
    pub legacy_source: Option<String>,
    pub stage: Stage,
    pub observed_at: String,
    pub outcome: ReportOutcome,
    pub profiles: Vec<ProfileObservation>,
    #[serde(default)]
    pub observations: RunObservations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReportOutcome {
    Success {
        summary: String,
        labels: Vec<String>,
    },
    Failure {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileObservation {
    pub role: String,
    pub name: String,
    pub kind: String,
    /// A method revision or generated profile revision when known.
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunObservations {
    pub finished: bool,
    pub closed_state: Option<String>,
    pub elapsed_seconds: Option<i64>,
    pub coordinator_waiting: bool,
    pub coordinator_waiting_episodes: Option<u32>,
    pub workers: Vec<WorkerObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerObservation {
    pub id: String,
    pub repository: String,
    pub restarts: u32,
    pub waiting_episodes: Option<u32>,
    pub report: Option<String>,
}

/// A workspace-qualified permission scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub workspace: String,
    pub team: String,
    pub routing: String,
}

pub(crate) struct ReportInput<'a> {
    pub(crate) run: &'a Run,
    pub(crate) record: &'a RunRecord,
    pub(crate) stage: Stage,
    pub(crate) attempt: &'a str,
    pub(crate) at: &'a str,
    pub(crate) routing: &'a str,
    pub(crate) outcome: ReportOutcome,
    pub(crate) postmortem_profile: Option<ProfileObservation>,
}

/// Builds a compact report and deliberately omits issue bodies and transcripts.
pub(crate) fn report_from_run(input: ReportInput<'_>) -> SupervisorReport {
    let ReportInput {
        run,
        record,
        stage,
        attempt,
        at,
        routing,
        outcome,
        postmortem_profile,
    } = input;
    let mut profiles = vec![ProfileObservation {
        role: "coordinator".into(),
        name: record.coordinator.profile.clone(),
        kind: record.coordinator.kind.clone(),
        revision: generated_revision(&record.coordinator.profile),
    }];
    let workers = crate::worker::list(run);
    profiles.extend(workers.iter().map(|worker| ProfileObservation {
        role: format!("worker:{}", worker.id),
        name: worker.agent.profile.clone(),
        kind: worker.agent.kind.clone(),
        revision: generated_revision(&worker.agent.profile),
    }));
    if let Some(profile) = postmortem_profile {
        profiles.push(profile);
    }
    SupervisorReport {
        schema_version: 1,
        id: report_id(&run.key, attempt),
        source_attempt: attempt.into(),
        source_hash: String::new(),
        run_key: run.key.clone(),
        issue_id: record.issue_id.clone(),
        workspace: record.workspace.clone(),
        team: record.team_key.clone(),
        routing: routing.into(),
        attribution_complete: true,
        legacy_source: None,
        stage,
        observed_at: at.into(),
        outcome,
        profiles,
        observations: RunObservations {
            finished: record.finished,
            closed_state: (!record.closed_state.is_empty()).then(|| record.closed_state.clone()),
            elapsed_seconds: at.parse::<Timestamp>().ok().and_then(|ended| {
                record
                    .created
                    .parse::<Timestamp>()
                    .ok()
                    .map(|started| ended.duration_since(started).as_secs())
            }),
            coordinator_waiting: record.awaiting_reply.is_some(),
            coordinator_waiting_episodes: record.coordinator.waiting_episodes,
            workers: workers
                .into_iter()
                .map(|worker| {
                    let report =
                        fs::read_to_string(crate::worker::home_report_path(run, &worker.id))
                            .ok()
                            .filter(|report| !report.trim().is_empty())
                            .map(|report| clip_text(&report, 4000));
                    WorkerObservation {
                        id: worker.id,
                        repository: worker.repo,
                        restarts: worker.restarts,
                        waiting_episodes: worker.agent.waiting_episodes,
                        report,
                    }
                })
                .collect(),
        },
    }
}

fn generated_revision(name: &str) -> Option<String> {
    name.starts_with("generated-")
        .then(|| {
            name.rsplit_once("-r")
                .map(|(_, revision)| revision.to_string())
        })
        .flatten()
}

/// Stable across feed rebuilds, while keeping equal outcomes from distinct
/// workspace-qualified runs and attempts separate.
pub fn report_id(run_key: &str, attempt: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(run_key.as_bytes());
    hash.update([0]);
    hash.update(attempt.as_bytes());
    let digest = hash.finalize();
    format!(
        "r-{}",
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Hashes the original saved-attempt fields without recursively including the
/// report which carries this hash.
pub fn source_hash(artifact: &SavedPostmortem) -> Result<String> {
    let mut source = artifact.clone();
    source.supervisor_report = None;
    Ok(crate::files::sha256_hex(&serde_json::to_vec(&source)?))
}

fn legacy_source_path(run: &Run, basename: &str) -> Result<PathBuf> {
    ensure!(
        !basename.is_empty()
            && basename.len() <= 160
            && basename.ends_with(".json")
            && basename
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') }),
        "invalid legacy postmortem source name"
    );
    Ok(run.state_dir().join("postmortems").join(basename))
}

/// Reads a historical postmortem outcome while assigning an immutable local
/// identity. Routing and run-time observations absent from the old artifact
/// remain explicitly unavailable and cannot enter an authorized batch.
fn legacy_artifact(run: &Run, basename: &str) -> Result<SavedPostmortem> {
    let path = legacy_source_path(run, basename)?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("could not inspect legacy outcome {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "legacy postmortem source is not a regular file"
    );
    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read legacy outcome {}", path.display()))?;
    let mut artifact: SavedPostmortem = serde_json::from_str(&text)
        .with_context(|| format!("{} does not parse", path.display()))?;
    ensure!(
        artifact.supervisor_report.is_none(),
        "legacy source unexpectedly contains a supervisor report"
    );
    ensure!(
        artifact.run_key.is_empty() || artifact.run_key == run.key,
        "legacy postmortem belongs to another run"
    );
    artifact.run_key.clone_from(&run.key);
    if artifact.attempt_id.is_empty() {
        let digest = crate::files::sha256_hex(basename.as_bytes());
        artifact.attempt_id = format!("legacy-{}", &digest[..24]);
    }
    if artifact.schema_version == 0 {
        artifact.schema_version = 1;
    }
    ensure!(
        artifact.stage.is_some(),
        "legacy postmortem has no recorded stage"
    );
    Ok(artifact)
}

fn legacy_report(
    run: &Run,
    artifact: &SavedPostmortem,
    basename: &str,
) -> Result<SupervisorReport> {
    let record = run.record()?;
    let stage = artifact
        .stage
        .context("legacy postmortem has no recorded stage")?;
    let outcome = if let Some(error) = &artifact.failure {
        ReportOutcome::Failure {
            error: error.clone(),
        }
    } else {
        ReportOutcome::Success {
            summary: artifact.summary.clone(),
            labels: artifact.labels.clone(),
        }
    };
    let profiles = (!artifact.method.is_empty())
        .then(|| ProfileObservation {
            role: "postmortem".into(),
            name: artifact.method.clone(),
            kind: artifact.method_kind.clone(),
            revision: (!artifact.version.is_empty()).then(|| artifact.version.clone()),
        })
        .into_iter()
        .collect();
    Ok(SupervisorReport {
        schema_version: 1,
        id: report_id(&run.key, &artifact.attempt_id),
        source_attempt: artifact.attempt_id.clone(),
        source_hash: source_hash(artifact)?,
        run_key: run.key.clone(),
        issue_id: record.issue_id,
        workspace: record.workspace,
        team: record.team_key,
        routing: String::new(),
        attribution_complete: false,
        legacy_source: Some(basename.into()),
        stage,
        observed_at: artifact.at.clone(),
        outcome,
        profiles,
        observations: RunObservations::default(),
    })
}

/// Current authorities available to the supervisor.
pub fn authorized_scopes(config: &Config) -> Vec<Scope> {
    let mut scopes = Vec::new();
    for (workspace, value) in &config.workspaces {
        for (team, value) in &value.teams {
            scopes.push(Scope {
                workspace: workspace.clone(),
                team: team.clone(),
                routing: value.routing.clone(),
            });
        }
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

/// Returns the explicitly configured profile bases for current scopes.
pub fn authorized_profile_policies(config: &Config) -> Result<Vec<ProfilePolicyView>> {
    let max_generated_profiles = config
        .supervisor
        .as_ref()
        .map_or(5, |settings| settings.max_generated_profiles_per_routing);
    authorized_scopes(config)
        .into_iter()
        .map(|scope| {
            let routing = config.routing_of(&scope.workspace, &scope.team)?;
            let mut base_names = routing.generated.coordinators.clone();
            base_names.extend(routing.generated.workers.iter().cloned());
            base_names.extend(routing.generated.postmortems.iter().cloned());
            base_names.sort();
            base_names.dedup();
            let base_fingerprints = base_names
                .into_iter()
                .map(|name| Ok((name.clone(), profile_fingerprint(config.profile(&name)?))))
                .collect::<Result<BTreeMap<_, _>>>()?;
            Ok(ProfilePolicyView {
                scope,
                coordinator_bases: routing.generated.coordinators.clone(),
                worker_bases: routing.generated.workers.clone(),
                postmortem_bases: routing.generated.postmortems.clone(),
                base_fingerprints,
                max_generated_profiles,
            })
        })
        .collect()
}

/// A stable fingerprint for the resolved human-owned profile used by the
/// supervisor. It includes inherited instructions and launch settings.
pub fn profile_fingerprint(profile: &Profile) -> String {
    let value = serde_json::json!({
        "kind": profile.kind,
        "model": profile.model,
        "effort": profile.effort,
        "args": profile.args,
        "description": profile.description,
        "instructions": profile.instructions.iter().map(|item| (&item.profile, &item.text)).collect::<Vec<_>>(),
        "env": profile.env,
        "timeout_seconds": profile.timeout_seconds,
    });
    crate::files::sha256_hex(&serde_json::to_vec(&value).unwrap_or_default())
}

fn profile_export_equivalent(left: &Profile, right: &Profile) -> bool {
    left.kind == right.kind
        && left.model == right.model
        && left.effort == right.effort
        && left.args == right.args
        && left.description == right.description
        && left.env == right.env
        && left.timeout_seconds == right.timeout_seconds
        && left
            .instructions
            .iter()
            .map(|layer| &layer.text)
            .eq(right.instructions.iter().map(|layer| &layer.text))
}

fn memory_queries(
    reports: &[SupervisorReport],
    checkpoint: Option<&SupervisorCheckpoint>,
) -> Vec<String> {
    let mut texts = Vec::new();
    for report in reports {
        match &report.outcome {
            ReportOutcome::Success { summary, .. } => texts.push(summary.as_str()),
            ReportOutcome::Failure { error } => texts.push(error.as_str()),
        }
        texts.extend(report.profiles.iter().map(|profile| profile.name.as_str()));
        texts.extend(
            report
                .observations
                .workers
                .iter()
                .filter_map(|worker| worker.report.as_deref()),
        );
    }
    if let Some(checkpoint) = checkpoint {
        texts.push(checkpoint.summary.as_str());
    }
    let mut queries = Vec::new();
    for text in texts {
        for token in text.split_whitespace() {
            let token = token.trim_matches(|char: char| !char.is_alphanumeric());
            let chars: Vec<_> = token.chars().collect();
            if chars.len() < 3 {
                continue;
            }
            if chars.iter().all(|char| char.is_ascii_alphanumeric()) {
                if !queries.iter().any(|query| query == token) {
                    queries.push(token.to_string());
                }
            } else {
                for window in chars.windows(3).step_by(3) {
                    let query: String = window.iter().collect();
                    if !queries.contains(&query) {
                        queries.push(query);
                    }
                }
            }
            if queries.len() >= 12 {
                return queries;
            }
        }
    }
    queries
}

fn topic_profiles_match(profiles: &[String], reports: &[SupervisorReport]) -> bool {
    if profiles.is_empty() {
        return true;
    }
    let observed: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| &report.profiles)
        .map(|profile| profile.name.as_str())
        .collect();
    observed.is_empty()
        || profiles
            .iter()
            .any(|profile| observed.contains(profile.as_str()))
}

/// Hash of human-owned supervisor and generated-profile policy.
pub fn policy_revision(config: &Config) -> Result<String> {
    let Some(supervisor) = &config.supervisor else {
        bail!("[supervisor] is not configured");
    };
    let value = serde_json::json!({
        "max_generated_profiles_per_routing": supervisor.max_generated_profiles_per_routing,
        "profile_bases": authorized_profile_policies(config)?,
        "routing": config.routing.iter().map(|(name, routing)| {
            (name, &routing.generated)
        }).collect::<Vec<_>>(),
        "workspaces": config.workspaces.iter().map(|(name, workspace)| {
            (name, workspace.teams.iter().map(|(key, team)| (key, &team.routing)).collect::<Vec<_>>())
        }).collect::<Vec<_>>(),
    });
    Ok(crate::files::sha256_hex(&serde_json::to_vec(&value)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportEnvelope {
    pub schema_version: u32,
    pub report_hash: String,
    pub report: SupervisorReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedEntry {
    pub id: String,
    pub report_hash: String,
    pub observed_at: String,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReportFeed {
    pub schema_version: u32,
    pub reports: Vec<FeedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryManifest {
    pub schema_version: u32,
    pub generation: u64,
    pub topics: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogManifest {
    pub schema_version: u32,
    pub generation: u64,
    pub profiles: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedProfileStatus {
    Active,
    Retired,
}

/// One immutable generated profile revision. `profile` is a fully resolved
/// snapshot so a later edit to its manual base cannot change an existing pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedProfileRevision {
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub role: crate::config::GeneratedRole,
    pub scope: Scope,
    pub base_profile: String,
    pub base_fingerprint: String,
    pub description: String,
    pub instructions_md: String,
    pub source_report_ids: Vec<String>,
    pub status: GeneratedProfileStatus,
    pub retirement_reason: Option<String>,
    pub decision_id: String,
    pub updated_at: String,
    pub profile: Profile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCatalogEntry {
    pub revision: GeneratedProfileRevision,
    /// True when this revision is selected by the committed catalog pointer.
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileChange {
    Create {
        id: String,
        role: crate::config::GeneratedRole,
        scope: Scope,
        base_profile: String,
        description: String,
        instructions_md: String,
        source_report_ids: Vec<String>,
    },
    Revise {
        id: String,
        expected_revision: u64,
        #[serde(default)]
        base_profile: Option<String>,
        description: String,
        instructions_md: String,
        source_report_ids: Vec<String>,
    },
    Retire {
        id: String,
        expected_revision: u64,
        reason: String,
        source_report_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorCommit {
    pub schema_version: u32,
    pub sequence: u64,
    pub previous_decision: Option<String>,
    pub decision_id: Option<String>,
    pub decision_hash: Option<String>,
    pub checkpoint_id: Option<String>,
    pub memory_generation: u64,
    pub catalog_generation: u64,
    pub policy_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorCheckpoint {
    pub schema_version: u32,
    pub decision_id: String,
    pub summary: String,
    pub pending_report_ids: Vec<String>,
    pub unresolved_questions: Vec<String>,
    pub intended_checks: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorStatus {
    #[default]
    Starting,
    Idle,
    Busy,
    Draining,
    Closing,
    Blocked,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorRecord {
    pub schema_version: u32,
    pub generation: u64,
    pub status: SupervisorStatus,
    pub profile: String,
    pub profile_fingerprint: String,
    pub herdr_session: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub agent_session: String,
    pub policy_revision: String,
    pub active_batch: Option<String>,
    pub wake_pending: bool,
    pub wake_attempts: u8,
    pub last_wake_at: String,
    #[serde(default)]
    pub launch_attempts: u8,
    #[serde(default)]
    pub last_launch_at: String,
    #[serde(default)]
    pub placement_attempts: u8,
    #[serde(default)]
    pub last_placement_at: String,
    pub pending_replacement: Option<PendingReplacement>,
    pub last_decision_id: String,
    pub report_repair_cursor: String,
    pub report_repair_pending: bool,
    #[serde(default)]
    pub policy_wake_pending: bool,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingReplacement {
    pub enabled: bool,
    pub profile: String,
    pub fingerprint: String,
    pub herdr_session: String,
    pub policy_revision: String,
    pub deadline: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorBatch {
    pub schema_version: u32,
    pub id: String,
    pub generation: u64,
    pub commit_sequence: u64,
    pub policy_revision: String,
    pub issued_at: String,
    pub report_ids: Vec<String>,
    #[serde(default)]
    pub remaining_report_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorContext {
    pub schema_version: u32,
    pub generation: u64,
    pub batch: SupervisorBatch,
    pub reports: Vec<SupervisorReport>,
    pub remaining_report_count: usize,
    pub checkpoint: Option<SupervisorCheckpoint>,
    pub profile_policies: Vec<ProfilePolicyView>,
    pub active_profiles: Vec<ActiveProfileView>,
    pub omitted_active_profile_count: usize,
    pub recent_decisions: Vec<RecentDecisionView>,
    pub omitted_recent_decision_count: usize,
    pub knowledge: Vec<KnowledgeView>,
}

/// Human-owned generated-profile permissions relevant to a report batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePolicyView {
    pub scope: Scope,
    pub coordinator_bases: Vec<String>,
    pub worker_bases: Vec<String>,
    pub postmortem_bases: Vec<String>,
    pub base_fingerprints: BTreeMap<String, String>,
    pub max_generated_profiles: u32,
}

impl ProfilePolicyView {
    fn allows_base_name(&self, role: crate::config::GeneratedRole, name: &str) -> bool {
        let allowed = match role {
            crate::config::GeneratedRole::Coordinator => &self.coordinator_bases,
            crate::config::GeneratedRole::Worker => &self.worker_bases,
            crate::config::GeneratedRole::Postmortem => &self.postmortem_bases,
        };
        allowed.iter().any(|base| base == name)
    }

    fn base_fingerprint_is_current(&self, name: &str, fingerprint: &str) -> bool {
        self.base_fingerprints
            .get(name)
            .is_some_and(|value| value == fingerprint)
    }
}

/// Compact current catalog entry. Full instruction text is available by ID
/// and revision through the read-only `supervisor profiles show` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveProfileView {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub role: crate::config::GeneratedRole,
    pub scope: Scope,
    pub base_profile: String,
    pub eligible: bool,
    pub ineligibility_reason: Option<String>,
    pub description: String,
    pub instructions_preview: String,
}

/// Bounded history of recent report-backed decisions in the current scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentDecisionView {
    pub id: String,
    pub sequence: u64,
    pub reason: String,
    pub expected_result: String,
    pub consumed_report_ids: Vec<String>,
    pub profile_change: Option<String>,
}

struct ContextEnvironment<'a> {
    scopes: &'a BTreeSet<Scope>,
    profile_policies: &'a [ProfilePolicyView],
    feed: &'a ReportFeed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeView {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub scope: Scope,
    pub status: KnowledgeStatus,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Active,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeMetadata {
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub content_hash: String,
    pub scope: Scope,
    pub repositories: Vec<String>,
    pub profiles: Vec<String>,
    pub source_report_ids: Vec<String>,
    pub status: KnowledgeStatus,
    pub supersedes: Vec<String>,
    pub decision_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action", deny_unknown_fields)]
pub enum TopicChange {
    Create {
        id: String,
        title: String,
        scope: Scope,
        repositories: Vec<String>,
        profiles: Vec<String>,
        source_report_ids: Vec<String>,
        content_md: String,
    },
    Revise {
        id: String,
        expected_revision: u64,
        title: String,
        repositories: Vec<String>,
        profiles: Vec<String>,
        source_report_ids: Vec<String>,
        content_md: String,
    },
    Retire {
        id: String,
        expected_revision: u64,
        reason: String,
        source_report_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub report_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionProposal {
    pub schema_version: u32,
    pub id: String,
    pub batch_id: String,
    pub base_commit_sequence: u64,
    pub policy_revision: String,
    pub consumed_report_ids: Vec<String>,
    pub observations: Vec<Observation>,
    pub reason: String,
    pub expected_result: String,
    pub topic_changes: Vec<TopicChange>,
    #[serde(default)]
    pub profile_change: Option<ProfileChange>,
    pub checkpoint: SupervisorCheckpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionActor {
    Agent { generation: u64, batch_id: String },
    Operator { operation: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorDecision {
    pub schema_version: u32,
    pub id: String,
    pub sequence: u64,
    pub previous_decision: Option<String>,
    pub actor: DecisionActor,
    pub base_commit_sequence: u64,
    pub policy_revision: String,
    pub batch_id: Option<String>,
    pub consumed_report_ids: Vec<String>,
    pub observations: Vec<Observation>,
    pub reason: String,
    pub expected_result: String,
    pub topic_changes: Vec<TopicChange>,
    #[serde(default)]
    pub profile_change: Option<ProfileChange>,
    pub checkpoint_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionReceipt {
    pub decision_id: String,
    pub commit_sequence: u64,
    pub acknowledged_report_ids: Vec<String>,
    pub memory_generation: u64,
    pub catalog_generation: u64,
    pub projections_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ReceiptSnapshot {
    schema_version: u32,
    commit_sequence: u64,
    acknowledged_report_ids: BTreeSet<String>,
}

/// Filesystem-backed supervisor state. The current pointer is the only
/// authority for committed decisions, knowledge and generated catalogs.
#[derive(Debug, Clone)]
pub struct SupervisorStore {
    root: PathBuf,
}

struct SupervisorLock {
    _file: File,
}

impl SupervisorStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> Result<SupervisorLock> {
        ensure_private_dir(&self.root)?;
        let path = self.root.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("could not open supervisor lock {}", path.display()))?;
        file.lock()?;
        Ok(SupervisorLock { _file: file })
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("current.json")
    }

    fn record_path(&self) -> PathBuf {
        self.root.join("record.json")
    }

    fn current_unlocked(&self) -> Result<SupervisorCommit> {
        let path = self.current_path();
        let commit: SupervisorCommit = read_required(&path)?;
        ensure!(
            commit.schema_version == 1,
            "unsupported supervisor commit schema"
        );
        let memory = self.memory_manifest_unlocked(commit.memory_generation)?;
        ensure!(
            memory.generation == commit.memory_generation,
            "supervisor memory manifest generation does not match current.json"
        );
        let catalog = self.catalog_manifest_unlocked(commit.catalog_generation)?;
        ensure!(
            catalog.generation == commit.catalog_generation,
            "supervisor catalog manifest generation does not match current.json"
        );
        let history: SupervisorCommit = read_required(&self.commit_path(commit.sequence))?;
        ensure!(
            history == commit,
            "supervisor commit history does not match current.json"
        );
        if let Some(id) = &commit.decision_id {
            validate_uuid(id)?;
            let decision: SupervisorDecision = read_required(&self.decision_path(id))?;
            let bytes = serde_json::to_vec(&decision)?;
            ensure!(
                Some(crate::files::sha256_hex(&bytes)) == commit.decision_hash,
                "current supervisor decision hash does not match"
            );
            ensure!(
                decision.sequence == commit.sequence,
                "decision sequence mismatch"
            );
        }
        if let Some(id) = &commit.checkpoint_id {
            validate_component(id)?;
            let _: SupervisorCheckpoint = read_required(&self.checkpoint_path(id))?;
        }
        Ok(commit)
    }

    /// Loads the immutable profile snapshots that existing run records may pin.
    pub fn profile_revisions(&self) -> Result<Vec<GeneratedProfileRevision>> {
        let _lock = self.lock()?;
        let current = self.current_unlocked()?;
        let mut committed = BTreeSet::new();
        for sequence in 0..=current.sequence {
            let commit: SupervisorCommit = read_required(&self.commit_path(sequence))?;
            ensure!(
                commit.sequence == sequence,
                "supervisor commit history sequence mismatch"
            );
            let manifest = self.catalog_manifest_unlocked(commit.catalog_generation)?;
            committed.extend(manifest.profiles);
        }
        let mut revisions = committed
            .into_iter()
            .map(|(id, revision)| self.read_profile_revision(&id, revision))
            .collect::<Result<Vec<_>>>()?;
        revisions.sort_by(|a, b| (&a.id, a.revision).cmp(&(&b.id, b.revision)));
        Ok(revisions)
    }

    /// Lists only committed immutable revisions and marks each profile's
    /// revision selected by the current catalog.
    pub fn profile_catalog(&self) -> Result<Vec<ProfileCatalogEntry>> {
        let _lock = self.lock()?;
        let current = self.current_unlocked()?;
        let active = self.catalog_manifest_unlocked(current.catalog_generation)?;
        let mut committed = BTreeSet::new();
        for sequence in 0..=current.sequence {
            let commit: SupervisorCommit = read_required(&self.commit_path(sequence))?;
            ensure!(
                commit.sequence == sequence,
                "supervisor commit history sequence mismatch"
            );
            let manifest = self.catalog_manifest_unlocked(commit.catalog_generation)?;
            committed.extend(manifest.profiles);
        }
        committed
            .into_iter()
            .map(|(id, revision)| {
                Ok(ProfileCatalogEntry {
                    current: active.profiles.get(&id) == Some(&revision),
                    revision: self.read_profile_revision(&id, revision)?,
                })
            })
            .collect()
    }

    /// Exports one immutable revision as a regular manual profile directory.
    /// The exact destination is created atomically and must not exist.
    pub fn export_profile(
        &self,
        config: &Config,
        id: &str,
        revision: u64,
        name: &str,
        destination: &Path,
    ) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name.len() <= 96
                && name != "."
                && name != ".."
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                })
                && !name.starts_with("generated-"),
            "export profile name must use letters, digits, `.`, `_` or `-` and must not start with `generated-`"
        );
        let entry = self
            .profile_catalog()?
            .into_iter()
            .find(|entry| entry.revision.id == id && entry.revision.revision == revision)
            .context("generated profile revision is not committed")?;
        let generated = entry.revision;
        ensure!(
            generated_bases(config, &generated.scope, generated.role)?
                .contains(&generated.base_profile),
            "generated profile base is no longer authorized"
        );
        let base = config.profile(&generated.base_profile)?;
        ensure!(
            profile_fingerprint(base) == generated.base_fingerprint,
            "generated profile base changed; export is refused"
        );
        let mut resolved = base.clone();
        resolved.description.clone_from(&generated.description);
        resolved.instructions.push(crate::config::Instructions {
            profile: name.into(),
            text: generated.instructions_md.clone(),
        });
        ensure!(
            profile_export_equivalent(&generated.profile, &resolved),
            "exported profile would not match the generated revision"
        );

        #[derive(Serialize)]
        struct ExportConfig<'a> {
            base: &'a str,
            description: &'a str,
        }
        let config_text = toml::to_string(&ExportConfig {
            base: &generated.base_profile,
            description: &generated.description,
        })?;
        let provenance = format!(
            "# Generated profile provenance\n\n- Name: `{name}`\n- Source: `{}@{}`\n- Role: `{:?}`\n- Scope: `{} / {} / {}`\n- Base: `{}`\n- Base fingerprint: `{}`\n- Instruction SHA-256: `{}`\n- Decision: `{}`\n",
            generated.id,
            generated.revision,
            generated.role,
            generated.scope.workspace,
            generated.scope.team,
            generated.scope.routing,
            generated.base_profile,
            generated.base_fingerprint,
            crate::files::sha256_hex(generated.instructions_md.as_bytes()),
            generated.decision_id,
        );
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        ensure!(
            parent.is_dir(),
            "export parent directory does not exist: {}",
            parent.display()
        );
        ensure!(
            fs::symlink_metadata(destination).is_err(),
            "export destination already exists: {}",
            destination.display()
        );
        let staging = parent.join(format!(".profile-export-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&staging).with_context(|| {
            format!(
                "could not create export staging folder {}",
                staging.display()
            )
        })?;
        let write_result = (|| -> Result<()> {
            fs::write(staging.join("config.toml"), config_text)?;
            fs::write(staging.join("instructions.md"), &generated.instructions_md)?;
            fs::write(staging.join("README.md"), provenance)?;
            fs::rename(&staging, destination).with_context(|| {
                format!(
                    "could not publish profile export to {}",
                    destination.display()
                )
            })?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        write_result
    }

    /// Commits an operator catalog transition through the same manifest pointer
    /// as agent decisions. Operator changes never acknowledge report receipts.
    pub fn operator_profile_change(
        &self,
        config: &Config,
        change: ProfileChange,
        operation: &str,
        reason: &str,
        now: &str,
    ) -> Result<DecisionReceipt> {
        validate_profile_change(&change)?;
        ensure!(
            operation.chars().count() <= 120 && !operation.trim().is_empty(),
            "operator operation is invalid"
        );
        ensure!(
            reason.chars().count() <= MAX_FIELD_CHARS && !reason.trim().is_empty(),
            "operator reason is empty or too long"
        );
        let policy = policy_revision(config)?;
        let _lock = self.lock()?;
        let current = self.current_unlocked()?;
        ensure!(
            current.policy_revision == policy,
            "supervisor policy changed; reload before changing profiles"
        );
        let mut record = self.record_unlocked()?;
        ensure!(
            record.policy_revision == policy,
            "supervisor policy changed; reload before changing profiles"
        );
        ensure!(
            record.active_batch.is_none(),
            "the supervisor has an active report batch; wait for it to finish"
        );
        let scopes: BTreeSet<_> = authorized_scopes(config).into_iter().collect();
        let (scope, mut base_profile, source_report_ids) = match &change {
            ProfileChange::Create {
                scope,
                base_profile,
                source_report_ids,
                ..
            } => (scope.clone(), base_profile.clone(), source_report_ids),
            ProfileChange::Revise {
                id,
                source_report_ids,
                ..
            }
            | ProfileChange::Retire {
                id,
                source_report_ids,
                ..
            } => {
                let manifest = self.catalog_manifest_unlocked(current.catalog_generation)?;
                let revision = manifest
                    .profiles
                    .get(id)
                    .copied()
                    .context("generated profile does not exist")?;
                let profile = self.read_profile_revision(id, revision)?;
                ensure!(
                    profile.status == GeneratedProfileStatus::Active,
                    "generated profile is already retired"
                );
                (profile.scope, profile.base_profile, source_report_ids)
            }
        };
        if let ProfileChange::Revise {
            base_profile: Some(next_base),
            ..
        } = &change
        {
            base_profile.clone_from(next_base);
        }
        ensure!(scopes.contains(&scope), "profile scope is not authorized");
        ensure!(
            generated_bases(
                config,
                &scope,
                match &change {
                    ProfileChange::Create { role, .. } => *role,
                    ProfileChange::Revise { id, .. } | ProfileChange::Retire { id, .. } => {
                        let manifest =
                            self.catalog_manifest_unlocked(current.catalog_generation)?;
                        let profile = self.read_profile_revision(
                            id,
                            *manifest
                                .profiles
                                .get(id)
                                .context("generated profile does not exist")?,
                        )?;
                        profile.role
                    }
                }
            )?
            .contains(&base_profile),
            "generated profile base is no longer authorized"
        );
        if !source_report_ids.is_empty() {
            let feed: ReportFeed = read_required(&self.root.join("feed.json"))?;
            for report_id in source_report_ids {
                let entry = feed
                    .reports
                    .iter()
                    .find(|entry| &entry.id == report_id)
                    .context("source report is missing")?;
                ensure!(
                    entry.scope == scope,
                    "source report is outside profile scope"
                );
                self.read_report_unlocked(entry)?;
            }
        }

        let id = uuid::Uuid::new_v4().to_string();
        let checkpoint = SupervisorCheckpoint {
            schema_version: 1,
            decision_id: id.clone(),
            summary: reason.to_string(),
            pending_report_ids: Vec::new(),
            unresolved_questions: Vec::new(),
            intended_checks: Vec::new(),
        };
        let decision = SupervisorDecision {
            schema_version: 1,
            id: id.clone(),
            sequence: current.sequence + 1,
            previous_decision: current.decision_id.clone(),
            actor: DecisionActor::Operator {
                operation: operation.to_string(),
            },
            base_commit_sequence: current.sequence,
            policy_revision: policy.clone(),
            batch_id: None,
            consumed_report_ids: Vec::new(),
            observations: Vec::new(),
            reason: reason.to_string(),
            expected_result: "the catalog reflects the operator's explicit profile change".into(),
            topic_changes: Vec::new(),
            profile_change: Some(change),
            checkpoint_id: id.clone(),
        };
        write_json_immutable(&self.decision_path(&id), &decision)?;
        write_json_immutable(&self.checkpoint_path(&id), &checkpoint)?;
        let memory = self.next_memory_manifest(&current, &decision, now)?;
        let catalog = self.next_catalog_manifest(&current, &decision, config, now)?;
        let commit = SupervisorCommit {
            schema_version: 1,
            sequence: decision.sequence,
            previous_decision: current.decision_id.clone(),
            decision_id: Some(id.clone()),
            decision_hash: Some(crate::files::sha256_hex(&serde_json::to_vec(&decision)?)),
            checkpoint_id: Some(id.clone()),
            memory_generation: memory.generation,
            catalog_generation: catalog.generation,
            policy_revision: policy,
        };
        write_json_immutable(&self.commit_path(commit.sequence), &commit)?;
        crate::files::write_json(&self.current_path(), &commit)?;
        let mut receipts = self.receipts_unlocked(&current)?;
        receipts.commit_sequence = commit.sequence;
        crate::files::write_json(&self.root.join("receipts.json"), &receipts)?;
        record.last_decision_id = id.clone();
        record.policy_wake_pending = true;
        crate::files::write_json(&self.record_path(), &record)?;
        Ok(DecisionReceipt {
            decision_id: id,
            commit_sequence: commit.sequence,
            acknowledged_report_ids: Vec::new(),
            memory_generation: memory.generation,
            catalog_generation: commit.catalog_generation,
            projections_pending: true,
        })
    }

    fn read_profile_revision(&self, id: &str, revision: u64) -> Result<GeneratedProfileRevision> {
        validate_profile_id(id)?;
        let dir = self.generated_profile_dir(id, revision);
        let profile: GeneratedProfileRevision = read_required(&dir.join("profile.json"))?;
        ensure!(
            profile.schema_version == 1,
            "unsupported generated profile schema"
        );
        ensure!(
            profile.id == id && profile.revision == revision,
            "generated profile identity mismatch"
        );
        ensure!(
            profile.name == generated_profile_name(id, revision),
            "generated profile name mismatch"
        );
        ensure!(
            profile.instructions_md.len() <= MAX_TOPIC_BYTES,
            "generated instructions are too long"
        );
        ensure!(
            profile.profile.description == profile.description,
            "generated profile description mismatch"
        );
        ensure!(
            profile.profile.instructions.last().is_some_and(
                |layer| layer.profile == profile.name && layer.text == profile.instructions_md
            ),
            "generated profile instructions mismatch"
        );
        ensure!(
            fs::read_to_string(dir.join("instructions.md"))? == profile.instructions_md,
            "generated profile instruction file does not match metadata"
        );
        Ok(profile)
    }

    /// Current variants eligible for new work in one exact scope and role.
    pub fn active_profile_names(
        &self,
        config: &Config,
        scope: &Scope,
        role: crate::config::GeneratedRole,
    ) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let commit = self.current_unlocked()?;
        let manifest = self.catalog_manifest_unlocked(commit.catalog_generation)?;
        let allowed = generated_bases(config, scope, role)?;
        let mut active = Vec::new();
        for (id, revision) in manifest.profiles {
            let profile = self.read_profile_revision(&id, revision)?;
            if profile.status != GeneratedProfileStatus::Active
                || profile.scope != *scope
                || profile.role != role
                || !allowed.contains(&profile.base_profile)
            {
                continue;
            }
            let base = config.profile(&profile.base_profile)?;
            if profile.base_fingerprint != profile_fingerprint(base) {
                continue;
            }
            active.push(profile.name);
        }
        active.sort();
        let limit = config
            .supervisor
            .as_ref()
            .map_or(5, |s| s.max_generated_profiles_per_routing) as usize;
        ensure!(
            active.len() <= limit,
            "routing `{}` has {} active generated profiles; limit is {limit}",
            scope.routing,
            active.len()
        );
        Ok(active)
    }

    /// Whether a pinned generated profile is still authorized to launch or resume.
    pub fn profile_is_authorized(
        &self,
        config: &Config,
        name: &str,
        scope: &Scope,
        role: crate::config::GeneratedRole,
        require_active: bool,
    ) -> Result<bool> {
        let _lock = self.lock()?;
        let commit = self.current_unlocked()?;
        let manifest = self.catalog_manifest_unlocked(commit.catalog_generation)?;
        for (id, revision) in manifest.profiles {
            let latest = self.read_profile_revision(&id, revision)?;
            if latest.name == name {
                let allowed = generated_bases(config, scope, role)?;
                let current_base = config.profile(&latest.base_profile)?;
                return Ok(
                    (!require_active || latest.status == GeneratedProfileStatus::Active)
                        && (!require_active
                            || latest.base_fingerprint == profile_fingerprint(current_base))
                        && latest.scope == *scope
                        && latest.role == role
                        && allowed.contains(&latest.base_profile),
                );
            }
            if latest.id == id {
                for pin in 1..revision {
                    let pinned = self.read_profile_revision(&id, pin)?;
                    if pinned.name == name {
                        if require_active {
                            return Ok(false);
                        }
                        let allowed = generated_bases(config, scope, role)?;
                        return Ok(pinned.scope == *scope
                            && pinned.role == role
                            && allowed.contains(&pinned.base_profile));
                    }
                }
            }
        }
        Ok(false)
    }

    pub fn record(&self) -> Result<SupervisorRecord> {
        let _lock = self.lock()?;
        self.record_unlocked()
    }

    pub fn current(&self) -> Result<SupervisorCommit> {
        let _lock = self.lock()?;
        self.current_unlocked()
    }

    /// Reads only the atomic commit pointer to let the ticker detect catalog
    /// changes without reloading every generated profile on every pass.
    pub fn catalog_generation_hint(&self) -> Result<Option<u64>> {
        let path = self.current_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let commit: SupervisorCommit = serde_json::from_slice(&bytes)
            .with_context(|| format!("could not parse {}", path.display()))?;
        ensure!(
            commit.schema_version == 1,
            "unsupported supervisor commit schema"
        );
        Ok(Some(commit.catalog_generation))
    }

    pub fn update_record(
        &self,
        change: impl FnOnce(&mut SupervisorRecord),
    ) -> Result<SupervisorRecord> {
        let _lock = self.lock()?;
        let mut record = self.record_unlocked()?;
        change(&mut record);
        crate::files::write_json(&self.record_path(), &record)?;
        Ok(record)
    }

    pub fn pending_report_count(&self, scopes: &[Scope]) -> Result<usize> {
        let _lock = self.lock()?;
        let commit = self.current_unlocked()?;
        let receipts = self.receipts_unlocked(&commit)?;
        let feed: ReportFeed = read_required(&self.root.join("feed.json"))?;
        let allowed: BTreeSet<_> = scopes.iter().cloned().collect();
        Ok(feed
            .reports
            .iter()
            .filter(|entry| {
                allowed.contains(&entry.scope)
                    && !receipts.acknowledged_report_ids.contains(&entry.id)
            })
            .count())
    }

    fn record_unlocked(&self) -> Result<SupervisorRecord> {
        read_required(&self.record_path())
    }

    /// Creates the empty generation only when the state has no current
    /// pointer. A malformed pointer remains a recovery error.
    pub fn initialize(
        &self,
        profile: &str,
        profile_fingerprint: &str,
        herdr_session: &str,
        policy_revision: &str,
    ) -> Result<()> {
        let _lock = self.lock()?;
        if self.current_path().exists() {
            let current = self.current_unlocked()?;
            let feed_path = self.root.join("feed.json");
            let mut repair_missing_feed = false;
            if !feed_path.exists() {
                crate::files::write_json(
                    &feed_path,
                    &ReportFeed {
                        schema_version: 1,
                        reports: Vec::new(),
                    },
                )?;
                repair_missing_feed = true;
            } else {
                let feed: ReportFeed = read_required(&feed_path)?;
                ensure!(
                    feed.schema_version == 1,
                    "unsupported supervisor feed schema"
                );
            }
            let receipt_path = self.root.join("receipts.json");
            if !receipt_path.exists() {
                self.receipts_unlocked(&current)?;
            } else {
                let receipt: ReceiptSnapshot = read_required(&receipt_path)?;
                if receipt.commit_sequence != current.sequence {
                    self.receipts_unlocked(&current)?;
                }
            }
            if !self.record_path().exists() {
                write_json_immutable(
                    &self.record_path(),
                    &SupervisorRecord {
                        schema_version: 1,
                        generation: 1,
                        status: SupervisorStatus::Starting,
                        profile: profile.into(),
                        profile_fingerprint: profile_fingerprint.into(),
                        herdr_session: herdr_session.into(),
                        policy_revision: policy_revision.into(),
                        report_repair_pending: true,
                        ..SupervisorRecord::default()
                    },
                )?;
            } else {
                let mut record = self.record_unlocked()?;
                if repair_missing_feed {
                    record.report_repair_pending = true;
                    record.report_repair_cursor.clear();
                    crate::files::write_json(&self.record_path(), &record)?;
                }
            }
            return Ok(());
        }
        for dir in [
            "commits",
            "decisions",
            "checkpoints",
            "batches",
            "reports",
            "memory/manifests",
            "memory/topics",
            "generated-profiles/manifests",
            "generated-profiles/revisions",
        ] {
            ensure_private_dir(&self.root.join(dir))?;
        }
        let memory = MemoryManifest {
            schema_version: 1,
            generation: 0,
            topics: BTreeMap::new(),
        };
        let catalog = CatalogManifest {
            schema_version: 1,
            generation: 0,
            profiles: BTreeMap::new(),
        };
        write_json_immutable(&self.memory_manifest_path(0), &memory)?;
        write_json_immutable(&self.catalog_manifest_path(0), &catalog)?;
        let commit = SupervisorCommit {
            schema_version: 1,
            sequence: 0,
            previous_decision: None,
            decision_id: None,
            decision_hash: None,
            checkpoint_id: None,
            memory_generation: 0,
            catalog_generation: 0,
            policy_revision: policy_revision.into(),
        };
        write_json_immutable(&self.commit_path(0), &commit)?;
        crate::files::write_json(&self.current_path(), &commit)?;
        write_json_immutable(
            &self.record_path(),
            &SupervisorRecord {
                schema_version: 1,
                generation: 1,
                status: SupervisorStatus::Starting,
                profile: profile.into(),
                profile_fingerprint: profile_fingerprint.into(),
                herdr_session: herdr_session.into(),
                policy_revision: policy_revision.into(),
                report_repair_pending: true,
                ..SupervisorRecord::default()
            },
        )?;
        let feed_path = self.root.join("feed.json");
        if !feed_path.exists() {
            write_json_immutable(
                &feed_path,
                &ReportFeed {
                    schema_version: 1,
                    reports: Vec::new(),
                },
            )?;
        }
        let receipts_path = self.root.join("receipts.json");
        if !receipts_path.exists() {
            write_json_immutable(
                &receipts_path,
                &ReceiptSnapshot {
                    schema_version: 1,
                    commit_sequence: 0,
                    acknowledged_report_ids: BTreeSet::new(),
                },
            )?;
        }
        Ok(())
    }

    /// Adds one immutable report file and updates the compact feed index.
    /// This is called after releasing the run lock.
    pub fn materialize_report(&self, artifact: &SavedPostmortem) -> Result<()> {
        let report = artifact
            .supervisor_report
            .as_ref()
            .context("saved postmortem has no supervisor report")?;
        ensure!(
            artifact.schema_version >= 1,
            "unsupported postmortem schema"
        );
        ensure!(
            artifact.run_key == report.run_key,
            "report run key mismatch"
        );
        ensure!(
            artifact.attempt_id == report.source_attempt,
            "report attempt mismatch"
        );
        ensure!(
            report.id == report_id(&report.run_key, &report.source_attempt),
            "report id mismatch"
        );
        let source_hash = source_hash(artifact)?;
        let mut report = report.clone();
        report.source_hash = source_hash;
        let mut envelope = ReportEnvelope {
            schema_version: 1,
            report_hash: String::new(),
            report,
        };
        let report_bytes = serde_json::to_vec(&envelope.report)?;
        envelope.report_hash = crate::files::sha256_hex(&report_bytes);
        let _lock = self.lock()?;
        let reports_dir = self.root.join("reports");
        ensure_private_dir(&reports_dir)?;
        let report_path = self.report_path(&envelope.report.id)?;
        write_json_immutable(&report_path, &envelope)?;
        let feed_path = self.root.join("feed.json");
        let mut feed = if feed_path.exists() {
            read_required::<ReportFeed>(&feed_path)?
        } else {
            ReportFeed {
                schema_version: 1,
                reports: Vec::new(),
            }
        };
        ensure!(
            feed.schema_version == 1,
            "unsupported supervisor feed schema"
        );
        let entry = FeedEntry {
            id: envelope.report.id.clone(),
            report_hash: envelope.report_hash.clone(),
            observed_at: envelope.report.observed_at.clone(),
            scope: Scope {
                workspace: envelope.report.workspace.clone(),
                team: envelope.report.team.clone(),
                routing: envelope.report.routing.clone(),
            },
        };
        if let Some(existing) = feed.reports.iter().find(|known| known.id == entry.id) {
            ensure!(
                existing == &entry,
                "report feed identity already has different content"
            );
            return Ok(());
        }
        feed.reports.push(entry);
        feed.reports
            .sort_by(|a, b| (&a.observed_at, &a.id).cmp(&(&b.observed_at, &b.id)));
        crate::files::write_json(&feed_path, &feed)
    }

    pub fn report(&self, id: &str) -> Result<SupervisorReport> {
        let _lock = self.lock()?;
        let envelope: ReportEnvelope = read_required(&self.report_path(id)?)?;
        ensure!(
            envelope.schema_version == 1,
            "unsupported supervisor report schema"
        );
        ensure!(envelope.report.id == id, "supervisor report id mismatch");
        let bytes = serde_json::to_vec(&envelope.report)?;
        ensure!(
            crate::files::sha256_hex(&bytes) == envelope.report_hash,
            "supervisor report hash does not match"
        );
        drop(_lock);
        let run = Run::load(&self.runs_dir(), &envelope.report.run_key)?;
        let artifact = if let Some(source) = &envelope.report.legacy_source {
            legacy_artifact(&run, source)?
        } else {
            crate::postmortem::saved_attempt(&run, &envelope.report.source_attempt)?
                .context("supervisor report source artifact is missing")?
        };
        ensure!(
            envelope.report.source_attempt == artifact.attempt_id,
            "supervisor report source attempt does not match"
        );
        ensure!(
            source_hash(&artifact)? == envelope.report.source_hash,
            "supervisor report source hash does not match"
        );
        Ok(envelope.report)
    }

    pub fn report_in_scopes(&self, id: &str, scopes: &[Scope]) -> Result<SupervisorReport> {
        let report = self.report(id)?;
        ensure!(
            report.attribution_complete,
            "legacy report has unknown routing attribution and cannot be used for a decision"
        );
        let scope = Scope {
            workspace: report.workspace.clone(),
            team: report.team.clone(),
            routing: report.routing.clone(),
        };
        ensure!(
            scopes.contains(&scope),
            "supervisor report is outside the current authorization scope"
        );
        Ok(report)
    }

    pub fn decision_in_scopes(&self, id: &str, scopes: &[Scope]) -> Result<SupervisorDecision> {
        let _lock = self.lock()?;
        let current = self.current_unlocked()?;
        ensure!(
            self.decision_is_committed_unlocked(&current, id)?,
            "supervisor decision is not committed"
        );
        let decision = self.read_decision_unlocked(id)?;
        let feed: ReportFeed = read_required(&self.root.join("feed.json"))?;
        let allowed: BTreeSet<_> = scopes.iter().cloned().collect();
        for report_id in &decision.consumed_report_ids {
            let entry = feed
                .reports
                .iter()
                .find(|entry| &entry.id == report_id)
                .context("decision report is missing from the feed")?;
            ensure!(
                allowed.contains(&entry.scope),
                "supervisor decision is outside the current authorization scope"
            );
        }
        Ok(decision)
    }

    /// Reconciles one bounded slice of saved postmortems into the derived
    /// report feed. Callers repeat while this returns `true`.
    pub fn repair_reports(&self, limit: usize) -> Result<bool> {
        let record = self.record()?;
        if !record.report_repair_pending {
            return Ok(false);
        }
        let mut artifacts = Vec::<(String, String, PathBuf)>::new();
        for run in Run::list(&self.runs_dir()) {
            let dir = run.state_dir().join("postmortems");
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    let cursor = format!("{}/{}", run.key, entry.file_name().to_string_lossy());
                    artifacts.push((cursor, run.key.clone(), path));
                }
            }
        }
        artifacts.sort_by(|a, b| a.0.cmp(&b.0));
        let remaining: Vec<_> = artifacts
            .into_iter()
            .filter(|(cursor, _, _)| {
                record.report_repair_cursor.is_empty() || cursor > &record.report_repair_cursor
            })
            .collect();
        let limit = limit.max(1);
        let more = remaining.len() > limit;
        let selected: Vec<_> = remaining.into_iter().take(limit).collect();
        let mut last_cursor = String::new();
        for (cursor, run_key, path) in &selected {
            last_cursor.clone_from(cursor);
            let text = fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let artifact: SavedPostmortem = serde_json::from_str(&text)
                .with_context(|| format!("{} does not parse", path.display()))?;
            if artifact.supervisor_report.is_some() {
                self.materialize_report(&artifact)?;
            } else {
                let run = Run::load(&self.runs_dir(), run_key)?;
                let basename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("legacy postmortem source name is not UTF-8")?;
                let mut artifact = legacy_artifact(&run, basename)?;
                artifact.supervisor_report = Some(legacy_report(&run, &artifact, basename)?);
                self.materialize_report(&artifact)?;
            }
        }
        let _lock = self.lock()?;
        let mut record = self.record_unlocked()?;
        if more {
            record.report_repair_cursor = last_cursor;
        } else {
            record.report_repair_cursor.clear();
            record.report_repair_pending = false;
        }
        crate::files::write_json(&self.record_path(), &record)?;
        Ok(more)
    }

    fn receipts_unlocked(&self, commit: &SupervisorCommit) -> Result<ReceiptSnapshot> {
        let path = self.root.join("receipts.json");
        if path.exists() {
            let snapshot: ReceiptSnapshot = read_required(&path)?;
            if snapshot.commit_sequence == commit.sequence {
                return Ok(snapshot);
            }
            ensure!(
                snapshot.commit_sequence < commit.sequence,
                "receipt index is ahead of the committed supervisor state"
            );
        }
        let mut snapshot = ReceiptSnapshot {
            schema_version: 1,
            commit_sequence: 0,
            acknowledged_report_ids: BTreeSet::new(),
        };
        for sequence in 1..=commit.sequence {
            let historical: SupervisorCommit = read_required(&self.commit_path(sequence))?;
            if let Some(id) = historical.decision_id {
                let decision: SupervisorDecision = read_required(&self.decision_path(&id))?;
                ensure!(
                    decision.sequence == sequence && decision.id == id,
                    "supervisor decision history is inconsistent"
                );
                snapshot
                    .acknowledged_report_ids
                    .extend(decision.consumed_report_ids);
            }
            snapshot.commit_sequence = sequence;
        }
        crate::files::write_json(&path, &snapshot)?;
        Ok(snapshot)
    }

    fn read_decision_unlocked(&self, id: &str) -> Result<SupervisorDecision> {
        validate_uuid(id)?;
        let decision: SupervisorDecision = read_required(&self.decision_path(id))?;
        ensure!(decision.id == id, "supervisor decision id mismatch");
        Ok(decision)
    }

    fn read_topic_unlocked(&self, id: &str, revision: u64) -> Result<(KnowledgeMetadata, String)> {
        validate_component(id)?;
        let dir = self
            .root
            .join("memory/topics")
            .join(id)
            .join(format!("{revision:020}"));
        let metadata: KnowledgeMetadata = read_required(&dir.join("metadata.json"))?;
        ensure!(
            metadata.schema_version == 1,
            "unsupported knowledge metadata schema"
        );
        ensure!(
            metadata.id == id && metadata.revision == revision,
            "knowledge identity mismatch"
        );
        let content = fs::read_to_string(dir.join("content.md"))?;
        ensure!(
            content.len() <= MAX_TOPIC_BYTES,
            "knowledge topic exceeds its size limit"
        );
        ensure!(
            crate::files::sha256_hex(content.as_bytes()) == metadata.content_hash,
            "knowledge content hash does not match"
        );
        Ok((metadata, content))
    }

    /// Returns a committed topic revision only. Unreferenced staged folders
    /// are never visible through this method.
    pub fn topic(&self, id: &str, revision: Option<u64>) -> Result<(KnowledgeMetadata, String)> {
        let _lock = self.lock()?;
        let commit = self.current_unlocked()?;
        let manifest = self.memory_manifest_unlocked(commit.memory_generation)?;
        let revision = revision
            .or_else(|| manifest.topics.get(id).copied())
            .context("topic is not in the committed memory")?;
        ensure!(
            manifest
                .topics
                .get(id)
                .is_some_and(|current| *current >= revision),
            "topic revision is not committed"
        );
        self.read_topic_unlocked(id, revision)
    }

    /// Commits a batch decision and its portable memory through `current.json`.
    /// The report receipt file and SQLite index are projections and can be
    /// rebuilt after a successful pointer swap.
    pub fn decide(
        &self,
        generation: u64,
        policy: &str,
        scopes: &[Scope],
        config: &Config,
        proposal: DecisionProposal,
    ) -> Result<DecisionReceipt> {
        validate_uuid(&proposal.id)?;
        ensure!(
            proposal.schema_version == 1,
            "unsupported decision proposal schema"
        );
        ensure!(
            proposal.reason.chars().count() <= MAX_FIELD_CHARS,
            "decision reason is too long"
        );
        ensure!(
            proposal.expected_result.chars().count() <= MAX_FIELD_CHARS,
            "expected result is too long"
        );
        ensure!(
            proposal.topic_changes.len() <= MAX_TOPIC_CHANGES,
            "too many topic changes in one decision"
        );
        if let Some(change) = &proposal.profile_change {
            validate_profile_change(change)?;
        }
        ensure!(
            serde_json::to_vec(&proposal)?.len() <= MAX_DECISION_BYTES,
            "decision proposal exceeds {MAX_DECISION_BYTES} bytes"
        );
        ensure!(
            proposal.observations.len() <= MAX_REPORT_BATCH,
            "too many observations in one decision"
        );
        ensure!(
            proposal.consumed_report_ids.len() <= MAX_REPORT_BATCH,
            "too many reports in one decision"
        );
        ensure!(
            proposal.checkpoint.summary.chars().count() <= MAX_FIELD_CHARS,
            "checkpoint summary is too long"
        );
        ensure!(
            serde_json::to_vec(&proposal.checkpoint)?.len() <= MAX_CHECKPOINT_BYTES,
            "checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
        );
        ensure!(
            proposal.checkpoint.pending_report_ids.len() <= MAX_REPORT_BATCH,
            "too many pending report ids in checkpoint"
        );
        ensure!(
            proposal.checkpoint.unresolved_questions.len() <= 20,
            "too many unresolved questions"
        );
        ensure!(
            proposal.checkpoint.intended_checks.len() <= 20,
            "too many intended checks"
        );
        for item in proposal
            .checkpoint
            .unresolved_questions
            .iter()
            .chain(&proposal.checkpoint.intended_checks)
        {
            ensure!(
                item.chars().count() <= MAX_FIELD_CHARS,
                "checkpoint item is too long"
            );
        }
        let consumed: BTreeSet<_> = proposal.consumed_report_ids.iter().cloned().collect();
        ensure!(
            consumed.len() == proposal.consumed_report_ids.len(),
            "duplicate consumed report id"
        );
        let observation_ids: BTreeSet<_> = proposal
            .observations
            .iter()
            .map(|item| item.report_id.clone())
            .collect();
        ensure!(
            observation_ids.len() == proposal.observations.len(),
            "duplicate observation report id"
        );
        ensure!(
            observation_ids.is_subset(&consumed),
            "observations must refer to consumed reports"
        );
        for observation in &proposal.observations {
            ensure!(
                observation.text.chars().count() <= MAX_FIELD_CHARS,
                "observation is too long"
            );
        }
        let allowed_scopes: BTreeSet<_> = scopes.iter().cloned().collect();
        let _lock = self.lock()?;
        let current = self.current_unlocked()?;
        let mut record = self.record_unlocked()?;

        if self.decision_is_committed_unlocked(&current, &proposal.id)? {
            let decision = self.read_decision_unlocked(&proposal.id)?;
            ensure!(
                decision_matches_proposal(&decision, &proposal, generation),
                "decision id is already committed with different content"
            );
            let checkpoint: SupervisorCheckpoint =
                read_required(&self.checkpoint_path(&decision.checkpoint_id))?;
            ensure!(
                checkpoint_matches_proposal(&checkpoint, &proposal),
                "decision id is already committed with a different checkpoint"
            );
            let historical: SupervisorCommit = read_required(&self.commit_path(decision.sequence))?;
            return Ok(DecisionReceipt {
                decision_id: decision.id,
                commit_sequence: decision.sequence,
                acknowledged_report_ids: decision.consumed_report_ids,
                memory_generation: historical.memory_generation,
                catalog_generation: historical.catalog_generation,
                projections_pending: self.receipts_unlocked(&current)?.commit_sequence
                    != current.sequence,
            });
        }
        ensure!(
            record.generation == generation,
            "supervisor generation is stale"
        );
        ensure!(
            record.policy_revision == policy && proposal.policy_revision == policy,
            "supervisor policy changed; reload before deciding"
        );
        ensure!(
            proposal.base_commit_sequence == current.sequence,
            "decision is based on a stale commit; refresh context"
        );
        ensure!(
            record.active_batch.as_deref() == Some(&proposal.batch_id),
            "decision does not match the active batch"
        );
        let batch: SupervisorBatch = read_required(&self.batch_path(&proposal.batch_id))?;
        ensure!(
            batch.id == proposal.batch_id && batch.generation == generation,
            "decision batch is stale"
        );
        ensure!(
            batch.policy_revision == policy && batch.commit_sequence == current.sequence,
            "decision batch is based on stale policy or state"
        );
        let batch_ids: BTreeSet<_> = batch.report_ids.iter().cloned().collect();
        ensure!(
            consumed.is_subset(&batch_ids),
            "decision consumes a report that was not shown in this batch"
        );
        ensure!(
            consumed == batch_ids,
            "decision must account for every report in the issued batch"
        );
        let feed: ReportFeed = read_required(&self.root.join("feed.json"))?;
        for id in &consumed {
            let entry = feed
                .reports
                .iter()
                .find(|entry| &entry.id == id)
                .context("consumed report is missing from the feed")?;
            ensure!(
                allowed_scopes.contains(&entry.scope),
                "report is no longer authorized by current policy"
            );
            self.read_report_unlocked(entry)?;
        }
        for change in &proposal.topic_changes {
            let source_ids = match change {
                TopicChange::Create {
                    scope,
                    source_report_ids,
                    ..
                } => {
                    ensure!(
                        allowed_scopes.contains(scope),
                        "knowledge scope is not authorized by current policy"
                    );
                    source_report_ids
                }
                TopicChange::Revise {
                    id,
                    source_report_ids,
                    ..
                }
                | TopicChange::Retire {
                    id,
                    source_report_ids,
                    ..
                } => {
                    let manifest = self.memory_manifest_unlocked(current.memory_generation)?;
                    let revision = manifest
                        .topics
                        .get(id)
                        .copied()
                        .context("knowledge topic does not exist")?;
                    let (metadata, _) = self.read_topic_unlocked(id, revision)?;
                    ensure!(
                        allowed_scopes.contains(&metadata.scope),
                        "knowledge scope is not authorized by current policy"
                    );
                    source_report_ids
                }
            };
            ensure!(
                source_ids.iter().all(|id| consumed.contains(id)),
                "topic evidence must come from reports consumed by this decision"
            );
        }

        if let Some(change) = &proposal.profile_change {
            let source_ids = match change {
                ProfileChange::Create {
                    scope,
                    source_report_ids,
                    ..
                } => {
                    ensure!(
                        allowed_scopes.contains(scope),
                        "generated profile scope is not authorized"
                    );
                    source_report_ids
                }
                ProfileChange::Revise {
                    id,
                    source_report_ids,
                    ..
                }
                | ProfileChange::Retire {
                    id,
                    source_report_ids,
                    ..
                } => {
                    let manifest = self.catalog_manifest_unlocked(current.catalog_generation)?;
                    let revision = manifest
                        .profiles
                        .get(id)
                        .copied()
                        .context("generated profile does not exist")?;
                    let existing = self.read_profile_revision(id, revision)?;
                    ensure!(
                        existing.status == GeneratedProfileStatus::Active,
                        "generated profile is already retired"
                    );
                    ensure!(
                        allowed_scopes.contains(&existing.scope),
                        "generated profile scope is no longer authorized"
                    );
                    source_report_ids
                }
            };
            ensure!(
                !source_ids.is_empty(),
                "generated profile changes need report evidence"
            );
            ensure!(
                source_ids.iter().all(|id| consumed.contains(id)),
                "generated profile evidence must come from reports consumed by this decision"
            );
            let (scope, role, base_profile) = match change {
                ProfileChange::Create {
                    scope,
                    role,
                    base_profile,
                    ..
                } => (scope.clone(), *role, base_profile.clone()),
                ProfileChange::Revise {
                    id, base_profile, ..
                } => {
                    let manifest = self.catalog_manifest_unlocked(current.catalog_generation)?;
                    let revision = manifest
                        .profiles
                        .get(id)
                        .copied()
                        .context("generated profile does not exist")?;
                    let existing = self.read_profile_revision(id, revision)?;
                    ensure!(
                        allowed_scopes.contains(&existing.scope),
                        "generated profile scope is no longer authorized"
                    );
                    (
                        existing.scope,
                        existing.role,
                        base_profile.clone().unwrap_or(existing.base_profile),
                    )
                }
                ProfileChange::Retire { id, .. } => {
                    let manifest = self.catalog_manifest_unlocked(current.catalog_generation)?;
                    let revision = manifest
                        .profiles
                        .get(id)
                        .copied()
                        .context("generated profile does not exist")?;
                    let existing = self.read_profile_revision(id, revision)?;
                    ensure!(
                        allowed_scopes.contains(&existing.scope),
                        "generated profile scope is no longer authorized"
                    );
                    (existing.scope, existing.role, existing.base_profile)
                }
            };
            ensure!(
                generated_bases(config, &scope, role)?.contains(&base_profile),
                "generated profile base is not authorized for this role"
            );
            ensure!(
                !base_profile.starts_with("generated-"),
                "generated profiles cannot inherit from generated profiles"
            );
            config.profile(&base_profile)?;
            for id in source_ids {
                let entry = feed
                    .reports
                    .iter()
                    .find(|entry| &entry.id == id)
                    .context("generated profile evidence is missing from the feed")?;
                ensure!(
                    entry.scope == scope,
                    "generated profile evidence crosses authorization scopes"
                );
            }
            if let ProfileChange::Create {
                role,
                scope,
                base_profile,
                ..
            } = change
            {
                ensure!(
                    generated_bases(config, scope, *role)?.contains(base_profile),
                    "generated profile base is not allowed for this role"
                );
                ensure!(
                    !base_profile.starts_with("generated-"),
                    "generated profiles cannot inherit from generated profiles"
                );
                config.profile(base_profile)?;
            }
        }

        let staged_decision = if self.decision_path(&proposal.id).exists() {
            let decision = self.read_decision_unlocked(&proposal.id)?;
            ensure!(
                decision_matches_proposal(&decision, &proposal, generation),
                "staged decision id has different content"
            );
            ensure!(
                decision.sequence == current.sequence + 1
                    && decision.previous_decision == current.decision_id,
                "staged decision is based on a different commit"
            );
            let checkpoint: SupervisorCheckpoint =
                read_required(&self.checkpoint_path(&decision.checkpoint_id))?;
            ensure!(
                checkpoint_matches_proposal(&checkpoint, &proposal),
                "staged decision id has a different checkpoint"
            );
            decision
        } else {
            let mut checkpoint = proposal.checkpoint.clone();
            checkpoint.schema_version = 1;
            checkpoint.decision_id = proposal.id.clone();
            let decision = SupervisorDecision {
                schema_version: 1,
                id: proposal.id.clone(),
                sequence: current.sequence + 1,
                previous_decision: current.decision_id.clone(),
                actor: DecisionActor::Agent {
                    generation,
                    batch_id: proposal.batch_id.clone(),
                },
                base_commit_sequence: proposal.base_commit_sequence,
                policy_revision: policy.into(),
                batch_id: Some(proposal.batch_id.clone()),
                consumed_report_ids: proposal.consumed_report_ids.clone(),
                observations: proposal.observations.clone(),
                reason: proposal.reason.clone(),
                expected_result: proposal.expected_result.clone(),
                topic_changes: proposal.topic_changes.clone(),
                profile_change: proposal.profile_change.clone(),
                checkpoint_id: proposal.id.clone(),
            };
            write_json_immutable(&self.decision_path(&proposal.id), &decision)?;
            write_json_immutable(&self.checkpoint_path(&proposal.id), &checkpoint)?;
            decision
        };
        let memory = self.next_memory_manifest(&current, &staged_decision, &batch.issued_at)?;
        let catalog =
            self.next_catalog_manifest(&current, &staged_decision, config, &batch.issued_at)?;
        let commit = SupervisorCommit {
            schema_version: 1,
            sequence: staged_decision.sequence,
            previous_decision: current.decision_id.clone(),
            decision_id: Some(staged_decision.id.clone()),
            decision_hash: Some(crate::files::sha256_hex(&serde_json::to_vec(
                &staged_decision,
            )?)),
            checkpoint_id: Some(staged_decision.checkpoint_id.clone()),
            memory_generation: memory.generation,
            catalog_generation: catalog.generation,
            policy_revision: policy.into(),
        };
        write_json_immutable(&self.commit_path(commit.sequence), &commit)?;
        crate::files::write_json(&self.current_path(), &commit)?;

        let mut receipts = self.receipts_unlocked(&current)?;
        receipts.commit_sequence = commit.sequence;
        receipts.acknowledged_report_ids.extend(consumed);
        crate::files::write_json(&self.root.join("receipts.json"), &receipts)?;
        record.active_batch = None;
        record.status = if record.pending_replacement.is_some() {
            SupervisorStatus::Draining
        } else {
            SupervisorStatus::Idle
        };
        record.last_decision_id = staged_decision.id.clone();
        record.wake_pending = false;
        record.wake_attempts = 0;
        record.last_wake_at.clear();
        crate::files::write_json(&self.record_path(), &record)?;

        Ok(DecisionReceipt {
            decision_id: staged_decision.id,
            commit_sequence: commit.sequence,
            acknowledged_report_ids: staged_decision.consumed_report_ids,
            memory_generation: memory.generation,
            catalog_generation: commit.catalog_generation,
            projections_pending: true,
        })
    }

    /// Rebuilds the disposable search index from the committed memory view.
    pub fn rebuild_memory_index(&self) -> Result<u64> {
        let (generation, documents) = {
            let _lock = self.lock()?;
            let commit = self.current_unlocked()?;
            let manifest = self.memory_manifest_unlocked(commit.memory_generation)?;
            let mut documents = Vec::new();
            for (id, revision) in &manifest.topics {
                let (metadata, content) = self.read_topic_unlocked(id, *revision)?;
                if metadata.status == KnowledgeStatus::Active {
                    documents.push(supervisor_index::Document {
                        id: id.clone(),
                        revision: *revision,
                        workspace: metadata.scope.workspace,
                        team: metadata.scope.team,
                        routing: metadata.scope.routing,
                        title: metadata.title,
                        body: content,
                    });
                }
            }
            (manifest.generation, documents)
        };
        supervisor_index::rebuild(
            &self.root.join("memory/index.sqlite3"),
            generation,
            &documents,
        )
    }

    /// Searches committed knowledge in caller-provided current scopes. The
    /// caller can rebuild once when the returned index generation is stale.
    pub fn search_memory(
        &self,
        scopes: &[Scope],
        query: &str,
        limit: usize,
    ) -> Result<supervisor_index::SearchResults> {
        let (generation, allowed) = {
            let _lock = self.lock()?;
            let commit = self.current_unlocked()?;
            (
                commit.memory_generation,
                scopes.iter().cloned().collect::<BTreeSet<_>>(),
            )
        };
        let mut combined = Vec::new();
        let mut indexed_generation = None;
        for scope in allowed {
            let result = supervisor_index::search(
                &self.root.join("memory/index.sqlite3"),
                generation,
                &supervisor_index::Scope {
                    workspace: scope.workspace,
                    team: scope.team,
                    routing: scope.routing,
                },
                query,
                limit,
            )?;
            indexed_generation = Some(result.indexed_generation);
            combined.extend(result.results);
        }
        combined.truncate(limit.clamp(1, 50));
        Ok(supervisor_index::SearchResults {
            indexed_generation: indexed_generation.unwrap_or(generation),
            results: combined,
        })
    }

    fn topic_dir(&self, id: &str, revision: u64) -> Result<PathBuf> {
        validate_component(id)?;
        Ok(self
            .root
            .join("memory/topics")
            .join(id)
            .join(format!("{revision:020}")))
    }

    fn generated_profile_dir(&self, id: &str, revision: u64) -> PathBuf {
        self.root
            .join("generated-profiles/revisions")
            .join(id)
            .join(format!("{revision:020}"))
    }

    fn decision_is_committed_unlocked(&self, current: &SupervisorCommit, id: &str) -> Result<bool> {
        let mut sequence = current.sequence;
        let mut expected = current.decision_id.clone();
        while let Some(decision_id) = expected {
            if decision_id == id {
                return Ok(true);
            }
            if sequence == 0 {
                return Ok(false);
            }
            let historical: SupervisorCommit = read_required(&self.commit_path(sequence))?;
            expected = historical.previous_decision;
            sequence -= 1;
        }
        Ok(false)
    }

    fn next_memory_manifest(
        &self,
        current: &SupervisorCommit,
        decision: &SupervisorDecision,
        now: &str,
    ) -> Result<MemoryManifest> {
        let mut manifest = self.memory_manifest_unlocked(current.memory_generation)?;
        manifest.schema_version = 1;
        manifest.generation = current.memory_generation + 1;
        for change in &decision.topic_changes {
            match change {
                TopicChange::Create {
                    id,
                    title,
                    scope,
                    repositories,
                    profiles,
                    source_report_ids,
                    content_md,
                } => {
                    validate_topic_fields(
                        id,
                        title,
                        content_md,
                        repositories,
                        profiles,
                        source_report_ids,
                    )?;
                    ensure!(
                        !manifest.topics.contains_key(id),
                        "knowledge topic `{id}` already exists"
                    );
                    let metadata = KnowledgeMetadata {
                        schema_version: 1,
                        id: id.clone(),
                        revision: 1,
                        title: title.clone(),
                        content_hash: crate::files::sha256_hex(content_md.as_bytes()),
                        scope: scope.clone(),
                        repositories: repositories.clone(),
                        profiles: profiles.clone(),
                        source_report_ids: source_report_ids.clone(),
                        status: KnowledgeStatus::Active,
                        supersedes: Vec::new(),
                        decision_id: decision.id.clone(),
                        updated_at: now.into(),
                    };
                    self.stage_topic(&metadata, content_md)?;
                    manifest.topics.insert(id.clone(), 1);
                }
                TopicChange::Revise {
                    id,
                    expected_revision,
                    title,
                    repositories,
                    profiles,
                    source_report_ids,
                    content_md,
                } => {
                    validate_topic_fields(
                        id,
                        title,
                        content_md,
                        repositories,
                        profiles,
                        source_report_ids,
                    )?;
                    let current_revision = manifest
                        .topics
                        .get(id)
                        .copied()
                        .context("knowledge topic does not exist")?;
                    ensure!(
                        current_revision == *expected_revision,
                        "knowledge topic revision changed; refresh context"
                    );
                    let (previous, _) = self.read_topic_unlocked(id, current_revision)?;
                    ensure!(
                        previous.status == KnowledgeStatus::Active,
                        "retired knowledge cannot be revised"
                    );
                    let revision = current_revision + 1;
                    let metadata = KnowledgeMetadata {
                        schema_version: 1,
                        id: id.clone(),
                        revision,
                        title: title.clone(),
                        content_hash: crate::files::sha256_hex(content_md.as_bytes()),
                        scope: previous.scope.clone(),
                        repositories: repositories.clone(),
                        profiles: profiles.clone(),
                        source_report_ids: source_report_ids.clone(),
                        status: KnowledgeStatus::Active,
                        supersedes: vec![format!("{id}@{current_revision}")],
                        decision_id: decision.id.clone(),
                        updated_at: now.into(),
                    };
                    self.stage_topic(&metadata, content_md)?;
                    manifest.topics.insert(id.clone(), revision);
                }
                TopicChange::Retire {
                    id,
                    expected_revision,
                    reason,
                    source_report_ids,
                } => {
                    ensure!(
                        reason.chars().count() <= MAX_FIELD_CHARS,
                        "retirement reason is too long"
                    );
                    ensure!(
                        source_report_ids.len() <= MAX_TOPIC_CHANGES,
                        "too many source reports"
                    );
                    let current_revision = manifest
                        .topics
                        .get(id)
                        .copied()
                        .context("knowledge topic does not exist")?;
                    ensure!(
                        current_revision == *expected_revision,
                        "knowledge topic revision changed; refresh context"
                    );
                    let (mut metadata, content) = self.read_topic_unlocked(id, current_revision)?;
                    ensure!(
                        metadata.status == KnowledgeStatus::Active,
                        "knowledge topic is already retired"
                    );
                    metadata.revision += 1;
                    metadata.status = KnowledgeStatus::Retired;
                    metadata.source_report_ids = source_report_ids.clone();
                    metadata.supersedes = vec![format!("{id}@{current_revision}")];
                    metadata.decision_id = decision.id.clone();
                    metadata.updated_at = now.into();
                    self.stage_topic(&metadata, &content)?;
                    manifest.topics.insert(id.clone(), metadata.revision);
                }
            }
        }
        let path = self.memory_manifest_path(manifest.generation);
        write_json_immutable(&path, &manifest)?;
        Ok(manifest)
    }

    fn next_catalog_manifest(
        &self,
        current: &SupervisorCommit,
        decision: &SupervisorDecision,
        config: &Config,
        now: &str,
    ) -> Result<CatalogManifest> {
        let mut manifest = self.catalog_manifest_unlocked(current.catalog_generation)?;
        let Some(change) = &decision.profile_change else {
            return Ok(manifest);
        };
        let (id, revision) = match change {
            ProfileChange::Create {
                id,
                role,
                scope,
                base_profile,
                description,
                instructions_md,
                source_report_ids,
            } => {
                validate_profile_id(id)?;
                ensure!(
                    !manifest.profiles.contains_key(id),
                    "generated profile id already exists"
                );
                let active_count = manifest.profiles.iter().try_fold(
                    0usize,
                    |count, (known_id, known_revision)| {
                        let known = self.read_profile_revision(known_id, *known_revision)?;
                        Ok::<_, anyhow::Error>(
                            count
                                + usize::from(
                                    known.scope.routing == scope.routing
                                        && known.status == GeneratedProfileStatus::Active,
                                ),
                        )
                    },
                )?;
                let max = config
                    .supervisor
                    .as_ref()
                    .map_or(5, |s| s.max_generated_profiles_per_routing)
                    as usize;
                ensure!(
                    active_count < max,
                    "routing `{}` already has {max} active generated profiles",
                    scope.routing
                );
                let base = config.profile(base_profile)?;
                let revision = 1;
                let name = generated_profile_name(id, revision);
                let mut profile = base.clone();
                profile.description = description.clone();
                profile.instructions.push(crate::config::Instructions {
                    profile: name.clone(),
                    text: instructions_md.clone(),
                });
                let generated = GeneratedProfileRevision {
                    schema_version: 1,
                    id: id.clone(),
                    revision,
                    name,
                    role: *role,
                    scope: scope.clone(),
                    base_profile: base_profile.clone(),
                    base_fingerprint: profile_fingerprint(base),
                    description: description.clone(),
                    instructions_md: instructions_md.clone(),
                    source_report_ids: source_report_ids.clone(),
                    status: GeneratedProfileStatus::Active,
                    retirement_reason: None,
                    decision_id: decision.id.clone(),
                    updated_at: now.into(),
                    profile,
                };
                self.stage_profile_revision(&generated)?;
                manifest.profiles.insert(id.clone(), revision);
                (id.clone(), revision)
            }
            ProfileChange::Revise {
                id,
                expected_revision,
                base_profile,
                description,
                instructions_md,
                source_report_ids,
            } => {
                let old_revision = manifest
                    .profiles
                    .get(id)
                    .copied()
                    .context("generated profile does not exist")?;
                ensure!(
                    old_revision == *expected_revision,
                    "generated profile revision changed; refresh context"
                );
                let old = self.read_profile_revision(id, old_revision)?;
                ensure!(
                    old.status == GeneratedProfileStatus::Active,
                    "generated profile is already retired"
                );
                let next_base_profile = base_profile.as_deref().unwrap_or(&old.base_profile);
                ensure!(
                    generated_bases(config, &old.scope, old.role)?
                        .iter()
                        .any(|base| base == next_base_profile),
                    "generated profile base is not allowed for this role"
                );
                ensure!(
                    !next_base_profile.starts_with("generated-"),
                    "generated profiles cannot inherit from generated profiles"
                );
                let base = config.profile(next_base_profile)?;
                let revision = old_revision + 1;
                let name = generated_profile_name(id, revision);
                let mut profile = base.clone();
                profile.description = description.clone();
                profile.instructions.push(crate::config::Instructions {
                    profile: name.clone(),
                    text: instructions_md.clone(),
                });
                let generated = GeneratedProfileRevision {
                    schema_version: 1,
                    id: id.clone(),
                    revision,
                    name,
                    role: old.role,
                    scope: old.scope,
                    base_profile: next_base_profile.into(),
                    base_fingerprint: profile_fingerprint(base),
                    description: description.clone(),
                    instructions_md: instructions_md.clone(),
                    source_report_ids: source_report_ids.clone(),
                    status: GeneratedProfileStatus::Active,
                    retirement_reason: None,
                    decision_id: decision.id.clone(),
                    updated_at: now.into(),
                    profile,
                };
                self.stage_profile_revision(&generated)?;
                manifest.profiles.insert(id.clone(), revision);
                (id.clone(), revision)
            }
            ProfileChange::Retire {
                id,
                expected_revision,
                reason,
                source_report_ids,
            } => {
                let old_revision = manifest
                    .profiles
                    .get(id)
                    .copied()
                    .context("generated profile does not exist")?;
                ensure!(
                    old_revision == *expected_revision,
                    "generated profile revision changed; refresh context"
                );
                let old = self.read_profile_revision(id, old_revision)?;
                ensure!(
                    old.status == GeneratedProfileStatus::Active,
                    "generated profile is already retired"
                );
                let revision = old_revision + 1;
                let name = generated_profile_name(id, revision);
                let mut profile = old.profile.clone();
                if let Some(layer) = profile.instructions.last_mut() {
                    layer.profile = name.clone();
                }
                let mut sources = old.source_report_ids.clone();
                sources.extend(source_report_ids.iter().cloned());
                sources.sort();
                sources.dedup();
                let generated = GeneratedProfileRevision {
                    schema_version: 1,
                    id: id.clone(),
                    revision,
                    name,
                    role: old.role,
                    scope: old.scope,
                    base_profile: old.base_profile,
                    base_fingerprint: old.base_fingerprint,
                    description: old.description,
                    instructions_md: old.instructions_md,
                    source_report_ids: sources,
                    status: GeneratedProfileStatus::Retired,
                    retirement_reason: Some(reason.clone()),
                    decision_id: decision.id.clone(),
                    updated_at: now.into(),
                    profile,
                };
                self.stage_profile_revision(&generated)?;
                manifest.profiles.insert(id.clone(), revision);
                (id.clone(), revision)
            }
        };
        ensure!(
            manifest.profiles.get(&id) == Some(&revision),
            "generated profile catalog update failed"
        );
        manifest.schema_version = 1;
        manifest.generation = current.catalog_generation.saturating_add(1);
        write_json_immutable(&self.catalog_manifest_path(manifest.generation), &manifest)?;
        Ok(manifest)
    }

    fn stage_profile_revision(&self, profile: &GeneratedProfileRevision) -> Result<()> {
        let dir = self.generated_profile_dir(&profile.id, profile.revision);
        ensure_private_dir(&dir)?;
        write_json_immutable(&dir.join("profile.json"), profile)?;
        write_immutable(
            &dir.join("instructions.md"),
            profile.instructions_md.as_bytes(),
        )
    }

    fn stage_topic(&self, metadata: &KnowledgeMetadata, content: &str) -> Result<()> {
        let dir = self.topic_dir(&metadata.id, metadata.revision)?;
        ensure_private_dir(&dir)?;
        let content_path = dir.join("content.md");
        if content_path.exists() {
            let existing = fs::read_to_string(&content_path)?;
            ensure!(
                existing == content,
                "immutable knowledge revision has different content"
            );
        } else {
            write_immutable(&content_path, content.as_bytes())?;
        }
        write_json_immutable(&dir.join("metadata.json"), metadata)
    }

    /// Returns a durable batch of authorized unseen reports. Repeated calls
    /// return the same batch until its decision commits.
    pub fn context(
        &self,
        generation: u64,
        policy: &str,
        scopes: &[Scope],
        profile_policies: &[ProfilePolicyView],
        now: &str,
    ) -> Result<SupervisorContext> {
        self.ensure_memory_index_current()?;
        let _lock = self.lock()?;
        let commit = self.current_unlocked()?;
        let mut record = self.record_unlocked()?;
        ensure!(
            record.generation == generation,
            "supervisor generation is stale"
        );
        ensure!(
            record.policy_revision == policy,
            "supervisor policy changed; reload before requesting context"
        );
        ensure!(
            !record.report_repair_pending,
            "supervisor report feed is still being repaired"
        );
        let receipts = self.receipts_unlocked(&commit)?;
        let feed: ReportFeed = read_required(&self.root.join("feed.json"))?;
        ensure!(
            feed.schema_version == 1,
            "unsupported supervisor feed schema"
        );
        let scope_set: BTreeSet<_> = scopes.iter().cloned().collect();

        if let Some(batch_id) = record.active_batch.clone() {
            let batch: SupervisorBatch = read_required(&self.batch_path(&batch_id))?;
            let batch_was_committed = commit.decision_id.as_ref().is_some_and(|id| {
                self.read_decision_unlocked(id)
                    .is_ok_and(|decision| decision.batch_id.as_deref() == Some(&batch_id))
            });
            if batch_was_committed {
                record.active_batch = None;
                crate::files::write_json(&self.record_path(), &record)?;
            } else {
                ensure!(batch.id == batch_id, "active supervisor batch id mismatch");
                ensure!(
                    batch.generation == generation,
                    "active supervisor batch belongs to a stale generation"
                );
                ensure!(
                    batch.policy_revision == policy,
                    "active supervisor batch belongs to a stale policy"
                );
                return self.context_for_batch(
                    batch,
                    &commit,
                    &scope_set,
                    profile_policies,
                    &receipts.acknowledged_report_ids,
                    &feed,
                );
            }
        }

        ensure!(
            !matches!(
                record.status,
                SupervisorStatus::Draining
                    | SupervisorStatus::Closing
                    | SupervisorStatus::Disabled
                    | SupervisorStatus::Blocked
            ),
            "supervisor is not accepting a new batch while its lifecycle is {:?}",
            record.status
        );

        let acknowledged = &receipts.acknowledged_report_ids;
        let eligible: Vec<_> = feed
            .reports
            .iter()
            .filter(|entry| scope_set.contains(&entry.scope) && !acknowledged.contains(&entry.id))
            .collect();
        let batch_id = uuid::Uuid::new_v4().to_string();
        let mut report_ids = Vec::new();
        let mut reports = Vec::new();
        let mut rendered_bytes = 0usize;
        for entry in eligible.iter().take(MAX_REPORT_BATCH) {
            let mut report = self.read_report_unlocked(entry)?;
            clip_report(&mut report, 1000);
            let bytes = serde_json::to_vec(&report)?.len();
            if rendered_bytes + bytes > MAX_CONTEXT_BYTES / 2 && !reports.is_empty() {
                break;
            }
            rendered_bytes += bytes;
            report_ids.push(entry.id.clone());
            reports.push(report);
        }
        let remaining_report_count = eligible.len().saturating_sub(report_ids.len());
        let batch = SupervisorBatch {
            schema_version: 1,
            id: batch_id.clone(),
            generation,
            commit_sequence: commit.sequence,
            policy_revision: policy.into(),
            issued_at: now.into(),
            report_ids,
            remaining_report_count,
        };
        let context = self.context_with_data(
            batch,
            reports,
            remaining_report_count,
            &commit,
            ContextEnvironment {
                scopes: &scope_set,
                profile_policies,
                feed: &feed,
            },
        )?;
        write_json_immutable(&self.batch_path(&batch_id), &context.batch)?;
        record.active_batch = Some(batch_id);
        record.status = SupervisorStatus::Busy;
        record.wake_pending = false;
        record.wake_attempts = 0;
        record.last_wake_at.clear();
        record.policy_wake_pending = false;
        crate::files::write_json(&self.record_path(), &record)?;
        Ok(context)
    }

    fn ensure_memory_index_current(&self) -> Result<()> {
        let current = self.current()?;
        let path = self.root.join("memory/index.sqlite3");
        let indexed = supervisor_index::generation(&path).unwrap_or(None);
        if indexed != Some(current.memory_generation) {
            self.rebuild_memory_index()?;
        }
        Ok(())
    }

    fn context_for_batch(
        &self,
        batch: SupervisorBatch,
        commit: &SupervisorCommit,
        scope_set: &BTreeSet<Scope>,
        profile_policies: &[ProfilePolicyView],
        acknowledged: &BTreeSet<String>,
        feed: &ReportFeed,
    ) -> Result<SupervisorContext> {
        let mut reports = Vec::new();
        for id in &batch.report_ids {
            ensure!(
                !acknowledged.contains(id),
                "active batch includes an acknowledged report"
            );
            let entry = feed
                .reports
                .iter()
                .find(|entry| &entry.id == id)
                .context("active batch report is missing from the feed")?;
            ensure!(
                scope_set.contains(&entry.scope),
                "active batch includes a report outside current policy"
            );
            let mut report = self.read_report_unlocked(entry)?;
            clip_report(&mut report, 1000);
            reports.push(report);
        }
        self.context_with_data(
            batch.clone(),
            reports,
            batch.remaining_report_count,
            commit,
            ContextEnvironment {
                scopes: scope_set,
                profile_policies,
                feed,
            },
        )
    }

    fn context_with_data(
        &self,
        batch: SupervisorBatch,
        reports: Vec<SupervisorReport>,
        remaining_report_count: usize,
        commit: &SupervisorCommit,
        environment: ContextEnvironment<'_>,
    ) -> Result<SupervisorContext> {
        let ContextEnvironment {
            scopes,
            profile_policies,
            feed,
        } = environment;
        let checkpoint = match &commit.checkpoint_id {
            Some(id) => Some(read_required(&self.checkpoint_path(id))?),
            None => None,
        };
        let manifest = self.memory_manifest_unlocked(commit.memory_generation)?;
        let queries = memory_queries(&reports, checkpoint.as_ref());
        let mut knowledge = Vec::new();
        let mut seen = BTreeSet::new();
        if !queries.is_empty() {
            for scope in scopes {
                for query in &queries {
                    let found = supervisor_index::search(
                        &self.root.join("memory/index.sqlite3"),
                        commit.memory_generation,
                        &supervisor_index::Scope {
                            workspace: scope.workspace.clone(),
                            team: scope.team.clone(),
                            routing: scope.routing.clone(),
                        },
                        query,
                        5,
                    )?;
                    for hit in found.results {
                        if !seen.insert(hit.id.clone())
                            || manifest.topics.get(&hit.id) != Some(&hit.revision)
                        {
                            continue;
                        }
                        let (metadata, content) =
                            self.read_topic_unlocked(&hit.id, hit.revision)?;
                        if metadata.status != KnowledgeStatus::Active
                            || metadata.scope != *scope
                            || !topic_profiles_match(&metadata.profiles, &reports)
                        {
                            continue;
                        }
                        knowledge.push(KnowledgeView {
                            id: hit.id,
                            revision: hit.revision,
                            title: metadata.title,
                            scope: metadata.scope,
                            status: metadata.status,
                            content: clip_text(&content, 1500),
                        });
                        if knowledge.len() >= 5 {
                            break;
                        }
                    }
                    if knowledge.len() >= 5 {
                        break;
                    }
                }
                if knowledge.len() >= 5 {
                    break;
                }
            }
        }
        let batch_scopes: BTreeSet<_> = reports
            .iter()
            .map(|report| Scope {
                workspace: report.workspace.clone(),
                team: report.team.clone(),
                routing: report.routing.clone(),
            })
            .collect();
        let relevant_scopes = if batch_scopes.is_empty() {
            scopes.clone()
        } else {
            batch_scopes.clone()
        };
        let policies: Vec<_> = profile_policies
            .iter()
            .filter(|policy| relevant_scopes.contains(&policy.scope))
            .cloned()
            .collect();
        let active_catalog = self.catalog_manifest_unlocked(commit.catalog_generation)?;
        let mut active_profiles = Vec::new();
        for (id, revision) in active_catalog.profiles {
            let profile = self.read_profile_revision(&id, revision)?;
            let policy = policies.iter().find(|policy| policy.scope == profile.scope);
            if profile.status == GeneratedProfileStatus::Active
                && relevant_scopes.contains(&profile.scope)
            {
                let base_authorized = policy.is_some_and(|policy| {
                    policy.allows_base_name(profile.role, &profile.base_profile)
                });
                let base_unchanged = policy.is_some_and(|policy| {
                    policy.base_fingerprint_is_current(
                        &profile.base_profile,
                        &profile.base_fingerprint,
                    )
                });
                let ineligibility_reason = if !base_authorized {
                    Some("manual base is no longer authorized".into())
                } else if !base_unchanged {
                    Some("manual base changed; republish against its current revision".into())
                } else {
                    None
                };
                active_profiles.push(ActiveProfileView {
                    id: profile.id,
                    revision: profile.revision,
                    name: profile.name,
                    role: profile.role,
                    scope: profile.scope,
                    base_profile: profile.base_profile,
                    eligible: ineligibility_reason.is_none(),
                    ineligibility_reason,
                    description: clip_text(&profile.description, 300),
                    instructions_preview: clip_text(&profile.instructions_md, 700),
                });
            }
        }
        active_profiles.sort_by(|a, b| (&a.scope, a.role, &a.id).cmp(&(&b.scope, b.role, &b.id)));
        let recent_decisions = self.recent_decisions(commit, &relevant_scopes, feed)?;
        let mut context = SupervisorContext {
            schema_version: 1,
            generation: batch.generation,
            batch,
            reports,
            remaining_report_count,
            checkpoint,
            profile_policies: policies,
            active_profiles,
            omitted_active_profile_count: 0,
            recent_decisions,
            omitted_recent_decision_count: 0,
            knowledge,
        };
        while serde_json::to_vec(&context)?.len() > MAX_CONTEXT_BYTES
            && context.knowledge.pop().is_some()
        {}
        while serde_json::to_vec(&context)?.len() > MAX_CONTEXT_BYTES
            && context.active_profiles.pop().is_some()
        {
            context.omitted_active_profile_count += 1;
        }
        while serde_json::to_vec(&context)?.len() > MAX_CONTEXT_BYTES
            && context.recent_decisions.pop().is_some()
        {
            context.omitted_recent_decision_count += 1;
        }
        while serde_json::to_vec(&context)?.len() > MAX_CONTEXT_BYTES && context.reports.len() > 1 {
            context.reports.pop();
            context.batch.report_ids.pop();
            context.remaining_report_count += 1;
        }
        ensure!(
            serde_json::to_vec(&context)?.len() <= MAX_CONTEXT_BYTES,
            "supervisor context exceeds its size limit"
        );
        Ok(context)
    }

    fn recent_decisions(
        &self,
        current: &SupervisorCommit,
        scopes: &BTreeSet<Scope>,
        feed: &ReportFeed,
    ) -> Result<Vec<RecentDecisionView>> {
        if current.sequence == 0 || scopes.is_empty() {
            return Ok(Vec::new());
        }
        let first = current.sequence.saturating_sub(100).max(1);
        let mut recent = Vec::new();
        for sequence in (first..=current.sequence).rev() {
            let commit: SupervisorCommit = read_required(&self.commit_path(sequence))?;
            ensure!(
                commit.sequence == sequence,
                "supervisor commit history sequence mismatch"
            );
            let Some(id) = commit.decision_id else {
                continue;
            };
            let decision = self.read_decision_unlocked(&id)?;
            if decision.consumed_report_ids.is_empty() {
                continue;
            }
            let mut relevant = true;
            for report_id in &decision.consumed_report_ids {
                let entry = feed
                    .reports
                    .iter()
                    .find(|entry| &entry.id == report_id)
                    .context("recent decision report is missing from the feed")?;
                if !scopes.contains(&entry.scope) {
                    relevant = false;
                    break;
                }
            }
            if !relevant {
                continue;
            }
            let profile_change = decision.profile_change.as_ref().map(|change| match change {
                ProfileChange::Create {
                    id, base_profile, ..
                } => {
                    format!("created `{id}` from `{base_profile}`")
                }
                ProfileChange::Revise {
                    id,
                    expected_revision,
                    ..
                } => format!("revised `{id}` from revision {expected_revision}"),
                ProfileChange::Retire {
                    id,
                    expected_revision,
                    ..
                } => format!("retired `{id}` at revision {expected_revision}"),
            });
            recent.push(RecentDecisionView {
                id: decision.id,
                sequence: decision.sequence,
                reason: clip_text(&decision.reason, 300),
                expected_result: clip_text(&decision.expected_result, 300),
                consumed_report_ids: decision.consumed_report_ids,
                profile_change,
            });
            if recent.len() >= 5 {
                break;
            }
        }
        Ok(recent)
    }

    fn read_report_unlocked(&self, entry: &FeedEntry) -> Result<SupervisorReport> {
        let report = self.read_report_unlocked_by_id(&entry.id)?;
        let bytes = serde_json::to_vec(&report)?;
        ensure!(
            crate::files::sha256_hex(&bytes) == entry.report_hash,
            "supervisor feed report hash mismatch"
        );
        ensure!(
            entry.scope
                == Scope {
                    workspace: report.workspace.clone(),
                    team: report.team.clone(),
                    routing: report.routing.clone(),
                },
            "supervisor feed report scope mismatch"
        );
        Ok(report)
    }

    fn read_report_unlocked_by_id(&self, id: &str) -> Result<SupervisorReport> {
        let path = self.report_path(id)?;
        let envelope: ReportEnvelope = read_required(&path)?;
        ensure!(
            envelope.schema_version == 1,
            "unsupported supervisor report schema"
        );
        ensure!(envelope.report.id == id, "supervisor report id mismatch");
        let bytes = serde_json::to_vec(&envelope.report)?;
        ensure!(
            crate::files::sha256_hex(&bytes) == envelope.report_hash,
            "supervisor report hash does not match"
        );
        Ok(envelope.report)
    }

    fn runs_dir(&self) -> PathBuf {
        self.root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("runs")
    }

    fn report_path(&self, id: &str) -> Result<PathBuf> {
        validate_report_id(id)?;
        Ok(self.root.join("reports").join(format!("{id}.json")))
    }

    fn memory_manifest_path(&self, generation: u64) -> PathBuf {
        self.root
            .join("memory/manifests")
            .join(format!("{generation:020}.json"))
    }

    fn catalog_manifest_path(&self, generation: u64) -> PathBuf {
        self.root
            .join("generated-profiles/manifests")
            .join(format!("{generation:020}.json"))
    }

    fn commit_path(&self, sequence: u64) -> PathBuf {
        self.root
            .join("commits")
            .join(format!("{sequence:020}.json"))
    }

    fn decision_path(&self, id: &str) -> PathBuf {
        self.root.join("decisions").join(format!("{id}.json"))
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.root.join("checkpoints").join(format!("{id}.json"))
    }

    fn batch_path(&self, id: &str) -> PathBuf {
        self.root.join("batches").join(format!("{id}.json"))
    }

    fn memory_manifest_unlocked(&self, generation: u64) -> Result<MemoryManifest> {
        let manifest: MemoryManifest = read_required(&self.memory_manifest_path(generation))?;
        ensure!(
            manifest.schema_version == 1,
            "unsupported memory manifest schema"
        );
        Ok(manifest)
    }

    fn catalog_manifest_unlocked(&self, generation: u64) -> Result<CatalogManifest> {
        let manifest: CatalogManifest = read_required(&self.catalog_manifest_path(generation))?;
        ensure!(
            manifest.schema_version == 1,
            "unsupported catalog manifest schema"
        );
        Ok(manifest)
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not restrict permissions on {}", path.display()))
}

fn read_required<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} does not parse", path.display()))
}

fn write_json_immutable<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_immutable(path, &bytes)
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("immutable path has no parent")?;
    ensure_private_dir(parent)?;
    if path.exists() {
        let existing = std::fs::read(path)
            .with_context(|| format!("could not read existing {}", path.display()))?;
        ensure!(
            existing == bytes,
            "immutable artifact {} already has different content",
            path.display()
        );
        return Ok(());
    }
    let name = path.file_name().context("immutable path has no filename")?;
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temp, path)?;
        std::fs::remove_file(&temp)?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        if error.kind() == std::io::ErrorKind::AlreadyExists && path.exists() {
            let existing = std::fs::read(path)
                .with_context(|| format!("could not read existing {}", path.display()))?;
            ensure!(
                existing == bytes,
                "immutable artifact {} already has different content",
                path.display()
            );
            return Ok(());
        }
        return Err(error).with_context(|| format!("could not create {}", path.display()));
    }
    Ok(())
}

fn validate_component(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 96,
        "invalid supervisor artifact id"
    );
    ensure!(
        id.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid supervisor artifact id"
    );
    Ok(())
}

fn validate_profile_id(id: &str) -> Result<()> {
    validate_component(id)?;
    ensure!(
        id.starts_with("p-") && id.len() <= 48,
        "generated profile ids must start with `p-` and contain at most 48 characters"
    );
    Ok(())
}

fn generated_profile_name(id: &str, revision: u64) -> String {
    format!("generated-{id}-r{revision}")
}

fn generated_bases<'a>(
    config: &'a Config,
    scope: &Scope,
    role: crate::config::GeneratedRole,
) -> Result<&'a [String]> {
    let team = config.team(&scope.workspace, &scope.team)?;
    ensure!(
        team.routing == scope.routing,
        "scope routing is not the configured routing for this team"
    );
    let routing = config.routing_of(&scope.workspace, &scope.team)?;
    Ok(match role {
        crate::config::GeneratedRole::Coordinator => &routing.generated.coordinators,
        crate::config::GeneratedRole::Worker => &routing.generated.workers,
        crate::config::GeneratedRole::Postmortem => &routing.generated.postmortems,
    })
}

fn validate_uuid(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).context("decision id must be a UUID")?;
    Ok(())
}

fn validate_report_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 34 && id.starts_with("r-"),
        "invalid supervisor report id"
    );
    ensure!(
        id[2..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid supervisor report id"
    );
    Ok(())
}

fn validate_topic_fields(
    id: &str,
    title: &str,
    content: &str,
    repositories: &[String],
    profiles: &[String],
    source_report_ids: &[String],
) -> Result<()> {
    validate_component(id)?;
    ensure!(
        id.starts_with("t-"),
        "knowledge topic ids must start with `t-`"
    );
    ensure!(
        !title.trim().is_empty() && title.chars().count() <= 200,
        "knowledge title must be 1 to 200 characters"
    );
    ensure!(
        content.len() <= MAX_TOPIC_BYTES,
        "knowledge topic exceeds {MAX_TOPIC_BYTES} bytes"
    );
    ensure!(
        !content.trim().is_empty(),
        "knowledge topic content is empty"
    );
    ensure!(
        repositories.len() <= 32 && profiles.len() <= 32,
        "knowledge topic has too many repository or profile references"
    );
    ensure!(
        source_report_ids.len() <= MAX_TOPIC_CHANGES,
        "knowledge topic has too many source reports"
    );
    ensure!(
        source_report_ids
            .iter()
            .all(|id| validate_report_id(id).is_ok()),
        "knowledge topic has an invalid report id"
    );
    for value in repositories.iter().chain(profiles) {
        ensure!(
            !value.trim().is_empty() && value.chars().count() <= 256,
            "knowledge reference is invalid or too long"
        );
    }
    Ok(())
}

fn validate_profile_change(change: &ProfileChange) -> Result<()> {
    let (id, description, instructions, sources) = match change {
        ProfileChange::Create {
            id,
            description,
            instructions_md,
            source_report_ids,
            ..
        }
        | ProfileChange::Revise {
            id,
            description,
            instructions_md,
            source_report_ids,
            ..
        } => (
            id,
            description,
            Some(instructions_md.as_str()),
            source_report_ids,
        ),
        ProfileChange::Retire {
            id,
            reason,
            source_report_ids,
            ..
        } => (id, reason, None, source_report_ids),
    };
    validate_profile_id(id)?;
    ensure!(
        description.chars().count() <= MAX_FIELD_CHARS,
        "generated profile description or retirement reason is too long"
    );
    if let Some(instructions) = instructions {
        ensure!(
            !instructions.trim().is_empty(),
            "generated profile instructions are empty"
        );
        ensure!(
            instructions.len() <= MAX_TOPIC_BYTES,
            "generated profile instructions are too long"
        );
    }
    ensure!(
        !sources.is_empty() && sources.len() <= MAX_TOPIC_CHANGES,
        "generated profile change needs one to five report sources"
    );
    ensure!(
        sources.iter().all(|id| validate_report_id(id).is_ok()),
        "generated profile has an invalid report id"
    );
    let mut unique = BTreeSet::new();
    ensure!(
        sources.iter().all(|id| unique.insert(id)),
        "generated profile has duplicate report ids"
    );
    if let ProfileChange::Revise {
        expected_revision, ..
    }
    | ProfileChange::Retire {
        expected_revision, ..
    } = change
    {
        ensure!(
            *expected_revision > 0,
            "generated profile revision must be positive"
        );
    }
    Ok(())
}

fn checkpoint_matches_proposal(
    checkpoint: &SupervisorCheckpoint,
    proposal: &DecisionProposal,
) -> bool {
    let mut expected = proposal.checkpoint.clone();
    expected.schema_version = 1;
    expected.decision_id = proposal.id.clone();
    checkpoint == &expected
}

fn decision_matches_proposal(
    decision: &SupervisorDecision,
    proposal: &DecisionProposal,
    generation: u64,
) -> bool {
    decision.schema_version == 1
        && decision.id == proposal.id
        && decision.base_commit_sequence == proposal.base_commit_sequence
        && decision.policy_revision == proposal.policy_revision
        && decision.batch_id.as_deref() == Some(&proposal.batch_id)
        && decision.consumed_report_ids == proposal.consumed_report_ids
        && decision.observations == proposal.observations
        && decision.reason == proposal.reason
        && decision.expected_result == proposal.expected_result
        && decision.topic_changes == proposal.topic_changes
        && decision.profile_change == proposal.profile_change
        && matches!(
            &decision.actor,
            DecisionActor::Agent {
                generation: recorded,
                batch_id
            } if *recorded == generation && batch_id == &proposal.batch_id
        )
}

fn clip_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!(
            "{}…",
            text.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

fn clip_report(report: &mut SupervisorReport, max: usize) {
    report.source_attempt = clip_text(&report.source_attempt, 128);
    report.run_key = clip_text(&report.run_key, 256);
    report.issue_id = clip_text(&report.issue_id, 256);
    report.workspace = clip_text(&report.workspace, 256);
    report.team = clip_text(&report.team, 128);
    report.routing = clip_text(&report.routing, 128);
    report.observed_at = clip_text(&report.observed_at, 64);
    match &mut report.outcome {
        ReportOutcome::Success { summary, labels } => {
            *summary = clip_text(summary, max);
            labels.truncate(32);
            for label in labels {
                *label = clip_text(label, 256);
            }
        }
        ReportOutcome::Failure { error } => *error = clip_text(error, max),
    }
    report.profiles.truncate(32);
    for profile in &mut report.profiles {
        profile.role = clip_text(&profile.role, 128);
        profile.name = clip_text(&profile.name, 128);
        profile.kind = clip_text(&profile.kind, 128);
        profile.revision = profile
            .revision
            .as_deref()
            .map(|revision| clip_text(revision, 128));
    }
    report.observations.closed_state = report
        .observations
        .closed_state
        .as_deref()
        .map(|state| clip_text(state, 128));
    report.observations.workers.truncate(32);
    for worker in &mut report.observations.workers {
        worker.id = clip_text(&worker.id, 128);
        worker.repository = clip_text(&worker.repository, 256);
        worker.report = worker.report.as_deref().map(|text| clip_text(text, 600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_report() -> (tempfile::TempDir, SupervisorStore, Scope, String) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let runs = state.join("runs");
        let run = Run::create(
            &runs,
            RunRecord {
                workspace: "acme".into(),
                issue_id: "issue-1".into(),
                identifier: "DATA-1".into(),
                title: "Retry the request".into(),
                team_key: "DATA".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let store = SupervisorStore::new(state.join("supervisor"));
        store
            .initialize("manager", "fingerprint", "session", "policy")
            .unwrap();
        let attempt = uuid::Uuid::new_v4().to_string();
        let mut artifact = SavedPostmortem {
            schema_version: 1,
            attempt_id: attempt.clone(),
            run_key: run.key.clone(),
            stage: Some(Stage::Final),
            at: "2026-10-15T10:00:00Z".into(),
            method: "reviewer".into(),
            method_kind: "claude".into(),
            picked: "only".into(),
            version: "abcdef".into(),
            summary: "Retry the transient request once.".into(),
            labels: Vec::new(),
            failure: None,
            supervisor_report: None,
        };
        let report_hash = source_hash(&artifact).unwrap();
        artifact.supervisor_report = Some(SupervisorReport {
            schema_version: 1,
            id: report_id(&run.key, &attempt),
            source_attempt: attempt,
            source_hash: report_hash,
            run_key: run.key.clone(),
            issue_id: "issue-1".into(),
            workspace: "acme".into(),
            team: "DATA".into(),
            routing: "default".into(),
            attribution_complete: true,
            legacy_source: None,
            stage: Stage::Final,
            observed_at: "2026-10-15T10:00:00Z".into(),
            outcome: ReportOutcome::Success {
                summary: artifact.summary.clone(),
                labels: Vec::new(),
            },
            profiles: Vec::new(),
            observations: RunObservations::default(),
        });
        crate::postmortem::save_attempt(&run, artifact.clone()).unwrap();
        store.materialize_report(&artifact).unwrap();
        (
            dir,
            store,
            Scope {
                workspace: "acme".into(),
                team: "DATA".into(),
                routing: "default".into(),
            },
            artifact.supervisor_report.unwrap().id,
        )
    }

    fn add_successful_report(
        store: &SupervisorStore,
        run: &Run,
        summary: &str,
        at: &str,
    ) -> String {
        let attempt = uuid::Uuid::new_v4().to_string();
        let mut artifact = SavedPostmortem {
            schema_version: 1,
            attempt_id: attempt.clone(),
            run_key: run.key.clone(),
            stage: Some(Stage::Final),
            at: at.into(),
            method: "reviewer".into(),
            method_kind: "claude".into(),
            picked: "only".into(),
            version: "abcdef".into(),
            summary: summary.into(),
            labels: Vec::new(),
            failure: None,
            supervisor_report: None,
        };
        let record = run.record().unwrap();
        let mut report = report_from_run(ReportInput {
            run,
            record: &record,
            stage: Stage::Final,
            attempt: &attempt,
            at,
            routing: "default",
            outcome: ReportOutcome::Success {
                summary: summary.into(),
                labels: Vec::new(),
            },
            postmortem_profile: None,
        });
        report.source_hash = source_hash(&artifact).unwrap();
        let id = report.id.clone();
        artifact.supervisor_report = Some(report);
        crate::postmortem::save_attempt(run, artifact.clone()).unwrap();
        store.materialize_report(&artifact).unwrap();
        id
    }

    #[test]
    fn report_ids_include_workspace_qualified_run_and_attempt() {
        assert_ne!(
            report_id("acme/DATA-1", "attempt-1"),
            report_id("other/DATA-1", "attempt-1")
        );
        assert_ne!(
            report_id("acme/DATA-1", "attempt-1"),
            report_id("acme/DATA-1", "attempt-2")
        );
        assert!(report_id("acme/DATA-1", "attempt-1").starts_with("r-"));
    }

    #[test]
    fn staged_profile_files_are_invisible_until_a_catalog_commit_references_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = SupervisorStore::new(dir.path().join("state/supervisor"));
        store
            .initialize("manager", "fingerprint", "session", "policy")
            .unwrap();
        let config = crate::config::tests::sample();
        let name = generated_profile_name("p-orphan", 1);
        let mut profile = config.profile("standard").unwrap().clone();
        profile.instructions.push(crate::config::Instructions {
            profile: name.clone(),
            text: "One generated delta.".into(),
        });
        store
            .stage_profile_revision(&GeneratedProfileRevision {
                schema_version: 1,
                id: "p-orphan".into(),
                revision: 1,
                name,
                role: crate::config::GeneratedRole::Worker,
                scope: Scope {
                    workspace: "acme".into(),
                    team: "DATA".into(),
                    routing: "default".into(),
                },
                base_profile: "standard".into(),
                base_fingerprint: profile_fingerprint(config.profile("standard").unwrap()),
                description: "Orphaned staged profile".into(),
                instructions_md: "One generated delta.".into(),
                source_report_ids: vec!["r-00000000000000000000000000000000".into()],
                status: GeneratedProfileStatus::Active,
                retirement_reason: None,
                decision_id: uuid::Uuid::new_v4().to_string(),
                updated_at: "2026-10-15T10:00:00Z".into(),
                profile,
            })
            .unwrap();
        assert!(store.profile_revisions().unwrap().is_empty());
        assert!(store.profile_catalog().unwrap().is_empty());
    }

    #[test]
    fn legacy_postmortems_are_recovered_but_unknown_routing_cannot_be_decided() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let run = Run::create(
            &state.join("runs"),
            RunRecord {
                workspace: "acme".into(),
                issue_id: "issue-1".into(),
                identifier: "DATA-1".into(),
                team_key: "DATA".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let path = run
            .state_dir()
            .join("postmortems/20260101T120000000000Z-final.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"stage":"final","at":"2026-01-01T12:00:00Z","method":"reviewer","picked":"only","version":"abcdef","summary":"Retry once after a transient timeout.","labels":[]}"#,
        )
        .unwrap();
        let store = SupervisorStore::new(state.join("supervisor"));
        store
            .initialize("manager", "fingerprint", "session", "policy")
            .unwrap();

        assert!(!store.repair_reports(10).unwrap());
        let feed: ReportFeed = read_required(&store.root.join("feed.json")).unwrap();
        assert_eq!(feed.reports.len(), 1);
        assert_eq!(feed.reports[0].scope.routing, "");
        let report = store.report(&feed.reports[0].id).unwrap();
        assert!(!report.attribution_complete);
        assert_eq!(
            report.legacy_source.as_deref(),
            Some("20260101T120000000000Z-final.json")
        );
        assert!(
            store
                .report_in_scopes(
                    &report.id,
                    &[Scope {
                        workspace: "acme".into(),
                        team: "DATA".into(),
                        routing: "default".into(),
                    }]
                )
                .is_err()
        );
    }

    #[test]
    fn decisions_commit_memory_before_ack_and_rebuild_search() {
        let (_dir, store, scope, report_id) = store_with_report();
        assert!(!store.repair_reports(10).unwrap());
        let context = store
            .context(
                1,
                "policy",
                std::slice::from_ref(&scope),
                &[],
                "2026-10-15T10:01:00Z",
            )
            .unwrap();
        assert_eq!(
            context.batch.report_ids.as_slice(),
            std::slice::from_ref(&report_id)
        );

        let proposal = DecisionProposal {
            schema_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            batch_id: context.batch.id.clone(),
            base_commit_sequence: context.batch.commit_sequence,
            policy_revision: "policy".into(),
            consumed_report_ids: context.batch.report_ids.clone(),
            observations: vec![Observation {
                report_id: report_id.clone(),
                text: "The request failed once and succeeded after retry.".into(),
            }],
            reason: "The evidence supports one retry.".into(),
            expected_result: "Fewer transient failures.".into(),
            topic_changes: vec![TopicChange::Create {
                id: "t-retry-timeouts".into(),
                title: "Retry transient timeouts".into(),
                scope: scope.clone(),
                repositories: Vec::new(),
                profiles: Vec::new(),
                source_report_ids: vec![report_id.clone()],
                content_md:
                    "For a transient timeout, retry the request once and verify the result.".into(),
            }],
            profile_change: None,
            checkpoint: SupervisorCheckpoint {
                schema_version: 1,
                decision_id: String::new(),
                summary: "Reviewed one final report.".into(),
                pending_report_ids: Vec::new(),
                unresolved_questions: Vec::new(),
                intended_checks: vec!["Compare later timeout reports.".into()],
            },
        };
        let config = crate::config::tests::sample();
        let receipt = store
            .decide(
                1,
                "policy",
                std::slice::from_ref(&scope),
                &config,
                proposal.clone(),
            )
            .unwrap();
        assert_eq!(receipt.commit_sequence, 1);
        assert_eq!(
            receipt.acknowledged_report_ids.as_slice(),
            std::slice::from_ref(&report_id)
        );
        assert_eq!(store.current().unwrap().sequence, 1);
        let (metadata, content) = store.topic("t-retry-timeouts", None).unwrap();
        assert_eq!(metadata.status, KnowledgeStatus::Active);
        assert!(content.contains("retry the request once"));

        let replay = store
            .decide(1, "policy", std::slice::from_ref(&scope), &config, proposal)
            .unwrap();
        assert_eq!(replay.decision_id, receipt.decision_id);
        assert_eq!(store.current().unwrap().sequence, 1);
        assert_eq!(store.rebuild_memory_index().unwrap(), 1);
        let found = store
            .search_memory(std::slice::from_ref(&scope), "transient timeout", 10)
            .unwrap();
        assert_eq!(found.indexed_generation, 1);
        assert_eq!(found.results.len(), 1);
        assert_eq!(found.results[0].id, "t-retry-timeouts");
        fs::remove_file(store.root.join("memory/index.sqlite3")).unwrap();
        store.ensure_memory_index_current().unwrap();
        assert_eq!(
            supervisor_index::generation(&store.root.join("memory/index.sqlite3")).unwrap(),
            Some(1)
        );
        let committed = store.current().unwrap();
        let report = store.report(&report_id).unwrap();
        let feed: ReportFeed = read_required(&store.root.join("feed.json")).unwrap();
        let context = store
            .context_with_data(
                SupervisorBatch {
                    schema_version: 1,
                    id: "batch-test".into(),
                    generation: 1,
                    commit_sequence: committed.sequence,
                    policy_revision: "policy".into(),
                    issued_at: "2026-10-15T10:03:00Z".into(),
                    report_ids: vec![report_id],
                    remaining_report_count: 0,
                },
                vec![report],
                0,
                &committed,
                ContextEnvironment {
                    scopes: &BTreeSet::from([scope]),
                    profile_policies: &[],
                    feed: &feed,
                },
            )
            .unwrap();
        assert_eq!(context.knowledge.len(), 1);
        assert_eq!(context.knowledge[0].id, "t-retry-timeouts");
    }

    #[test]
    fn published_profile_is_loaded_from_the_new_commit_generation() {
        let (dir, store, scope, report_id) = store_with_report();
        assert!(!store.repair_reports(10).unwrap());
        let config_dir = dir.path().join("config");
        let config_text = format!(
            "{}\n[routing.default.generated]\nworkers = [\"standard\", \"deep\"]\n\n[supervisor]\nprofile = \"router\"\n",
            crate::config::tests::SAMPLE
        );
        crate::config::tests::write_sample(&config_dir, &config_text);
        let state_dir = dir.path().join("state");
        let config = Config::load_with_state_dir(&config_dir, &state_dir).unwrap();
        let policy = policy_revision(&config).unwrap();
        let mut changed_base = config.clone();
        changed_base
            .profiles
            .get_mut("standard")
            .unwrap()
            .description
            .push_str(" edited");
        assert_ne!(policy_revision(&changed_base).unwrap(), policy);
        store
            .update_record(|record| record.policy_revision.clone_from(&policy))
            .unwrap();
        let context = store
            .context(
                1,
                &policy,
                std::slice::from_ref(&scope),
                &authorized_profile_policies(&config).unwrap(),
                "2026-10-15T10:01:00Z",
            )
            .unwrap();
        let proposal = DecisionProposal {
            schema_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            batch_id: context.batch.id.clone(),
            base_commit_sequence: context.batch.commit_sequence,
            policy_revision: policy.clone(),
            consumed_report_ids: context.batch.report_ids.clone(),
            observations: vec![Observation {
                report_id: report_id.clone(),
                text: "The failure occurred in a worker run.".into(),
            }],
            reason: "A scoped retry instruction is supported by this report.".into(),
            expected_result: "Future worker runs retry one transient request.".into(),
            topic_changes: Vec::new(),
            profile_change: Some(ProfileChange::Create {
                id: "p-retry-worker".into(),
                role: crate::config::GeneratedRole::Worker,
                scope: scope.clone(),
                base_profile: "standard".into(),
                description: "Retries one transient request".into(),
                instructions_md: "Retry one transient request, then verify its result.".into(),
                source_report_ids: vec![report_id.clone()],
            }),
            checkpoint: SupervisorCheckpoint {
                schema_version: 1,
                decision_id: String::new(),
                summary: "Published a worker variant.".into(),
                pending_report_ids: Vec::new(),
                unresolved_questions: Vec::new(),
                intended_checks: vec!["Compare the next matching failure.".into()],
            },
        };
        let decision_receipt = store
            .decide(1, &policy, std::slice::from_ref(&scope), &config, proposal)
            .unwrap();

        let refreshed = Config::load_with_state_dir(&config_dir, &state_dir).unwrap();
        assert_eq!(refreshed.generated_catalog_generation, Some(1));
        assert_eq!(
            refreshed
                .worker_profile_for("acme", "DATA", "generated-p-retry-worker-r1")
                .unwrap()
                .instructions
                .last()
                .unwrap()
                .text,
            "Retry one transient request, then verify its result."
        );
        assert_eq!(
            store.catalog_generation_hint().unwrap(),
            Some(refreshed.generated_catalog_generation.unwrap())
        );

        let run = Run::load(&dir.path().join("state/runs"), "acme/DATA-1").unwrap();
        let followup_report = add_successful_report(
            &store,
            &run,
            "The one-retry instruction worked on this follow-up.",
            "2026-10-15T10:02:00Z",
        );
        let context = store
            .context(
                1,
                &policy,
                std::slice::from_ref(&scope),
                &authorized_profile_policies(&refreshed).unwrap(),
                "2026-10-15T10:03:00Z",
            )
            .unwrap();
        assert_eq!(context.batch.report_ids, [followup_report]);
        assert_eq!(context.profile_policies.len(), 1);
        assert_eq!(
            context.profile_policies[0].worker_bases,
            ["standard", "deep"]
        );
        assert!(
            context.profile_policies[0]
                .base_fingerprints
                .contains_key("standard")
        );
        assert_eq!(context.active_profiles.len(), 1);
        assert_eq!(
            context.active_profiles[0].name,
            "generated-p-retry-worker-r1"
        );
        assert!(context.active_profiles[0].eligible);
        assert!(
            context.active_profiles[0]
                .instructions_preview
                .contains("Retry one transient request")
        );
        assert_eq!(context.recent_decisions.len(), 1);
        assert_eq!(context.recent_decisions[0].id, decision_receipt.decision_id);
        assert_eq!(
            context.recent_decisions[0].consumed_report_ids.as_slice(),
            std::slice::from_ref(&report_id)
        );
        store
            .decide(
                1,
                &policy,
                std::slice::from_ref(&scope),
                &refreshed,
                DecisionProposal {
                    schema_version: 1,
                    id: uuid::Uuid::new_v4().to_string(),
                    batch_id: context.batch.id.clone(),
                    base_commit_sequence: context.batch.commit_sequence,
                    policy_revision: policy.clone(),
                    consumed_report_ids: context.batch.report_ids.clone(),
                    observations: vec![Observation {
                        report_id: context.batch.report_ids[0].clone(),
                        text: "The revised instruction remains within the allowed worker policy."
                            .into(),
                    }],
                    reason: "The follow-up confirmed the current instruction; no further change is needed.".into(),
                    expected_result: "Keep the current worker variant.".into(),
                    topic_changes: Vec::new(),
                    profile_change: None,
                    checkpoint: SupervisorCheckpoint {
                        schema_version: 1,
                        decision_id: String::new(),
                        summary: "Validated the worker variant once.".into(),
                        pending_report_ids: Vec::new(),
                        unresolved_questions: Vec::new(),
                        intended_checks: Vec::new(),
                    },
                },
            )
            .unwrap();

        let export = dir.path().join("manual-export");
        store
            .export_profile(&refreshed, "p-retry-worker", 1, "manual-retry", &export)
            .unwrap();
        let exported_config: toml::Value =
            toml::from_str(&fs::read_to_string(export.join("config.toml")).unwrap()).unwrap();
        assert_eq!(exported_config["base"].as_str(), Some("standard"));
        assert_eq!(
            fs::read_to_string(export.join("instructions.md")).unwrap(),
            "Retry one transient request, then verify its result."
        );
        assert!(
            fs::read_to_string(export.join("README.md"))
                .unwrap()
                .contains("p-retry-worker@1")
        );
        assert!(
            store
                .export_profile(&refreshed, "p-retry-worker", 1, "manual-retry", &export)
                .is_err()
        );

        let receipt = store
            .operator_profile_change(
                &refreshed,
                ProfileChange::Revise {
                    id: "p-retry-worker".into(),
                    expected_revision: 1,
                    base_profile: Some("deep".into()),
                    description: "A newer worker variant".into(),
                    instructions_md: "A newer instruction delta.".into(),
                    source_report_ids: vec![report_id.clone()],
                },
                "profile-revise:p-retry-worker@1",
                "Use the earlier behavior while testing an alternative.",
                "2026-10-15T10:02:00Z",
            )
            .unwrap();
        assert_eq!(receipt.catalog_generation, 2);
        let catalog = store.profile_catalog().unwrap();
        assert!(catalog.iter().any(|entry| {
            entry.revision.id == "p-retry-worker" && entry.revision.revision == 1 && !entry.current
        }));
        assert!(catalog.iter().any(|entry| {
            entry.revision.id == "p-retry-worker"
                && entry.revision.revision == 2
                && entry.current
                && entry.revision.base_profile == "deep"
        }));

        store
            .operator_profile_change(
                &refreshed,
                ProfileChange::Revise {
                    id: "p-retry-worker".into(),
                    expected_revision: 2,
                    base_profile: Some("standard".into()),
                    description: catalog
                        .iter()
                        .find(|entry| {
                            entry.revision.id == "p-retry-worker" && entry.revision.revision == 1
                        })
                        .unwrap()
                        .revision
                        .description
                        .clone(),
                    instructions_md: catalog
                        .iter()
                        .find(|entry| {
                            entry.revision.id == "p-retry-worker" && entry.revision.revision == 1
                        })
                        .unwrap()
                        .revision
                        .instructions_md
                        .clone(),
                    source_report_ids: vec![report_id.clone()],
                },
                "profile-rollback:p-retry-worker@1",
                "The prior revision is the chosen recovery point.",
                "2026-10-15T10:03:00Z",
            )
            .unwrap();
        let catalog = store.profile_catalog().unwrap();
        let current = catalog
            .iter()
            .find(|entry| entry.revision.id == "p-retry-worker" && entry.current)
            .unwrap();
        assert_eq!(current.revision.revision, 3);
        assert_eq!(current.revision.base_profile, "standard");
        assert_eq!(
            current.revision.instructions_md,
            "Retry one transient request, then verify its result."
        );
        store
            .operator_profile_change(
                &refreshed,
                ProfileChange::Retire {
                    id: "p-retry-worker".into(),
                    expected_revision: 3,
                    reason: "Adopt the reviewed manual revision.".into(),
                    source_report_ids: vec![report_id],
                },
                "profile-retire:p-retry-worker@3",
                "Adopt the reviewed manual revision.",
                "2026-10-15T10:04:00Z",
            )
            .unwrap();
        let catalog = store.profile_catalog().unwrap();
        let retired = catalog
            .iter()
            .find(|entry| entry.revision.id == "p-retry-worker" && entry.current)
            .unwrap();
        assert_eq!(retired.revision.revision, 4);
        assert_eq!(retired.revision.status, GeneratedProfileStatus::Retired);
    }

    #[test]
    fn context_never_returns_reports_outside_the_current_scope() {
        let (_dir, store, _scope, _) = store_with_report();
        assert!(!store.repair_reports(10).unwrap());
        let context = store
            .context(
                1,
                "policy",
                &[Scope {
                    workspace: "acme".into(),
                    team: "OTHER".into(),
                    routing: "default".into(),
                }],
                &[],
                "2026-10-15T10:01:00Z",
            )
            .unwrap();
        assert!(context.reports.is_empty());
        assert!(context.batch.report_ids.is_empty());
        assert_eq!(context.remaining_report_count, 0);
    }
}
