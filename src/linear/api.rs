//! The Linear operations herdr-linear-agent uses. Every query and mutation is
//! a fixed string in this file; nothing from the config or an agent becomes
//! GraphQL text. Every read also selects the viewer, so the transport can
//! check that the credential still belongs to the app user.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ApiError;
use super::transport::Transport;

/// Pages read per poll at most: 50 issues each.
pub(crate) const MAX_PAGES: usize = 4;

const VIEWER_QUERY: &str = r#"query HlaViewer {
  viewer { id app isMe name }
}"#;

const DELEGATED_QUERY: &str = r#"query HlaDelegatedIssues($teamKeys: [String!]!, $after: String) {
  viewer { id app isMe }
  issues(
    first: 50
    after: $after
    filter: {
      delegate: { isMe: { eq: true } }
      team: { key: { in: $teamKeys } }
      state: { type: { nin: ["completed", "canceled"] } }
    }
  ) {
    nodes { id identifier title url updatedAt state { type } team { key } }
    pageInfo { hasNextPage endCursor }
  }
}"#;

const ISSUE_QUERY: &str = r#"query HlaIssue($id: String!) {
  viewer { id app isMe }
  issue(id: $id) {
    id identifier title url description updatedAt estimate
    state { id name type }
    delegate { id }
    team {
      id key name issueEstimationType
      states { nodes { id name type position } }
    }
    labels { nodes { name parent { name } } }
    comments(first: 50) { nodes { body createdAt user { name } } }
  }
}"#;

/// The app's recent sessions. Linear creates one when an issue is delegated
/// to the app (with the agent session webhook category enabled).
const SESSIONS_QUERY: &str = r#"query HlaSessions {
  viewer { id app isMe }
  agentSessions(first: 50) { nodes { id status createdAt issue { id } appUser { id } } }
}"#;

const SESSION_CREATE: &str = r#"mutation HlaSessionCreate($issueId: String!) {
  agentSessionCreateOnIssue(input: { issueId: $issueId }) { success agentSession { id } }
}"#;

const ACTIVITY_CREATE: &str = r#"mutation HlaActivityCreate($input: AgentActivityCreateInput!) {
  agentActivityCreate(input: $input) { success agentActivity { id } }
}"#;

const ACTIVITY_FIND: &str = r#"query HlaActivityFind($sessionId: String!, $id: ID!) {
  viewer { id app isMe }
  agentSession(id: $sessionId) { activities(filter: { id: { eq: $id } }) { nodes { id } } }
}"#;

const SESSION_UPDATE: &str = r#"mutation HlaSessionUpdate($id: String!, $input: AgentSessionUpdateInput!) {
  agentSessionUpdate(id: $id, input: $input) { success }
}"#;

const ISSUE_STATE_UPDATE: &str = r#"mutation HlaIssueState($id: String!, $stateId: String!) {
  issueUpdate(id: $id, input: { stateId: $stateId }) { success }
}"#;

/// One run's part of `HlaRuns`; `@` is replaced by the run's index. The text
/// is fixed: only the alias number varies.
const RUN_PART: &str = r#"  i@: issue(id: $i@) { updatedAt state { type name } delegate { id } }
  s@: agentSession(id: $s@) {
    activities(first: 50, filter: { type: { eq: "prompt" }, createdAt: { gt: $c@ } }) {
      nodes { id createdAt signal user { id } content { ... on AgentActivityPromptContent { body } } }
    }
  }
"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Viewer {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// One delegated issue as the poll sees it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRef {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub updated_at: String,
    #[serde(deserialize_with = "state_type")]
    pub state: String,
    #[serde(deserialize_with = "team_key")]
    pub team: String,
}

fn state_type<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct State {
        r#type: String,
    }
    Ok(State::deserialize(deserializer)?.r#type)
}

