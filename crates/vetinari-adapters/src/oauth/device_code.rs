//! RFC 8628 device authorisation.
//!
//! Chosen over a redirect flow because Vetinari lives in a terminal: it can
//! show a code and a URL, but it cannot usefully be a web server waiting for
//! a browser to come back. The provider's own endpoints come from the domain's
//! auth spec, so adding a provider does not mean touching this file; an OIDC
//! issuer's come from its discovery document (`oidc.rs`), and both make the
//! same requests through [`Exchange`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use time::Duration;
use vetinari_core::application::config::OAuthClientConfig;
use vetinari_core::application::ports::{
    Clock, DeviceCodeChallenge, DeviceCodeFlow, DeviceCodePoll, PortError, PortResult,
};
use vetinari_core::domain::credentials::{AuthMethod, Provider, Secret, StoredCredential};

use crate::HttpClient;

/// The requests of a device-code flow, at whichever endpoints.
pub(crate) struct Exchange {
    http: HttpClient,
    clock: Arc<dyn Clock>,
}

impl Exchange {
    pub(crate) const fn new(http: HttpClient, clock: Arc<dyn Clock>) -> Self {
        Self { http, clock }
    }

    pub(crate) fn http(&self) -> &HttpClient {
        &self.http
    }

    /// Ask for a device and user code. `fallback_page` is the verification
    /// page to show when the reply names none.
    pub(crate) async fn challenge(
        &self,
        provider: Provider,
        url: &str,
        form: &[(&str, &str)],
        fallback_page: Option<&str>,
    ) -> PortResult<DeviceCodeChallenge> {
        let reply = self.post_form(url, form).await?;

        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(PortError::Refused {
                what: format!("{} device authorisation", provider.name()),
                detail: error.to_owned(),
            });
        }

        let device_code = reply
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PortError::protocol("oauth", "the reply carried no device code".to_owned())
            })?
            .to_owned();
        let user_code = reply
            .get("user_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PortError::protocol("oauth", "the reply carried no user code".to_owned())
            })?
            .to_owned();
        let verification_uri = reply
            .get("verification_url")
            .or_else(|| reply.get("verification_uri"))
            .and_then(Value::as_str)
            .or(fallback_page)
            .ok_or_else(|| {
                PortError::protocol("oauth", "the reply named no verification page".to_owned())
            })?
            .to_owned();
        let interval_secs = reply
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1);
        let expires_in = reply
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(600);

        Ok(DeviceCodeChallenge {
            user_code,
            verification_uri,
            device_code,
            interval_secs,
            expires_at: self.clock.now() + Duration::seconds(expires_in),
        })
    }

    /// Poll the token endpoint once.
    pub(crate) async fn poll(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> PortResult<DeviceCodePoll> {
        let reply = self.post_form(url, form).await?;

        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Ok(match error {
                "authorization_pending" | "slow_down" => DeviceCodePoll::Pending,
                "access_denied" => DeviceCodePoll::Rejected {
                    reason: "the request was declined".to_owned(),
                },
                "expired_token" => DeviceCodePoll::Rejected {
                    reason: "the code expired before it was approved".to_owned(),
                },
                other => DeviceCodePoll::Rejected {
                    reason: other.to_owned(),
                },
            });
        }

        Ok(DeviceCodePoll::Granted(self.credential_from(&reply)?))
    }

    /// Exchange `refresh` for a fresh credential.
    pub(crate) async fn refresh(
        &self,
        provider: Provider,
        url: &str,
        form: &[(&str, &str)],
        refresh: &Secret,
    ) -> PortResult<StoredCredential> {
        let reply = self.post_form(url, form).await?;
        if reply.get("error").and_then(Value::as_str).is_some() {
            return Err(PortError::Unauthenticated { provider });
        }

        let refreshed = self.credential_from(&reply)?;
        // Providers often omit the refresh token on a refresh; keep the one
        // that still works rather than losing the ability to renew again.
        Ok(match refreshed.refresh_token() {
            Some(_) => refreshed,
            None => StoredCredential::oauth(
                refreshed.token().clone(),
                Some(refresh.clone()),
                refreshed.expires_at(),
            ),
        })
    }

    async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> PortResult<Value> {
        let response = self
            .http
            .inner()
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .map_err(|error| PortError::unavailable("oauth", error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| PortError::unavailable("oauth", error.to_string()))?;

        if status.is_server_error() {
            return Err(PortError::unavailable(
                "oauth",
                format!("{status}: {}", body.trim()),
            ));
        }
        if body.trim().is_empty() {
            return Err(PortError::protocol(
                "oauth",
                "the reply was empty".to_owned(),
            ));
        }
        serde_json::from_str(&body).map_err(|error| {
            PortError::protocol("oauth", format!("could not read the reply: {error}"))
        })
    }

    fn credential_from(&self, token: &Value) -> PortResult<StoredCredential> {
        let access = token
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PortError::protocol("oauth", "the reply carried no access token".to_owned())
            })?;
        let refresh = token
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(Secret::new);
        let expires_at = token
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|seconds| self.clock.now() + Duration::seconds(seconds));
        Ok(StoredCredential::oauth(
            Secret::new(access),
            refresh,
            expires_at,
        ))
    }
}

