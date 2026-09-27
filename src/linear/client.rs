//! The async Linear client the event-driven ticker reads and writes through.
//!
//! The rules are those of `transport.rs`: HTTPS only, no redirects, no proxy,
//! no retries, the same timeouts and response bound; reads are accepted only
//! when the viewer is this app, and writes need a credential already bound to
//! that viewer. The credential manager stays synchronous and is not `Send`, so
//! one dedicated thread owns it; each request is a job on that thread, which
//! drives the HTTP call on the runtime with `Handle::block_on` while the lease
//! is held.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use super::api::{
    self, Activity, Call, ExternalUrl, IssueDetail, IssueRef, RunQuery, RunUpdate, Viewer,
};
use super::credentials::{CredentialError, CredentialManager};
use super::transport::{
    CONNECT_TIMEOUT, GRAPHQL_ENDPOINT, MAX_RESPONSE_BYTES, REQUEST_TIMEOUT, decode,
    json_content_type, verified,
};
use super::{ApiError, VerifiedReadOutcome};

/// Header maps kept for a caller that has not taken them yet, newest last.
const KEPT_HEADERS: usize = 32;

/// The Linear operations the ticker needs. An implementation supplies
/// `execute`; every operation's text and parsing is shared with the sync
/// client in `api.rs`. Tests fake Linear by implementing `execute`.
pub trait LinearApi: Sync {
    /// Sends one GraphQL operation and returns its `data` object.
    fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    /// The response headers of every call since the last take, oldest first
    /// (the rate-limit headers are read from them).
    // The budget that reads them (PR 3) is not written yet.
    #[allow(dead_code)]
    fn take_headers(&self) -> Vec<HeaderMap> {
        Vec::new()
    }

    /// Sends a prepared call: a read's data, or a write's checked payload.
    fn call(&self, call: Call) -> impl Future<Output = Result<Value, ApiError>> + Send {
        async move {
            let mut call = call;
            let variables = std::mem::take(&mut call.variables);
            let data = self
                .execute(call.operation, &call.query, variables, call.is_write())
                .await?;
            call.finish(data)
        }
    }

    fn viewer(&self) -> impl Future<Output = Result<Viewer, ApiError>> + Send {
        async move { api::parse_viewer(&self.call(Call::viewer()).await?) }
    }

    /// Issues delegated to the app user in the teams whose state is neither
    /// completed nor canceled.
    fn delegated_issues(
        &self,
        team_keys: &[String],
    ) -> impl Future<Output = Result<Vec<IssueRef>, ApiError>> + Send {
        async move {
            let mut issues = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..api::MAX_PAGES {
                let data = self
                    .call(Call::delegated_issues(team_keys, after.as_deref()))
                    .await?;
                let (page, next) = api::parse_delegated_page(&data)?;
                issues.extend(page);
                match next {
                    Some(cursor) => after = Some(cursor),
                    None => break,
                }
            }
            Ok(issues)
        }
    }

    fn issue(&self, id: &str) -> impl Future<Output = Result<IssueDetail, ApiError>> + Send {
        async move { api::parse_issue_data(&self.call(Call::issue(id)).await?) }
    }

    /// The issue's newest open session of this app, usually the one Linear
    /// created when the issue was delegated.
    fn find_session(
        &self,
        issue_id: &str,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> + Send {
        async move { api::newest_open_session(&self.call(Call::sessions()).await?, issue_id) }
    }

    /// Opens a new Agent Session on the issue and returns its ID.
    fn create_session(
        &self,
        issue_id: &str,
    ) -> impl Future<Output = Result<String, ApiError>> + Send {
        async move { api::created_session(&self.call(Call::session_create(issue_id)).await?) }
    }

    /// Sends an activity with the caller's UUID as its ID.
    fn create_activity(
        &self,
        session_id: &str,
        id: &str,
        activity: &Activity,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            let call = Call::activity_create(session_id, id, activity)?;
            self.call(call).await.map(|_| ())
        }
    }

    fn activity_exists(
        &self,
        session_id: &str,
        id: &str,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send {
        async move { api::activity_found(&self.call(Call::activity_find(session_id, id)).await?, id) }
    }

    fn set_plan(
        &self,
        session_id: &str,
        plan: &Value,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            self.call(Call::set_plan(session_id, plan))
                .await
                .map(|_| ())
        }
    }