fn team_key<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct Team {
        key: String,
    }
    Ok(Team::deserialize(deserializer)?.key)
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    pub r#type: String,
    #[serde(default)]
    pub position: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
    /// `notUsed`, `exponential`, `fibonacci`, `linear` or `tShirt`.
    pub estimation_type: String,
    pub states: Vec<WorkflowState>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Label {
    pub name: String,
    /// The label group's name, for a label inside a group.
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Comment {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// Everything the run folder's `issue.md` and the routing rules need.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct IssueDetail {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub description: String,
    pub updated_at: String,
    pub estimate: Option<f64>,
    pub state: WorkflowState,
    pub delegate_id: Option<String>,
    pub team: Team,
    pub labels: Vec<Label>,
    pub comments: Vec<Comment>,
}

/// The content of an activity the plugin sends. The JSON shape follows
/// Linear's activity content payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Thought {
        body: String,
    },
    Action {
        action: String,
        parameter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    Elicitation {
        body: String,
    },
    Response {
        body: String,
    },
    Error {
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub content: Content,
    #[serde(default)]
    pub ephemeral: bool,
    /// `select` for an elicitation with options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_metadata: Option<Value>,
}

impl Activity {
    pub fn new(content: Content) -> Self {
        Activity {
            content,
            ephemeral: false,
            signal: None,
            signal_metadata: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalUrl {
    pub label: String,
    pub url: String,
}

/// What one tick reads for one active run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunQuery {
    pub issue_id: String,
    pub session_id: String,
    /// Prompts created after this RFC 3339 timestamp are returned.
    pub cursor: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueStatus {
    pub updated_at: String,
    pub state_type: String,
    pub state_name: String,
    pub delegate_id: Option<String>,
}

/// A person's message in an Agent Session.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub id: String,
    pub created_at: String,
    pub signal: Option<String>,
    pub user_id: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunUpdate {
    pub issue: IssueStatus,
    /// Oldest first.
    pub prompts: Vec<Prompt>,
}

/// One GraphQL operation: its fixed text and its variables. The sync
/// [`Linear`] and the async [`super::client::LinearApi`] both send these, so
/// the text and the parsing of every operation live only here.
pub(crate) struct Call {
    pub operation: &'static str,
    pub query: std::borrow::Cow<'static, str>,
    pub variables: Value,
    /// For a mutation, the payload field whose `success` must be true.
    pub payload: Option<&'static str>,
}

impl Call {
    fn read(operation: &'static str, query: &'static str, variables: Value) -> Call {
        Call {
            operation,
            query: query.into(),
            variables,
            payload: None,
        }
    }

    fn write(
        operation: &'static str,
        query: &'static str,
        variables: Value,
        payload: &'static str,
    ) -> Call {
        Call {
            operation,
            query: query.into(),
            variables,
            payload: Some(payload),
        }
    }

    pub fn is_write(&self) -> bool {
        self.payload.is_some()
    }

    pub fn viewer() -> Call {
        Call::read("HlaViewer", VIEWER_QUERY, json!({}))
    }

    pub fn delegated_issues(team_keys: &[String], after: Option<&str>) -> Call {
        Call::read(
            "HlaDelegatedIssues",
            DELEGATED_QUERY,
            json!({ "teamKeys": team_keys, "after": after }),
        )
    }

    pub fn issue(id: &str) -> Call {
        Call::read("HlaIssue", ISSUE_QUERY, json!({ "id": id }))
    }

    pub fn sessions() -> Call {
        Call::read("HlaSessions", SESSIONS_QUERY, json!({}))
    }

    pub fn session_create(issue_id: &str) -> Call {
        Call::write(
            "HlaSessionCreate",
            SESSION_CREATE,
            json!({ "issueId": issue_id }),
            "agentSessionCreateOnIssue",
        )
    }

    pub fn activity_create(
        session_id: &str,
        id: &str,
        activity: &Activity,
    ) -> Result<Call, ApiError> {
        let mut input = json!({
            "agentSessionId": session_id,
            "id": id,
            "content": serde_json::to_value(&activity.content).map_err(|_| ApiError::Configuration)?,
            "ephemeral": activity.ephemeral,
        });
        if let Some(signal) = &activity.signal {
            input["signal"] = json!(signal);
        }
        if let Some(metadata) = &activity.signal_metadata {
            input["signalMetadata"] = metadata.clone();
        }
        Ok(Call::write(
            "HlaActivityCreate",
            ACTIVITY_CREATE,
            json!({ "input": input }),
            "agentActivityCreate",
        ))
    }

    pub fn activity_find(session_id: &str, id: &str) -> Call {
        Call::read(
            "HlaActivityFind",
            ACTIVITY_FIND,
            json!({ "sessionId": session_id, "id": id }),
        )
    }

    pub fn set_plan(session_id: &str, plan: &Value) -> Call {
        Call::write(
            "HlaSessionUpdate",
            SESSION_UPDATE,
            json!({ "id": session_id, "input": { "plan": plan } }),
            "agentSessionUpdate",
        )
    }

    pub fn set_external_urls(session_id: &str, urls: &[ExternalUrl]) -> Call {
        Call::write(
            "HlaSessionUpdate",
            SESSION_UPDATE,
            json!({ "id": session_id, "input": { "externalUrls": urls } }),
            "agentSessionUpdate",
        )
    }

    pub fn set_issue_state(issue_id: &str, state_id: &str) -> Call {
        Call::write(
            "HlaIssueState",
            ISSUE_STATE_UPDATE,
            json!({ "id": issue_id, "stateId": state_id }),
            "issueUpdate",
        )
    }

    /// `HlaRuns` for a non-empty list of runs.
    pub fn run_updates(runs: &[RunQuery]) -> Call {
        let mut declarations = Vec::new();
        let mut body = String::from("  viewer { id app isMe }\n");
        let mut variables = serde_json::Map::new();
        for (n, run) in runs.iter().enumerate() {
            declarations.push(format!(
                "$i{n}: String!, $s{n}: String!, $c{n}: DateTimeOrDuration!"
            ));
            body.push_str(&RUN_PART.replace('@', &n.to_string()));
            variables.insert(format!("i{n}"), json!(run.issue_id));
            variables.insert(format!("s{n}"), json!(run.session_id));
            variables.insert(format!("c{n}"), json!(run.cursor));
        }
        let query = format!("query HlaRuns({}) {{\n{body}}}", declarations.join(", "));
        Call {
            operation: "HlaRuns",
            query: query.into(),
            variables: Value::Object(variables),
            payload: None,
        }
    }

    /// A read's data, or a write's payload once its `success` is true.
    pub fn finish(&self, data: Value) -> Result<Value, ApiError> {
        let Some(payload) = self.payload else {
            return Ok(data);
        };
        let result = field(&data, payload)?;
        if result["success"].as_bool() != Some(true) {
            return Err(ApiError::Graphql(format!("{payload} did not succeed")));
        }
        Ok(result.clone())
    }
}

pub(crate) fn parse_viewer(data: &Value) -> Result<Viewer, ApiError> {
    serde_json::from_value(field(data, "viewer")?.clone()).map_err(|_| ApiError::ReadFieldsInvalid)
}

/// One page of the delegated poll, and the cursor of the next page when
/// there is one.
pub(crate) fn parse_delegated_page(
    data: &Value,
) -> Result<(Vec<IssueRef>, Option<String>), ApiError> {
    let page = field(data, "issues")?;
    let mut issues = Vec::new();
    for node in nodes(page) {
        issues.push(serde_json::from_value(node.clone()).map_err(|_| ApiError::ReadFieldsInvalid)?);
    }
    let info = &page["pageInfo"];
    if info["hasNextPage"].as_bool() != Some(true) {
        return Ok((issues, None));
    }
    Ok((issues, Some(text(info, "endCursor")?)))
}

pub(crate) fn parse_issue_data(data: &Value) -> Result<IssueDetail, ApiError> {
    parse_issue(field(data, "issue")?)
}

/// The issue's newest open session of this app: the one Linear created on
/// delegation, or one the plugin created earlier.
pub(crate) fn newest_open_session(
    data: &Value,
    issue_id: &str,
) -> Result<Option<String>, ApiError> {
    let viewer = text(field(data, "viewer")?, "id")?;
    nodes(field(data, "agentSessions")?)
        .filter(|s| s["issue"]["id"] == issue_id && s["appUser"]["id"] == viewer.as_str())
        .filter(|s| s["status"] != "complete")
        .max_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()))
        .map(|session| text(session, "id"))
        .transpose()
}

/// The ID of the session a `session_create` payload names.
pub(crate) fn created_session(payload: &Value) -> Result<String, ApiError> {
    text(field(payload, "agentSession")?, "id")
}

pub(crate) fn activity_found(data: &Value, id: &str) -> Result<bool, ApiError> {
    Ok(nodes(&field(data, "agentSession")?["activities"]).any(|n| n["id"] == id))
}

/// The updates of `count` runs from an `HlaRuns` answer, in query order.
pub(crate) fn parse_run_updates(data: &Value, count: usize) -> Result<Vec<RunUpdate>, ApiError> {
    (0..count)
        .map(|n| {
            let issue = field(data, &format!("i{n}"))?;
            let session = field(data, &format!("s{n}"))?;
            let mut prompts = nodes(&session["activities"])
                .map(|a| {
                    Ok(Prompt {
                        id: text(a, "id")?,
                        created_at: text(a, "createdAt")?,
                        signal: a["signal"].as_str().map(str::to_string),
                        user_id: a["user"]["id"].as_str().unwrap_or("").to_string(),
                        body: a["content"]["body"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            prompts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            Ok(RunUpdate {
                issue: IssueStatus {
                    updated_at: text(issue, "updatedAt")?,
                    state_type: text(field(issue, "state")?, "type")?,
                    state_name: text(field(issue, "state")?, "name")?,
                    delegate_id: issue["delegate"]["id"].as_str().map(str::to_string),
                },
                prompts,
            })
        })
        .collect()
}

/// The typed Linear API over a transport.
pub struct Linear<T: Transport> {
    transport: T,
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, ApiError> {
    value
        .get(name)
        .filter(|v| !v.is_null())
        .ok_or(ApiError::ReadFieldsInvalid)
}

fn text(value: &Value, name: &str) -> Result<String, ApiError> {
    field(value, name)?
        .as_str()
        .map(str::to_string)
        .ok_or(ApiError::ReadFieldsInvalid)
}

fn nodes(value: &Value) -> impl Iterator<Item = &Value> {
    value["nodes"].as_array().into_iter().flatten()
}

impl<T: Transport> Linear<T> {
    pub fn new(transport: T) -> Self {
        Linear { transport }
    }

    #[cfg(test)]
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    fn call(&mut self, mut call: Call) -> Result<Value, ApiError> {
        let variables = std::mem::take(&mut call.variables);
        let data =
            self.transport
                .execute(call.operation, &call.query, variables, call.is_write())?;
        call.finish(data)
    }

    pub fn viewer(&mut self) -> Result<Viewer, ApiError> {
        parse_viewer(&self.call(Call::viewer())?)
    }

    /// Issues delegated to the app user in the configured teams whose state is
    /// neither completed nor canceled.
    pub fn delegated_issues(&mut self, team_keys: &[String]) -> Result<Vec<IssueRef>, ApiError> {
        let mut issues = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let data = self.call(Call::delegated_issues(team_keys, after.as_deref()))?;
            let (page, next) = parse_delegated_page(&data)?;
            issues.extend(page);
            match next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        Ok(issues)
    }

    pub fn issue(&mut self, id: &str) -> Result<IssueDetail, ApiError> {
        parse_issue_data(&self.call(Call::issue(id))?)
    }

    /// Creates an Agent Session on the issue and returns its ID.
    /// The issue's newest open session of this app: the one Linear created on
    /// delegation, or one the plugin created earlier. Otherwise a new one.
    pub fn open_session(&mut self, issue_id: &str) -> Result<String, ApiError> {
        let data = self.call(Call::sessions())?;
        match newest_open_session(&data, issue_id)? {
            Some(session) => Ok(session),
            None => self.create_session(issue_id),
        }
    }

    fn create_session(&mut self, issue_id: &str) -> Result<String, ApiError> {
        created_session(&self.call(Call::session_create(issue_id))?)
    }

    /// Sends an activity with the caller's UUID as its ID, so a lost response
    /// can be checked with `activity_exists` before anything is sent again.
    pub fn create_activity(
        &mut self,
        session_id: &str,
        id: &str,
        activity: &Activity,
    ) -> Result<(), ApiError> {
        self.call(Call::activity_create(session_id, id, activity)?)
            .map(|_| ())
    }

    pub fn activity_exists(&mut self, session_id: &str, id: &str) -> Result<bool, ApiError> {
        activity_found(&self.call(Call::activity_find(session_id, id))?, id)
    }

    /// Replaces the session's plan (Linear's list of `{content, status}`).
    pub fn set_plan(&mut self, session_id: &str, plan: &Value) -> Result<(), ApiError> {
        self.call(Call::set_plan(session_id, plan)).map(|_| ())
    }

    /// Replaces the session's external URLs with the full list.
    pub fn set_external_urls(
        &mut self,
        session_id: &str,
        urls: &[ExternalUrl],
    ) -> Result<(), ApiError> {
        self.call(Call::set_external_urls(session_id, urls))
            .map(|_| ())
    }

    pub fn set_issue_state(&mut self, issue_id: &str, state_id: &str) -> Result<(), ApiError> {
        self.call(Call::set_issue_state(issue_id, state_id))
            .map(|_| ())
    }

    /// Every active run's issue state and new prompts, in one request.
    pub fn run_updates(&mut self, runs: &[RunQuery]) -> Result<Vec<RunUpdate>, ApiError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let data = self.call(Call::run_updates(runs))?;
        parse_run_updates(&data, runs.len())
    }
}

fn parse_state(value: &Value) -> Result<WorkflowState, ApiError> {
    Ok(WorkflowState {
        id: text(value, "id")?,
        name: text(value, "name")?,
        r#type: text(value, "type")?,
        position: value["position"].as_f64().unwrap_or(0.0),
    })
}

fn parse_issue(issue: &Value) -> Result<IssueDetail, ApiError> {
    let team = field(issue, "team")?;
    let states = nodes(field(team, "states")?)
        .map(parse_state)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IssueDetail {
        id: text(issue, "id")?,
        identifier: text(issue, "identifier")?,
        title: text(issue, "title")?,
        url: text(issue, "url")?,
        description: issue["description"].as_str().unwrap_or("").to_string(),
        updated_at: text(issue, "updatedAt")?,
        estimate: issue["estimate"].as_f64(),
        state: parse_state(field(issue, "state")?)?,
        delegate_id: issue["delegate"]["id"].as_str().map(str::to_string),
        team: Team {
            id: text(team, "id")?,
            key: text(team, "key")?,
            name: text(team, "name")?,
            estimation_type: team["issueEstimationType"]
                .as_str()
                .unwrap_or("notUsed")
                .to_string(),
            states,
        },
        labels: nodes(&issue["labels"])
            .map(|l| {
                Ok(Label {
                    name: text(l, "name")?,
                    group: l["parent"]["name"].as_str().map(str::to_string),
                })
            })
            .collect::<Result<_, ApiError>>()?,
        comments: nodes(&issue["comments"])
            .map(|c| {
                Ok(Comment {
                    author: c["user"]["name"]
                        .as_str()
                        .unwrap_or("(unknown)")
                        .to_string(),
                    created_at: text(c, "createdAt")?,
                    body: c["body"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect::<Result<_, ApiError>>()?,
    })
}

#[cfg(test)]
pub mod fake {
    //! An in-memory Linear behind the `Transport` boundary. It answers the
    //! operations this module sends by name, keeps the issues, sessions and
    //! activities a test sets up, and records every call.

    use super::*;

    pub const APP_USER: &str = "app-user-1";

    #[derive(Debug, Clone, Default)]
    pub struct FakeSession {
        pub id: String,
        pub issue_id: String,
        /// `pending` when created; tests set other states.
        pub status: String,
        pub created_at: String,
        /// Activity records in the shape the API returns, oldest first.
        pub activities: Vec<Value>,
        pub plan: Option<Value>,
        pub external_urls: Vec<ExternalUrl>,
    }

    impl FakeSession {
        /// The type of every activity the plugin sent, oldest first.
        pub fn sent_types(&self) -> Vec<String> {
            self.activities
                .iter()
                .filter(|a| a["type"] != "prompt")
                .map(|a| a["type"].as_str().unwrap_or("").to_string())
                .collect()
        }

        /// Activities of one type the plugin sent.
        pub fn sent(&self, kind: &str) -> Vec<Value> {
            self.activities
                .iter()
                .filter(|a| a["type"] == kind)
                .cloned()
                .collect()
        }
    }

    #[derive(Default)]
    pub struct FakeLinear {
        /// Issue records in the shape `HlaIssue` returns.
        pub issues: Vec<Value>,
        pub sessions: Vec<FakeSession>,
        /// (operation name, variables, write) of every call.
        pub calls: Vec<(String, Value, bool)>,
        /// The next call fails with this error without taking effect, once.
        pub fail_next: Option<ApiError>,
        /// The next write takes effect but its response is lost, once.
        pub lose_next_response: bool,
        /// Sessions fail as for an app without the agent session webhook category.
        pub sessions_disabled: bool,
        clock: i64,
    }

    fn viewer() -> Value {
        json!({ "id": APP_USER, "app": true, "isMe": true, "name": "Agent" })
    }

    impl FakeLinear {
        /// A timestamp after the present and after every earlier one, so
        /// activities sort after a cursor the plugin took from the clock.
        fn tick_clock(&mut self) -> String {
            self.clock += 1;
            (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(self.clock)).to_string()
        }

        /// Adds an issue delegated to the app user in `team`.
        pub fn add_issue(&mut self, identifier: &str, team: &str, title: &str) -> String {
            let id = format!("00000000-0000-4000-8000-{:012}", self.issues.len() + 1);
            self.issues.push(json!({
                "id": id,
                "identifier": identifier,
                "title": title,
                "url": format!("https://linear.app/acme/issue/{identifier}/x"),
                "description": format!("Description of {identifier}"),
                "updatedAt": "2026-09-25T00:00:00.000Z",
                "estimate": null,
                "state": { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1.0 },
                "delegate": { "id": APP_USER },
                "team": {
                    "id": format!("team-{team}"), "key": team, "name": team, "issueEstimationType": "fibonacci",
                    "states": { "nodes": [
                        { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1.0 },
                        { "id": "state-review", "name": "In Review", "type": "started", "position": 3.0 },
                        { "id": "state-progress", "name": "In Progress", "type": "started", "position": 2.0 },
                        { "id": "state-done", "name": "Done", "type": "completed", "position": 4.0 },
                        { "id": "state-canceled", "name": "Canceled", "type": "canceled", "position": 5.0 }
                    ] }
                },
                "labels": { "nodes": [] },
                "comments": { "nodes": [] }
            }));
            id
        }

        pub fn issue_mut(&mut self, identifier: &str) -> &mut Value {
            self.issues
                .iter_mut()
                .find(|i| i["identifier"] == identifier || i["id"] == identifier)
                .expect("fake issue")
        }

        pub fn issue(&self, identifier: &str) -> &Value {
            self.issues
                .iter()
                .find(|i| i["identifier"] == identifier || i["id"] == identifier)
                .expect("fake issue")
        }

        /// Moves an issue to the team state named `name`.
        pub fn set_state(&mut self, identifier: &str, name: &str) {
            let issue = self.issue_mut(identifier);
            let state = issue["team"]["states"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == name)
                .cloned()
                .expect("state");
            issue["state"] = state;
            issue["updatedAt"] = json!("2026-09-26T00:00:00.000Z");
        }

        /// The session created on an issue.
        pub fn session(&self, identifier: &str) -> &FakeSession {
            let id = self.issue(identifier)["id"].clone();
            self.sessions
                .iter()
                .find(|s| s.issue_id == id)
                .expect("fake session")
        }

        fn new_session(&mut self, issue_id: String) -> String {
            let id = format!("session-{}", self.sessions.len() + 1);
            let created_at = self.tick_clock();
            self.sessions.push(FakeSession {
                id: id.clone(),
                issue_id,
                status: "pending".into(),
                created_at,
                ..FakeSession::default()
            });
            id
        }

        /// The session Linear creates by itself when the issue is delegated.
        pub fn delegate_session(&mut self, identifier: &str) -> String {
            let issue_id = self.issue(identifier)["id"].as_str().unwrap().to_string();
            self.new_session(issue_id)
        }

        /// A person's message in the issue's session.
        pub fn add_prompt(
            &mut self,
            identifier: &str,
            user_id: &str,
            body: &str,
            signal: Option<&str>,
        ) {
            let created = self.tick_clock();
            let id = self.issue(identifier)["id"].clone();
            let n = self
                .sessions
                .iter()
                .map(|s| s.activities.len())
                .sum::<usize>();
            let session = self
                .sessions
                .iter_mut()
                .find(|s| s.issue_id == id)
                .expect("fake session");
            session.activities.push(json!({
                "id": format!("prompt-{n}"), "type": "prompt", "createdAt": created, "signal": signal,
                "user": { "id": user_id }, "content": { "type": "prompt", "body": body }
            }));
        }

        pub fn count(&self, operation: &str) -> usize {
            self.calls
                .iter()
                .filter(|(name, _, _)| name == operation)
                .count()
        }

        fn session_mut(&mut self, id: &str) -> Result<&mut FakeSession, ApiError> {
            self.sessions
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or(ApiError::Graphql("Entity not found: AgentSession".into()))
        }

        fn answer(&mut self, operation: &str, variables: &Value) -> Result<Value, ApiError> {
            match operation {
                "HlaViewer" => Ok(json!({ "viewer": viewer() })),
                "HlaDelegatedIssues" => {
                    let teams: Vec<&str> = variables["teamKeys"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let nodes: Vec<Value> = self
                        .issues
                        .iter()
                        .filter(|i| i["delegate"]["id"] == APP_USER)
                        .filter(|i| teams.contains(&i["team"]["key"].as_str().unwrap_or("")))
                        .filter(|i| !matches!(i["state"]["type"].as_str(), Some("completed" | "canceled")))
                        .map(|i| {
                            json!({
                                "id": i["id"], "identifier": i["identifier"], "title": i["title"], "url": i["url"],
                                "updatedAt": i["updatedAt"], "state": { "type": i["state"]["type"] }, "team": { "key": i["team"]["key"] }
                            })
                        })
                        .collect();
                    Ok(
                        json!({ "viewer": viewer(), "issues": { "nodes": nodes, "pageInfo": { "hasNextPage": false, "endCursor": null } } }),
                    )
                }
                "HlaIssue" => {
                    let id = variables["id"].as_str().unwrap_or("");
                    let issue = self
                        .issues
                        .iter()
                        .find(|i| i["id"] == id || i["identifier"] == id)
                        .cloned()
                        .ok_or(ApiError::Graphql("Entity not found".into()))?;
                    Ok(json!({ "viewer": viewer(), "issue": issue }))
                }
                "HlaSessions" | "HlaSessionCreate" if self.sessions_disabled => {
                    Err(ApiError::Graphql(
                        "Agent sessions are not enabled for this application.".into(),
                    ))
                }
                "HlaSessions" => {
                    let nodes: Vec<Value> = self
                        .sessions
                        .iter()
                        .map(|s| json!({ "id": s.id, "status": s.status, "createdAt": s.created_at, "issue": { "id": s.issue_id }, "appUser": { "id": APP_USER } }))
                        .collect();
                    Ok(json!({ "viewer": viewer(), "agentSessions": { "nodes": nodes } }))
                }
                "HlaSessionCreate" => {
                    let issue_id = self.issue(variables["issueId"].as_str().unwrap_or(""))["id"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let id = self.new_session(issue_id);
                    Ok(
                        json!({ "agentSessionCreateOnIssue": { "success": true, "agentSession": { "id": id } } }),
                    )
                }
                "HlaActivityCreate" => {
                    let input = &variables["input"];
                    let created = self.tick_clock();
                    let session =
                        self.session_mut(input["agentSessionId"].as_str().unwrap_or(""))?;
                    if session.activities.iter().any(|a| a["id"] == input["id"]) {
                        return Err(ApiError::Graphql("duplicate activity id".into()));
                    }
                    let mut activity = json!({
                        "id": input["id"], "type": input["content"]["type"], "content": input["content"], "createdAt": created,
                        "ephemeral": input["ephemeral"], "signal": input["signal"], "signalMetadata": input["signalMetadata"], "user": { "id": APP_USER }
                    });
                    if activity["signal"].is_null() {
                        activity["signal"] = Value::Null;
                    }
                    session.activities.push(activity);
                    Ok(
                        json!({ "agentActivityCreate": { "success": true, "agentActivity": { "id": input["id"] } } }),
                    )
                }
                "HlaActivityFind" => {
                    let id = variables["id"].clone();
                    let session =
                        self.session_mut(variables["sessionId"].as_str().unwrap_or(""))?;
                    let found: Vec<Value> = session
                        .activities
                        .iter()
                        .filter(|a| a["id"] == id)
                        .map(|a| json!({ "id": a["id"] }))
                        .collect();
                    Ok(
                        json!({ "viewer": viewer(), "agentSession": { "activities": { "nodes": found } } }),
                    )
                }
                "HlaSessionUpdate" => {
                    let input = variables["input"].clone();
                    let session = self.session_mut(variables["id"].as_str().unwrap_or(""))?;
                    if let Some(plan) = input.get("plan") {
                        session.plan = Some(plan.clone());
                    }
                    if let Some(urls) = input.get("externalUrls") {
                        session.external_urls = serde_json::from_value(urls.clone()).unwrap();
                    }
                    Ok(json!({ "agentSessionUpdate": { "success": true } }))
                }
                "HlaIssueState" => {
                    let state_id = variables["stateId"].clone();
                    let issue = self.issue_mut(variables["id"].as_str().unwrap_or(""));
                    let state = issue["team"]["states"]["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|s| s["id"] == state_id)
                        .cloned()
                        .ok_or(ApiError::Graphql("bad state".into()))?;
                    issue["state"] = state;
                    Ok(json!({ "issueUpdate": { "success": true } }))
                }
                "HlaRuns" => {
                    let mut data = json!({ "viewer": viewer() });
                    let mut n = 0;
                    while let Some(issue_id) =
                        variables.get(format!("i{n}")).and_then(Value::as_str)
                    {
                        let issue = self.issue(issue_id).clone();
                        data[format!("i{n}")] = json!({ "updatedAt": issue["updatedAt"], "state": { "type": issue["state"]["type"], "name": issue["state"]["name"] }, "delegate": issue["delegate"] });
                        let cursor: jiff::Timestamp = variables[format!("c{n}")]
                            .as_str()
                            .unwrap()
                            .parse()
                            .unwrap();
                        let session =
                            self.session_mut(variables[format!("s{n}")].as_str().unwrap())?;
                        let prompts: Vec<Value> = session
                            .activities
                            .iter()
                            .filter(|a| a["type"] == "prompt")
                            .filter(|a| {
                                a["createdAt"]
                                    .as_str()
                                    .unwrap()
                                    .parse::<jiff::Timestamp>()
                                    .unwrap()
                                    > cursor
                            })
                            .cloned()
                            .collect();
                        data[format!("s{n}")] = json!({ "activities": { "nodes": prompts } });
                        n += 1;
                    }
                    Ok(data)
                }
                other => panic!("FakeLinear: unexpected operation {other}"),
            }
        }
    }

    impl Transport for FakeLinear {
        fn execute(
            &mut self,
            operation: &str,
            _query: &str,
            variables: Value,
            write: bool,
        ) -> Result<Value, ApiError> {
            self.calls
                .push((operation.to_string(), variables.clone(), write));
            if let Some(error) = self.fail_next.take() {
                return Err(error);
            }
            let answer = self.answer(operation, &variables);
            if write && std::mem::take(&mut self.lose_next_response) {
                return Err(ApiError::RequestFailed);
            }
            answer
        }
    }

    /// A fake shared between a test and the ticker that owns the transport.
    #[derive(Clone, Default)]
    pub struct Shared(pub std::rc::Rc<std::cell::RefCell<FakeLinear>>);

    impl Transport for Shared {
        fn execute(
            &mut self,
            operation: &str,
            query: &str,
            variables: Value,
            write: bool,
        ) -> Result<Value, ApiError> {
            self.0
                .borrow_mut()
                .execute(operation, query, variables, write)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeLinear;
    use super::*;

    #[test]
    fn reads_parse_into_typed_records() {
        let mut fake = FakeLinear::default();
        let id = fake.add_issue("DATA-1", "DATA", "First");
        fake.add_issue("OTHER-1", "OTHER", "Elsewhere");
        fake.add_issue("DATA-2", "DATA", "Done already");
        fake.issue_mut("DATA-2")["state"]["type"] = json!("completed");
        fake.issue_mut("DATA-1")["labels"] = json!({ "nodes": [{ "name": "S", "parent": { "name": "size" } }, { "name": "bug", "parent": null }] });
        let mut linear = Linear::new(fake);

        assert_eq!(linear.viewer().unwrap().id, fake::APP_USER);
        let issues = linear.delegated_issues(&["DATA".into()]).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(
            (
                issues[0].identifier.as_str(),
                issues[0].team.as_str(),
                issues[0].state.as_str()
            ),
            ("DATA-1", "DATA", "unstarted")
        );

        let detail = linear.issue(&id).unwrap();
        assert_eq!(detail.team.states.len(), 5);
        assert_eq!(
            detail.labels[0],
            Label {
                name: "S".into(),
                group: Some("size".into())
            }
        );
        assert_eq!(detail.labels[1].group, None);
        assert_eq!(detail.delegate_id.as_deref(), Some(fake::APP_USER));
        assert!(linear.transport().calls.iter().all(|(_, _, write)| !write));
    }

    #[test]
    fn writes_and_run_updates_round_trip() {
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let mut linear = Linear::new(fake);
        let session = linear.open_session(&issue).unwrap();
        let thought = Activity::new(Content::Thought {
            body: "Picked up".into(),
        });
        linear.create_activity(&session, "a-1", &thought).unwrap();
        assert!(linear.activity_exists(&session, "a-1").unwrap());
        assert!(!linear.activity_exists(&session, "a-2").unwrap());
        let ask = Activity {
            signal: Some("select".into()),
            signal_metadata: Some(json!({ "options": [{ "label": "Yes", "value": "yes" }] })),
            ..Activity::new(Content::Elicitation {
                body: "Proceed?".into(),
            })
        };
        linear.create_activity(&session, "a-2", &ask).unwrap();
        linear
            .set_plan(
                &session,
                &json!([{ "content": "Plan", "status": "pending" }]),
            )
            .unwrap();
        linear
            .set_external_urls(
                &session,
                &[ExternalUrl {
                    label: "PR".into(),
                    url: "https://github.com/o/r/pull/1".into(),
                }],
            )
            .unwrap();
        linear.set_issue_state(&issue, "state-progress").unwrap();

        let fake = linear.transport();
        let stored = &fake.sessions[0];
        assert_eq!(stored.sent_types(), ["thought", "elicitation"]);
        assert_eq!(
            stored.sent("elicitation")[0]["signalMetadata"]["options"][0]["value"],
            "yes"
        );
        assert_eq!(stored.plan.as_ref().unwrap()[0]["status"], "pending");
        assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
        fake.add_prompt("DATA-1", "user-1", "first", None);
        fake.add_prompt("DATA-1", "user-2", "stop now", Some("stop"));

        let query = RunQuery {
            issue_id: issue.clone(),
            session_id: session.clone(),
            cursor: "2026-09-24T00:00:00Z".into(),
        };
        let updates = linear.run_updates(&[query.clone(), query]).unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].issue.state_type, "started");
        assert_eq!(updates[0].prompts.len(), 2);
        assert_eq!(updates[0].prompts[1].signal.as_deref(), Some("stop"));
        assert_eq!(updates[0].prompts[0].user_id, "user-1");
        let later = RunQuery {
            issue_id: issue,
            session_id: session,
            cursor: updates[0].prompts[1].created_at.clone(),
        };
        assert!(linear.run_updates(&[later]).unwrap()[0].prompts.is_empty());
        assert!(linear.run_updates(&[]).unwrap().is_empty());
    }

    #[test]
    fn a_missing_field_is_an_error() {
        assert_eq!(
            parse_issue(&json!({ "id": "x" })).unwrap_err(),
            ApiError::ReadFieldsInvalid
        );
    }
}