/// Runs device-code flows over HTTP.
pub struct HttpDeviceCodeFlow {
    exchange: Exchange,
    clients: BTreeMap<Provider, OAuthClientConfig>,
    base_url_override: Option<String>,
}

impl HttpDeviceCodeFlow {
    /// Build a flow with the configured OAuth clients.
    #[must_use]
    pub fn new(http: HttpClient, clients: Vec<OAuthClientConfig>, clock: Arc<dyn Clock>) -> Self {
        Self {
            exchange: Exchange::new(http, clock),
            clients: clients
                .into_iter()
                .map(|client| (client.provider, client))
                .collect(),
            base_url_override: None,
        }
    }

    /// Send every request to one host instead of the provider's, for testing.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    fn client(&self, provider: Provider) -> PortResult<&OAuthClientConfig> {
        self.clients.get(&provider).ok_or_else(|| {
            PortError::not_configured(
                provider.name(),
                format!(
                    "no OAuth client is configured; add [oauth.{}] with a client_id",
                    provider.name()
                ),
            )
        })
    }

    fn endpoints(provider: Provider) -> PortResult<(&'static str, &'static str, Vec<String>)> {
        match provider.auth_spec().method {
            AuthMethod::OAuthDeviceCode {
                device_authorization_url,
                token_url,
                scopes,
                ..
            } => Ok((
                device_authorization_url,
                token_url,
                scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            )),
            AuthMethod::ApiToken { .. } => Err(PortError::unsupported(
                provider.name(),
                "device-code authentication; paste an API token instead",
            )),
            AuthMethod::OidcDeviceCode => Err(PortError::unsupported(
                provider.name(),
                "signing in here; vetinarid holds this credential",
            )),
        }
    }

    /// Rewrite a provider URL onto the override host, keeping its path.
    fn url(&self, provider_url: &str) -> String {
        match &self.base_url_override {
            None => provider_url.to_owned(),
            Some(base) => {
                let path = provider_url
                    .split_once("://")
                    .and_then(|(_, rest)| rest.split_once('/'))
                    .map_or_else(String::new, |(_, path)| format!("/{path}"));
                format!("{}{path}", base.trim_end_matches('/'))
            }
        }
    }
}

#[async_trait]
impl DeviceCodeFlow for HttpDeviceCodeFlow {
    async fn start(&self, provider: Provider) -> PortResult<DeviceCodeChallenge> {
        let client = self.client(provider)?;
        let (device_url, _, scopes) = Self::endpoints(provider)?;
        let scope = scopes.join(" ");
        self.exchange
            .challenge(
                provider,
                &self.url(device_url),
                &[("client_id", client.client_id.as_str()), ("scope", &scope)],
                Some("https://www.google.com/device"),
            )
            .await
    }

