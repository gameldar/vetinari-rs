//! `HttpGateway`: the engine's task and run endpoints over the gateway's
//! HTTP API.
//!
//! The status table of the dev workflows design (7.1) lives here and nowhere
//! else: the ports speak replies, and only this module knows they were
//! statuses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use vetinari_core::application::devflow::ports::{
    CancelReply, LeaseReply, RunEventStream, RunEvents, RunGateway, TaskGateway, TaskReply,
};
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::devflow::compile::WorkflowGraph;
use vetinari_core::domain::devflow::ids::{
    DefinitionId, LeaseToken, RunId, TaskId, TaskQueue, WorkerName,
};
use vetinari_core::domain::devflow::run_view::{RunEvent, RunGraph, RunHeader};
use vetinari_core::domain::devflow::step::StepConfig;
use vetinari_core::domain::devflow::task::{RawTask, StepFailure, StepOutput};

use super::sse::SseDecoder;
use super::wire::{
    ClaimReply, RunGraphReply, RunViewReply, StartedReply, WorkflowBody, WorkflowSummaryReply,
    WorkflowViewReply, run_event,
};
use crate::HttpClient;
use vetinari_core::application::auth::AuthService;
use vetinari_core::domain::credentials::Provider;

/// How errors from this backend name it.
const WHAT: &str = "autognomus";

/// Where the gateway's bearer token comes from.
#[async_trait]
pub trait Bearer: Send + Sync {
    /// A token to send. `refresh` asks for a new one, because the gateway
    /// refused the last.
    async fn token(&self, refresh: bool) -> PortResult<String>;
}

/// A token that never changes, for development against a local gateway.
#[derive(Debug, Clone)]
pub struct StaticBearer(String);

impl StaticBearer {
    /// Wrap a token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

#[async_trait]
impl Bearer for StaticBearer {
    async fn token(&self, _refresh: bool) -> PortResult<String> {
        Ok(self.0.clone())
    }
}

/// The token vetinarid signed in for, renewed near its expiry and when
/// the gateway refuses it. Only the daemon holds one: refresh tokens rotate
/// on use.
pub struct AuthBearer {
    auth: Arc<AuthService>,
}

impl AuthBearer {
    /// Read the autognomus credential through `auth`.
    #[must_use]
    pub const fn new(auth: Arc<AuthService>) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl Bearer for AuthBearer {
    async fn token(&self, refresh: bool) -> PortResult<String> {
        self.auth
            .renewed_token(Provider::Autognomus, None, refresh)
            .await
            .map(|token| token.expose().to_owned())
    }
}

/// The gateway's task and run endpoints.
pub struct HttpGateway {
    base_url: String,
    http: HttpClient,
    bearer: Arc<dyn Bearer>,
}

impl HttpGateway {
    /// Build a client for the gateway at `base_url`.
    #[must_use]
    pub fn new(base_url: &str, http: HttpClient, bearer: Arc<dyn Bearer>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            http,
            bearer,
        }
    }

    /// The URL for a path. Task ids go in as they are: `~` and `@` are both
    /// legal in a path segment, and the gateway matches them unencoded.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Send a request, refreshing the token once if the gateway refuses it.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Option<StdDuration>,
    ) -> PortResult<Answer> {
        let first = self.attempt(&method, path, body, timeout, false).await?;
        if first.status != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let second = self.attempt(&method, path, body, timeout, true).await?;
        if second.status == StatusCode::UNAUTHORIZED {
            return Err(PortError::Unauthenticated {
                provider: Provider::Autognomus,
            });
        }
        Ok(second)
    }

    async fn attempt(
        &self,
        method: &Method,
        path: &str,
        body: Option<&Value>,
        timeout: Option<StdDuration>,
        refresh: bool,
    ) -> PortResult<Answer> {
        let token = self.bearer.token(refresh).await?;
        let mut request = self
            .http
            .inner()
            .request(method.clone(), self.url(path))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request
            .send()
            .await
            .map_err(|error| PortError::unavailable(WHAT, error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable(WHAT, error.to_string()))?;
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(WHAT, describe(status, &body)));
        }
        Ok(Answer { status, body })
    }
}

/// A status and body the gateway answered with.
struct Answer {
    status: StatusCode,
    body: String,
}

impl Answer {
    fn json<T: for<'de> Deserialize<'de>>(&self, what: &str) -> PortResult<T> {
        serde_json::from_str(&self.body)
            .map_err(|error| PortError::protocol(WHAT, format!("{what}: {error}")))
    }

    fn unexpected(&self, what: &str) -> PortError {
        PortError::protocol(
            WHAT,
            format!("{what}: {}", describe(self.status, &self.body)),
        )
    }

