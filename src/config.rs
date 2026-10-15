//! `$XDG_CONFIG_HOME/herdr-linear-agent/config.toml` and the profile folders
//! next to it, `profiles/<name>/config.toml` with an optional
//! `profiles/<name>/instructions.md`, all written by the user. A profile is a
//! folder of its own so it can be handed to other people as it is.
//!
//! The config is the only place that names Linear teams, repositories, agent
//! profiles (kind, model, effort, approval flags), the routing candidates, limits and
//! the Herdr session. Agents choose among these by name; they never pass a
//! raw flag.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::{agents, routing};

pub const FILE_NAME: &str = "config.toml";
const PROFILES_DIR: &str = "profiles";
const INSTRUCTIONS_FILE: &str = "instructions.md";
pub const DEFAULT_CALLBACK_PORT: u16 = 43871;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The Linear workspaces, by the name runs use for them.
    pub workspaces: BTreeMap<String, Workspace>,
    #[serde(default)]
    pub herdr: Herdr,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub notifications: Notifications,
    #[serde(default)]
    pub claude: Claude,
    #[serde(default)]
    pub supervisor: Option<SupervisorConfig>,
    #[serde(default)]
    pub repositories: BTreeMap<String, Repository>,
    /// Read from the profile folders, never from `config.toml`.
    #[serde(skip)]
    pub profiles: BTreeMap<String, Profile>,
    /// Catalog candidates are generated from the current supervisor commit.
    #[serde(skip)]
    pub generated_candidates: BTreeMap<GeneratedCandidateKey, Vec<String>>,
    /// Last catalog generation whose profile snapshot was loaded.
    #[serde(skip)]
    pub generated_catalog_generation: Option<u64>,
    /// A damaged generated catalog is reported without making manual profiles unusable.
    #[serde(skip)]
    pub generated_catalog_error: Option<String>,
    /// The routings teams use, by name.
    pub routing: BTreeMap<String, Routing>,
}

/// One Linear workspace: its own OAuth application, token and app user.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// The private OAuth application's client ID. Not a secret.
    pub client_id: String,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
    /// Seconds between two polls of the delegated issues.
    #[serde(default = "default_linear_interval")]
    pub intake_interval_seconds: u64,
    /// Seconds between two reads of the active runs.
    #[serde(default = "default_linear_interval")]
    pub run_read_interval_seconds: u64,
    /// The teams whose delegated issues are picked up, by team key.
    pub teams: BTreeMap<String, Team>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Team {
    /// Linear user IDs whose replies in an Agent Session reach the coordinator.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    /// Linear user IDs whose delegations the ticker takes; `allowed_user_ids`
    /// when unset.
    #[serde(default)]
    pub allowed_delegator_ids: Option<Vec<String>>,
    #[serde(default = "default_review_state")]
    pub review_state: String,
    #[serde(default = "default_started_state")]
    pub started_state: String,
    #[serde(default = "default_deploy_state")]
    pub deploy_state: String,
    #[serde(default = "default_postmortem_state")]
    pub postmortem_state: String,
    #[serde(default = "default_done_state")]
    pub done_state: String,
    /// Let an allowed Linear reply authorize a worker PR merge.
    #[serde(default)]
    pub merge_on_authorization: bool,
    /// Assess a worker PR with one configured headless decision profile.
    #[serde(default)]
    pub merge_decision: Option<MergeDecisionConfig>,
    /// The routing of this team's issues: a table under `routing`.
    pub routing: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MergeDecisionConfig {
    pub profile: String,
    pub max_risk_score: u8,
}

impl Team {
    /// Whose delegations the ticker takes.
    pub fn delegators(&self) -> &[String] {
        self.allowed_delegator_ids
            .as_deref()
            .unwrap_or(&self.allowed_user_ids)
    }
}

fn default_callback_port() -> u16 {
    DEFAULT_CALLBACK_PORT
}

fn default_review_state() -> String {
    "In Review".into()
}

fn default_started_state() -> String {
    "In Progress".into()
}
fn default_deploy_state() -> String {
    "In Deploy".into()
}
fn default_postmortem_state() -> String {
    "In Postmortem".into()
}
fn default_done_state() -> String {
    "Done".into()
}

fn default_linear_interval() -> u64 {
    5
}

/// The longest Linear interval accepted: one hour.
const MAX_LINEAR_INTERVAL: u64 = 3600;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Herdr {
    /// The Herdr session runs are started in; herdr's default session when unset.
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_runs: u32,
    pub max_workers_per_run: u32,
    pub max_agents: u32,
    /// After this many hours a run asks a person whether to continue.
    pub ask_to_continue_after_hours: u64,
    /// How long a routing agent may take, unless its profile says.
    pub routing_agent_timeout_seconds: u64,
    /// How long a postmortem agent may take, unless its profile says.
    pub postmortem_agent_timeout_seconds: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_runs: 2,
            max_workers_per_run: 4,
            max_agents: 8,
            ask_to_continue_after_hours: 8,
            routing_agent_timeout_seconds: 120,
            postmortem_agent_timeout_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Notifications {
    /// Also show a Herdr notification when a run needs a person.
    pub herdr: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications { herdr: true }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Claude {
    /// Accept Claude Code's workspace trust dialog ahead of time: before
    /// starting Claude Code in a run folder or a worktree, set
    /// `hasTrustDialogAccepted` for the folder (and the worktree's main
    /// checkout) in `~/.claude.json`.
    pub auto_accept_trust_dialog: bool,
}

/// The optional, long-lived agent that reviews saved postmortems.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SupervisorConfig {
    /// An ordinary profile controlling the supervisor agent's CLI and model.
    pub profile: String,
    /// Maximum active generated variants per routing, defaults to five.
    #[serde(default = "default_generated_limit")]
    pub max_generated_profiles_per_routing: u32,
}

fn default_generated_limit() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// The main checkout, an absolute path.
    pub path: PathBuf,
    /// The branch workers start from. Never guessed.
    pub base: String,
    #[serde(default)]
    pub description: String,
}

/// A profile with its base chain resolved: what launch, routing and validation
/// see.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub kind: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Extra agent CLI arguments (permission mode, sandbox), passed unchecked.
    pub args: Vec<String>,
    pub description: String,
    /// The `instructions.md` of the profile and of the profiles it is based
    /// on, from the root to the profile itself; layers without one are left
    /// out. Markdown the plugin adds to the agent's instructions for work
    /// under this profile, after the built-in sheet. Only people write it.
    pub instructions: Vec<Instructions>,
    /// Environment variables for routing, postmortem, and merge decision
    /// agent calls. Herdr starts coordinators and workers and cannot pass
    /// them any.
    pub env: BTreeMap<String, String>,
    /// How long a routing, postmortem, or merge decision call may take.
    /// Unset values use the role's limit; merge decisions use the routing limit.
    pub timeout_seconds: Option<u64>,
}

/// One layer of a profile's instructions: whose `instructions.md` it is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Instructions {
    pub profile: String,
    pub text: String,
}