    async fn poll(
        &self,
        provider: Provider,
        challenge: &DeviceCodeChallenge,
    ) -> PortResult<DeviceCodePoll> {
        let client = self.client(provider)?;
        let (_, token_url, _) = Self::endpoints(provider)?;

        let mut form = vec![
            ("client_id", client.client_id.as_str()),
            ("device_code", challenge.device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        if let Some(secret) = &client.client_secret {
            form.push(("client_secret", secret.as_str()));
        }
        self.exchange.poll(&self.url(token_url), &form).await
    }

    async fn refresh(
        &self,
        provider: Provider,
        credential: &StoredCredential,
    ) -> PortResult<StoredCredential> {
        let client = self.client(provider)?;
        let (_, token_url, _) = Self::endpoints(provider)?;
        let refresh = credential
            .refresh_token()
            .ok_or(PortError::Unauthenticated { provider })?;

        let mut form = vec![
            ("client_id", client.client_id.as_str()),
            ("refresh_token", refresh.expose()),
            ("grant_type", "refresh_token"),
        ];
        if let Some(secret) = &client.client_secret {
            form.push(("client_secret", secret.as_str()));
        }
        self.exchange
            .refresh(provider, &self.url(token_url), &form, refresh)
            .await
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::testing::MockServer;
    use time::macros::datetime;
    use vetinari_core::testing::FixedClock;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn flow(base_url: &str, with_client: bool) -> HttpDeviceCodeFlow {
        let clients = if with_client {
            vec![OAuthClientConfig {
                provider: Provider::GoogleCalendar,
                client_id: "123.apps.googleusercontent.com".to_owned(),
                client_secret: Some("GOCSPX-secret".to_owned()),
            }]
        } else {
            Vec::new()
        };
        HttpDeviceCodeFlow::new(
            HttpClient::new().expect("client"),
            clients,
            Arc::new(FixedClock::new(NOW)),
        )
        .with_base_url(base_url)
    }

    fn challenge() -> DeviceCodeChallenge {
        DeviceCodeChallenge {
            user_code: "WXYZ-1234".to_owned(),
            verification_uri: "https://google.test/device".to_owned(),
            device_code: "dev-code".to_owned(),
            interval_secs: 5,
            expires_at: NOW + Duration::minutes(10),
        }
    }

    #[tokio::test]
    async fn starting_a_flow_asks_for_the_scopes_the_provider_needs() {
        let server = MockServer::with_json(
            r#"{"device_code":"dev-code","user_code":"WXYZ-1234","verification_url":"https://google.test/device","interval":5,"expires_in":1800}"#,
        )
        .await;

        let challenge = flow(&server.base_url, true)
            .start(Provider::GoogleCalendar)
            .await
            .expect("start");

        let request = server.request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/device/code");
        assert!(
            request
                .body
                .contains("client_id=123.apps.googleusercontent.com"),
            "got {}",
            request.body
        );
        assert!(
            request.body.contains("calendar.readonly"),
            "got {}",
            request.body
        );

        assert_eq!(challenge.user_code, "WXYZ-1234");
        assert_eq!(challenge.verification_uri, "https://google.test/device");
        assert_eq!(challenge.interval_secs, 5);
        assert_eq!(challenge.expires_at, NOW + Duration::minutes(30));
    }

    #[tokio::test]
    async fn a_provider_with_no_configured_client_says_what_to_add() {
        let server = MockServer::with_json("{}").await;
        let error = flow(&server.base_url, false)
            .start(Provider::GoogleCalendar)
            .await
            .expect_err("no client");

        assert!(
            error.to_string().contains("[oauth.google-calendar]"),
            "got {error}"
        );
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn a_provider_that_uses_api_tokens_cannot_run_a_device_flow() {
        let server = MockServer::with_json("{}").await;
        let error = flow(&server.base_url, true)
            .start(Provider::GitHub)
            .await
            .expect_err("wrong method");
        assert!(matches!(
            error,
            PortError::NotConfigured { .. } | PortError::Unsupported { .. }
        ));
    }

    #[tokio::test]
    async fn a_reply_without_a_device_code_is_a_protocol_error() {
        let server = MockServer::with_json(r#"{"user_code":"WXYZ"}"#).await;
        let error = flow(&server.base_url, true)
            .start(Provider::GoogleCalendar)
            .await
            .expect_err("no device code");
        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[tokio::test]
    async fn an_unapproved_request_reports_itself_as_pending() {
        let server = MockServer::with_json(r#"{"error":"authorization_pending"}"#).await;
        assert_eq!(
            flow(&server.base_url, true)
                .poll(Provider::GoogleCalendar, &challenge())
                .await
                .expect("poll"),
            DeviceCodePoll::Pending
        );
    }

    #[tokio::test]
    async fn being_told_to_slow_down_is_still_pending_rather_than_a_failure() {
        let server = MockServer::with_json(r#"{"error":"slow_down"}"#).await;
        assert_eq!(
            flow(&server.base_url, true)
                .poll(Provider::GoogleCalendar, &challenge())
                .await
                .expect("poll"),
            DeviceCodePoll::Pending
        );
    }

    #[tokio::test]
    async fn a_declined_request_is_rejected_with_a_reason() {
        let server = MockServer::with_json(r#"{"error":"access_denied"}"#).await;
        match flow(&server.base_url, true)
            .poll(Provider::GoogleCalendar, &challenge())
            .await
            .expect("poll")
        {
            DeviceCodePoll::Rejected { reason } => assert!(reason.contains("declined")),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_expired_code_is_rejected_rather_than_polled_forever() {
        let server = MockServer::with_json(r#"{"error":"expired_token"}"#).await;
        match flow(&server.base_url, true)
            .poll(Provider::GoogleCalendar, &challenge())
            .await
            .expect("poll")
        {
            DeviceCodePoll::Rejected { reason } => assert!(reason.contains("expired")),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_approved_request_grants_a_credential_with_its_expiry() {
        let server = MockServer::with_json(
            r#"{"access_token":"ya29.access","refresh_token":"1//refresh","expires_in":3599}"#,
        )
        .await;

        let granted = flow(&server.base_url, true)
            .poll(Provider::GoogleCalendar, &challenge())
            .await
            .expect("poll");

        let request = server.request();
        assert_eq!(request.target, "/token");
        assert!(request.body.contains("device_code=dev-code"));
        assert!(request.body.contains("client_secret=GOCSPX-secret"));
        assert!(
            request
                .body
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
            "got {}",
            request.body
        );

        match granted {
            DeviceCodePoll::Granted(credential) => {
                assert_eq!(credential.token().expose(), "ya29.access");
                assert_eq!(
                    credential
                        .refresh_token()
                        .map(|secret| secret.expose().to_owned()),
                    Some("1//refresh".to_owned())
                );
                assert_eq!(credential.expires_at(), Some(NOW + Duration::seconds(3599)));
            }
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refreshing_exchanges_the_refresh_token() {
        let server =
            MockServer::with_json(r#"{"access_token":"ya29.new","expires_in":3600}"#).await;
        let stale = StoredCredential::oauth(
            Secret::new("ya29.old"),
            Some(Secret::new("1//refresh")),
            Some(NOW - Duration::hours(1)),
        );

        let refreshed = flow(&server.base_url, true)
            .refresh(Provider::GoogleCalendar, &stale)
            .await
            .expect("refresh");

        assert!(server.request().body.contains("grant_type=refresh_token"));
        assert_eq!(refreshed.token().expose(), "ya29.new");
        assert_eq!(
            refreshed
                .refresh_token()
                .map(|secret| secret.expose().to_owned()),
            Some("1//refresh".to_owned()),
            "a provider that omits the refresh token must not cost us the one we have"
        );
        assert_eq!(refreshed.expires_at(), Some(NOW + Duration::seconds(3600)));
    }

    #[tokio::test]
    async fn a_refresh_token_the_provider_rejects_means_authenticating_again() {
        let server = MockServer::with_json(r#"{"error":"invalid_grant"}"#).await;
        let stale = StoredCredential::oauth(
            Secret::new("old"),
            Some(Secret::new("revoked")),
            Some(NOW - Duration::hours(1)),
        );

        let error = flow(&server.base_url, true)
            .refresh(Provider::GoogleCalendar, &stale)
            .await
            .expect_err("rejected");

        assert_eq!(
            error,
            PortError::Unauthenticated {
                provider: Provider::GoogleCalendar
            }
        );
    }

    #[tokio::test]
    async fn a_credential_with_nothing_to_refresh_cannot_be_refreshed() {
        let server = MockServer::with_json("{}").await;
        let error = flow(&server.base_url, true)
            .refresh(
                Provider::GoogleCalendar,
                &StoredCredential::api_token(Secret::new("t")),
            )
            .await
            .expect_err("nothing to refresh");

        assert!(matches!(error, PortError::Unauthenticated { .. }));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn without_an_override_the_providers_own_endpoints_are_used() {
        let flow = HttpDeviceCodeFlow::new(
            HttpClient::new().expect("client"),
            Vec::new(),
            Arc::new(FixedClock::new(NOW)),
        );
        assert_eq!(
            flow.url("https://oauth2.googleapis.com/device/code"),
            "https://oauth2.googleapis.com/device/code"
        );
    }
}