    /// A status that is not a reply: a `400` refused, a `404` for `id`, or
    /// something unexpected.
    fn refusal(&self, what: &str, id: &str) -> PortError {
        match self.status {
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
                PortError::Refused {
                    what: WHAT.to_owned(),
                    detail: describe(self.status, &self.body),
                }
            }
            StatusCode::NOT_FOUND => PortError::not_found(what, id),
            _ => self.unexpected(what),
        }
    }

    /// A `409`'s reason, as the reply it stands for.
    fn conflict(&self, what: &str) -> PortResult<Conflict> {
        let body: ConflictBody = self.json(what)?;
        match body.reason.as_str() {
            "stale" => Ok(Conflict::Stale),
            "gone" => Ok(Conflict::Gone),
            other => Err(PortError::protocol(
                WHAT,
                format!("{what}: unknown conflict reason {other:?}"),
            )),
        }
    }
}

enum Conflict {
    Stale,
    Gone,
}

#[derive(Deserialize)]
struct ConflictBody {
    reason: String,
}

#[derive(Deserialize)]
struct Extended {
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

/// The gateway's error body, `{ "error": { "code", "message" } }`, as text.
fn describe(status: StatusCode, body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ErrorDetail,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        code: String,
        message: String,
    }
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(ErrorBody { error }) => format!("{status} {}: {}", error.code, error.message),
        Err(_) => format!("{status}: {}", body.trim()),
    }
}

/// How long a claim may take: the long-poll plus the usual allowance, since
/// the gateway's own wait is longer than the client's default timeout.
fn claim_timeout(wait: Duration) -> StdDuration {
    StdDuration::try_from(wait).unwrap_or_default() + HttpClient::TIMEOUT
}

fn task_reply(answer: &Answer, what: &str) -> PortResult<TaskReply> {
    match answer.status {
        StatusCode::NO_CONTENT | StatusCode::OK => Ok(TaskReply::Accepted),
        StatusCode::CONFLICT => Ok(match answer.conflict(what)? {
            Conflict::Stale => TaskReply::Stale,
            Conflict::Gone => TaskReply::Gone,
        }),
        StatusCode::NOT_FOUND => Ok(TaskReply::NotFound),
        _ => Err(answer.unexpected(what)),
    }
}

#[async_trait]
impl TaskGateway for HttpGateway {
    async fn claim(
        &self,
        queue: &TaskQueue,
        worker: &WorkerName,
        wait: Duration,
    ) -> PortResult<Option<RawTask>> {
        let body = json!({
            "queue": queue.to_string(),
            "worker": worker.to_string(),
            "wait_seconds": wait.whole_seconds(),
        });
        let answer = self
            .send(
                Method::POST,
                "/v1/tasks/claim",
                Some(&body),
                Some(claim_timeout(wait)),
            )
            .await?;
        match answer.status {
            StatusCode::OK => answer.json::<ClaimReply>("claim")?.into_raw().map(Some),
            StatusCode::NO_CONTENT => Ok(None),
            _ => Err(answer.unexpected("claim")),
        }
    }

    async fn heartbeat(&self, task: &TaskId, lease: &LeaseToken) -> PortResult<LeaseReply> {
        let body = json!({ "lease": lease.to_string() });
        let answer = self
            .send(
                Method::POST,
                &format!("/v1/tasks/{task}/heartbeat"),
                Some(&body),
                None,
            )
            .await?;
        match answer.status {
            StatusCode::OK => Ok(LeaseReply::Extended {
                expires_at: answer.json::<Extended>("heartbeat")?.expires_at,
            }),
            StatusCode::CONFLICT => Ok(match answer.conflict("heartbeat")? {
                Conflict::Stale => LeaseReply::Stale,
                Conflict::Gone => LeaseReply::Gone,
            }),
            StatusCode::NOT_FOUND => Ok(LeaseReply::NotFound),
            _ => Err(answer.unexpected("heartbeat")),
        }
    }

    async fn complete(
        &self,
        task: &TaskId,
        lease: &LeaseToken,
        output: &StepOutput,
    ) -> PortResult<TaskReply> {
        let body = json!({ "lease": lease.to_string(), "output": output.to_json() });
        let answer = self
            .send(
                Method::POST,
                &format!("/v1/tasks/{task}/complete"),
                Some(&body),
                None,
            )
            .await?;
        task_reply(&answer, "complete")
    }

    async fn fail(
        &self,
        task: &TaskId,
        lease: &LeaseToken,
        failure: &StepFailure,
    ) -> PortResult<TaskReply> {
        let body = json!({
            "lease": lease.to_string(),
            "kind": failure.kind.name(),
            "message": failure.message,
        });
        let answer = self
            .send(
                Method::POST,
                &format!("/v1/tasks/{task}/fail"),
                Some(&body),
                None,
            )
            .await?;
        task_reply(&answer, "fail")
    }
}

