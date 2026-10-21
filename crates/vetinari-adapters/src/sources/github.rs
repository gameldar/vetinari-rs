//! GitHub as a code review and issue source.
//!
//! One GraphQL request per refresh rather than a REST search followed by a
//! status call per result: the check state is the whole reason a pull request
//! is urgent, and fetching it separately would mean an API call per row.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::ports::{AttentionSource, CodeHostSource, PortError, PortResult};
use vetinari_core::domain::attention::{
    AttentionItem, IssueRef, ItemContext, ItemState, PullRequestRef, SourceCategory,
};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{Slug, SourceId};
use vetinari_core::domain::text::NonEmptyText;

use crate::HttpClient;

/// How a GitHub source is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSettings {
    /// The search query that defines the inbox.
    pub query: String,
    /// An enterprise API base URL, when not github.com.
    pub base_url: Option<String>,
    /// The credential account to use.
    pub account: Option<Slug>,
}

/// Pull requests, checks, and issues from GitHub.
pub struct GitHubSource {
    id: SourceId,
    settings: GitHubSettings,
    http: HttpClient,
    auth: Arc<AuthService>,
}

const SEARCH_QUERY: &str = "\
query($q: String!, $n: Int!) {
  search(query: $q, type: ISSUE, first: $n) {
    nodes {
      __typename
      ... on PullRequest {
        number title url isDraft updatedAt headRefName baseRefName reviewDecision
        author { login }
        repository { nameWithOwner url }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
      ... on Issue {
        number title url updatedAt
        author { login }
        repository { nameWithOwner }
      }
    }
  }
}";

impl GitHubSource {
    /// How many search results to ask for.
    pub const PAGE_SIZE: u32 = 50;

    /// Build a source.
    #[must_use]
    pub fn new(
        id: SourceId,
        settings: GitHubSettings,
        http: HttpClient,
        auth: Arc<AuthService>,
    ) -> Self {
        Self {
            id,
            settings,
            http,
            auth,
        }
    }

    /// The URL for an API path.
    #[must_use]
    pub fn endpoint(&self, path: &str) -> String {
        let base = self
            .settings
            .base_url
            .as_deref()
            .unwrap_or("https://api.github.com")
            .trim_end_matches('/');
        format!("{base}{path}")
    }

    async fn token(&self) -> PortResult<String> {
        self.auth
            .token(Provider::GitHub, self.settings.account.clone())
            .await
            .map(|secret| secret.expose().to_owned())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> PortResult<Value> {
        let response = request
            .send()
            .await
            .map_err(|error| PortError::unavailable("github", error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable("github", error.to_string()))?;

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN && body.contains("Bad credentials")
        {
            return Err(PortError::Unauthenticated {
                provider: Provider::GitHub,
            });
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(
                "github",
                format!("{status}: {}", body.trim()),
            ));
        }
        if !status.is_success() {
            return Err(PortError::Refused {
                what: "github".to_owned(),
                detail: message_of(&body).unwrap_or_else(|| format!("{status}: {}", body.trim())),
            });
        }

        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|error| {
            PortError::protocol("github", format!("could not read the reply: {error}"))
        })
    }
}

fn message_of(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

#[async_trait]
impl AttentionSource for GitHubSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        SourceCategory::CodeReview
    }

    async fn fetch(&self) -> PortResult<Vec<AttentionItem>> {
        let token = self.token().await?;
        let body = json!({
            "query": SEARCH_QUERY,
            "variables": { "q": self.settings.query, "n": Self::PAGE_SIZE },
        });
        let reply = self
            .send(
                self.http
                    .inner()
                    .post(self.endpoint("/graphql"))
                    .bearer_auth(token)
                    .json(&body),
            )
            .await?;

        if let Some(errors) = reply.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let detail = errors
                .iter()
                .filter_map(|error| error.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(PortError::protocol("github", detail));
        }

        let nodes = reply
            .pointer("/data/search/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(nodes
            .iter()
            .filter_map(|node| parse_node(&self.id, node))
            .collect())
    }
}