    fn set_external_urls(
        &self,
        session_id: &str,
        urls: &[ExternalUrl],
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            self.call(Call::set_external_urls(session_id, urls))
                .await
                .map(|_| ())
        }
    }

    fn set_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            self.call(Call::set_issue_state(issue_id, state_id))
                .await
                .map(|_| ())
        }
    }

    /// Every given run's issue state and new prompts, in one request.
    fn run_updates(
        &self,
        runs: &[RunQuery],
    ) -> impl Future<Output = Result<Vec<RunUpdate>, ApiError>> + Send {
        async move {
            if runs.is_empty() {
                return Ok(Vec::new());
            }
            let data = self.call(Call::run_updates(runs)).await?;
            api::parse_run_updates(&data, runs.len())
        }
    }

    /// The batched run read, one result per run in order. When the batch
    /// fails, each run is read with its own request so one broken run does
    /// not hide the others; a rate-limited batch is not split.
    fn read_runs(
        &self,
        runs: &[RunQuery],
    ) -> impl Future<Output = Vec<Result<RunUpdate, ApiError>>> + Send {
        async move {
            match self.run_updates(runs).await {
                Ok(updates) => updates.into_iter().map(Ok).collect(),
                Err(ApiError::RateLimited) => {
                    runs.iter().map(|_| Err(ApiError::RateLimited)).collect()
                }
                Err(_) => {
                    let mut updates = Vec::with_capacity(runs.len());
                    for run in runs {
                        updates.push(
                            self.run_updates(std::slice::from_ref(run))
                                .await
                                .map(|mut one| one.remove(0)),
                        );
                    }
                    updates
                }
            }
        }
    }
}

/// One answer from Linear: the response headers (empty when no response
/// arrived) and the `data` object or the error.
#[derive(Debug)]
pub struct Response {
    pub headers: HeaderMap,
    pub result: Result<Value, ApiError>,
}

/// A token lease with the rules of the credential manager: a read's token
/// is released only for a verified read, a write's only once the viewer is
/// bound to the credential.
pub(crate) trait Lease {
    fn read(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
    ) -> Result<Value, ApiError>;

    fn write(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
    ) -> Result<Value, ApiError>;
}

impl Lease for CredentialManager {
    fn read(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
    ) -> Result<Value, ApiError> {
        self.with_verified_read(callback)
    }

    fn write(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        self.with_bound_access_token(callback)
    }
}

type Job = Box<dyn FnOnce(&mut dyn Lease) + Send>;

/// The production client: HTTPS to Linear with the Keychain-held token.
pub struct Client {
    http: reqwest::Client,
    endpoint: &'static str,
    jobs: std::sync::mpsc::Sender<Job>,
    headers: Mutex<VecDeque<HeaderMap>>,
}

impl Client {
    /// Builds the client with the production credential manager.
    // Tests cannot reach the Keychain; they build the client with a test lease.
    #[cfg_attr(test, allow(dead_code))]
    pub async fn production(
        client_id: String,
        callback_port: u16,
        lock_path: PathBuf,
    ) -> Result<Client, ApiError> {
        Self::with_lease(
            move || CredentialManager::production(client_id, callback_port, lock_path),
            GRAPHQL_ENDPOINT,
        )
        .await
    }