/// A profile's `config.toml` as written. `base` names the profile it
/// inherits from; a field it leaves out comes from there.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    base: Option<String>,
    kind: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    args: Option<Vec<String>>,
    description: Option<String>,
    env: Option<BTreeMap<String, String>>,
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    /// The profile of the routing agent that picks an issue's coordinator
    /// and postmortem; needed only when one of them has several candidates.
    pub agent: Option<String>,
    /// The coordinator profiles the routing agent picks from; the first one
    /// when the agent gives no valid answer.
    pub coordinators: Vec<String>,
    /// The profiles that write the runs' postmortems, picked like the
    /// coordinator; none writes no postmortem.
    #[serde(default)]
    pub postmortems: Vec<String>,

    /// The profiles a coordinator may start workers with.
    pub workers: Vec<String>,
    /// Human-approved manual bases for generated profile variants.
    #[serde(default)]
    pub generated: GeneratedPolicy,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GeneratedPolicy {
    #[serde(default)]
    pub coordinators: Vec<String>,
    #[serde(default)]
    pub workers: Vec<String>,
    #[serde(default)]
    pub postmortems: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedRole {
    Coordinator,
    Worker,
    Postmortem,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GeneratedCandidateKey {
    pub workspace: String,
    pub team: String,
    pub routing: String,
    pub role: GeneratedRole,
}

impl GeneratedPolicy {
    pub fn is_empty(&self) -> bool {
        self.coordinators.is_empty() && self.workers.is_empty() && self.postmortems.is_empty()
    }
}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    pub fn load(config_dir: &Path) -> Result<Config> {
        let path = Self::path(config_dir);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let profiles = load_profiles(&config_dir.join(PROFILES_DIR))?;
        Self::parse(&text, profiles).with_context(|| format!("{} is not valid", path.display()))
    }

    /// Loads one config snapshot only when its before/after content hashes
    /// match. This detects ordinary concurrent edits without claiming a
    /// multi-file atomic read.
    pub fn load_stable(config_dir: &Path) -> Result<(Config, [u8; 32])> {
        let before = Self::fingerprint(config_dir)?;
        let config = Self::load(config_dir)?;
        let after = Self::fingerprint(config_dir)?;
        anyhow::ensure!(before == after, "config changed while it was being read");
        Ok((config, after))
    }

    pub fn fingerprint_hex(fingerprint: &[u8; 32]) -> String {
        fingerprint
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Loads manual config and profile snapshots from the committed generated catalog.
    /// Catalog damage disables generated selection but leaves ordinary config usable.
    pub fn load_with_state_dir(config_dir: &Path, state_dir: &Path) -> Result<Config> {
        let mut config = Self::load(config_dir)?;
        let store = crate::supervisor::SupervisorStore::new(state_dir.join("supervisor"));
        if !store.root().join("current.json").exists() {
            return Ok(config);
        }
        let current = match store.current() {
            Ok(current) => current,
            Err(error) => {
                config.generated_catalog_error = Some(format!("{error:#}"));
                return Ok(config);
            }
        };
        config.generated_catalog_generation = Some(current.catalog_generation);
        let revisions = match store.profile_revisions() {
            Ok(revisions) => revisions,
            Err(error) => {
                config.generated_catalog_error = Some(format!("{error:#}"));
                return Ok(config);
            }
        };
        let manual_profiles = config.profiles.clone();
        for revision in revisions {
            if config.profiles.contains_key(&revision.name) {
                config.generated_catalog_error = Some(format!(
                    "generated profile name `{}` collides with a manual profile",
                    revision.name
                ));
                config.profiles = manual_profiles;
                return Ok(config);
            }
            config
                .profiles
                .insert(revision.name.clone(), revision.profile);
        }
        let mut generated_candidates = BTreeMap::new();
        for (workspace, configured) in &config.workspaces {
            for (team, value) in &configured.teams {
                let scope = crate::supervisor::Scope {
                    workspace: workspace.clone(),
                    team: team.clone(),
                    routing: value.routing.clone(),
                };
                for role in [
                    GeneratedRole::Coordinator,
                    GeneratedRole::Worker,
                    GeneratedRole::Postmortem,
                ] {
                    match store.active_profile_names(&config, &scope, role) {
                        Ok(names) if !names.is_empty() => {
                            generated_candidates.insert(
                                GeneratedCandidateKey {
                                    workspace: workspace.clone(),
                                    team: team.clone(),
                                    routing: value.routing.clone(),
                                    role,
                                },
                                names,
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            config.generated_catalog_error = Some(format!("{error:#}"));
                            config.profiles = manual_profiles;
                            return Ok(config);
                        }
                    }
                }
            }
        }
        config.generated_candidates = generated_candidates;
        Ok(config)
    }

    /// A SHA-256 over the files `load` reads, by sorted path and content:
    /// `config.toml` and each profile folder's `config.toml` and
    /// `instructions.md`. Rewriting a file with the same content keeps it.
    pub fn fingerprint(config_dir: &Path) -> Result<[u8; 32]> {
        let path = Self::path(config_dir);
        let mut files = vec![path];
        for entry in std::fs::read_dir(config_dir.join(PROFILES_DIR))
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            files.push(entry.path().join(FILE_NAME));
            files.push(entry.path().join(INSTRUCTIONS_FILE));
        }
        files.sort();
        let mut hash = sha2::Sha256::new();
        for file in files {
            match std::fs::read(&file) {
                Ok(bytes) => {
                    hash.update(file.as_os_str().as_encoded_bytes());
                    hash.update([0]);
                    hash.update((bytes.len() as u64).to_le_bytes());
                    hash.update(&bytes);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && file != Self::path(config_dir) => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("could not read {}", file.display()));
                }
            }
        }
        Ok(hash.finalize().into())
    }

    fn parse(text: &str, profiles: BTreeMap<String, Profile>) -> Result<Config> {
        let table: toml::Table = toml::from_str(text)?;
        ensure!(
            !table.contains_key("profiles"),
            "profiles are not set here: put each one in {PROFILES_DIR}/<name>/{FILE_NAME} next to this file"
        );
        let mut config: Config = table.try_into()?;
        config.profiles = profiles;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.workspaces.is_empty(), "no workspace is configured");
        for (name, workspace) in &self.workspaces {
            ensure!(
                crate::run::valid_workspace(name),
                "workspace name `{name}` may use only lower-case letters, digits and `-`, start with a letter, and have at most 16 characters"
            );
            ensure!(
                !workspace.client_id.trim().is_empty(),
                "workspaces.{name}.client_id is empty"
            );
            ensure!(
                workspace.callback_port != 0,
                "workspaces.{name}.callback_port may not be 0"
            );
            ensure!(
                !workspace.teams.is_empty(),
                "workspaces.{name}.teams lists no team"
            );
            for (key, team) in &workspace.teams {
                ensure!(
                    crate::run::valid_team_key(key),
                    "workspaces.{name}.teams.{key}: `{key}` is not a Linear team key"
                );
                ensure!(
                    !team.review_state.trim().is_empty(),
                    "workspaces.{name}.teams.{key}.review_state is empty"
                );
                for (field, value) in [
                    ("started_state", &team.started_state),
                    ("deploy_state", &team.deploy_state),
                    ("postmortem_state", &team.postmortem_state),
                    ("done_state", &team.done_state),
                ] {
                    ensure!(
                        !value.trim().is_empty(),
                        "workspaces.{name}.teams.{key}.{field} is empty"
                    );
                }
            }
            for (key, seconds) in [
                ("intake_interval_seconds", workspace.intake_interval_seconds),
                (
                    "run_read_interval_seconds",
                    workspace.run_read_interval_seconds,
                ),
            ] {
                ensure!(
                    (1..=MAX_LINEAR_INTERVAL).contains(&seconds),
                    "workspaces.{name}.{key} must be between 1 and {MAX_LINEAR_INTERVAL}"
                );
            }
        }
        let limits = &self.limits;
        ensure!(
            limits.max_runs >= 1 && limits.max_workers_per_run >= 1 && limits.max_agents >= 2,
            "limits must allow one run with one worker"
        );
        if let Some(supervisor) = &self.supervisor {
            self.profile(&supervisor.profile)
                .with_context(|| "supervisor.profile".to_string())?;
            ensure!(
                (1..=20).contains(&supervisor.max_generated_profiles_per_routing),
                "supervisor.max_generated_profiles_per_routing must be between 1 and 20"
            );
            ensure!(
                limits.max_agents >= 3,
                "limits.max_agents must be at least 3 when the supervisor is enabled"
            );
        }
        ensure!(
            limits.ask_to_continue_after_hours >= 1,
            "limits.ask_to_continue_after_hours must be at least 1"
        );
        for (key, seconds) in [
            (
                "routing_agent_timeout_seconds",
                limits.routing_agent_timeout_seconds,
            ),
            (
                "postmortem_agent_timeout_seconds",
                limits.postmortem_agent_timeout_seconds,
            ),
        ] {
            ensure!(
                (1..=3600).contains(&seconds),
                "limits.{key} must be between 1 and 3600"
            );
        }

        for (name, repo) in &self.repositories {
            ensure!(
                valid_name(name),
                "repository name `{name}` may use only letters, digits, `.`, `_` and `-`"
            );
            ensure!(
                repo.path.is_absolute(),
                "repositories.{name}.path must be an absolute path"
            );
            ensure!(
                !repo.base.trim().is_empty() && !repo.base.starts_with('-'),
                "repositories.{name}.base is not a branch name"
            );
        }
        for (name, profile) in &self.profiles {
            ensure!(
                valid_name(name),
                "profile name `{name}` may use only letters, digits, `.`, `_` and `-`"
            );
            ensure!(
                agents::is_kind(&profile.kind),
                "profile `{name}`: kind `{}` is not a Herdr agent kind",
                profile.kind
            );
            if profile.effort.is_some() && !agents::has_effort_flag(&profile.kind) {
                bail!(
                    "profile `{name}`: the `{}` CLI has no effort flag; put the effort in the model ID instead",
                    profile.kind
                );
            }
        }

        ensure!(!self.routing.is_empty(), "no routing is configured");
        let headless = |at: &str, name: &str, what: &str| -> Result<()> {
            let profile = self.profile(name).with_context(|| at.to_string())?;
            ensure!(
                routing::registered(&profile.kind),
                "{at}: the `{}` kind cannot be {what}",
                profile.kind
            );
            Ok(())
        };
        for (name, routing) in &self.routing {
            let at = |key: &str| format!("routing.{name}.{key}");
            ensure!(
                !routing.coordinators.is_empty(),
                "{} lists no profile",
                at("coordinators")
            );
            for profile in &routing.coordinators {
                self.profile(profile).with_context(|| at("coordinators"))?;
            }
            ensure!(
                !routing.workers.is_empty(),
                "{} lists no profile",
                at("workers")
            );
            for profile in &routing.workers {
                self.profile(profile).with_context(|| at("workers"))?;
            }
            for profile in &routing.postmortems {
                headless(&at("postmortems"), profile, "a postmortem agent")?;
                ensure!(
                    !self.profiles[profile].instructions.is_empty(),
                    "{}: profile `{profile}` has no {INSTRUCTIONS_FILE}, which holds the method",
                    at("postmortems")
                );
            }
            ensure!(
                self.supervisor.is_some() || routing.generated.is_empty(),
                "routing.{name}.generated requires [supervisor]"
            );
            let generated_role = |key: &str, allowed: &[String], manual: &[String]| -> Result<()> {
                for base in allowed {
                    ensure!(
                        !base.starts_with("generated-"),
                        "routing.{name}.generated.{key}: generated profiles cannot be bases"
                    );
                    ensure!(
                        manual.iter().any(|profile| profile == base),
                        "routing.{name}.generated.{key}: `{base}` is not an approved manual profile for this role"
                    );
                    self.profile(base)
                        .with_context(|| format!("routing.{name}.generated.{key}"))?;
                }
                Ok(())
            };
            generated_role(
                "coordinators",
                &routing.generated.coordinators,
                &routing.coordinators,
            )?;
            generated_role("workers", &routing.generated.workers, &routing.workers)?;
            generated_role(
                "postmortems",
                &routing.generated.postmortems,
                &routing.postmortems,
            )?;
            let several = routing.coordinators.len() > 1
                || routing.postmortems.len() > 1
                || !routing.generated.coordinators.is_empty()
                || !routing.generated.postmortems.is_empty();
            match &routing.agent {
                Some(agent) => headless(&at("agent"), agent, "a routing agent")?,
                None => ensure!(
                    !several,
                    "{}: set it, since the coordinators or the postmortem profiles have several candidates",
                    at("agent")
                ),
            }
        }
        for (name, profile) in &self.profiles {
            for key in profile.env.keys() {
                ensure!(
                    !key.is_empty() && !key.contains('='),
                    "profile `{name}`: env has an invalid variable name `{key}`"
                );
            }
            let used = self
                .routing
                .values()
                .any(|r| r.coordinators.contains(name) || r.workers.contains(name));
            ensure!(
                profile.env.is_empty() || !used,
                "profile `{name}`: env applies only to routing, postmortem, or merge decision agents, but `{name}` is also a coordinator or worker profile"
            );
            ensure!(
                profile.timeout_seconds.is_none() || !used,
                "profile `{name}`: timeout_seconds applies only to routing, postmortem, or merge decision agents, but `{name}` is also a coordinator or worker profile"
            );
            if let Some(seconds) = profile.timeout_seconds {
                ensure!(
                    (1..=3600).contains(&seconds),
                    "profile `{name}`: timeout_seconds must be between 1 and 3600"
                );
            }
        }
        for (name, workspace) in &self.workspaces {
            for (key, team) in &workspace.teams {
                ensure!(
                    self.routing.contains_key(&team.routing),
                    "workspaces.{name}.teams.{key}.routing: there is no routing.{}",
                    team.routing
                );
                if let Some(decision) = &team.merge_decision {
                    let at = format!("workspaces.{name}.teams.{key}.merge_decision");
                    ensure!(
                        !team.merge_on_authorization,
                        "{at}: set either merge_on_authorization or merge_decision, not both"
                    );
                    ensure!(
                        decision.max_risk_score <= 100,
                        "{at}.max_risk_score must be between 0 and 100"
                    );
                    headless(&at, &decision.profile, "a merge decision agent")?;
                    ensure!(
                        !self.profiles[&decision.profile].instructions.is_empty(),
                        "{at}: profile `{}` has no {INSTRUCTIONS_FILE}, which holds the method",
                        decision.profile
                    );
                    for key in ["HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR"] {
                        ensure!(
                            !self.profiles[&decision.profile].env.contains_key(key),
                            "{at}: profile `{}` may not set {key}",
                            decision.profile
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles.get(name).with_context(|| {
            format!("no profile named `{name}`: add {PROFILES_DIR}/{name}/{FILE_NAME}")
        })
    }

    /// The routing of a team's issues.
    pub fn routing_of(&self, workspace: &str, team: &str) -> Result<&Routing> {
        let name = &self.team(workspace, team)?.routing;
        self.routing
            .get(name)
            .with_context(|| format!("there is no routing.{name}"))
    }

    pub fn worker_profile_for<'a>(
        &'a self,
        workspace: &str,
        team: &str,
        name: &str,
    ) -> Result<&'a Profile> {
        let routing = self.routing_of(workspace, team)?;
        let key = GeneratedCandidateKey {
            workspace: workspace.into(),
            team: team.into(),
            routing: self.team(workspace, team)?.routing.clone(),
            role: GeneratedRole::Worker,
        };
        if routing.workers.iter().any(|candidate| candidate == name)
            || self
                .generated_candidates
                .get(&key)
                .is_some_and(|candidates| candidates.iter().any(|candidate| candidate == name))
        {
            return self.profile(name);
        }
        bail!(
            "`{name}` is not a worker profile authorized for {workspace}/{team}; the routing's workers are {}",
            routing.workers.join(", ")
        )
    }

    pub fn profile_candidates_for(
        &self,
        workspace: &str,
        team: &str,
        role: GeneratedRole,
        manual: &[String],
    ) -> Vec<String> {
        let Some(team_config) = self
            .workspaces
            .get(workspace)
            .and_then(|workspace| workspace.teams.get(team))
        else {
            return manual.to_vec();
        };
        let Some(_routing) = self.routing.get(&team_config.routing) else {
            return manual.to_vec();
        };
        let key = GeneratedCandidateKey {
            workspace: workspace.into(),
            team: team.into(),
            routing: team_config.routing.clone(),
            role,
        };
        let mut names = manual.to_vec();
        names.extend(
            self.generated_candidates
                .get(&key)
                .into_iter()
                .flatten()
                .cloned(),
        );
        names
    }

    pub fn workspace(&self, name: &str) -> Result<&Workspace> {
        self.workspaces
            .get(name)
            .with_context(|| format!("no workspace named `{name}` in the config"))
    }

    /// The team a run belongs to: its workspace's entry for the team key.
    pub fn team(&self, workspace: &str, team_key: &str) -> Result<&Team> {
        self.workspace(workspace)?
            .teams
            .get(team_key)
            .with_context(|| {
                format!("team `{team_key}` is not configured in workspace `{workspace}`")
            })
    }

    pub fn repository(&self, name: &str) -> Result<&Repository> {
        self.repositories.get(name).with_context(|| {
            let known: Vec<&str> = self.repositories.keys().map(String::as_str).collect();
            format!(
                "`{name}` is not in the repository catalog ({})",
                if known.is_empty() {
                    "empty".to_string()
                } else {
                    known.join(", ")
                }
            )
        })
    }
}

/// Reads every profile folder under `dir`: the folder's name is the profile's
/// name. Entries starting with `.` are skipped, symbolic links are followed,
/// and files other than the two a profile has are left alone. A missing `dir`
/// gives no profiles. Each profile is then resolved against its base chain.
fn load_profiles(dir: &Path) -> Result<BTreeMap<String, Profile>> {
    let mut files = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", dir.display()));
        }
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("could not read {}", dir.display()))?
            .path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') {
            continue;
        }
        ensure!(
            !name.starts_with("generated-"),
            "profile name `{name}` uses the reserved generated profile namespace"
        );
        ensure!(
            path.is_dir(),
            "{} is not a folder: each profile is a folder with a {FILE_NAME}",
            path.display()
        );
        files.insert(name, read_profile(&path)?);
    }
    files
        .keys()
        .map(|name| Ok((name.clone(), resolve(name, &files)?)))
        .collect()
}

