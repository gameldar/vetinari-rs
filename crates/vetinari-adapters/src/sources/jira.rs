//! Jira as an issue tracker source.
//!
//! Jira authenticates with an email and API token over HTTP basic auth, which
//! is why the email is configuration rather than a secret: it identifies the
//! account the token belongs to.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::ports::{
    AttentionSource, IssueTrackerSource, PortError, PortResult,
};
use vetinari_core::domain::attention::{
    AttentionItem, IssueRef, ItemContext, ItemState, SourceCategory,
};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{Slug, SourceId};
use vetinari_core::domain::text::NonEmptyText;

use crate::HttpClient;

/// How a Jira source is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraSettings {
    /// The site URL, e.g. `https://acme.atlassian.net`.
    pub base_url: String,
    /// The account email the API token belongs to.
    pub email: String,
    /// The JQL that defines the inbox.
    pub jql: String,
}

/// Issues from Jira.
pub struct JiraSource {
    id: SourceId,
    settings: JiraSettings,
    http: HttpClient,
    auth: Arc<AuthService>,
}

impl JiraSource {
    /// How many issues to ask for.
    pub const PAGE_SIZE: u32 = 50;

    /// The fields the feed needs.
    const FIELDS: &'static [&'static str] = &[
        "summary",
        "status",
        "assignee",
        "duedate",
        "updated",
        "priority",
        "issuetype",
    ];

    /// Build a source.
    #[must_use]
    pub fn new(
        id: SourceId,
        settings: JiraSettings,
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
        format!("{}{path}", self.settings.base_url.trim_end_matches('/'))
    }

    async fn token(&self, account: Option<&Slug>) -> PortResult<String> {
        self.auth
            .token(Provider::Jira, account.cloned())
            .await
            .map(|secret| secret.expose().to_owned())
    }

    async fn send(
        &self,
        account: Option<&Slug>,
        request: reqwest::RequestBuilder,
    ) -> PortResult<Value> {
        let token = self.token(account).await?;
        let response = request
            .basic_auth(&self.settings.email, Some(token))
            .send()
            .await
            .map_err(|error| PortError::unavailable("jira", error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable("jira", error.to_string()))?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PortError::Unauthenticated {
                provider: Provider::Jira,
            });
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(
                "jira",
                format!("{status}: {}", body.trim()),
            ));
        }
        if !status.is_success() {
            return Err(PortError::Refused {
                what: "jira".to_owned(),
                detail: error_messages(&body).unwrap_or_else(|| format!("{status}")),
            });
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|error| {
            PortError::protocol("jira", format!("could not read the reply: {error}"))
        })
    }
}

fn error_messages(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let messages = value.get("errorMessages")?.as_array()?;
    let joined = messages
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    (!joined.is_empty()).then_some(joined)
}

#[async_trait]
impl AttentionSource for JiraSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        SourceCategory::IssueTracking
    }

    async fn fetch(&self, account: Option<&Slug>) -> PortResult<Vec<AttentionItem>> {
        let body = json!({
            "jql": self.settings.jql,
            "maxResults": Self::PAGE_SIZE,
            "fields": Self::FIELDS,
        });
        let reply = self
            .send(
                account,
                self.http
                    .inner()
                    .post(self.endpoint("/rest/api/3/search/jql"))
                    .json(&body),
            )
            .await?;

        let issues = reply
            .get("issues")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(issues
            .iter()
            .filter_map(|issue| parse_issue(&self.id, issue, &self.settings.base_url))
            .collect())
    }
}