/// How many runs of one definition to list: the gateway's cap. A
/// definition has one run, so this is never reached.
const MAX_RUNS: u32 = 200;

#[async_trait]
impl RunGateway for HttpGateway {
    async fn register(&self, graph: &WorkflowGraph) -> PortResult<()> {
        let body = serde_json::to_value(WorkflowBody::from(graph))
            .map_err(|error| PortError::protocol(WHAT, error.to_string()))?;
        let answer = self
            .send(Method::POST, "/v1/workflows", Some(&body), None)
            .await?;
        match answer.status {
            StatusCode::CREATED | StatusCode::OK => Ok(()),
            _ => Err(answer.refusal("register", graph.id.as_str())),
        }
    }

    async fn start(&self, definition: &DefinitionId, input: &Value) -> PortResult<RunId> {
        let body = json!({ "input": input });
        let answer = self
            .send(
                Method::POST,
                &format!("/v1/workflows/{definition}/runs"),
                Some(&body),
                None,
            )
            .await?;
        match answer.status {
            StatusCode::CREATED | StatusCode::OK => {
                let started: StartedReply = answer.json("start")?;
                RunId::parse(&started.run)
                    .map_err(|error| PortError::protocol(WHAT, format!("run id: {error}")))
            }
            _ => Err(answer.refusal("workflow", definition.as_str())),
        }
    }

    async fn remove(&self, definition: &DefinitionId) -> PortResult<()> {
        let answer = self
            .send(
                Method::DELETE,
                &format!("/v1/workflows/{definition}"),
                None,
                None,
            )
            .await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(answer.refusal("workflow", definition.as_str())),
        }
    }

    async fn definitions(&self) -> PortResult<Vec<DefinitionId>> {
        let answer = self.send(Method::GET, "/v1/workflows", None, None).await?;
        if answer.status != StatusCode::OK {
            return Err(answer.unexpected("workflows"));
        }
        let listed: Vec<WorkflowSummaryReply> = answer.json("workflows")?;
        Ok(listed
            .into_iter()
            .filter_map(|summary| DefinitionId::parse(&summary.id).ok())
            .collect())
    }

    async fn step_config(&self, definition: &DefinitionId) -> PortResult<StepConfig> {
        let answer = self
            .send(
                Method::GET,
                &format!("/v1/workflows/{definition}"),
                None,
                None,
            )
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.refusal("workflow", definition.as_str()));
        }
        answer
            .json::<WorkflowViewReply>("workflow")?
            .into_step_config()
    }

    async fn runs(&self, definition: &DefinitionId) -> PortResult<Vec<RunHeader>> {
        let answer = self
            .send(
                Method::GET,
                &format!("/v1/workflows/{definition}/runs?limit={MAX_RUNS}"),
                None,
                None,
            )
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.refusal("workflow", definition.as_str()));
        }
        let runs: Vec<RunViewReply> = answer.json("runs")?;
        runs.into_iter().map(RunViewReply::into_header).collect()
    }

    async fn graph(&self, run: &RunId) -> PortResult<RunGraph> {
        let answer = self
            .send(Method::GET, &format!("/v1/runs/{run}/graph"), None, None)
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.refusal("run", run.as_str()));
        }
        answer.json::<RunGraphReply>("graph")?.into_graph()
    }

    async fn cancel(&self, run: &RunId) -> PortResult<CancelReply> {
        let answer = self
            .send(Method::POST, &format!("/v1/runs/{run}/cancel"), None, None)
            .await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(CancelReply::Cancelled),
            StatusCode::CONFLICT => Ok(CancelReply::AlreadyFinished),
            _ => Err(answer.refusal("run", run.as_str())),
        }
    }

    async fn outputs(&self, run: &RunId) -> PortResult<BTreeMap<String, Value>> {
        let answer = self
            .send(Method::GET, &format!("/v1/runs/{run}"), None, None)
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.refusal("run", run.as_str()));
        }
        Ok(answer.json::<RunViewReply>("run")?.outputs)
    }
}

/// How long one stream connection may last before it is reopened. The
/// client's default would end a quiet stream after twenty seconds; this
/// bounds it without one being noticed, since a reopened stream refetches.
const STREAM_TIMEOUT: StdDuration = StdDuration::from_secs(6 * 60 * 60);

#[async_trait]
impl RunEvents for HttpGateway {
    async fn subscribe(&self) -> PortResult<Box<dyn RunEventStream>> {
        let mut response = self.open_stream(false).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            response = self.open_stream(true).await?;
        }
        let status = response.status();
        if status == StatusCode::OK {
            return Ok(Box::new(SseStream {
                response,
                decoder: SseDecoder::default(),
                pending: VecDeque::new(),
            }));
        }
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            return Err(PortError::Refused {
                what: WHAT.to_owned(),
                detail: "the gateway refused the token after a refresh".to_owned(),
            });
        }
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(WHAT, describe(status, &body)));
        }
        Err(PortError::protocol(
            WHAT,
            format!("run events: {}", describe(status, &body)),
        ))
    }
}