#[async_trait]
impl CodeHostSource for GitHubSource {
    async fn approve(&self, pr: &PullRequestRef, comment: Option<&str>) -> PortResult<()> {
        let token = self.token().await?;
        let body = json!({ "event": "APPROVE", "body": comment.unwrap_or("") });
        self.send(
            self.http
                .inner()
                .post(self.endpoint(&format!("/repos/{}/pulls/{}/reviews", pr.repo, pr.number)))
                .bearer_auth(token)
                .json(&body),
        )
        .await?;
        Ok(())
    }

    async fn comment(&self, pr: &PullRequestRef, body: &str) -> PortResult<()> {
        let token = self.token().await?;
        self.send(
            self.http
                .inner()
                .post(self.endpoint(&format!("/repos/{}/issues/{}/comments", pr.repo, pr.number)))
                .bearer_auth(token)
                .json(&json!({ "body": body })),
        )
        .await?;
        Ok(())
    }
}

fn parse_node(source: &SourceId, node: &Value) -> Option<AttentionItem> {
    match node.get("__typename").and_then(Value::as_str)? {
        "PullRequest" => parse_pull_request(source, node),
        "Issue" => parse_issue(source, node),
        _ => None,
    }
}

fn parse_pull_request(source: &SourceId, node: &Value) -> Option<AttentionItem> {
    let number = node.get("number").and_then(Value::as_u64)?;
    let repo = node
        .pointer("/repository/nameWithOwner")
        .and_then(Value::as_str)?
        .to_owned();
    let title = NonEmptyText::parse(node.get("title").and_then(Value::as_str)?).ok()?;
    let updated_at = parse_time(node.get("updatedAt").and_then(Value::as_str))?;

    let check_state = node
        .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
        .and_then(Value::as_str)
        .unwrap_or("");
    let review_decision = node
        .get("reviewDecision")
        .and_then(Value::as_str)
        .unwrap_or("");
    let draft = node
        .get("isDraft")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let state = if draft {
        ItemState::InProgress
    } else {
        match check_state {
            "FAILURE" | "ERROR" => ItemState::CiFailed,
            "PENDING" | "EXPECTED" => ItemState::CiRunning,
            _ => match review_decision {
                "CHANGES_REQUESTED" => ItemState::ChangesRequested,
                "APPROVED" => ItemState::Resolved,
                _ => ItemState::NeedsReview,
            },
        }
    };

    let repo_url = node
        .pointer("/repository/url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut builder = AttentionItem::builder(
        source,
        &format!("{repo}#{number}"),
        SourceCategory::CodeReview,
        title,
    )
    .state(state)
    .occurred_at(updated_at)
    .detail("Repository", repo.clone())
    .detail(
        "Branch",
        node.get("headRefName")
            .and_then(Value::as_str)
            .unwrap_or("?"),
    )
    .context(ItemContext::PullRequest(PullRequestRef {
        repo: repo.clone(),
        number,
        head_ref: node
            .get("headRefName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        base_ref: node
            .get("baseRefName")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_owned(),
        clone_url: format!("{repo_url}.git"),
    }));

    if let Some(author) = node.pointer("/author/login").and_then(Value::as_str) {
        builder = builder.author(author);
    }
    if let Some(url) = node.get("url").and_then(Value::as_str) {
        builder = builder.url(url);
    }
    if !check_state.is_empty() {
        builder = builder.detail("Checks", check_state.to_lowercase());
    }
    Some(builder.build())
}

fn parse_issue(source: &SourceId, node: &Value) -> Option<AttentionItem> {
    let number = node.get("number").and_then(Value::as_u64)?;
    let repo = node
        .pointer("/repository/nameWithOwner")
        .and_then(Value::as_str)?
        .to_owned();
    let title = NonEmptyText::parse(node.get("title").and_then(Value::as_str)?).ok()?;
    let updated_at = parse_time(node.get("updatedAt").and_then(Value::as_str))?;
    let key = format!("{repo}#{number}");

    let mut builder = AttentionItem::builder(source, &key, SourceCategory::IssueTracking, title)
        .state(ItemState::Assigned)
        .occurred_at(updated_at)
        .detail("Repository", repo)
        .context(ItemContext::Issue(IssueRef {
            key,
            status: "open".to_owned(),
            transitions: vec!["closed".to_owned()],
        }));

    if let Some(author) = node.pointer("/author/login").and_then(Value::as_str) {
        builder = builder.author(author);
    }
    if let Some(url) = node.get("url").and_then(Value::as_str) {
        builder = builder.url(url);
    }
    Some(builder.build())
}

fn parse_time(raw: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw?, &time::format_description::well_known::Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockServer;
    use time::macros::datetime;
    use vetinari_core::application::ports::{CodeHostSource, DeviceCodeFlow};
    use vetinari_core::domain::attention::{ItemContext, ItemState};
    use vetinari_core::domain::credentials::Secret;
    use vetinari_core::testing::{FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore};

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn auth(with_token: bool) -> Arc<AuthService> {
        let store = Arc::new(MemoryCredentialStore::default());
        if with_token {
            store.seed(
                &vetinari_core::domain::credentials::CredentialKey::new(Provider::GitHub, None),
                vetinari_core::domain::credentials::StoredCredential::api_token(Secret::new(
                    "ghp_test",
                )),
            );
        }
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, NOW));
        Arc::new(AuthService::new(
            store,
            flow,
            Arc::new(FixedClock::new(NOW)),
        ))
    }

    fn source(base_url: &str, with_token: bool) -> GitHubSource {
        GitHubSource::new(
            SourceId::parse("github").expect("id"),
            GitHubSettings {
                query: "is:open is:pr review-requested:@me".to_owned(),
                base_url: Some(base_url.to_owned()),
                account: None,
            },
            HttpClient::new().expect("client"),
            auth(with_token),
        )
    }

    fn pull_request_payload(state: &str, review_decision: &str, draft: bool) -> String {
        format!(
            r#"{{"data":{{"search":{{"nodes":[{{
              "__typename":"PullRequest",
              "number":142,
              "title":"Fix memory leak in auth parser",
              "url":"https://github.test/acme/web/pull/142",
              "isDraft":{draft},
              "updatedAt":"2026-10-21T08:30:00Z",
              "headRefName":"fix/memory-leak",
              "baseRefName":"main",
              "author":{{"login":"developer"}},
              "reviewDecision":"{review_decision}",
              "repository":{{"nameWithOwner":"acme/web","url":"https://github.test/acme/web"}},
              "commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{state}"}}}}}}]}}
            }}]}}}}}}"#
        )
    }

    #[tokio::test]
    async fn the_inbox_query_is_sent_to_the_graphql_endpoint_with_the_token() {
        let server =
            MockServer::with_json(&pull_request_payload("SUCCESS", "REVIEW_REQUIRED", false)).await;
        let source = source(&server.base_url, true);

        source.fetch().await.expect("fetch");

        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/graphql");
        assert_eq!(request.header("authorization"), Some("Bearer ghp_test"));
        let body = request.json();
        assert_eq!(body["variables"]["q"], "is:open is:pr review-requested:@me");
        assert!(
            body["query"]
                .as_str()
                .expect("query")
                .contains("statusCheckRollup"),
            "the check state is asked for in the same round trip"
        );
    }

    #[tokio::test]
    async fn a_pull_request_becomes_an_item_with_everything_a_review_needs() {
        let server =
            MockServer::with_json(&pull_request_payload("SUCCESS", "REVIEW_REQUIRED", false)).await;
        let items = source(&server.base_url, true).fetch().await.expect("fetch");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id().as_str(), "github:acme/web#142");
        assert_eq!(item.title().as_str(), "Fix memory leak in auth parser");
        assert_eq!(item.category(), SourceCategory::CodeReview);
        assert_eq!(item.state(), ItemState::NeedsReview);
        assert_eq!(item.author(), Some("developer"));
        assert_eq!(item.url(), Some("https://github.test/acme/web/pull/142"));
        assert_eq!(item.occurred_at(), datetime!(2026-10-21 08:30:00 UTC));

        match item.context() {
            ItemContext::PullRequest(pr) => {
                assert_eq!(pr.repo, "acme/web");
                assert_eq!(pr.number, 142);
                assert_eq!(pr.head_ref, "fix/memory-leak");
                assert_eq!(pr.base_ref, "main");
                assert_eq!(pr.clone_url, "https://github.test/acme/web.git");
            }
            other => panic!("expected a pull request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_build_outranks_a_review_request_in_the_state_it_reports() {
        let server =
            MockServer::with_json(&pull_request_payload("FAILURE", "REVIEW_REQUIRED", false)).await;
        let items = source(&server.base_url, true).fetch().await.expect("fetch");
        assert_eq!(items[0].state(), ItemState::CiFailed);
    }

    #[tokio::test]
    async fn a_build_still_running_says_so() {
        let server =
            MockServer::with_json(&pull_request_payload("PENDING", "REVIEW_REQUIRED", false)).await;
        let items = source(&server.base_url, true).fetch().await.expect("fetch");
        assert_eq!(items[0].state(), ItemState::CiRunning);
    }

    #[tokio::test]
    async fn changes_requested_is_reported_when_the_build_is_green() {
        let server =
            MockServer::with_json(&pull_request_payload("SUCCESS", "CHANGES_REQUESTED", false))
                .await;
        let items = source(&server.base_url, true).fetch().await.expect("fetch");
        assert_eq!(items[0].state(), ItemState::ChangesRequested);
    }

    #[tokio::test]
    async fn a_draft_pull_request_is_work_in_progress_rather_than_a_review() {
        let server =
            MockServer::with_json(&pull_request_payload("SUCCESS", "REVIEW_REQUIRED", true)).await;
        let items = source(&server.base_url, true).fetch().await.expect("fetch");
        assert_eq!(items[0].state(), ItemState::InProgress);
    }

    #[tokio::test]
    async fn an_issue_in_the_same_search_becomes_a_tracking_item() {
        let server = MockServer::with_json(
            r#"{"data":{"search":{"nodes":[{
                "__typename":"Issue",
                "number":17,
                "title":"Parser rejects unicode",
                "url":"https://github.test/acme/web/issues/17",
                "updatedAt":"2026-10-20T10:00:00Z",
                "author":{"login":"sarah"},
                "repository":{"nameWithOwner":"acme/web"}
            }]}}}"#,
        )
        .await;

        let items = source(&server.base_url, true).fetch().await.expect("fetch");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].category(), SourceCategory::IssueTracking);
        assert_eq!(items[0].state(), ItemState::Assigned);
        match items[0].context() {
            ItemContext::Issue(issue) => assert_eq!(issue.key, "acme/web#17"),
            other => panic!("expected an issue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_node_github_will_not_describe_is_skipped_rather_than_failing_the_fetch() {
        let server = MockServer::with_json(
            r#"{"data":{"search":{"nodes":[
                {"__typename":"Repository"},
                {},
                {"__typename":"PullRequest","number":1,"title":"","url":"u","repository":{"nameWithOwner":"a/b"}},
                {"__typename":"Issue","number":2,"title":"Fine","url":"u","updatedAt":"2026-10-20T10:00:00Z","repository":{"nameWithOwner":"a/b"}}
            ]}}}"#,
        )
        .await;

        let items = source(&server.base_url, true).fetch().await.expect("fetch");

        assert_eq!(items.len(), 1, "only the describable node survives");
        assert_eq!(items[0].title().as_str(), "Fine");
    }

    #[tokio::test]
    async fn graphql_errors_are_reported_rather_than_read_as_an_empty_inbox() {
        let server = MockServer::with_json(
            r#"{"errors":[{"message":"Field 'nope' doesn't exist"}],"data":null}"#,
        )
        .await;

        let error = source(&server.base_url, true)
            .fetch()
            .await
            .expect_err("graphql error");

        assert!(matches!(error, PortError::Protocol { .. }));
        assert!(error.to_string().contains("doesn't exist"));
    }

    #[tokio::test]
    async fn a_rejected_token_asks_for_authentication_rather_than_retrying() {
        let server =
            MockServer::start(vec![(401, r#"{"message":"Bad credentials"}"#.to_owned())]).await;

        let error = source(&server.base_url, true)
            .fetch()
            .await
            .expect_err("unauthorized");

        assert_eq!(
            error,
            PortError::Unauthenticated {
                provider: Provider::GitHub
            }
        );
    }

    #[tokio::test]
    async fn a_server_error_is_transient_so_the_next_refresh_tries_again() {
        let server = MockServer::start(vec![(500, "boom".to_owned())]).await;
        let error = source(&server.base_url, true)
            .fetch()
            .await
            .expect_err("server error");
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn rate_limiting_is_transient_too() {
        let server = MockServer::start(vec![(429, "slow down".to_owned())]).await;
        let error = source(&server.base_url, true)
            .fetch()
            .await
            .expect_err("rate limited");
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_a_protocol_error() {
        let server = MockServer::with_json("<html>nope</html>").await;
        let error = source(&server.base_url, true)
            .fetch()
            .await
            .expect_err("not json");
        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[tokio::test]
    async fn without_a_token_nothing_is_sent_at_all() {
        let server = MockServer::with_json("{}").await;
        let error = source(&server.base_url, false)
            .fetch()
            .await
            .expect_err("no token");
        assert!(matches!(error, PortError::Unauthenticated { .. }));
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn approving_posts_a_review_to_the_pull_request() {
        let server = MockServer::start(vec![(200, "{}".to_owned())]).await;
        let source = source(&server.base_url, true);
        let pr = PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "https://github.test/acme/web.git".to_owned(),
        };

        source
            .approve(&pr, Some("looks good"))
            .await
            .expect("approve");

        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/repos/acme/web/pulls/142/reviews");
        assert_eq!(request.json()["event"], "APPROVE");
        assert_eq!(request.json()["body"], "looks good");
    }

    #[tokio::test]
    async fn commenting_posts_to_the_issue_thread_the_pull_request_shares() {
        let server = MockServer::start(vec![(200, "{}".to_owned())]).await;
        let source = source(&server.base_url, true);
        let pr = PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "https://github.test/acme/web.git".to_owned(),
        };

        source.comment(&pr, "one question").await.expect("comment");

        let request = server.request();
        assert_eq!(request.target, "/repos/acme/web/issues/142/comments");
        assert_eq!(request.json()["body"], "one question");
    }

    #[tokio::test]
    async fn a_refused_approval_is_reported_as_a_refusal() {
        let server = MockServer::start(vec![(
            422,
            r#"{"message":"Can not approve your own pull request"}"#.to_owned(),
        )])
        .await;
        let source = source(&server.base_url, true);
        let pr = PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "h".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "u".to_owned(),
        };

        let error = source.approve(&pr, None).await.expect_err("refused");

        match error {
            PortError::Refused { detail, .. } => {
                assert!(detail.contains("your own pull request"), "got {detail}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn github_dot_com_is_the_default_host() {
        let source = GitHubSource::new(
            SourceId::parse("github").expect("id"),
            GitHubSettings {
                query: "is:open".to_owned(),
                base_url: None,
                account: None,
            },
            HttpClient::new().expect("client"),
            auth(true),
        );
        assert_eq!(
            source.endpoint("/graphql"),
            "https://api.github.com/graphql"
        );
    }

    #[test]
    fn a_configured_host_is_used_as_given_without_a_trailing_slash() {
        let source = source("https://github.acme.test/api/v3/", true);
        assert_eq!(
            source.endpoint("/graphql"),
            "https://github.acme.test/api/v3/graphql"
        );
    }

    #[test]
    fn the_source_declares_itself_as_code_review() {
        let source = source("http://example.test", true);
        assert_eq!(source.category(), SourceCategory::CodeReview);
        assert_eq!(source.id().as_str(), "github");
    }
}
