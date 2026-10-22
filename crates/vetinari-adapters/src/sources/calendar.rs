//! Google Calendar as a schedule source.
//!
//! Events are asked for already expanded (`singleEvents`), so a recurring
//! standup arrives as the instance that is actually about to start rather than
//! as a rule Vetinari would have to interpret.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::ports::{
    AttentionSource, Clock, PortError, PortResult, ScheduleSource,
};
use vetinari_core::domain::attention::{
    AttentionItem, EventRef, ItemContext, ItemState, SourceCategory,
};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{Slug, SourceId};
use vetinari_core::domain::text::NonEmptyText;

use crate::HttpClient;

/// How a Google Calendar source is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleCalendarSettings {
    /// Which calendar to read.
    pub calendar_id: String,
    /// How far ahead to look.
    pub lookahead_hours: u16,
    /// An API base URL, for testing.
    pub base_url: Option<String>,
}

/// Events from Google Calendar.
pub struct GoogleCalendarSource {
    id: SourceId,
    settings: GoogleCalendarSettings,
    http: HttpClient,
    auth: Arc<AuthService>,
    clock: Arc<dyn Clock>,
}

impl GoogleCalendarSource {
    /// How many events to ask for.
    pub const PAGE_SIZE: u32 = 25;

    /// Build a source.
    #[must_use]
    pub fn new(
        id: SourceId,
        settings: GoogleCalendarSettings,
        http: HttpClient,
        auth: Arc<AuthService>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            id,
            settings,
            http,
            auth,
            clock,
        }
    }

    /// The URL for the configured calendar's events.
    #[must_use]
    pub fn events_url(&self) -> String {
        let base = self
            .settings
            .base_url
            .as_deref()
            .unwrap_or("https://www.googleapis.com")
            .trim_end_matches('/');
        format!(
            "{base}/calendar/v3/calendars/{}/events",
            urlencode(&self.settings.calendar_id)
        )
    }

    async fn events(
        &self,
        account: Option<&Slug>,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> PortResult<Vec<AttentionItem>> {
        let token = self
            .auth
            .token(Provider::GoogleCalendar, account.cloned())
            .await?;
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        let time_min = from
            .format(rfc3339)
            .map_err(|error| PortError::protocol("google calendar", error.to_string()))?;
        let time_max = to
            .format(rfc3339)
            .map_err(|error| PortError::protocol("google calendar", error.to_string()))?;

        let response = self
            .http
            .inner()
            .get(self.events_url())
            .bearer_auth(token.expose())
            .query(&[
                ("timeMin", time_min.as_str()),
                ("timeMax", time_max.as_str()),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("maxResults", "25"),
            ])
            .send()
            .await
            .map_err(|error| PortError::unavailable("google calendar", error.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable("google calendar", error.to_string()))?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PortError::Unauthenticated {
                provider: Provider::GoogleCalendar,
            });
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PortError::unavailable(
                "google calendar",
                format!("{status}: {}", body.trim()),
            ));
        }
        if !status.is_success() {
            return Err(PortError::Refused {
                what: "google calendar".to_owned(),
                detail: serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| format!("{status}")),
            });
        }

        let value: Value = serde_json::from_str(&body).map_err(|error| {
            PortError::protocol(
                "google calendar",
                format!("could not read the reply: {error}"),
            )
        })?;
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|event| parse_event(&self.id, event))
            .collect())
    }
}

#[async_trait]
impl AttentionSource for GoogleCalendarSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        SourceCategory::Calendar
    }

    async fn fetch(&self, account: Option<&Slug>) -> PortResult<Vec<AttentionItem>> {
        let now = self.clock.now();
        let lookahead = Duration::hours(i64::from(self.settings.lookahead_hours.max(1)));
        // A meeting that has already started still matters, so the window
        // opens slightly in the past.
        self.events(account, now - Duration::minutes(30), now + lookahead)
            .await
    }
}