impl HttpGateway {
    async fn open_stream(&self, refresh: bool) -> PortResult<reqwest::Response> {
        let token = self.bearer.token(refresh).await?;
        self.http
            .inner()
            .get(self.url("/v1/runs/events"))
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .timeout(STREAM_TIMEOUT)
            .send()
            .await
            .map_err(|error| PortError::unavailable(WHAT, error.to_string()))
    }
}

/// An open `GET /v1/runs/events`.
struct SseStream {
    response: reqwest::Response,
    decoder: SseDecoder,
    pending: VecDeque<RunEvent>,
}

#[async_trait]
impl RunEventStream for SseStream {
    async fn next(&mut self) -> PortResult<Option<RunEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            // A dropped connection ends the stream like a closed one: either
            // way events may have been missed, and the reopen refetches.
            let Ok(Some(chunk)) = self.response.chunk().await else {
                return Ok(None);
            };
            for message in self.decoder.feed(&chunk) {
                match run_event(&message.event, &message.data) {
                    Ok(event) => self.pending.push_back(event),
                    Err(error) => {
                        tracing::warn!(%error, event = message.event, "skipped a run event");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::testing::{MockServer, Reply};
    use time::macros::datetime;
    use vetinari_core::domain::devflow::outcome::{ReviewOutcome, StepOutcome, Summary};
    use vetinari_core::domain::devflow::run_view::RunStatus;
    use vetinari_core::domain::git::BranchName;

    const TASK: &str = "01JB7ZR2M4Q9V8C6T3N5HKD0WX~implement@2~1";
    const CLAIM: &str = include_str!("../../tests/fixtures/autognomus/claim-implement-rework.json");

    #[derive(Default)]
    struct RecordingBearer {
        asked: Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl Bearer for RecordingBearer {
        async fn token(&self, refresh: bool) -> PortResult<String> {
            self.asked.lock().expect("lock").push(refresh);
            Ok(if refresh { "fresh" } else { "old" }.to_owned())
        }
    }

    fn gateway(server: &MockServer) -> HttpGateway {
        gateway_with(&server.base_url, Arc::new(StaticBearer::new("tok")))
    }

    fn gateway_with(base_url: &str, bearer: Arc<dyn Bearer>) -> HttpGateway {
        HttpGateway::new(base_url, HttpClient::new().expect("client"), bearer)
    }

    fn task() -> TaskId {
        TaskId::parse(TASK).expect("task")
    }

    fn lease() -> LeaseToken {
        LeaseToken::parse("01JB7ZS9D1F4G7H2J5K8M3N6P0").expect("lease")
    }

    fn output() -> StepOutput {
        StepOutput {
            outcome: StepOutcome::Review(ReviewOutcome::Approved {
                summary: Summary::parse("Retries honour Retry-After").expect("summary"),
            }),
            git_branch: BranchName::sanitized("vetinari/eng-412-rate-limit-webhook-retries"),
        }
    }

    fn conflict(reason: &str) -> (u16, String) {
        (
            409,
            json!({ "error": { "code": reason, "message": "conflict" }, "reason": reason })
                .to_string(),
        )
    }

    // Claim

    #[tokio::test]
    async fn a_claim_posts_queue_worker_and_wait_with_the_bearer_token() {
        let server = MockServer::start(vec![(200, CLAIM.to_owned())]).await;
        let raw = gateway(&server)
            .claim(
                &TaskQueue::parse("vetinari-hex-agent").expect("queue"),
                &WorkerName::parse("hex").expect("worker"),
                Duration::seconds(30),
            )
            .await
            .expect("claim")
            .expect("a task");

        assert_eq!(raw.id(), &task());
        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/tasks/claim");
        assert_eq!(request.header("authorization"), Some("Bearer tok"));
        assert_eq!(
            request.json(),
            json!({ "queue": "vetinari-hex-agent", "worker": "hex", "wait_seconds": 30 })
        );
    }

    #[tokio::test]
    async fn a_claim_with_nothing_offered_is_none() {
        let server = MockServer::start(vec![(204, String::new())]).await;
        let claimed = gateway(&server)
            .claim(
                &TaskQueue::parse("vetinari-hex-agent").expect("queue"),
                &WorkerName::parse("hex").expect("worker"),
                Duration::seconds(30),
            )
            .await
            .expect("claim");
        assert_eq!(claimed, None);
    }

    #[tokio::test]
    async fn a_claim_reply_with_an_unreadable_id_is_a_protocol_error() {
        let reply = CLAIM.replace(TASK, "not-a-task-id");
        let server = MockServer::start(vec![(200, reply)]).await;
        let error = gateway(&server)
            .claim(
                &TaskQueue::parse("vetinari-hex-agent").expect("queue"),
                &WorkerName::parse("hex").expect("worker"),
                Duration::seconds(30),
            )
            .await
            .expect_err("unreadable");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    // Heartbeat

    #[tokio::test]
    async fn a_heartbeat_puts_the_task_id_in_the_path_unencoded() {
        let server = MockServer::start(vec![(
            200,
            json!({ "expires_at": "2026-10-25T10:10:00Z" }).to_string(),
        )])
        .await;
        let reply = gateway(&server)
            .heartbeat(&task(), &lease())
            .await
            .expect("heartbeat");

        assert_eq!(
            reply,
            LeaseReply::Extended {
                expires_at: datetime!(2026-10-25 10:10 UTC)
            }
        );
        let request = server.request();
        assert_eq!(request.target, format!("/v1/tasks/{TASK}/heartbeat"));
        assert_eq!(
            request.json(),
            json!({ "lease": "01JB7ZS9D1F4G7H2J5K8M3N6P0" })
        );
    }

    #[tokio::test]
    async fn a_heartbeat_conflict_is_stale_or_gone_by_its_reason() {
        let server = MockServer::start(vec![conflict("stale"), conflict("gone")]).await;
        let gateway = gateway(&server);
        assert_eq!(
            gateway.heartbeat(&task(), &lease()).await.expect("stale"),
            LeaseReply::Stale
        );
        assert_eq!(
            gateway.heartbeat(&task(), &lease()).await.expect("gone"),
            LeaseReply::Gone
        );
    }

    #[tokio::test]
    async fn a_heartbeat_for_an_unknown_task_is_not_found() {
        let server = MockServer::start(vec![(
            404,
            json!({ "error": { "code": "not_found", "message": "no run" } }).to_string(),
        )])
        .await;
        assert_eq!(
            gateway(&server)
                .heartbeat(&task(), &lease())
                .await
                .expect("reply"),
            LeaseReply::NotFound
        );
    }

    #[tokio::test]
    async fn a_conflict_with_an_unknown_reason_is_a_protocol_error() {
        let server = MockServer::start(vec![conflict("sideways")]).await;
        let error = gateway(&server)
            .heartbeat(&task(), &lease())
            .await
            .expect_err("unknown");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    // Complete and fail

    #[tokio::test]
    async fn a_completion_posts_the_lease_and_the_output_document() {
        let server = MockServer::start(vec![(204, String::new())]).await;
        let reply = gateway(&server)
            .complete(&task(), &lease(), &output())
            .await
            .expect("complete");

        assert_eq!(reply, TaskReply::Accepted);
        let request = server.request();
        assert_eq!(request.target, format!("/v1/tasks/{TASK}/complete"));
        assert_eq!(
            request.json(),
            json!({ "lease": "01JB7ZS9D1F4G7H2J5K8M3N6P0", "output": output().to_json() })
        );
    }

    #[tokio::test]
    async fn a_completion_conflict_and_a_missing_task_are_replies() {
        let server = MockServer::start(vec![conflict("stale"), (404, String::new())]).await;
        let gateway = gateway(&server);
        assert_eq!(
            gateway
                .complete(&task(), &lease(), &output())
                .await
                .expect("stale"),
            TaskReply::Stale
        );
        assert_eq!(
            gateway
                .complete(&task(), &lease(), &output())
                .await
                .expect("not found"),
            TaskReply::NotFound
        );
    }

    #[tokio::test]
    async fn a_refused_output_is_a_protocol_error() {
        let server = MockServer::start(vec![(
            422,
            json!({ "error": { "code": "invalid", "message": "output must be an object" } })
                .to_string(),
        )])
        .await;
        let error = gateway(&server)
            .complete(&task(), &lease(), &output())
            .await
            .expect_err("refused");
        match error {
            PortError::Protocol { detail, .. } => {
                assert!(detail.contains("output must be an object"), "{detail}");
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failure_posts_its_kind_and_message() {
        let server = MockServer::start(vec![(204, String::new()), conflict("gone")]).await;
        let gateway = gateway(&server);
        let accepted = gateway
            .fail(&task(), &lease(), &StepFailure::transient("agent crashed"))
            .await
            .expect("fail");
        let gone = gateway
            .fail(&task(), &lease(), &StepFailure::permanent("no result"))
            .await
            .expect("fail");

        assert_eq!((accepted, gone), (TaskReply::Accepted, TaskReply::Gone));
        let request = server.request();
        assert_eq!(request.target, format!("/v1/tasks/{TASK}/fail"));
        assert_eq!(
            request.json(),
            json!({
                "lease": "01JB7ZS9D1F4G7H2J5K8M3N6P0",
                "kind": "transient",
                "message": "agent crashed"
            })
        );
    }

    // Reaching the gateway

    #[tokio::test]
    async fn an_engine_that_is_down_or_slow_is_unavailable() {
        for (status, code) in [(502, "workflow_unavailable"), (504, "workflow_timeout")] {
            let server = MockServer::start(vec![(
                status,
                json!({ "error": { "code": code, "message": "engine down" } }).to_string(),
            )])
            .await;
            let error = gateway(&server)
                .heartbeat(&task(), &lease())
                .await
                .expect_err("down");
            assert!(error.is_transient(), "{status}: {error:?}");
        }
    }

    #[tokio::test]
    async fn a_gateway_that_cannot_be_reached_is_unavailable() {
        let error = gateway_with("http://127.0.0.1:1", Arc::new(StaticBearer::new("tok")))
            .heartbeat(&task(), &lease())
            .await
            .expect_err("unreachable");
        assert!(error.is_transient(), "{error:?}");
    }

    #[tokio::test]
    async fn a_refused_token_is_refreshed_once_and_the_request_retried() {
        let server = MockServer::start(vec![
            (401, String::new()),
            (
                200,
                json!({ "expires_at": "2026-10-25T10:10:00Z" }).to_string(),
            ),
        ])
        .await;
        let bearer = Arc::new(RecordingBearer::default());
        let reply = gateway_with(&server.base_url, bearer.clone())
            .heartbeat(&task(), &lease())
            .await
            .expect("heartbeat");

        assert!(matches!(reply, LeaseReply::Extended { .. }));
        assert_eq!(*bearer.asked.lock().expect("lock"), [false, true]);
        assert_eq!(
            server.requests()[1].header("authorization"),
            Some("Bearer fresh")
        );
    }

    #[tokio::test]
    async fn a_token_refused_after_a_refresh_is_not_retried_again() {
        let server = MockServer::start(vec![(401, String::new()), (401, String::new())]).await;
        let error = gateway(&server)
            .heartbeat(&task(), &lease())
            .await
            .expect_err("refused");
        assert_eq!(
            error,
            PortError::Unauthenticated {
                provider: Provider::Autognomus
            }
        );
        assert!(!error.is_transient());
    }

    #[tokio::test]
    async fn the_signed_in_token_is_sent_renewed_when_refused() {
        let server = MockServer::start(vec![
            (401, String::new()),
            (200, r#"{"expires_at":"2026-10-25T10:00:00Z"}"#.to_owned()),
        ])
        .await;
        let store = Arc::new(vetinari_core::testing::MemoryCredentialStore::default());
        store.seed(
            &vetinari_core::domain::credentials::CredentialKey::new(Provider::Autognomus, None),
            vetinari_core::domain::credentials::StoredCredential::oauth(
                vetinari_core::domain::credentials::Secret::new("refused"),
                Some(vetinari_core::domain::credentials::Secret::new("refresh")),
                None,
            ),
        );
        let clock = Arc::new(vetinari_core::testing::FixedClock::new(
            time::macros::datetime!(2026-10-25 09:00 UTC),
        ));
        let auth = Arc::new(vetinari_core::application::auth::AuthService::new(
            store,
            Arc::new(vetinari_core::testing::FakeDeviceCodeFlow::new(
                1,
                time::macros::datetime!(2026-10-25 10:00 UTC),
            )),
            clock,
        ));
        gateway_with(&server.base_url, Arc::new(AuthBearer::new(auth)))
            .heartbeat(&task(), &lease())
            .await
            .expect("extended");
        let requests = server.requests();
        assert_eq!(requests[0].header("authorization"), Some("Bearer refused"));
        assert_eq!(
            requests[1].header("authorization"),
            Some("Bearer refreshed-access")
        );
    }

    // Definitions and runs

    const RUN: &str = "01JB7ZR2M4Q9V8C6T3N5HKD0WX";
    const DEFINITION: &str = "vetinari-feature-01JB7ZQ3XK";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/autognomus/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture")
    }

    fn run() -> RunId {
        RunId::parse(RUN).expect("run")
    }

    fn definition() -> DefinitionId {
        DefinitionId::parse(DEFINITION).expect("definition")
    }

    fn compiled() -> WorkflowGraph {
        use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues, compile};
        use vetinari_core::domain::devflow::ids::Rounds;
        use vetinari_core::domain::devflow::template::{
            DevWorkflow, FeatureFromTicket, RepoTarget, TicketRef,
        };
        use vetinari_core::domain::ids::{ProfileId, SourceId};
        use vetinari_core::domain::text::NonEmptyText;

        let feature = FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse("jira").expect("id"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit webhook retries").expect("title"),
                url: Some("https://acme.atlassian.net/browse/ENG-412".to_owned()),
            },
            repo: RepoTarget {
                path: std::path::PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: ProfileId::parse("work").expect("id"),
            base: Some(BranchName::sanitized("main")),
            max_review_rounds: Rounds::new(3).expect("rounds"),
            max_intake_rounds: Rounds::new(3).expect("rounds"),
            agent: None,
        };
        compile(
            &DevWorkflow::FeatureFromTicket(feature),
            definition(),
            &WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
            &StepPolicies::default(),
        )
    }

    #[tokio::test]
    async fn registering_posts_the_compiled_body() {
        let server = MockServer::start(vec![(
            201,
            json!({ "id": DEFINITION, "version": 1 }).to_string(),
        )])
        .await;
        gateway(&server)
            .register(&compiled())
            .await
            .expect("register");

        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/workflows");
        assert_eq!(
            request.json(),
            serde_json::from_str::<Value>(&fixture("feature-from-ticket.workflow.json"))
                .expect("golden body")
        );
    }

    #[tokio::test]
    async fn a_definition_the_engine_will_not_take_is_refused() {
        let server = MockServer::start(vec![(
            400,
            json!({ "error": { "code": "invalid", "message": "exit not in body" } }).to_string(),
        )])
        .await;
        let error = gateway(&server)
            .register(&compiled())
            .await
            .expect_err("refused");
        match error {
            PortError::Refused { detail, .. } => assert!(detail.contains("exit not in body")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_definition_is_read_for_the_config_its_steps_carry() {
        let server = MockServer::start(vec![(200, fixture("workflow-view.json"))]).await;
        let config = gateway(&server)
            .step_config(&definition())
            .await
            .expect("config");

        assert_eq!(
            server.request().target,
            format!("/v1/workflows/{DEFINITION}")
        );
        let written = compiled()
            .nodes
            .into_iter()
            .find_map(|node| match node {
                vetinari_core::domain::devflow::compile::GraphNode::External { config, .. } => {
                    Some(config)
                }
                vetinari_core::domain::devflow::compile::GraphNode::Loop { .. } => None,
            })
            .expect("an external node");
        assert_eq!(config, *written);
    }

    #[tokio::test]
    async fn a_definition_with_no_step_vetinari_reads_is_a_protocol_error() {
        let server = MockServer::start(vec![(
            200,
            json!({ "id": DEFINITION, "version": 1, "triggers": [], "edges": [],
                "nodes": [{ "kind": "external", "id": "x", "queue": "q", "config": { "schema": 9 } }] })
            .to_string(),
        )])
        .await;
        let error = gateway(&server)
            .step_config(&definition())
            .await
            .expect_err("unreadable");
        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn reading_a_missing_definition_is_not_found() {
        let server = MockServer::start(vec![(
            404,
            json!({ "error": { "code": "not_found", "message": DEFINITION } }).to_string(),
        )])
        .await;
        let error = gateway(&server)
            .step_config(&definition())
            .await
            .expect_err("missing");
        assert!(matches!(error, PortError::NotFound { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn starting_a_run_posts_its_input_and_reads_the_run_id() {
        let server = MockServer::start(vec![(
            201,
            json!({ "run": RUN, "state": "pending" }).to_string(),
        )])
        .await;
        let started = gateway(&server)
            .start(&definition(), &json!({ "ticket": "ENG-412" }))
            .await
            .expect("start");

        assert_eq!(started, run());
        let request = server.request();
        assert_eq!(request.target, format!("/v1/workflows/{DEFINITION}/runs"));
        assert_eq!(request.json(), json!({ "input": { "ticket": "ENG-412" } }));
    }

    #[tokio::test]
    async fn starting_a_run_of_a_missing_definition_is_not_found() {
        let server = MockServer::start(vec![(
            404,
            json!({ "error": { "code": "not_found", "message": DEFINITION } }).to_string(),
        )])
        .await;
        let error = gateway(&server)
            .start(&definition(), &json!({}))
            .await
            .expect_err("missing");
        assert!(matches!(error, PortError::NotFound { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn removing_a_definition_deletes_it() {
        let server = MockServer::start(vec![(204, String::new())]).await;
        gateway(&server)
            .remove(&definition())
            .await
            .expect("remove");
        let request = server.request();
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.target, format!("/v1/workflows/{DEFINITION}"));
    }

    #[tokio::test]
    async fn only_vetinaris_definitions_are_listed() {
        let server = MockServer::start(vec![(200, fixture("workflows.json"))]).await;
        let listed = gateway(&server).definitions().await.expect("definitions");
        assert_eq!(listed, [definition()]);
        assert_eq!(server.request().target, "/v1/workflows");
    }

    #[tokio::test]
    async fn a_definitions_runs_are_listed_as_headers() {
        let server = MockServer::start(vec![(200, fixture("workflow-runs.json"))]).await;
        let runs = gateway(&server).runs(&definition()).await.expect("runs");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, RunStatus::Completed);
        assert_eq!(
            server.request().target,
            format!("/v1/workflows/{DEFINITION}/runs?limit=200")
        );
    }

    #[tokio::test]
    async fn a_runs_graph_is_fetched_and_read() {
        let server = MockServer::start(vec![(200, fixture("run-graph.json"))]).await;
        let graph = gateway(&server).graph(&run()).await.expect("graph");
        assert_eq!(graph.seq, 17);
        assert_eq!(server.request().target, format!("/v1/runs/{RUN}/graph"));
    }

    #[tokio::test]
    async fn the_graph_of_an_unknown_run_is_not_found() {
        let server = MockServer::start(vec![(404, String::new())]).await;
        let error = gateway(&server).graph(&run()).await.expect_err("missing");
        assert!(matches!(error, PortError::NotFound { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn cancelling_a_finished_run_says_so() {
        let server = MockServer::start(vec![
            (204, String::new()),
            (
                409,
                json!({ "error": { "code": "gone", "message": "run finished" }, "reason": "gone" })
                    .to_string(),
            ),
        ])
        .await;
        let gateway = gateway(&server);
        assert_eq!(
            gateway.cancel(&run()).await.expect("cancel"),
            CancelReply::Cancelled
        );
        assert_eq!(
            gateway.cancel(&run()).await.expect("cancel"),
            CancelReply::AlreadyFinished
        );
        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, format!("/v1/runs/{RUN}/cancel"));
    }

    #[tokio::test]
    async fn a_runs_outputs_come_from_the_run_view() {
        let server = MockServer::start(vec![(200, fixture("run-view.json"))]).await;
        let outputs = gateway(&server).outputs(&run()).await.expect("outputs");
        assert_eq!(outputs["ask-info@1"]["note"], json!("Page size is 50"));
        assert_eq!(server.request().target, format!("/v1/runs/{RUN}"));
    }

    // The run stream

    const EVENTS: &str = include_str!("../../tests/fixtures/autognomus/run-events.sse");

    async fn drain(stream: &mut Box<dyn RunEventStream>) -> Vec<RunEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await.expect("event") {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn the_stream_of_every_run_is_opened_with_the_bearer_token() {
        let server = MockServer::replying(vec![Reply::Stream(vec![])]).await;
        let mut stream = gateway(&server).subscribe().await.expect("subscribe");
        assert_eq!(stream.next().await.expect("closed"), None);

        let request = server.request();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/v1/runs/events");
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(request.header("authorization"), Some("Bearer tok"));
    }

    #[tokio::test]
    async fn events_arrive_however_the_stream_is_cut_and_end_when_it_closes() {
        let bytes = EVENTS.as_bytes();
        let chunks: Vec<String> = bytes
            .chunks(37)
            .map(|chunk| String::from_utf8(chunk.to_vec()).expect("ascii fixture"))
            .collect();
        let server = MockServer::replying(vec![Reply::Stream(chunks)]).await;
        let mut stream = gateway(&server).subscribe().await.expect("subscribe");
        let events = drain(&mut stream).await;

        assert_eq!(events.len(), 12);
        assert!(matches!(events[0], RunEvent::Run { seq: 1, .. }));
        assert_eq!(events[11], RunEvent::Resync);
    }

    #[tokio::test]
    async fn a_message_that_cannot_be_read_is_skipped() {
        let server = MockServer::replying(vec![Reply::Stream(vec![
            "event: node.ready\ndata: not json\n\n".to_owned(),
            "event: resync\ndata: {}\n\n".to_owned(),
        ])])
        .await;
        let mut stream = gateway(&server).subscribe().await.expect("subscribe");
        assert_eq!(drain(&mut stream).await, [RunEvent::Resync]);
    }

    #[tokio::test]
    async fn a_stream_refused_for_its_token_is_retried_with_a_fresh_one() {
        let server = MockServer::replying(vec![
            Reply::Json(401, String::new()),
            Reply::Stream(vec!["event: resync\ndata: {}\n\n".to_owned()]),
        ])
        .await;
        let bearer = Arc::new(RecordingBearer::default());
        let mut stream = gateway_with(&server.base_url, bearer.clone())
            .subscribe()
            .await
            .expect("subscribe");
        assert_eq!(drain(&mut stream).await, [RunEvent::Resync]);
        assert_eq!(*bearer.asked.lock().expect("lock"), [false, true]);
    }

    #[tokio::test]
    async fn a_stream_the_gateway_cannot_serve_is_unavailable() {
        let server = MockServer::start(vec![(502, String::new())]).await;
        let error = gateway(&server)
            .subscribe()
            .await
            .err()
            .expect("unavailable");
        assert!(error.is_transient(), "{error:?}");
    }
}