    /// Starts the thread that owns the lease `make` builds; the credential
    /// manager is not `Send`, so it is built on that thread and never leaves.
    pub(crate) async fn with_lease<L: Lease + 'static>(
        make: impl FnOnce() -> Result<L, CredentialError> + Send + 'static,
        endpoint: &'static str,
    ) -> Result<Client, ApiError> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ApiError::ClientConfiguration)?;
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let (ready, built) = oneshot::channel();
        std::thread::Builder::new()
            .name("linear-credentials".into())
            .spawn(move || {
                let mut lease = match make() {
                    Ok(lease) => {
                        let _ = ready.send(Ok(()));
                        lease
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                // Ends when the client, and with it the sender, is dropped.
                while let Ok(job) = queue.recv() {
                    job(&mut lease);
                }
            })
            .map_err(|_| ApiError::ClientConfiguration)?;
        built
            .await
            .map_err(|_| ApiError::ClientConfiguration)?
            .map_err(ApiError::Credential)?;
        Ok(Client {
            http,
            endpoint,
            jobs,
            headers: Mutex::new(VecDeque::new()),
        })
    }

    /// Sends one operation under a credential lease and returns Linear's
    /// answer with its headers.
    pub async fn request(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Response {
        let body = match serde_json::to_vec(
            &json!({ "operationName": operation, "query": query, "variables": variables }),
        ) {
            Ok(body) => body,
            Err(_) => {
                return Response {
                    headers: HeaderMap::new(),
                    result: Err(ApiError::Configuration),
                };
            }
        };
        let http = self.http.clone();
        let endpoint = self.endpoint;
        let runtime = tokio::runtime::Handle::current();
        let (answer, answered) = oneshot::channel();
        let job: Job = Box::new(move |lease| {
            let mut headers = HeaderMap::new();
            let mut send = |token: &str| {
                let response = runtime.block_on(post(&http, endpoint, token, &body));
                headers = response.headers;
                response.result
            };
            let result = if write {
                lease.write(&mut |token| send(token))
            } else {
                lease.read(&mut |token| verified(send(token)?))
            };
            let _ = answer.send(Response { headers, result });
        });
        if self.jobs.send(job).is_err() {
            return Response {
                headers: HeaderMap::new(),
                result: Err(ApiError::ClientConfiguration),
            };
        }
        answered.await.unwrap_or(Response {
            headers: HeaderMap::new(),
            result: Err(ApiError::ClientConfiguration),
        })
    }

    fn keep(&self, headers: HeaderMap) {
        let mut kept = self.headers.lock().unwrap_or_else(|e| e.into_inner());
        if kept.len() == KEPT_HEADERS {
            kept.pop_front();
        }
        kept.push_back(headers);
    }
}

impl LinearApi for Client {
    async fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError> {
        let response = self.request(operation, query, variables, write).await;
        self.keep(response.headers);
        response.result
    }

    fn take_headers(&self) -> Vec<HeaderMap> {
        let mut kept = self.headers.lock().unwrap_or_else(|e| e.into_inner());
        kept.drain(..).collect()
    }
}

async fn post(http: &reqwest::Client, endpoint: &str, token: &str, body: &[u8]) -> Response {
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};

    let mut authorization = Zeroizing::new(Vec::with_capacity(7 + token.len()));
    authorization.extend_from_slice(b"Bearer ");
    authorization.extend_from_slice(token.as_bytes());
    let Ok(mut authorization) = HeaderValue::from_bytes(&authorization) else {
        return Response {
            headers: HeaderMap::new(),
            result: Err(ApiError::Configuration),
        };
    };
    authorization.set_sensitive(true);
    let sent = http
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, authorization)
        .body(body.to_vec())
        .send()
        .await;
    let mut response = match sent {
        Ok(response) => response,
        Err(_) => {
            return Response {
                headers: HeaderMap::new(),
                result: Err(ApiError::RequestFailed),
            };
        }
    };
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let json_content = json_content_type(
        headers
            .get_all(CONTENT_TYPE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    );
    let mut bytes = Vec::with_capacity(4096);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES => {
                return Response {
                    headers,
                    result: Err(ApiError::ResponseTooLarge),
                };
            }
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => {
                return Response {
                    headers,
                    result: Err(ApiError::RequestFailed),
                };
            }
        }
    }
    Response {
        headers,
        result: decode_response(status, json_content, &bytes),
    }
}