/// A profile folder's `config.toml` and its `instructions.md`, `None` when
/// that is missing or blank.
fn read_profile(folder: &Path) -> Result<(ProfileFile, Option<String>)> {
    let path = folder.join(FILE_NAME);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let file: ProfileFile =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    let path = folder.join(INSTRUCTIONS_FILE);
    let instructions = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => None,
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    Ok((file, instructions))
}

/// Lays `name`'s base chain from its root to `name`: a later layer replaces
/// `model`, `effort`, `args` and `env` whole, may not change `kind`, and
/// adds its instructions; `description` is `name`'s own.
fn resolve(name: &str, files: &BTreeMap<String, (ProfileFile, Option<String>)>) -> Result<Profile> {
    let mut chain = vec![name];
    while let Some(base) = files[*chain.last().unwrap()].0.base.as_deref() {
        if chain.contains(&base) {
            chain.push(base);
            bail!(
                "profile `{name}`: its base chain loops: {}",
                chain.join(" \u{2192} ")
            );
        }
        ensure!(
            files.contains_key(base),
            "profile `{}`: base `{base}` is not a profile",
            chain.last().unwrap()
        );
        chain.push(base);
    }
    let mut kind: Option<(&str, &str)> = None;
    let (mut model, mut effort, mut args, mut env) = (None, None, Vec::new(), BTreeMap::new());
    let mut timeout_seconds = None;
    let mut instructions = Vec::new();
    for layer in chain.iter().rev() {
        let (file, text) = &files[*layer];
        if let Some(own) = file.kind.as_deref() {
            match kind {
                Some((inherited, from)) if inherited != own => bail!(
                    "profile `{layer}`: kind `{own}` differs from its base `{from}` (`{inherited}`)"
                ),
                Some(_) => {}
                None => kind = Some((own, layer)),
            }
        }
        model = file.model.clone().or(model);
        effort = file.effort.clone().or(effort);
        args = file.args.clone().unwrap_or(args);
        env = file.env.clone().unwrap_or(env);
        timeout_seconds = file.timeout_seconds.or(timeout_seconds);
        if let Some(text) = text {
            instructions.push(Instructions {
                profile: layer.to_string(),
                text: text.clone(),
            });
        }
    }
    let Some((kind, _)) = kind else {
        bail!("profile `{name}`: kind is set neither here nor in a base");
    };
    Ok(Profile {
        kind: kind.to_string(),
        model,
        effort,
        args,
        description: files[name].0.description.clone().unwrap_or_default(),
        instructions,
        env,
        timeout_seconds,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with(['-', '.'])
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A config with every section, used across the test suite. Its profiles
    /// are [`SAMPLE_PROFILES`].
    pub const SAMPLE: &str = r#"
[workspaces.acme]
client_id = "client-123"

[workspaces.acme.teams.DATA]
allowed_user_ids = ["user-1"]
review_state = "In Review"
started_state = "In Progress"
deploy_state = "In Deploy"
postmortem_state = "In Postmortem"
done_state = "Done"
# merge_decision = { profile = "merge-risk", max_risk_score = 20 }
routing = "default"

[herdr]
session = "work"

[repositories.api]
path = "/src/api"
base = "main"
description = "The API server"

[repositories.web]
path = "/src/web"
base = "develop"

[routing.default]
agent = "router"
coordinators = ["coordinator", "coordinator-light"]
workers = ["standard", "deep"]
"#;

    /// The sample's profile folders: name, `config.toml`, `instructions.md`.
    pub const SAMPLE_PROFILES: [(&str, &str, Option<&str>); 6] = [
        (
            "coordinator",
            "kind = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"default coordinator\"\n",
            None,
        ),
        (
            "coordinator-light",
            "kind = \"claude\"\nmodel = \"sonnet\"\ndescription = \"small issues\"\n",
            Some("Prefer one worker. Ask before you split the work.\n"),
        ),
        ("router", "kind = \"claude\"\nmodel = \"haiku\"\n", None),
        (
            "standard",
            "kind = \"claude\"\nmodel = \"sonnet\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"scoped features and fixes\"\n",
            None,
        ),
        (
            "deep",
            "kind = \"codex\"\nmodel = \"gpt-6-sol\"\neffort = \"xhigh\"\nargs = [\"-s\", \"workspace-write\"]\ndescription = \"cross-module changes\"\n",
            None,
        ),
        (
            "merge-risk",
            "kind = \"codex\"\nmodel = \"gpt-6-sol\"\n",
            Some("Assess the change and its evidence, then report its risks.\n"),
        ),
    ];

    /// Writes `config.toml` with `text` and the sample's profile folders.
    pub fn write_sample(config_dir: &Path, text: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join(FILE_NAME), text).unwrap();
        for (name, config, instructions) in SAMPLE_PROFILES {
            let folder = config_dir.join(PROFILES_DIR).join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(FILE_NAME), config).unwrap();
            if let Some(text) = instructions {
                std::fs::write(folder.join(INSTRUCTIONS_FILE), text).unwrap();
            }
        }
    }

    /// The sample config as the ticker loads it.
    pub fn sample() -> Config {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        Config::load(dir.path()).unwrap()
    }

    /// Loads the sample with `text` as `config.toml` and `profiles` replacing
    /// or adding profile folders' `config.toml`. The error is the full chain.
    fn load_with(text: &str, profiles: &[(&str, &str)]) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), text);
        for (name, config) in profiles {
            let folder = dir.path().join(PROFILES_DIR).join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(FILE_NAME), config).unwrap();
        }
        Config::load(dir.path()).map_err(|e| format!("{e:#}"))
    }

    fn load_with_merge_profile(
        text: &str,
        profile: &str,
        profile_config: &str,
        instructions: Option<&str>,
    ) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), text);
        let folder = dir.path().join(PROFILES_DIR).join(profile);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(FILE_NAME), profile_config).unwrap();
        let instructions_path = folder.join(INSTRUCTIONS_FILE);
        if let Some(text) = instructions {
            std::fs::write(instructions_path, text).unwrap();
        } else {
            let _ = std::fs::remove_file(instructions_path);
        }
        Config::load(dir.path()).map_err(|e| format!("{e:#}"))
    }

    fn merge_decision(text: &str, profile: &str, max_risk_score: u8) -> String {
        text.replacen(
            "routing = \"default\"",
            &format!(
                "merge_decision = {{ profile = \"{profile}\", max_risk_score = {max_risk_score} }}\nrouting = \"default\""
            ),
            1,
        )
    }

    #[test]
    fn the_sample_parses_with_defaults() {
        let config = sample();
        let acme = config.workspace("acme").unwrap();
        assert_eq!(acme.callback_port, DEFAULT_CALLBACK_PORT);
        assert_eq!(
            config.team("acme", "DATA").unwrap().review_state,
            "In Review"
        );
        assert!(
            config
                .team("acme", "DATA")
                .unwrap()
                .merge_decision
                .is_none()
        );
        assert!(
            config
                .team("acme", "WEB")
                .unwrap_err()
                .to_string()
                .contains("team `WEB` is not configured in workspace `acme`")
        );
        assert_eq!(config.limits, Limits::default());
        assert!(config.notifications.herdr);
        let routing = config.routing_of("acme", "DATA").unwrap();
        assert_eq!(routing.coordinators, ["coordinator", "coordinator-light"]);
        assert_eq!(
            config.profiles.keys().collect::<Vec<_>>(),
            [
                "coordinator",
                "coordinator-light",
                "deep",
                "merge-risk",
                "router",
                "standard"
            ]
        );
        assert_eq!(
            config.profile("coordinator-light").unwrap().instructions,
            [Instructions {
                profile: "coordinator-light".into(),
                text: "Prefer one worker. Ask before you split the work.\n".into(),
            }]
        );
        assert_eq!(config.profile("coordinator").unwrap().instructions, []);
        assert_eq!(
            config
                .worker_profile_for("acme", "DATA", "deep")
                .unwrap()
                .kind,
            "codex"
        );
        assert!(
            config
                .worker_profile_for("acme", "DATA", "coordinator")
                .is_err()
        );
        assert!(
            config
                .repository("nope")
                .unwrap_err()
                .to_string()
                .contains("api, web")
        );
    }

    #[test]
    fn workflow_states_default_when_omitted() {
        let text = SAMPLE
            .lines()
            .filter(|line| {
                !line.starts_with("review_state = ")
                    && !line.starts_with("started_state = ")
                    && !line.starts_with("deploy_state = ")
                    && !line.starts_with("postmortem_state = ")
                    && !line.starts_with("done_state = ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let team = load_with(&text, &[])
            .unwrap()
            .team("acme", "DATA")
            .unwrap()
            .clone();
        assert_eq!(
            (
                team.started_state.as_str(),
                team.review_state.as_str(),
                team.deploy_state.as_str(),
                team.postmortem_state.as_str(),
                team.done_state.as_str()
            ),
            (
                "In Progress",
                "In Review",
                "In Deploy",
                "In Postmortem",
                "Done"
            )
        );
    }

    #[test]
    fn invalid_configs_are_refused() {
        let bad = |from: &str, to: &str| {
            let text = SAMPLE.replacen(from, to, 1);
            assert_ne!(text, SAMPLE, "{from}");
            load_with(&text, &[]).unwrap_err()
        };
        bad(
            "[workspaces.acme.teams.DATA]",
            "[workspaces.acme.teams.data]",
        );
        bad("client_id = \"client-123\"", "client_id = \" \"");
        bad("[workspaces.acme]", "[workspaces.Acme]");
        for (field, value) in [
            ("review_state", "In Review"),
            ("started_state", "In Progress"),
            ("deploy_state", "In Deploy"),
            ("postmortem_state", "In Postmortem"),
            ("done_state", "Done"),
        ] {
            let error = bad(
                &format!("{field} = \"{value}\""),
                &format!("{field} = \"  \""),
            );
            assert!(error.contains(&format!("workspaces.acme.teams.DATA.{field} is empty")));
        }
        bad(
            "workers = [\"standard\", \"deep\"]",
            "workers = [\"standard\", \"deep\"]\ndefault = \"coordinator\"",
        );
        bad(
            "workers = [\"standard\", \"deep\"]",
            "workers = [\"missing\"]",
        );
        bad("path = \"/src/api\"", "path = \"src/api\"");
        bad("agent = \"router\"", "agent = \"deep-x\"");
        bad(
            "coordinators = [\"coordinator\", \"coordinator-light\"]",
            "coordinators = [\"coordinator\", \"missing\"]",
        );
        bad(
            "coordinators = [\"coordinator\", \"coordinator-light\"]",
            "coordinators = []",
        );
        bad(
            "[herdr]",
            "[limits]\nrouting_agent_timeout_seconds = 0\n\n[herdr]",
        );
        // The size-based routing is gone.
        bad(
            "workers = [\"standard\", \"deep\"]",
            "workers = [\"standard\", \"deep\"]\nsize_label_group = \"size\"",
        );
        bad("[herdr]", "[herdr]\nunknown = 1");
        assert!(
            load_with(SAMPLE, &[("deep", "kind = \"chatgpt\"\n")])
                .unwrap_err()
                .ends_with("profile `deep`: kind `chatgpt` is not a Herdr agent kind")
        );
        // Effort only for kinds with an effort flag.
        assert!(
            load_with(
                SAMPLE,
                &[("router", "kind = \"cursor\"\neffort = \"high\"\n")]
            )
            .unwrap_err()
            .ends_with("profile `router`: the `cursor` CLI has no effort flag; put the effort in the model ID instead")
        );
    }

    #[test]
    fn profiles_come_from_their_folders() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        std::fs::write(profiles.join(".DS_Store"), "").unwrap();
        std::fs::write(profiles.join("router").join("README.md"), "Shared router.").unwrap();
        std::fs::write(profiles.join("standard").join(INSTRUCTIONS_FILE), " \n").unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::write(shared.path().join(FILE_NAME), "kind = \"codex\"\n").unwrap();
        std::fs::write(shared.path().join(INSTRUCTIONS_FILE), "Run `make check`.\n").unwrap();
        std::os::unix::fs::symlink(shared.path(), profiles.join("shared")).unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(
            config.profiles.keys().collect::<Vec<_>>(),
            [
                "coordinator",
                "coordinator-light",
                "deep",
                "merge-risk",
                "router",
                "shared",
                "standard"
            ]
        );
        assert_eq!(config.profile("standard").unwrap().instructions, []);
        let shared = config.profile("shared").unwrap();
        assert_eq!(shared.kind, "codex");
        assert_eq!(shared.instructions[0].text, "Run `make check`.\n");
    }

    #[test]
    fn a_profile_inherits_from_its_base_chain() {
        let config = load_with(
            SAMPLE,
            &[
                (
                    "root",
                    "kind = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\nargs = [\"--permission-mode\", \"auto\"]\ndescription = \"the root\"\nenv = { A = \"1\" }\n",
                ),
                ("mid", "base = \"root\"\nmodel = \"sonnet\"\nargs = [\"--x\"]\n"),
                ("leaf", "base = \"mid\"\neffort = \"low\"\ndescription = \"the leaf\"\n"),
            ],
        )
        .unwrap();
        let leaf = config.profile("leaf").unwrap();
        assert_eq!(
            (
                leaf.kind.as_str(),
                leaf.model.as_deref(),
                leaf.effort.as_deref(),
                leaf.args.as_slice(),
                leaf.description.as_str(),
            ),
            (
                "claude",
                Some("sonnet"),
                Some("low"),
                &["--x".to_string()][..],
                "the leaf"
            )
        );
        assert_eq!(
            leaf.env,
            BTreeMap::from([("A".to_string(), "1".to_string())])
        );
        let mid = config.profile("mid").unwrap();
        assert_eq!(
            (mid.effort.as_deref(), mid.description.as_str()),
            (Some("high"), ""),
            "the description is not inherited"
        );
    }

    #[test]
    fn a_base_chain_must_end_at_a_profile_keep_its_kind_and_not_loop() {
        let error = |profiles: &[(&str, &str)]| load_with(SAMPLE, profiles).unwrap_err();
        assert!(
            error(&[
                ("b", "kind = \"claude\"\n"),
                ("a", "base = \"b\"\nkind = \"codex\"\n")
            ])
            .ends_with("profile `a`: kind `codex` differs from its base `b` (`claude`)")
        );
        assert!(
            error(&[("a", "base = \"a\"\nkind = \"claude\"\n")])
                .ends_with("profile `a`: its base chain loops: a \u{2192} a")
        );
        assert!(
            error(&[
                ("a", "base = \"b\"\nkind = \"claude\"\n"),
                ("b", "base = \"a\"\n")
            ])
            .ends_with("profile `a`: its base chain loops: a \u{2192} b \u{2192} a")
        );
        assert!(
            error(&[("a", "base = \"x\"\nkind = \"claude\"\n")])
                .ends_with("profile `a`: base `x` is not a profile")
        );
        assert!(
            error(&[("a", "base = \"b\"\n"), ("b", "model = \"opus\"\n")])
                .ends_with("profile `a`: kind is set neither here nor in a base")
        );
    }

    #[test]
    fn env_inherited_by_a_worker_profile_is_refused() {
        let error = load_with(
            SAMPLE,
            &[
                ("with-env", "kind = \"claude\"\nenv = { X = \"1\" }\n"),
                ("standard", "base = \"with-env\"\n"),
            ],
        )
        .unwrap_err();
        assert!(error.ends_with(
            "profile `standard`: env applies only to routing, postmortem, or merge decision agents, but `standard` is also a coordinator or worker profile"
        ), "{error}");
    }

    #[test]
    fn instructions_are_the_layers_that_have_them_from_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        for (name, config, instructions) in [
            ("root", "kind = \"claude\"\n", Some("Root rule.\n")),
            ("mid", "base = \"root\"\n", None),
            ("leaf", "base = \"mid\"\n", Some("Leaf rule.\n")),
        ] {
            std::fs::create_dir_all(profiles.join(name)).unwrap();
            std::fs::write(profiles.join(name).join(FILE_NAME), config).unwrap();
            if let Some(text) = instructions {
                std::fs::write(profiles.join(name).join(INSTRUCTIONS_FILE), text).unwrap();
            }
        }
        let config = Config::load(dir.path()).unwrap();
        let layer = |profile: &str, text: &str| Instructions {
            profile: profile.into(),
            text: text.into(),
        };
        assert_eq!(
            config.profile("leaf").unwrap().instructions,
            [layer("root", "Root rule.\n"), layer("leaf", "Leaf rule.\n")]
        );
        assert_eq!(
            config.profile("mid").unwrap().instructions,
            [layer("root", "Root rule.\n")]
        );
    }

    #[test]
    fn the_fingerprint_follows_the_content_of_the_files_load_reads() {
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let first = Config::fingerprint(dir.path()).unwrap();
        write_sample(dir.path(), SAMPLE);
        assert_eq!(
            Config::fingerprint(dir.path()).unwrap(),
            first,
            "rewritten with the same content"
        );
        let instructions = dir
            .path()
            .join(PROFILES_DIR)
            .join("deep")
            .join(INSTRUCTIONS_FILE);
        std::fs::write(&instructions, "Run the slow tests too.\n").unwrap();
        let with_instructions = Config::fingerprint(dir.path()).unwrap();
        assert_ne!(with_instructions, first, "a new instructions.md");
        std::fs::write(&instructions, "Run the fast tests only.\n").unwrap();
        assert_ne!(Config::fingerprint(dir.path()).unwrap(), with_instructions);
        std::fs::write(dir.path().join(PROFILES_DIR).join(".notes"), "x").unwrap();
        std::fs::write(
            dir.path().join(PROFILES_DIR).join("deep").join("README.md"),
            "x",
        )
        .unwrap();
        std::fs::remove_file(&instructions).unwrap();
        assert_eq!(
            Config::fingerprint(dir.path()).unwrap(),
            first,
            "files load does not read leave it alone"
        );
    }

    #[test]
    fn profiles_live_only_in_their_folders() {
        let in_config = SAMPLE.to_string() + "\n[profiles.extra]\nkind = \"claude\"\n";
        assert!(load_with(&in_config, &[]).unwrap_err().ends_with(
            "profiles are not set here: put each one in profiles/<name>/config.toml next to this file"
        ));
        let instructions = load_with(
            SAMPLE,
            &[("router", "kind = \"claude\"\ninstructions = \"x\"\n")],
        )
        .unwrap_err();
        assert!(
            instructions.contains("profiles/router/config.toml is not valid"),
            "{instructions}"
        );
        assert!(
            instructions.contains("unknown field `instructions`"),
            "{instructions}"
        );

        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), SAMPLE);
        let profiles = dir.path().join(PROFILES_DIR);
        std::fs::write(profiles.join("extra.toml"), "kind = \"claude\"\n").unwrap();
        assert!(
            format!("{:#}", Config::load(dir.path()).unwrap_err()).ends_with(
                "extra.toml is not a folder: each profile is a folder with a config.toml"
            )
        );
        std::fs::remove_file(profiles.join("extra.toml")).unwrap();
        std::fs::create_dir(profiles.join("empty")).unwrap();
        assert!(format!("{:#}", Config::load(dir.path()).unwrap_err()).contains("could not read "));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), SAMPLE).unwrap();
        assert!(
            format!("{:#}", Config::load(dir.path()).unwrap_err()).ends_with(
                "routing.default.coordinators: no profile named `coordinator`: add profiles/coordinator/config.toml"
            )
        );
    }

    #[test]
    fn linear_intervals_default_to_five_seconds_and_are_checked() {
        let intervals = |config: &Config| {
            let acme = config.workspace("acme").unwrap();
            (acme.intake_interval_seconds, acme.run_read_interval_seconds)
        };
        assert_eq!(intervals(&sample()), (5, 5));
        let set = |lines: &str| {
            load_with(
                &SAMPLE.replacen(
                    "client_id = \"client-123\"",
                    &format!("client_id = \"client-123\"\n{lines}"),
                    1,
                ),
                &[],
            )
        };
        let config = set("intake_interval_seconds = 30\nrun_read_interval_seconds = 10").unwrap();
        assert_eq!(intervals(&config), (30, 10));
        assert!(
            set("intake_interval_seconds = 0")
                .unwrap_err()
                .ends_with("workspaces.acme.intake_interval_seconds must be between 1 and 3600")
        );
        assert!(
            set("run_read_interval_seconds = 3601")
                .unwrap_err()
                .ends_with("workspaces.acme.run_read_interval_seconds must be between 1 and 3600")
        );
    }

    #[test]
    fn teams_pick_a_routing_whose_postmortems_are_profiles_with_a_method() {
        let routing_b = "\n[routing.light]\ncoordinators = [\"coordinator-light\"]\npostmortems = [\"reviewer\"]\nworkers = [\"standard\"]\n";
        let team = |text: &str| text.replace("routing = \"default\"", "routing = \"light\"");
        let reviewer = ("reviewer", "kind = \"claude\"\nmodel = \"haiku\"\n");
        let error = load_with(&team(SAMPLE), &[]).unwrap_err();
        assert!(
            error.ends_with("workspaces.acme.teams.DATA.routing: there is no routing.light"),
            "{error}"
        );
        let text = team(SAMPLE) + routing_b;
        let error = load_with(&text, &[reviewer]).unwrap_err();
        assert!(
            error.ends_with(
                "routing.light.postmortems: profile `reviewer` has no instructions.md, which holds the method"
            ),
            "{error}"
        );

        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), &text);
        let folder = dir.path().join(PROFILES_DIR).join("reviewer");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(FILE_NAME), reviewer.1).unwrap();
        std::fs::write(folder.join(INSTRUCTIONS_FILE), "Say what went well.\n").unwrap();
        let config = Config::load(dir.path()).unwrap();
        let routing = config.routing_of("acme", "DATA").unwrap();
        assert_eq!(
            (routing.agent.as_deref(), routing.postmortems.as_slice()),
            (None, &["reviewer".to_string()][..]),
            "one candidate each needs no routing agent"
        );
        assert_eq!(
            (
                config.limits.ask_to_continue_after_hours,
                config.limits.routing_agent_timeout_seconds,
                config.limits.postmortem_agent_timeout_seconds
            ),
            (8, 120, 300)
        );

        let two = text.replace(
            "postmortems = [\"reviewer\"]",
            "postmortems = [\"reviewer\", \"coordinator-light\"]",
        );
        std::fs::write(dir.path().join(FILE_NAME), &two).unwrap();
        let error = format!("{:#}", Config::load(dir.path()).unwrap_err());
        assert!(
            error.ends_with("routing.light.agent: set it, since the coordinators or the postmortem profiles have several candidates"),
            "{error}"
        );
    }

    #[test]
    fn delegators_default_to_the_users_whose_replies_count() {
        let config = load_with(SAMPLE, &[]).unwrap();
        assert!(!config.team("acme", "DATA").unwrap().merge_on_authorization);
        let opted_in = SAMPLE.replace(
            "routing = \"default\"",
            "merge_on_authorization = true\nrouting = \"default\"",
        );
        assert!(
            load_with(&opted_in, &[])
                .unwrap()
                .team("acme", "DATA")
                .unwrap()
                .merge_on_authorization
        );
        assert_eq!(
            config.team("acme", "DATA").unwrap().delegators(),
            ["user-1"]
        );
        let text = SAMPLE.replace(
            "allowed_user_ids = [\"user-1\"]",
            "allowed_user_ids = [\"user-1\"]\nallowed_delegator_ids = [\"user-1\", \"linear-agent\"]",
        );
        let config = load_with(&text, &[]).unwrap();
        let team = config.team("acme", "DATA").unwrap();
        assert_eq!(team.delegators(), ["user-1", "linear-agent"]);
        assert_eq!(team.allowed_user_ids, ["user-1"]);
    }

    #[test]
    fn merge_decision_config_is_opt_in_and_accepts_inclusive_threshold_bounds() {
        let profile =
            "kind = \"claude\"\nenv = { DECISION_PROFILE = \"isolated\" }\ntimeout_seconds = 60\n";
        let instructions = "Review the complete change and evidence.\n";
        for score in [0, 100] {
            let text = merge_decision(SAMPLE, "merge-risk", score);
            let config =
                load_with_merge_profile(&text, "merge-risk", profile, Some(instructions)).unwrap();
            assert_eq!(
                config.team("acme", "DATA").unwrap().merge_decision,
                Some(MergeDecisionConfig {
                    profile: "merge-risk".into(),
                    max_risk_score: score,
                })
            );
            assert_eq!(
                config.profile("merge-risk").unwrap().timeout_seconds,
                Some(60)
            );
        }
    }

    #[test]
    fn merge_decision_config_rejects_invalid_mode_score_and_schema() {
        let profile = "kind = \"claude\"\n";
        let instructions = Some("Review the complete change.\n");

        let over_limit = merge_decision(SAMPLE, "merge-risk", 101);
        assert!(
            load_with_merge_profile(&over_limit, "merge-risk", profile, instructions)
                .unwrap_err()
                .contains("max_risk_score must be between 0 and 100")
        );

        let both_modes = merge_decision(
            &SAMPLE.replace(
                "routing = \"default\"",
                "merge_on_authorization = true\nrouting = \"default\"",
            ),
            "merge-risk",
            20,
        );
        assert!(
            load_with_merge_profile(&both_modes, "merge-risk", profile, instructions)
                .unwrap_err()
                .contains("set either merge_on_authorization or merge_decision, not both")
        );

        let unknown = SAMPLE.replace(
            "routing = \"default\"",
            "merge_decision = { profile = \"merge-risk\", max_risk_score = 20, threshold = 20 }\nrouting = \"default\"",
        );
        assert!(
            load_with_merge_profile(&unknown, "merge-risk", profile, instructions)
                .unwrap_err()
                .contains("unknown field `threshold`")
        );

        for (entry, missing) in [
            (
                "merge_decision = { profile = \"merge-risk\" }\nrouting = \"default\"",
                "max_risk_score",
            ),
            (
                "merge_decision = { max_risk_score = 20 }\nrouting = \"default\"",
                "profile",
            ),
        ] {
            let text = SAMPLE.replacen("routing = \"default\"", entry, 1);
            assert!(
                load_with_merge_profile(&text, "merge-risk", profile, instructions)
                    .unwrap_err()
                    .contains(&format!("missing field `{missing}`"))
            );
        }

        let negative = SAMPLE.replace(
            "routing = \"default\"",
            "merge_decision = { profile = \"merge-risk\", max_risk_score = -1 }\nrouting = \"default\"",
        );
        assert!(load_with_merge_profile(&negative, "merge-risk", profile, instructions).is_err());
    }

    #[test]
    fn merge_decision_requires_an_existing_registered_profile_with_resolved_instructions() {
        let missing_profile = merge_decision(SAMPLE, "missing", 20);
        assert!(
            load_with_merge_profile(
                &missing_profile,
                "merge-risk",
                "kind = \"claude\"\n",
                Some("Review the change.\n"),
            )
            .unwrap_err()
            .contains("no profile named `missing`")
        );

        let unsupported = merge_decision(SAMPLE, "merge-risk", 20);
        assert!(
            load_with_merge_profile(
                &unsupported,
                "merge-risk",
                "kind = \"gemini\"\n",
                Some("Review the change.\n"),
            )
            .unwrap_err()
            .contains("the `gemini` kind cannot be a merge decision agent")
        );

        for instructions in [None, Some("  \n\t")] {
            assert!(
                load_with_merge_profile(
                    &unsupported,
                    "merge-risk",
                    "kind = \"claude\"\n",
                    instructions,
                )
                .unwrap_err()
                .contains("profile `merge-risk` has no instructions.md")
            );
        }
    }

    #[test]
    fn decision_profile_env_and_timeout_keep_headless_only_restrictions() {
        let shared = merge_decision(
            &SAMPLE.replace(
                "workers = [\"standard\", \"deep\"]",
                "workers = [\"standard\", \"deep\", \"merge-risk\"]",
            ),
            "merge-risk",
            20,
        );
        let instructions = Some("Review the complete change.\n");
        let env_error = load_with_merge_profile(
            &shared,
            "merge-risk",
            "kind = \"claude\"\nenv = { X = \"1\" }\n",
            instructions,
        )
        .unwrap_err();
        assert!(
            env_error.contains("env applies only to routing, postmortem, or merge decision agents")
        );

        let timeout_error = load_with_merge_profile(
            &shared,
            "merge-risk",
            "kind = \"claude\"\ntimeout_seconds = 30\n",
            instructions,
        )
        .unwrap_err();
        assert!(timeout_error.contains(
            "timeout_seconds applies only to routing, postmortem, or merge decision agents"
        ));

        for key in ["HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR"] {
            let error = load_with_merge_profile(
                &merge_decision(SAMPLE, "merge-risk", 20),
                "merge-risk",
                &format!("kind = \"claude\"\nenv = {{ {key} = \"/private\" }}\n"),
                instructions,
            )
            .unwrap_err();
            assert!(error.contains(&format!("profile `merge-risk` may not set {key}")));
        }
    }

    #[test]
    fn workspaces_and_their_teams_have_their_own_settings() {
        let text = SAMPLE.to_string()
            + "\n[workspaces.beta]\nclient_id = \"client-456\"\ncallback_port = 43872\n\n[workspaces.beta.teams.DATA]\nallowed_user_ids = [\"user-2\"]\nreview_state = \"Review\"\nrouting = \"default\"\n\n[workspaces.beta.teams.OPS]\nrouting = \"default\"\n";
        let config = load_with(&text, &[]).unwrap();
        assert_eq!(
            config.workspaces.keys().collect::<Vec<_>>(),
            ["acme", "beta"]
        );
        assert_eq!(config.workspace("beta").unwrap().callback_port, 43872);
        let beta = config.team("beta", "DATA").unwrap();
        assert_eq!(
            (beta.allowed_user_ids.as_slice(), beta.review_state.as_str()),
            (&["user-2".to_string()][..], "Review")
        );
        assert_eq!(
            config.team("acme", "DATA").unwrap().allowed_user_ids,
            ["user-1"]
        );
        let ops = config.team("beta", "OPS").unwrap();
        assert!(ops.allowed_user_ids.is_empty());
        assert_eq!(ops.review_state, "In Review");

        let no_teams =
            SAMPLE.to_string() + "\n[workspaces.beta]\nclient_id = \"client-456\"\nteams = {}\n";
        assert!(
            load_with(&no_teams, &[])
                .unwrap_err()
                .ends_with("workspaces.beta.teams lists no team")
        );
        let none = SAMPLE
            .replace(
                "[workspaces.acme.teams.DATA]\nallowed_user_ids = [\"user-1\"]\nreview_state = \"In Review\"\nstarted_state = \"In Progress\"\ndeploy_state = \"In Deploy\"\npostmortem_state = \"In Postmortem\"\ndone_state = \"Done\"\n# merge_decision = { profile = \"merge-risk\", max_risk_score = 20 }\nrouting = \"default\"\n",
                "",
            )
            .replace("[workspaces.acme]\nclient_id = \"client-123\"\n", "");
        assert!(
            load_with(&none, &[])
                .unwrap_err()
                .contains("missing field `workspaces`")
        );
    }

    #[test]
    fn a_profiles_timeout_is_for_headless_agents_only() {
        let config = load_with(
            SAMPLE,
            &[("router", "kind = \"claude\"\ntimeout_seconds = 30\n")],
        )
        .unwrap();
        assert_eq!(config.profile("router").unwrap().timeout_seconds, Some(30));
        assert_eq!(config.profile("standard").unwrap().timeout_seconds, None);
        let worker = "kind = \"claude\"\ntimeout_seconds = 30\n";
        let error = load_with(SAMPLE, &[("standard", worker)]).unwrap_err();
        assert!(
            error.ends_with("profile `standard`: timeout_seconds applies only to routing, postmortem, or merge decision agents, but `standard` is also a coordinator or worker profile"),
            "{error}"
        );
        let error = load_with(
            SAMPLE,
            &[("router", "kind = \"claude\"\ntimeout_seconds = 0\n")],
        )
        .unwrap_err();
        assert!(
            error.ends_with("profile `router`: timeout_seconds must be between 1 and 3600"),
            "{error}"
        );
    }

    #[test]
    fn only_headless_agent_profiles_may_set_env() {
        let with_env = |profile: &str| {
            let (_, config, _) = SAMPLE_PROFILES
                .iter()
                .find(|(n, _, _)| *n == profile)
                .unwrap();
            load_with(
                SAMPLE,
                &[(
                    profile,
                    &format!("{config}env = {{ OPENCODE_CONFIG_DIR = \"/tmp/empty\" }}\n"),
                )],
            )
        };
        let config = with_env("router").unwrap();
        assert_eq!(
            config
                .profile("router")
                .unwrap()
                .env
                .get("OPENCODE_CONFIG_DIR")
                .map(String::as_str),
            Some("/tmp/empty")
        );
        assert!(with_env("standard").unwrap_err().ends_with(
            "profile `standard`: env applies only to routing, postmortem, or merge decision agents, but `standard` is also a coordinator or worker profile"
        ));
    }

    #[test]
    fn a_routing_agent_must_be_a_registered_kind() {
        assert!(
            load_with(SAMPLE, &[("router", "kind = \"gemini\"\n")])
                .unwrap_err()
                .ends_with("routing.default.agent: the `gemini` kind cannot be a routing agent")
        );
    }

    #[test]
    fn supervisor_and_generated_profile_bases_are_opt_in_and_validated() {
        assert!(sample().supervisor.is_none());
        let text = format!(
            "{}\n[supervisor]\nprofile = \"supervisor\"\n\n[routing.default.generated]\ncoordinators = [\"coordinator\"]\nworkers = [\"standard\"]\n",
            SAMPLE
        );
        let config = load_with(&text, &[("supervisor", "kind = \"claude\"\n")]).unwrap();
        assert_eq!(
            config
                .supervisor
                .unwrap()
                .max_generated_profiles_per_routing,
            5
        );
        assert_eq!(config.routing["default"].generated.workers, ["standard"]);

        let bad_base = text.replace("workers = [\"standard\"]", "workers = [\"coordinator\"]");
        assert!(load_with(&bad_base, &[("supervisor", "kind = \"claude\"\n")])
            .unwrap_err()
            .ends_with("routing.default.generated.workers: `coordinator` is not an approved manual profile for this role"));

        let without_supervisor =
            SAMPLE.to_string() + "\n[routing.default.generated]\nworkers = [\"standard\"]\n";
        assert!(
            load_with(&without_supervisor, &[])
                .unwrap_err()
                .ends_with("routing.default.generated requires [supervisor]")
        );

        let too_few = text.replace("[supervisor]", "[limits]\nmax_agents = 2\n\n[supervisor]");
        assert!(
            load_with(&too_few, &[("supervisor", "kind = \"claude\"\n")])
                .unwrap_err()
                .ends_with("limits.max_agents must be at least 3 when the supervisor is enabled")
        );
    }
}