#[async_trait]
impl ScheduleSource for GoogleCalendarSource {
    async fn agenda(
        &self,
        account: Option<&Slug>,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> PortResult<Vec<AttentionItem>> {
        self.events(account, from, to).await
    }
}

fn parse_event(source: &SourceId, event: &Value) -> Option<AttentionItem> {
    let id = event.get("id").and_then(Value::as_str)?;
    let title = NonEmptyText::parse(
        event
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("(no title)"),
    )
    .ok()?;
    let starts_at = parse_moment(event.get("start"))?;
    let ends_at = parse_moment(event.get("end"));

    let attendees: Vec<String> = event
        .get("attendees")
        .and_then(Value::as_array)
        .map(|attendees| {
            attendees
                .iter()
                .filter_map(|attendee| {
                    attendee
                        .get("displayName")
                        .and_then(Value::as_str)
                        .or_else(|| attendee.get("email").and_then(Value::as_str))
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();

    let location = event
        .get("hangoutLink")
        .and_then(Value::as_str)
        .or_else(|| event.get("location").and_then(Value::as_str))
        .map(str::to_owned);

    let mut builder = AttentionItem::builder(source, id, SourceCategory::Calendar, title)
        .state(ItemState::Scheduled { starts_at })
        .occurred_at(starts_at)
        .detail("Starts", starts_at.to_string())
        .context(ItemContext::Event(EventRef {
            ends_at,
            attendees: attendees.clone(),
            location: location.clone(),
        }));

    if !attendees.is_empty() {
        builder = builder.detail("Attendees", attendees.join(", "));
    }
    if let Some(location) = location {
        builder = builder.detail("Where", location);
    }
    if let Some(url) = event.get("htmlLink").and_then(Value::as_str) {
        builder = builder.url(url);
    }
    if let Some(organiser) = event
        .pointer("/organizer/displayName")
        .or_else(|| event.pointer("/organizer/email"))
        .and_then(Value::as_str)
    {
        builder = builder.author(organiser);
    }
    Some(builder.build())
}

/// A Google event start or end is either a timestamp or, for an all-day
/// event, a bare date.
fn parse_moment(moment: Option<&Value>) -> Option<OffsetDateTime> {
    let moment = moment?;
    if let Some(stamp) = moment.get("dateTime").and_then(Value::as_str)
        && let Ok(parsed) =
            OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
    {
        return Some(parsed);
    }
    let date = moment.get("date").and_then(Value::as_str)?;
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Some(
        time::Date::parse(date, format)
            .ok()?
            .midnight()
            .assume_utc(),
    )
}

fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => ch.to_string(),
            other => other
                .to_string()
                .into_bytes()
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect(),
        })
        .collect()
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

    fn source(base_url: &str) -> GoogleCalendarSource {
        let store = Arc::new(MemoryCredentialStore::default());
        store.seed(
            &CredentialKey::new(Provider::GoogleCalendar, None),
            StoredCredential::api_token(Secret::new("ya29.token")),
        );
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, NOW));
        let clock = Arc::new(FixedClock::new(NOW));
        GoogleCalendarSource::new(
            SourceId::parse("calendar").expect("id"),
            GoogleCalendarSettings {
                calendar_id: "primary".to_owned(),
                lookahead_hours: 12,
                base_url: Some(base_url.to_owned()),
            },
            HttpClient::new().expect("client"),
            Arc::new(AuthService::new(store, flow, clock.clone())),
            clock,
        )
    }