#[async_trait]
impl IssueTrackerSource for JiraSource {
    async fn transition(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        status: &str,
    ) -> PortResult<()> {
        let available = self
            .send(
                account,
                self.http
                    .inner()
                    .get(self.endpoint(&format!("/rest/api/3/issue/{}/transitions", issue.key))),
            )
            .await?;

        let id = available
            .get("transitions")
            .and_then(Value::as_array)
            .and_then(|transitions| {
                transitions.iter().find_map(|transition| {
                    let name = transition.get("name").and_then(Value::as_str)?;
                    let to = transition.pointer("/to/name").and_then(Value::as_str);
                    (name.eq_ignore_ascii_case(status)
                        || to.is_some_and(|to| to.eq_ignore_ascii_case(status)))
                    .then(|| transition.get("id").and_then(Value::as_str))?
                })
            })
            .ok_or_else(|| PortError::Refused {
                what: "jira".to_owned(),
                detail: format!("{} cannot move to {status:?} from here", issue.key),
            })?
            .to_owned();

        self.send(
            account,
            self.http
                .inner()
                .post(self.endpoint(&format!("/rest/api/3/issue/{}/transitions", issue.key)))
                .json(&json!({ "transition": { "id": id } })),
        )
        .await?;
        Ok(())
    }

    async fn assign(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        assignee: &str,
    ) -> PortResult<()> {
        self.send(
            account,
            self.http
                .inner()
                .put(self.endpoint(&format!("/rest/api/3/issue/{}/assignee", issue.key)))
                .json(&json!({ "accountId": assignee })),
        )
        .await?;
        Ok(())
    }
}

