//! `HttpGateway`: the engine's task and run endpoints over the gateway's
//! HTTP API.
//!
//! The status table of the dev workflows design (7.1) lives here and nowhere
//! else: the ports speak replies, and only this module knows they were
//! statuses.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use vetinari_core::application::devflow::ports::{LeaseReply, TaskGateway, TaskReply};
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::devflow::ids::{LeaseToken, TaskId, TaskQueue, WorkerName};
use vetinari_core::domain::devflow::task::{RawTask, StepFailure, StepOutput};

use super::wire::ClaimReply;
use crate::HttpClient;

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
            return Err(PortError::Refused {
                what: WHAT.to_owned(),
                detail: "the gateway refused the token after a refresh".to_owned(),
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::testing::MockServer;
    use time::macros::datetime;
    use vetinari_core::domain::devflow::outcome::{ReviewOutcome, StepOutcome, Summary};
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
        (409, json!({ "reason": reason }).to_string())
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
    async fn an_engine_that_is_down_is_unavailable() {
        let server = MockServer::start(vec![(
            503,
            json!({ "error": { "code": "unavailable", "message": "engine down" } }).to_string(),
        )])
        .await;
        let error = gateway(&server)
            .heartbeat(&task(), &lease())
            .await
            .expect_err("down");
        assert!(error.is_transient(), "{error:?}");
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
        assert!(matches!(error, PortError::Refused { .. }), "{error:?}");
        assert!(!error.is_transient());
    }
}
