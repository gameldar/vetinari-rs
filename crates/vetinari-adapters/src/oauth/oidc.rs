//! Device-code sign-in to an OIDC issuer whose endpoints are not known
//! until its discovery document is read (design 12): the autognomus
//! issuer is whatever the developer deployed.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use vetinari_core::application::config::OidcClient;
use vetinari_core::application::ports::{
    Clock, DeviceCodeChallenge, DeviceCodeFlow, DeviceCodePoll, PortError, PortResult,
};
use vetinari_core::domain::credentials::{Provider, StoredCredential};

use super::device_code::Exchange;
use crate::HttpClient;

/// Signs in to one OIDC issuer for one provider.
pub struct OidcDeviceCodeFlow {
    provider: Provider,
    client: OidcClient,
    exchange: Exchange,
    endpoints: Mutex<Option<Endpoints>>,
}

/// What discovery says, of what the device flow needs.
#[derive(Debug, Clone)]
struct Endpoints {
    device: String,
    token: String,
}

impl OidcDeviceCodeFlow {
    /// A flow for `provider` against `client`'s issuer.
    #[must_use]
    pub fn new(
        provider: Provider,
        client: OidcClient,
        http: HttpClient,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            provider,
            client,
            exchange: Exchange::new(http, clock),
            endpoints: Mutex::new(None),
        }
    }

    fn ours(&self, provider: Provider) -> PortResult<()> {
        if provider == self.provider {
            Ok(())
        } else {
            Err(PortError::unsupported(
                provider.name(),
                format!("signing in at the {} issuer", self.provider.name()),
            ))
        }
    }

    /// The issuer's endpoints, read once; a failed read is tried again
    /// next time.
    async fn endpoints(&self) -> PortResult<Endpoints> {
        let mut known = self.endpoints.lock().await;
        if let Some(endpoints) = known.as_ref() {
            return Ok(endpoints.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.client.issuer);
        let response = self
            .exchange
            .http()
            .inner()
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| PortError::unavailable("oidc discovery", error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(PortError::unavailable(
                "oidc discovery",
                format!("{url} answered {status}"),
            ));
        }
        let document: Value = response
            .json()
            .await
            .map_err(|error| PortError::protocol("oidc discovery", format!("{url}: {error}")))?;
        let field = |name: &str| {
            document
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let device = field("device_authorization_endpoint").ok_or_else(|| {
            PortError::unsupported(
                self.client.issuer.as_str(),
                "the device authorization grant (no device_authorization_endpoint in discovery)",
            )
        })?;
        let token = field("token_endpoint").ok_or_else(|| {
            PortError::protocol("oidc discovery", format!("{url} names no token_endpoint"))
        })?;
        let endpoints = Endpoints { device, token };
        *known = Some(endpoints.clone());
        Ok(endpoints)
    }
}

#[async_trait]
impl DeviceCodeFlow for OidcDeviceCodeFlow {
    async fn start(&self, provider: Provider) -> PortResult<DeviceCodeChallenge> {
        self.ours(provider)?;
        let endpoints = self.endpoints().await?;
        let scope = self.client.scopes.join(" ");
        let mut form = vec![
            ("client_id", self.client.client_id.as_str()),
            ("scope", scope.as_str()),
        ];
        if let Some(audience) = &self.client.audience {
            form.push(("audience", audience.as_str()));
        }
        self.exchange
            .challenge(provider, &endpoints.device, &form, None)
            .await
    }

    async fn poll(
        &self,
        provider: Provider,
        challenge: &DeviceCodeChallenge,
    ) -> PortResult<DeviceCodePoll> {
        self.ours(provider)?;
        let endpoints = self.endpoints().await?;
        self.exchange
            .poll(
                &endpoints.token,
                &[
                    ("client_id", self.client.client_id.as_str()),
                    ("device_code", challenge.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await
    }

    async fn refresh(
        &self,
        provider: Provider,
        credential: &StoredCredential,
    ) -> PortResult<StoredCredential> {
        self.ours(provider)?;
        let refresh = credential
            .refresh_token()
            .ok_or(PortError::Unauthenticated { provider })?;
        let endpoints = self.endpoints().await?;
        self.exchange
            .refresh(
                provider,
                &endpoints.token,
                &[
                    ("client_id", self.client.client_id.as_str()),
                    ("refresh_token", refresh.expose()),
                    ("grant_type", "refresh_token"),
                ],
                refresh,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use time::{Duration, OffsetDateTime};
    use vetinari_core::application::config::HttpUrl;
    use vetinari_core::domain::credentials::Secret;
    use vetinari_core::testing::FixedClock;

    use super::*;
    use crate::testing::MockServer;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn discovery(endpoints: &MockServer) -> String {
        serde_json::json!({
            "issuer": "ignored",
            "device_authorization_endpoint": format!("{}/device", endpoints.base_url),
            "token_endpoint": format!("{}/token", endpoints.base_url),
        })
        .to_string()
    }

    fn flow(issuer: &MockServer) -> OidcDeviceCodeFlow {
        OidcDeviceCodeFlow::new(
            Provider::Autognomus,
            OidcClient {
                issuer: HttpUrl::parse(&format!("{}/realms/home", issuer.base_url)).expect("url"),
                client_id: "vetinari".to_owned(),
                audience: Some("autognomus".to_owned()),
                scopes: vec!["openid".to_owned(), "offline_access".to_owned()],
            },
            HttpClient::new().expect("client"),
            Arc::new(FixedClock::new(NOW)),
        )
    }

    fn challenge() -> DeviceCodeChallenge {
        DeviceCodeChallenge {
            user_code: "BDWP-HQPK".to_owned(),
            verification_uri: "https://id.example.net/device".to_owned(),
            device_code: "dev-1".to_owned(),
            interval_secs: 5,
            expires_at: NOW + Duration::minutes(10),
        }
    }

    #[tokio::test]
    async fn the_device_request_goes_where_discovery_says_with_the_audience() {
        let endpoints = MockServer::with_json(
            r#"{"device_code":"dev-1","user_code":"BDWP-HQPK","verification_uri":"https://id.example.net/device","interval":5,"expires_in":600}"#,
        )
        .await;
        let issuer = MockServer::with_json(&discovery(&endpoints)).await;

        let challenge = flow(&issuer)
            .start(Provider::Autognomus)
            .await
            .expect("challenge");

        assert_eq!(
            issuer.request().target,
            "/realms/home/.well-known/openid-configuration"
        );
        let request = endpoints.request();
        assert_eq!(request.target, "/device");
        for part in [
            "client_id=vetinari",
            "scope=openid+offline_access",
            "audience=autognomus",
        ] {
            assert!(request.body.contains(part), "{part} in {}", request.body);
        }
        assert_eq!(challenge.user_code, "BDWP-HQPK");
        assert_eq!(challenge.verification_uri, "https://id.example.net/device");
        assert_eq!(challenge.expires_at, NOW + Duration::minutes(10));
    }

    #[tokio::test]
    async fn discovery_is_read_once_and_the_token_endpoint_is_polled() {
        let endpoints = MockServer::start(vec![
            (
                200,
                r#"{"device_code":"dev-1","user_code":"BDWP-HQPK","verification_uri":"https://id.example.net/device"}"#
                    .to_owned(),
            ),
            (
                200,
                r#"{"access_token":"jwt","refresh_token":"r1","expires_in":300}"#.to_owned(),
            ),
        ])
        .await;
        let issuer = MockServer::with_json(&discovery(&endpoints)).await;
        let flow = flow(&issuer);

        let challenge = flow.start(Provider::Autognomus).await.expect("challenge");
        let granted = flow
            .poll(Provider::Autognomus, &challenge)
            .await
            .expect("poll");

        assert_eq!(issuer.requests().len(), 1);
        let poll = &endpoints.requests()[1];
        assert_eq!(poll.target, "/token");
        assert!(poll.body.contains("device_code=dev-1"), "{}", poll.body);
        let DeviceCodePoll::Granted(credential) = granted else {
            panic!("granted, got {granted:?}");
        };
        assert_eq!(credential.token().expose(), "jwt");
        assert_eq!(credential.expires_at(), Some(NOW + Duration::minutes(5)));
    }

    #[tokio::test]
    async fn a_refresh_rotates_the_refresh_token_at_the_token_endpoint() {
        let endpoints = MockServer::with_json(
            r#"{"access_token":"jwt-2","refresh_token":"r2","expires_in":300}"#,
        )
        .await;
        let issuer = MockServer::with_json(&discovery(&endpoints)).await;

        let refreshed = flow(&issuer)
            .refresh(
                Provider::Autognomus,
                &StoredCredential::oauth(Secret::new("jwt"), Some(Secret::new("r1")), None),
            )
            .await
            .expect("refreshed");

        let request = endpoints.request();
        assert_eq!(request.target, "/token");
        assert!(request.body.contains("grant_type=refresh_token"));
        assert!(request.body.contains("refresh_token=r1"));
        assert_eq!(refreshed.token().expose(), "jwt-2");
        assert_eq!(refreshed.refresh_token().map(Secret::expose), Some("r2"));
    }

    #[tokio::test]
    async fn an_issuer_that_advertises_no_device_grant_says_so() {
        let issuer =
            MockServer::with_json(r#"{"issuer":"x","token_endpoint":"http://127.0.0.1:1/token"}"#)
                .await;
        let error = flow(&issuer)
            .start(Provider::Autognomus)
            .await
            .expect_err("no device grant");
        assert!(matches!(error, PortError::Unsupported { .. }), "{error:?}");
        assert!(error.to_string().contains("device"), "{error}");
    }

    #[tokio::test]
    async fn the_flow_signs_in_only_its_own_provider() {
        let issuer = MockServer::with_json("{}").await;
        let error = flow(&issuer)
            .poll(Provider::GoogleCalendar, &challenge())
            .await
            .expect_err("another provider");
        assert!(matches!(error, PortError::Unsupported { .. }));
        assert!(issuer.requests().is_empty());
    }
}