fn parse_issue(source: &SourceId, issue: &Value, base_url: &str) -> Option<AttentionItem> {
    let key = issue.get("key").and_then(Value::as_str)?.to_owned();
    let title =
        NonEmptyText::parse(issue.pointer("/fields/summary").and_then(Value::as_str)?).ok()?;
    let status = issue
        .pointer("/fields/status/name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let category = issue
        .pointer("/fields/status/statusCategory/key")
        .and_then(Value::as_str)
        .unwrap_or("new");

    let state = match category {
        "done" => ItemState::Resolved,
        "indeterminate" => ItemState::InProgress,
        _ => ItemState::Assigned,
    };

    let mut builder = AttentionItem::builder(source, &key, SourceCategory::IssueTracking, title)
        .state(state)
        .url(format!("{}/browse/{key}", base_url.trim_end_matches('/')))
        .detail("Key", key.clone())
        .detail("Status", status.clone())
        .context(ItemContext::Issue(IssueRef {
            key: key.clone(),
            status,
            transitions: Vec::new(),
        }));

    if let Some(updated) = parse_jira_time(issue.pointer("/fields/updated").and_then(Value::as_str))
    {
        builder = builder.occurred_at(updated);
    }
    if let Some(due) = issue
        .pointer("/fields/duedate")
        .and_then(Value::as_str)
        .and_then(parse_jira_date)
    {
        builder = builder.due_at(due);
    }
    if let Some(assignee) = issue
        .pointer("/fields/assignee/displayName")
        .and_then(Value::as_str)
    {
        builder = builder.author(assignee);
    }
    if let Some(priority) = issue
        .pointer("/fields/priority/name")
        .and_then(Value::as_str)
    {
        builder = builder.detail("Priority", priority);
    }
    Some(builder.build())
}

/// Jira stamps times as `2026-10-21T08:30:00.000+0000`, which is RFC 3339
/// apart from the missing colon in the offset.
fn parse_jira_time(raw: Option<&str>) -> Option<OffsetDateTime> {
    let raw = raw?;
    let rfc3339 = &time::format_description::well_known::Rfc3339;
    if let Ok(parsed) = OffsetDateTime::parse(raw, rfc3339) {
        return Some(parsed);
    }
    let fixed = if raw.len() > 5 {
        let (head, tail) = raw.split_at(raw.len() - 5);
        if tail.starts_with('+') || tail.starts_with('-') {
            format!("{head}{}:{}", &tail[..3], &tail[3..])
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };
    OffsetDateTime::parse(&fixed, rfc3339).ok()
}

fn parse_jira_date(raw: &str) -> Option<OffsetDateTime> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let date = time::Date::parse(raw, format).ok()?;
    Some(date.with_hms(23, 59, 0).ok()?.assume_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockServer;
    use time::macros::datetime;
    use vetinari_core::application::ports::DeviceCodeFlow;
    use vetinari_core::domain::credentials::{CredentialKey, Secret, StoredCredential};
    use vetinari_core::testing::{FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore};

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source(base_url: &str) -> JiraSource {
        let store = Arc::new(MemoryCredentialStore::default());
        store.seed(
            &CredentialKey::new(Provider::Jira, None),
            StoredCredential::api_token(Secret::new("jira-token")),
        );
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, NOW));
        JiraSource::new(
            SourceId::parse("jira").expect("id"),
            JiraSettings {
                base_url: base_url.to_owned(),
                email: "dev@acme.test".to_owned(),
                jql: "assignee = currentUser()".to_owned(),
            },
            HttpClient::new().expect("client"),
            Arc::new(AuthService::new(
                store,
                flow,
                Arc::new(FixedClock::new(NOW)),
            )),
        )
    }

    fn issue_payload(status: &str, category: &str, duedate: &str) -> String {
        format!(
            r#"{{"issues":[{{
              "key":"DEV-891",
              "fields":{{
                "summary":"Review the orchestrator spec",
                "status":{{"name":"{status}","statusCategory":{{"key":"{category}"}}}},
                "assignee":{{"displayName":"Sam Vimes"}},
                "duedate":{duedate},
                "updated":"2026-10-21T08:30:00.000+0000",
                "priority":{{"name":"High"}}
              }}
            }}]}}"#
        )
    }

    #[tokio::test]
    async fn the_jql_is_posted_with_basic_auth_and_the_fields_the_feed_needs() {
        let server = MockServer::with_json(&issue_payload("To Do", "new", "null")).await;
        source(&server.base_url).fetch(None).await.expect("fetch");

        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rest/api/3/search/jql");
        assert!(
            request
                .header("authorization")
                .expect("auth")
                .starts_with("Basic "),
            "Jira wants the email and token as basic auth"
        );
        assert_eq!(request.json()["jql"], "assignee = currentUser()");
        assert!(
            request.json()["fields"]
                .as_array()
                .expect("fields")
                .contains(&serde_json::Value::from("duedate"))
        );
    }

    #[tokio::test]
    async fn an_issue_becomes_an_item_with_its_key_status_and_due_date() {
        let server = MockServer::with_json(&issue_payload("To Do", "new", "\"2026-10-24\"")).await;
        let items = source(&server.base_url).fetch(None).await.expect("fetch");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id().as_str(), "jira:DEV-891");
        assert_eq!(item.title().as_str(), "Review the orchestrator spec");
        assert_eq!(item.state(), ItemState::Assigned);
        assert_eq!(item.author(), Some("Sam Vimes"));
        assert_eq!(item.occurred_at(), datetime!(2026-10-21 08:30:00 UTC));
        assert_eq!(item.due_at(), Some(datetime!(2026-10-24 23:59:00 UTC)));
        assert_eq!(
            item.url(),
            Some(format!("{}/browse/DEV-891", server.base_url).as_str())
        );
        match item.context() {
            ItemContext::Issue(issue) => {
                assert_eq!(issue.key, "DEV-891");
                assert_eq!(issue.status, "To Do");
            }
            other => panic!("expected an issue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn work_in_progress_and_finished_work_are_distinguished() {
        let server =
            MockServer::with_json(&issue_payload("In Progress", "indeterminate", "null")).await;
        assert_eq!(
            source(&server.base_url).fetch(None).await.expect("fetch")[0].state(),
            ItemState::InProgress
        );

        let server = MockServer::with_json(&issue_payload("Done", "done", "null")).await;
        assert_eq!(
            source(&server.base_url).fetch(None).await.expect("fetch")[0].state(),
            ItemState::Resolved
        );
    }

    #[tokio::test]
    async fn an_issue_without_the_fields_we_need_is_skipped() {
        let server = MockServer::with_json(
            r#"{"issues":[{"key":"DEV-1"},{"fields":{"summary":"no key"}},
                {"key":"DEV-2","fields":{"summary":"fine","status":{"name":"To Do","statusCategory":{"key":"new"}},"updated":"2026-10-21T08:30:00.000+0000"}}]}"#,
        )
        .await;

        let items = source(&server.base_url).fetch(None).await.expect("fetch");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title().as_str(), "fine");
    }

    #[tokio::test]
    async fn a_jira_error_names_what_it_objected_to() {
        let server = MockServer::start(vec![(
            400,
            r#"{"errorMessages":["The JQL query is invalid"]}"#.to_owned(),
        )])
        .await;

        let error = source(&server.base_url)
            .fetch(None)
            .await
            .expect_err("bad jql");

        match error {
            PortError::Refused { detail, .. } => assert!(detail.contains("JQL query is invalid")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejected_token_asks_for_authentication() {
        let server = MockServer::start(vec![(401, String::new())]).await;
        assert_eq!(
            source(&server.base_url).fetch(None).await.expect_err("401"),
            PortError::Unauthenticated {
                provider: Provider::Jira
            }
        );
    }

    #[tokio::test]
    async fn a_status_change_looks_up_the_transition_jira_will_accept() {
        let server = MockServer::start(vec![
            (
                200,
                r#"{"transitions":[{"id":"11","name":"Start work","to":{"name":"In Progress"}},
                    {"id":"21","name":"Done","to":{"name":"Done"}}]}"#
                    .to_owned(),
            ),
            (204, String::new()),
        ])
        .await;
        let source = source(&server.base_url);
        let issue = IssueRef {
            key: "DEV-891".to_owned(),
            status: "To Do".to_owned(),
            transitions: Vec::new(),
        };

        source
            .transition(None, &issue, "In Progress")
            .await
            .expect("transition");

        let requests = server.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/rest/api/3/issue/DEV-891/transitions");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].json()["transition"]["id"], "11");
    }

    #[tokio::test]
    async fn a_status_jira_will_not_accept_is_reported_rather_than_attempted() {
        let server = MockServer::start(vec![(
            200,
            r#"{"transitions":[{"id":"21","name":"Done","to":{"name":"Done"}}]}"#.to_owned(),
        )])
        .await;
        let issue = IssueRef {
            key: "DEV-891".to_owned(),
            status: "To Do".to_owned(),
            transitions: Vec::new(),
        };

        let error = source(&server.base_url)
            .transition(None, &issue, "In Progress")
            .await
            .expect_err("no such transition");

        assert!(error.to_string().contains("In Progress"));
        assert_eq!(server.requests().len(), 1, "nothing is posted");
    }

    #[tokio::test]
    async fn assigning_puts_the_account_on_the_issue() {
        let server = MockServer::start(vec![(204, String::new())]).await;
        let issue = IssueRef {
            key: "DEV-891".to_owned(),
            status: "To Do".to_owned(),
            transitions: Vec::new(),
        };

        source(&server.base_url)
            .assign(None, &issue, "5b10a2844c20165700ede21g")
            .await
            .expect("assign");

        let request = server.request();
        assert_eq!(request.method, "PUT");
        assert_eq!(request.target, "/rest/api/3/issue/DEV-891/assignee");
        assert_eq!(request.json()["accountId"], "5b10a2844c20165700ede21g");
    }

    #[test]
    fn jiras_offset_format_is_understood_as_well_as_rfc_3339() {
        assert_eq!(
            parse_jira_time(Some("2026-10-21T08:30:00.000+0000")),
            Some(datetime!(2026-10-21 08:30:00 UTC))
        );
        assert_eq!(
            parse_jira_time(Some("2026-10-21T08:30:00Z")),
            Some(datetime!(2026-10-21 08:30:00 UTC))
        );
        assert_eq!(parse_jira_time(Some("not a time")), None);
        assert_eq!(parse_jira_time(None), None);
    }

    #[test]
    fn a_due_date_is_taken_as_the_end_of_that_day() {
        assert_eq!(
            parse_jira_date("2026-10-24"),
            Some(datetime!(2026-10-24 23:59:00 UTC))
        );
        assert_eq!(parse_jira_date("whenever"), None);
    }
}