    const EVENTS: &str = r#"{"items":[{
        "id":"evt-1",
        "summary":"Sprint planning",
        "htmlLink":"https://calendar.test/evt-1",
        "start":{"dateTime":"2026-10-21T09:15:00Z"},
        "end":{"dateTime":"2026-10-21T10:00:00Z"},
        "organizer":{"displayName":"Sarah"},
        "attendees":[{"displayName":"Sarah"},{"email":"dev@acme.test"}],
        "hangoutLink":"https://meet.test/abc"
    }]}"#;

    #[tokio::test]
    async fn the_window_asked_for_opens_slightly_in_the_past() {
        let server = MockServer::with_json(EVENTS).await;
        source(&server.base_url).fetch(None).await.expect("fetch");

        let target = server.request().target;
        assert!(target.starts_with("/calendar/v3/calendars/primary/events"));
        assert!(
            target.contains("timeMin=2026-10-21T08%3A30%3A00Z"),
            "got {target}"
        );
        assert!(
            target.contains("timeMax=2026-10-21T21%3A00%3A00Z"),
            "got {target}"
        );
        assert!(target.contains("singleEvents=true"));
        assert!(target.contains("orderBy=startTime"));
    }

    #[tokio::test]
    async fn the_access_token_is_sent_as_a_bearer_token() {
        let server = MockServer::with_json(EVENTS).await;
        source(&server.base_url).fetch(None).await.expect("fetch");
        assert_eq!(
            server.request().header("authorization"),
            Some("Bearer ya29.token")
        );
    }

    #[tokio::test]
    async fn an_event_becomes_a_scheduled_item_with_its_attendees_and_link() {
        let server = MockServer::with_json(EVENTS).await;
        let items = source(&server.base_url).fetch(None).await.expect("fetch");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id().as_str(), "calendar:evt-1");
        assert_eq!(item.title().as_str(), "Sprint planning");
        assert_eq!(
            item.state(),
            ItemState::Scheduled {
                starts_at: datetime!(2026-10-21 09:15:00 UTC)
            }
        );
        assert_eq!(item.author(), Some("Sarah"));
        assert_eq!(item.url(), Some("https://calendar.test/evt-1"));
        assert_eq!(item.relative_time(NOW).as_deref(), Some("in 15m"));

        match item.context() {
            ItemContext::Event(event) => {
                assert_eq!(event.ends_at, Some(datetime!(2026-10-21 10:00:00 UTC)));
                assert_eq!(event.attendees, vec!["Sarah", "dev@acme.test"]);
                assert_eq!(event.location.as_deref(), Some("https://meet.test/abc"));
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_all_day_event_is_taken_as_starting_at_midnight() {
        let server = MockServer::with_json(
            r#"{"items":[{"id":"evt-2","summary":"Conference","start":{"date":"2026-10-22"},"end":{"date":"2026-10-23"}}]}"#,
        )
        .await;

        let items = source(&server.base_url).fetch(None).await.expect("fetch");

        assert_eq!(
            items[0].state(),
            ItemState::Scheduled {
                starts_at: datetime!(2026-10-22 00:00:00 UTC)
            }
        );
    }

    #[tokio::test]
    async fn an_event_without_a_start_is_skipped() {
        let server = MockServer::with_json(
            r#"{"items":[{"id":"evt-3","summary":"Broken"},{"id":"evt-4","summary":"Fine","start":{"date":"2026-10-22"}}]}"#,
        )
        .await;

        let items = source(&server.base_url).fetch(None).await.expect("fetch");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title().as_str(), "Fine");
    }

    #[tokio::test]
    async fn an_event_with_no_title_still_appears() {
        let server =
            MockServer::with_json(r#"{"items":[{"id":"e","start":{"date":"2026-10-22"}}]}"#).await;
        let items = source(&server.base_url).fetch(None).await.expect("fetch");
        assert_eq!(items[0].title().as_str(), "(no title)");
    }

    #[tokio::test]
    async fn an_explicit_agenda_window_is_used_as_given() {
        let server = MockServer::with_json(EVENTS).await;
        source(&server.base_url)
            .agenda(
                None,
                datetime!(2026-10-22 00:00:00 UTC),
                datetime!(2026-10-23 00:00:00 UTC),
            )
            .await
            .expect("agenda");

        let target = server.request().target;
        assert!(
            target.contains("timeMin=2026-10-22T00%3A00%3A00Z"),
            "got {target}"
        );
        assert!(
            target.contains("timeMax=2026-10-23T00%3A00%3A00Z"),
            "got {target}"
        );
    }

    #[tokio::test]
    async fn an_expired_credential_asks_for_authentication() {
        let server = MockServer::start(vec![(401, String::new())]).await;
        assert_eq!(
            source(&server.base_url).fetch(None).await.expect_err("401"),
            PortError::Unauthenticated {
                provider: Provider::GoogleCalendar
            }
        );
    }

    #[tokio::test]
    async fn googles_error_message_is_passed_through() {
        let server = MockServer::start(vec![(
            403,
            r#"{"error":{"message":"Calendar usage limits exceeded"}}"#.to_owned(),
        )])
        .await;

        let error = source(&server.base_url)
            .fetch(None)
            .await
            .expect_err("refused");

        assert!(error.to_string().contains("usage limits"), "got {error}");
    }

    #[test]
    fn a_calendar_id_with_an_at_sign_is_escaped_into_the_path() {
        let source = source("http://example.test");
        let with_email = GoogleCalendarSource::new(
            SourceId::parse("calendar").expect("id"),
            GoogleCalendarSettings {
                calendar_id: "dev@acme.test".to_owned(),
                ..GoogleCalendarSettings {
                    calendar_id: String::new(),
                    lookahead_hours: 12,
                    base_url: Some("http://example.test".to_owned()),
                }
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
            Arc::new(FixedClock::new(NOW)),
        );

        assert!(source.events_url().ends_with("/calendars/primary/events"));
        assert!(
            with_email
                .events_url()
                .ends_with("/calendars/dev%40acme.test/events")
        );
    }

    #[test]
    fn the_source_declares_itself_as_a_calendar() {
        let source = source("http://example.test");
        assert_eq!(source.category(), SourceCategory::Calendar);
    }
}
