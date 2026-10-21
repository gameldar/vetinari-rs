//! Slack as a messaging source.
//!
//! One feed row per conversation, carrying its most recent message: the
//! question a developer asks of an inbox is "who is waiting on me", not "show
//! me every line of chat", and a row per message would bury everything else.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::ports::{AttentionSource, ChatSource, PortError, PortResult};
use vetinari_core::domain::attention::{
    AttentionItem, ItemContext, ItemState, MessageRef, SourceCategory,
};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{Slug, SourceId};
use vetinari_core::domain::text::NonEmptyText;

use crate::HttpClient;

/// How a Slack source is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSettings {
    /// Conversations to watch; empty means the direct messages Slack lists.
    pub channels: Vec<String>,
    /// The credential account to use.
    pub account: Option<Slug>,
    /// An API base URL, for testing.
    pub base_url: Option<String>,
}

/// Messages from Slack.
pub struct SlackSource {
    id: SourceId,
    settings: SlackSettings,
    http: HttpClient,
    auth: Arc<AuthService>,
}

impl SlackSource {
    /// How many conversations to watch when none are configured.
    pub const MAX_CONVERSATIONS: usize = 10;

    /// Build a source.
    #[must_use]
    pub fn new(
        id: SourceId,
        settings: SlackSettings,
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

    /// The URL for an API method.
    #[must_use]
    pub fn endpoint(&self, method: &str) -> String {
        let base = self
            .settings
            .base_url
            .as_deref()
            .unwrap_or("https://slack.com")
            .trim_end_matches('/');
        format!("{base}/api/{method}")
    }

    async fn token(&self) -> PortResult<String> {
        self.auth
            .token(Provider::Slack, self.settings.account.clone())
            .await
            .map(|secret| secret.expose().to_owned())
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> PortResult<Value> {
        let token = self.token().await?;
        let response = request
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| PortError::unavailable("slack", error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable("slack", error.to_string()))?;

        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(
                "slack",
                format!("{status}: {}", body.trim()),
            ));
        }
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            PortError::protocol("slack", format!("could not read the reply: {error}"))
        })?;

        // Slack answers 200 and puts the failure in the body.
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            return Err(match code {
                "invalid_auth" | "not_authed" | "token_revoked" | "account_inactive" => {
                    PortError::Unauthenticated {
                        provider: Provider::Slack,
                    }
                }
                "ratelimited" => PortError::unavailable("slack", "rate limited"),
                other => PortError::Refused {
                    what: "slack".to_owned(),
                    detail: other.to_owned(),
                },
            });
        }
        Ok(value)
    }

    async fn conversations(&self) -> PortResult<Vec<String>> {
        if !self.settings.channels.is_empty() {
            return Ok(self.settings.channels.clone());
        }
        let listed = self
            .call(
                self.http
                    .inner()
                    .get(self.endpoint("conversations.list"))
                    .query(&[("types", "im,mpim"), ("limit", "20")]),
            )
            .await?;
        Ok(listed
            .get("channels")
            .and_then(Value::as_array)
            .map(|channels| {
                channels
                    .iter()
                    .filter_map(|channel| {
                        channel.get("id").and_then(Value::as_str).map(str::to_owned)
                    })
                    .take(Self::MAX_CONVERSATIONS)
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
impl AttentionSource for SlackSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        SourceCategory::Messaging
    }

    async fn fetch(&self) -> PortResult<Vec<AttentionItem>> {
        let mut items = Vec::new();
        for channel in self.conversations().await? {
            let history = self
                .call(
                    self.http
                        .inner()
                        .get(self.endpoint("conversations.history"))
                        .query(&[("channel", channel.as_str()), ("limit", "1")]),
                )
                .await?;
            if let Some(item) = parse_latest(&self.id, &channel, &history) {
                items.push(item);
            }
        }
        Ok(items)
    }
}

#[async_trait]
impl ChatSource for SlackSource {
    async fn reply(&self, message: &MessageRef, body: &str) -> PortResult<()> {
        let mut payload = json!({ "channel": message.channel, "text": body });
        if let Some(thread) = &message.thread {
            payload["thread_ts"] = Value::from(thread.clone());
        }
        self.call(
            self.http
                .inner()
                .post(self.endpoint("chat.postMessage"))
                .json(&payload),
        )
        .await?;
        Ok(())
    }
}

fn parse_latest(source: &SourceId, channel: &str, history: &Value) -> Option<AttentionItem> {
    let message = history.get("messages").and_then(Value::as_array)?.first()?;
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("(no text)");
    let author = message
        .get("user")
        .and_then(Value::as_str)
        .or_else(|| message.get("username").and_then(Value::as_str))
        .unwrap_or("someone");
    let timestamp = message.get("ts").and_then(Value::as_str)?;

    let title = NonEmptyText::parse(&format!("Message from {author}")).ok()?;
    let mut builder = AttentionItem::builder(source, channel, SourceCategory::Messaging, title)
        .state(ItemState::Unread)
        .author(author)
        .detail("Message", first_line(text))
        .detail("Channel", channel)
        .context(ItemContext::Message(MessageRef {
            channel: channel.to_owned(),
            thread: message
                .get("thread_ts")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }));
    if let Some(occurred) = parse_slack_timestamp(timestamp) {
        builder = builder.occurred_at(occurred);
    }
    Some(builder.build())
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    if line.chars().count() > 120 {
        let truncated: String = line.chars().take(117).collect();
        format!("{truncated}...")
    } else {
        line.to_owned()
    }
}

/// Slack timestamps are `seconds.microseconds` since the epoch, as a string.
fn parse_slack_timestamp(raw: &str) -> Option<OffsetDateTime> {
    let seconds: i64 = raw.split('.').next()?.parse().ok()?;
    OffsetDateTime::from_unix_timestamp(seconds).ok()
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

    fn source(base_url: &str, channels: &[&str]) -> SlackSource {
        let store = Arc::new(MemoryCredentialStore::default());
        store.seed(
            &CredentialKey::new(Provider::Slack, None),
            StoredCredential::api_token(Secret::new("xoxb-test")),
        );
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, NOW));
        SlackSource::new(
            SourceId::parse("slack").expect("id"),
            SlackSettings {
                channels: channels.iter().map(|c| (*c).to_owned()).collect(),
                account: None,
                base_url: Some(base_url.to_owned()),
            },
            HttpClient::new().expect("client"),
            Arc::new(AuthService::new(
                store,
                flow,
                Arc::new(FixedClock::new(NOW)),
            )),
        )
    }

    const HISTORY: &str = r#"{"ok":true,"messages":[
        {"user":"U123","text":"can you look at the auth parser?","ts":"1792054200.000100"}
    ]}"#;

    #[tokio::test]
    async fn a_configured_conversation_is_read_directly() {
        let server = MockServer::start(vec![(200, HISTORY.to_owned())]).await;

        let items = source(&server.base_url, &["D123"])
            .fetch()
            .await
            .expect("fetch");

        let request = server.request();
        assert!(request.target.starts_with("/api/conversations.history"));
        assert!(request.target.contains("channel=D123"));
        assert_eq!(request.header("authorization"), Some("Bearer xoxb-test"));
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn a_message_becomes_one_row_for_its_conversation() {
        let server = MockServer::start(vec![(200, HISTORY.to_owned())]).await;
        let items = source(&server.base_url, &["D123"])
            .fetch()
            .await
            .expect("fetch");

        let item = &items[0];
        assert_eq!(item.id().as_str(), "slack:D123");
        assert_eq!(item.title().as_str(), "Message from U123");
        assert_eq!(item.state(), ItemState::Unread);
        assert_eq!(item.category(), SourceCategory::Messaging);
        assert_eq!(item.occurred_at(), datetime!(2026-10-15 08:50:00 UTC));
        assert!(
            item.details()
                .iter()
                .any(|(_, value)| value.contains("auth parser"))
        );
        match item.context() {
            ItemContext::Message(message) => {
                assert_eq!(message.channel, "D123");
                assert_eq!(message.thread, None);
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_no_conversations_configured_slacks_own_list_is_used() {
        let server = MockServer::start(vec![
            (
                200,
                r#"{"ok":true,"channels":[{"id":"D1"},{"id":"D2"}]}"#.to_owned(),
            ),
            (200, HISTORY.to_owned()),
            (200, HISTORY.to_owned()),
        ])
        .await;

        let items = source(&server.base_url, &[]).fetch().await.expect("fetch");

        assert_eq!(items.len(), 2);
        let requests = server.requests();
        assert!(requests[0].target.starts_with("/api/conversations.list"));
        assert!(requests[0].target.contains("types=im%2Cmpim"));
    }

    #[tokio::test]
    async fn an_empty_conversation_contributes_no_row() {
        let server =
            MockServer::start(vec![(200, r#"{"ok":true,"messages":[]}"#.to_owned())]).await;
        assert!(
            source(&server.base_url, &["D123"])
                .fetch()
                .await
                .expect("fetch")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_threaded_message_remembers_its_thread_so_a_reply_lands_in_it() {
        let server = MockServer::start(vec![(
            200,
            r#"{"ok":true,"messages":[{"user":"U1","text":"hi","ts":"1792054200.000100","thread_ts":"1792054100.000100"}]}"#.to_owned(),
        )])
        .await;

        let items = source(&server.base_url, &["C1"])
            .fetch()
            .await
            .expect("fetch");

        match items[0].context() {
            ItemContext::Message(message) => {
                assert_eq!(message.thread.as_deref(), Some("1792054100.000100"));
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slacks_own_failure_shape_is_understood() {
        let server = MockServer::start(vec![(
            200,
            r#"{"ok":false,"error":"invalid_auth"}"#.to_owned(),
        )])
        .await;
        assert_eq!(
            source(&server.base_url, &["D1"])
                .fetch()
                .await
                .expect_err("auth"),
            PortError::Unauthenticated {
                provider: Provider::Slack
            }
        );

        let server = MockServer::start(vec![(
            200,
            r#"{"ok":false,"error":"ratelimited"}"#.to_owned(),
        )])
        .await;
        assert!(
            source(&server.base_url, &["D1"])
                .fetch()
                .await
                .expect_err("rate limited")
                .is_transient()
        );

        let server = MockServer::start(vec![(
            200,
            r#"{"ok":false,"error":"channel_not_found"}"#.to_owned(),
        )])
        .await;
        let error = source(&server.base_url, &["D1"])
            .fetch()
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("channel_not_found"));
    }

    #[tokio::test]
    async fn replying_posts_into_the_conversation() {
        let server = MockServer::start(vec![(200, r#"{"ok":true}"#.to_owned())]).await;
        let message = MessageRef {
            channel: "D123".to_owned(),
            thread: None,
        };

        source(&server.base_url, &[])
            .reply(&message, "on it")
            .await
            .expect("reply");

        let request = server.request();
        assert_eq!(request.target, "/api/chat.postMessage");
        assert_eq!(request.json()["channel"], "D123");
        assert_eq!(request.json()["text"], "on it");
        assert!(request.json().get("thread_ts").is_none());
    }

    #[tokio::test]
    async fn replying_in_a_thread_stays_in_the_thread() {
        let server = MockServer::start(vec![(200, r#"{"ok":true}"#.to_owned())]).await;
        let message = MessageRef {
            channel: "C1".to_owned(),
            thread: Some("1792054100.000100".to_owned()),
        };

        source(&server.base_url, &[])
            .reply(&message, "answered")
            .await
            .expect("reply");

        assert_eq!(server.request().json()["thread_ts"], "1792054100.000100");
    }

    #[test]
    fn a_long_message_is_trimmed_to_one_readable_line() {
        let long = "x".repeat(200);
        assert_eq!(first_line(&long).chars().count(), 120);
        assert!(first_line(&long).ends_with("..."));
        assert_eq!(first_line("one\ntwo"), "one");
    }

    #[test]
    fn slack_timestamps_are_understood() {
        assert_eq!(
            parse_slack_timestamp("1792054200.000100"),
            Some(datetime!(2026-10-15 08:50:00 UTC))
        );
        assert_eq!(parse_slack_timestamp("not a timestamp"), None);
    }

    #[test]
    fn slack_dot_com_is_the_default_host() {
        let source = SlackSource::new(
            SourceId::parse("slack").expect("id"),
            SlackSettings {
                channels: Vec::new(),
                account: None,
                base_url: None,
            },
            HttpClient::new().expect("client"),
            {
                let store = Arc::new(MemoryCredentialStore::default());
                let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, NOW));
                Arc::new(AuthService::new(
                    store,
                    flow,
                    Arc::new(FixedClock::new(NOW)),
                ))
            },
        );
        assert_eq!(
            source.endpoint("chat.postMessage"),
            "https://slack.com/api/chat.postMessage"
        );
    }
}