/// `transport::decode`, except that HTTP 429 and a GraphQL error whose
/// `extensions.code` is `RATELIMITED` (Linear answers those with HTTP 400)
/// are `RateLimited`, never a definitive refusal.
pub fn decode_response(status: u16, json_content: bool, body: &[u8]) -> Result<Value, ApiError> {
    if status == 429 {
        return Err(ApiError::RateLimited);
    }
    if json_content
        && let Ok(reply) = serde_json::from_slice::<Value>(body)
        && reply["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|error| error["extensions"]["code"] == "RATELIMITED")
    {
        return Err(ApiError::RateLimited);
    }
    decode(status, json_content, body)
}

#[cfg(test)]
impl LinearApi for Mutex<api::fake::FakeLinear> {
    fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let result = self
            .lock()
            .unwrap()
            .execute(operation, query, variables, write);
        async move { result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::api::fake::FakeLinear;

    #[test]
    fn rate_limits_decode_apart_from_refusals() {
        assert_eq!(
            decode_response(
                400,
                true,
                br#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#
            ),
            Err(ApiError::RateLimited)
        );
        assert_eq!(
            decode_response(429, false, b"Too Many Requests"),
            Err(ApiError::RateLimited)
        );
        assert_eq!(
            decode_response(429, true, b"{}"),
            Err(ApiError::RateLimited)
        );
        assert_eq!(
            decode_response(
                400,
                true,
                br#"{"errors":[{"message":"Entity not found","extensions":{"code":"INVALID_INPUT"}}]}"#
            ),
            Err(ApiError::Graphql("Entity not found".into()))
        );
        assert_eq!(
            decode_response(502, false, b"<html>"),
            Err(ApiError::HttpStatus(502))
        );
        assert_eq!(
            decode_response(200, true, br#"{"data":{"x":1}}"#).unwrap()["x"],
            1
        );
        assert_eq!(
            ApiError::RateLimited.to_string(),
            "Linear rate-limited the request"
        );
    }

    /// A lease that hands out a fixed token, bound or not.
    struct TestLease {
        bound: bool,
    }

    impl Lease for TestLease {
        fn read(
            &mut self,
            callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
        ) -> Result<Value, ApiError> {
            Ok(callback("token")?.into_parts().1)
        }

        fn write(
            &mut self,
            callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
        ) -> Result<Value, ApiError> {
            if !self.bound {
                return Err(ApiError::Credential(CredentialError::NotReady));
            }
            callback("token")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_are_https_only_and_writes_need_a_bound_credential() {
        let unbound = Client::with_lease(
            || Ok(TestLease { bound: false }),
            "http://127.0.0.1:9/graphql",
        )
        .await
        .unwrap();
        let read = unbound
            .request(
                "HlaViewer",
                "query HlaViewer { viewer { id } }",
                json!({}),
                false,
            )
            .await;
        assert_eq!(
            read.result,
            Err(ApiError::RequestFailed),
            "plain HTTP is refused"
        );
        assert!(read.headers.is_empty());
        assert_eq!(
            unbound
                .execute("HlaIssueState", "mutation", json!({}), true)
                .await,
            Err(ApiError::Credential(CredentialError::NotReady))
        );
        assert_eq!(unbound.take_headers().len(), 1);
        assert!(unbound.take_headers().is_empty());

        let bound = Client::with_lease(
            || Ok(TestLease { bound: true }),
            "http://127.0.0.1:9/graphql",
        )
        .await
        .unwrap();
        assert_eq!(
            bound
                .execute("HlaIssueState", "mutation", json!({}), true)
                .await,
            Err(ApiError::RequestFailed)
        );
        assert_eq!(
            Client::with_lease(
                || Err::<TestLease, _>(CredentialError::NotReady),
                GRAPHQL_ENDPOINT
            )
            .await
            .err(),
            Some(ApiError::Credential(CredentialError::NotReady))
        );
    }

    #[tokio::test]
    async fn the_fake_answers_the_async_operations() {
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let linear = Mutex::new(fake);

        assert_eq!(linear.viewer().await.unwrap().id, api::fake::APP_USER);
        let issues = linear.delegated_issues(&["DATA".into()]).await.unwrap();
        assert_eq!(issues[0].identifier, "DATA-1");
        assert_eq!(linear.issue(&issue).await.unwrap().identifier, "DATA-1");
        assert_eq!(linear.find_session(&issue).await.unwrap(), None);
        let session = linear.create_session(&issue).await.unwrap();
        assert_eq!(session, "session-1");
        assert_eq!(
            linear.find_session(&issue).await.unwrap().as_deref(),
            Some("session-1")
        );
        let thought = Activity::new(api::Content::Thought { body: "Hi".into() });
        linear
            .create_activity(&session, "a-1", &thought)
            .await
            .unwrap();
        assert!(linear.activity_exists(&session, "a-1").await.unwrap());
        assert!(!linear.activity_exists(&session, "a-2").await.unwrap());
        linear
            .set_plan(
                &session,
                &json!([{ "content": "Plan", "status": "pending" }]),
            )
            .await
            .unwrap();
        linear
            .set_issue_state(&issue, "state-progress")
            .await
            .unwrap();
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent_types(), ["thought"]);
        assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
        assert!(linear.take_headers().is_empty());
    }

    #[tokio::test]
    async fn the_batched_run_read_falls_back_to_one_request_per_run() {
        let mut fake = FakeLinear::default();
        let one = fake.add_issue("DATA-1", "DATA", "First");
        let two = fake.add_issue("DATA-2", "DATA", "Second");
        let session = fake.delegate_session("DATA-1");
        let linear = Mutex::new(fake);
        let query = |issue_id: &str, session_id: &str| RunQuery {
            issue_id: issue_id.into(),
            session_id: session_id.into(),
            cursor: "2026-09-24T00:00:00Z".into(),
        };
        let updates = linear
            .read_runs(&[query(&one, &session), query(&two, "session-missing")])
            .await;
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].as_ref().unwrap().issue.state_type, "unstarted");
        assert_eq!(
            updates[1],
            Err(ApiError::Graphql("Entity not found: AgentSession".into()))
        );
        assert_eq!(linear.lock().unwrap().count("HlaRuns"), 3);

        linear.lock().unwrap().fail_next = Some(ApiError::RateLimited);
        let updates = linear.read_runs(&[query(&one, &session)]).await;
        assert_eq!(updates, [Err(ApiError::RateLimited)]);
        assert_eq!(linear.lock().unwrap().count("HlaRuns"), 4);
    }
}
