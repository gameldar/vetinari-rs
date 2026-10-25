//! The authentication service.
//!
//! Adapters never touch the keychain directly: they ask here for a token and
//! get either a usable one or a clear instruction to authenticate. Expired
//! credentials are refreshed on the way out and written back, so a long
//! session does not start failing halfway through the afternoon.

use std::sync::Arc;

use time::OffsetDateTime;

use super::ports::{
    Clock, CredentialStore, DeviceCodeChallenge, DeviceCodeFlow, DeviceCodePoll, PortError,
    PortResult,
};
use crate::domain::credentials::{
    AuthMethod, CredentialKey, CredentialStatus, Provider, Secret, StoredCredential,
};
use crate::domain::ids::Slug;

/// Whether a provider is ready to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    /// Nothing stored.
    Missing,
    /// Stored and usable.
    Valid,
    /// Usable, but due for renewal.
    NearingExpiry,
    /// Stored but past its expiry, and not refreshable.
    Expired,
    /// Stored but unreadable.
    Corrupt,
}

impl AuthState {
    /// The word shown in the wizard and the header.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            AuthState::Missing => "not configured",
            AuthState::Valid => "authenticated",
            AuthState::NearingExpiry => "renew soon",
            AuthState::Expired => "expired",
            AuthState::Corrupt => "unreadable",
        }
    }
}

/// One provider's authentication state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    /// The provider.
    pub provider: Provider,
    /// Where its credential is filed.
    pub key: CredentialKey,
    /// Whether it is usable.
    pub state: AuthState,
    /// When it expires, when it does.
    pub expires_at: Option<OffsetDateTime>,
}

impl ProviderStatus {
    /// Whether a call made right now would work.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(self.state, AuthState::Valid | AuthState::NearingExpiry)
    }

    /// Whether the developer needs to do something about it.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        matches!(self.state, AuthState::Expired | AuthState::Corrupt)
    }
}

/// Reads, stores, and renews credentials.
pub struct AuthService {
    store: Arc<dyn CredentialStore>,
    flow: Arc<dyn DeviceCodeFlow>,
    clock: Arc<dyn Clock>,
}

impl AuthService {
    /// Build the service.
    #[must_use]
    pub fn new(
        store: Arc<dyn CredentialStore>,
        flow: Arc<dyn DeviceCodeFlow>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { store, flow, clock }
    }

    /// Where a provider's credential is filed for an account.
    #[must_use]
    pub fn key(provider: Provider, account: Option<Slug>) -> CredentialKey {
        CredentialKey::new(provider, account)
    }

    /// One provider's state.
    pub async fn status(
        &self,
        provider: Provider,
        account: Option<Slug>,
    ) -> PortResult<ProviderStatus> {
        let key = Self::key(provider, account);
        let stored = self.store.get(&key).await?;
        let (state, expires_at) = match stored {
            None => (AuthState::Missing, None),
            Some(credential) => {
                let expires_at = credential.expires_at();
                let state = match credential.status(self.clock.now()) {
                    CredentialStatus::Valid => AuthState::Valid,
                    CredentialStatus::NearingExpiry => AuthState::NearingExpiry,
                    CredentialStatus::Expired => {
                        if credential.refresh_token().is_some() {
                            AuthState::NearingExpiry
                        } else {
                            AuthState::Expired
                        }
                    }
                };
                (state, expires_at)
            }
        };
        Ok(ProviderStatus {
            provider,
            key,
            state,
            expires_at,
        })
    }

    /// Every provider's state, for the setup wizard.
    pub async fn statuses(&self, account: Option<Slug>) -> Vec<ProviderStatus> {
        let mut out = Vec::with_capacity(Provider::ALL.len());
        for provider in Provider::ALL {
            match self.status(*provider, account.clone()).await {
                Ok(status) => out.push(status),
                Err(_) => out.push(ProviderStatus {
                    provider: *provider,
                    key: Self::key(*provider, account.clone()),
                    state: AuthState::Corrupt,
                    expires_at: None,
                }),
            }
        }
        out
    }

    /// The line shown in the TUI header.
    pub async fn summary(&self, account: Option<Slug>) -> String {
        let statuses = self.statuses(account).await;
        let authenticated = statuses.iter().filter(|s| s.is_usable()).count();
        let attention = statuses.iter().filter(|s| s.needs_attention()).count();
        let unreadable = statuses
            .iter()
            .filter(|s| s.state == AuthState::Corrupt)
            .count();

        // If nothing at all can be read, the store is the problem rather than
        // the credentials, and saying so is more use than counting failures.
        if unreadable == statuses.len() && !statuses.is_empty() {
            return "Keychain: unavailable on this machine".to_owned();
        }

        match (authenticated, attention) {
            (0, 0) => "Keychain: none configured".to_owned(),
            (count, 0) => format!("Keychain: {count} authenticated"),
            (count, needs) => format!("Keychain: {count} authenticated, {needs} needs attention"),
        }
    }

    /// Store a pasted API token.
    pub async fn save_api_token(
        &self,
        provider: Provider,
        account: Option<Slug>,
        token: Secret,
    ) -> PortResult<()> {
        self.store
            .set(
                &Self::key(provider, account),
                &StoredCredential::api_token(token),
            )
            .await
    }

    /// Forget a credential, reporting whether one was there.
    pub async fn forget(&self, provider: Provider, account: Option<Slug>) -> PortResult<bool> {
        self.store.delete(&Self::key(provider, account)).await
    }

    /// A usable token, refreshing first when the stored one has expired.
    pub async fn token(&self, provider: Provider, account: Option<Slug>) -> PortResult<Secret> {
        let key = Self::key(provider, account);
        let credential = self
            .store
            .get(&key)
            .await?
            .ok_or(PortError::Unauthenticated { provider })?;

        if credential.status(self.clock.now()) != CredentialStatus::Expired {
            return Ok(credential.token().clone());
        }
        if credential.refresh_token().is_none() {
            return Err(PortError::Unauthenticated { provider });
        }
        let refreshed = self.flow.refresh(provider, &credential).await?;
        self.store.set(&key, &refreshed).await?;
        Ok(refreshed.token().clone())
    }

    /// A token for a call that may run long, renewed first when it is
    /// near its expiry, or whatever its expiry when `rejected` says the
    /// service refused it. Only the credential's one holder calls this:
    /// a renewal rotates the refresh token.
    pub async fn renewed_token(
        &self,
        provider: Provider,
        account: Option<Slug>,
        rejected: bool,
    ) -> PortResult<Secret> {
        let key = Self::key(provider, account);
        let credential = self
            .store
            .get(&key)
            .await?
            .ok_or(PortError::Unauthenticated { provider })?;
        let due = rejected || credential.status(self.clock.now()) != CredentialStatus::Valid;
        if !due {
            return Ok(credential.token().clone());
        }
        if credential.refresh_token().is_none() {
            return if rejected || credential.status(self.clock.now()) == CredentialStatus::Expired {
                Err(PortError::Unauthenticated { provider })
            } else {
                Ok(credential.token().clone())
            };
        }
        let refreshed = self.flow.refresh(provider, &credential).await?;
        self.store.set(&key, &refreshed).await?;
        Ok(refreshed.token().clone())
    }

    /// Begin a device-code flow, for providers that use one.
    pub async fn begin_device_flow(&self, provider: Provider) -> PortResult<DeviceCodeChallenge> {
        match provider.auth_spec().method {
            AuthMethod::OAuthDeviceCode { .. } | AuthMethod::OidcDeviceCode => {
                self.flow.start(provider).await
            }
            AuthMethod::ApiToken { .. } => Err(PortError::unsupported(
                provider.name(),
                "device-code authentication; paste an API token instead",
            )),
        }
    }

    /// Poll a device-code flow, storing the credential once it is granted.
    pub async fn poll_device_flow(
        &self,
        provider: Provider,
        account: Option<Slug>,
        challenge: &DeviceCodeChallenge,
    ) -> PortResult<DeviceCodePoll> {
        if self.clock.now() >= challenge.expires_at {
            return Ok(DeviceCodePoll::Rejected {
                reason: "the authorisation code expired; start again".to_owned(),
            });
        }
        let outcome = self.flow.poll(provider, challenge).await?;
        if let DeviceCodePoll::Granted(credential) = &outcome {
            self.store
                .set(&Self::key(provider, account), credential)
                .await?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    use crate::domain::credentials::Secret;
    use crate::testing::{FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);
    const LATER: OffsetDateTime = datetime!(2026-10-21 12:00 UTC);

    fn service() -> (AuthService, Arc<MemoryCredentialStore>, Arc<FixedClock>) {
        let store = Arc::new(MemoryCredentialStore::default());
        let clock = Arc::new(FixedClock::new(NOW));
        let flow = Arc::new(FakeDeviceCodeFlow::new(1, LATER));
        (
            AuthService::new(store.clone(), flow, clock.clone()),
            store,
            clock,
        )
    }

    #[tokio::test]
    async fn an_unconfigured_provider_reports_itself_missing() {
        let (service, _, _) = service();
        let status = service
            .status(Provider::GitHub, None)
            .await
            .expect("status");
        assert_eq!(status.state, AuthState::Missing);
        assert!(!status.is_usable());
        assert_eq!(status.provider, Provider::GitHub);
    }

    #[tokio::test]
    async fn a_pasted_token_is_stored_and_immediately_usable() {
        let (service, store, _) = service();
        service
            .save_api_token(Provider::GitHub, None, Secret::new("ghp_1"))
            .await
            .expect("save");

        assert_eq!(store.len(), 1);
        let status = service
            .status(Provider::GitHub, None)
            .await
            .expect("status");
        assert_eq!(status.state, AuthState::Valid);
        assert!(status.is_usable());
        assert_eq!(
            service
                .token(Provider::GitHub, None)
                .await
                .expect("token")
                .expose(),
            "ghp_1"
        );
    }

    #[tokio::test]
    async fn asking_for_a_token_that_was_never_stored_says_to_authenticate() {
        let (service, _, _) = service();
        let error = service
            .token(Provider::Jira, None)
            .await
            .expect_err("no credential");
        assert_eq!(
            error,
            PortError::Unauthenticated {
                provider: Provider::Jira
            }
        );
    }

    #[tokio::test]
    async fn a_credential_nearing_expiry_is_reported_before_it_breaks() {
        let (service, store, _) = service();
        store.seed(
            &CredentialKey::new(Provider::GoogleCalendar, None),
            StoredCredential::oauth(
                Secret::new("access"),
                None,
                Some(NOW + Duration::minutes(2)),
            ),
        );
        let status = service
            .status(Provider::GoogleCalendar, None)
            .await
            .expect("status");
        assert_eq!(status.state, AuthState::NearingExpiry);
        assert!(status.is_usable());
    }

    #[tokio::test]
    async fn an_expired_credential_with_no_refresh_token_needs_re_authentication() {
        let (service, store, _) = service();
        store.seed(
            &CredentialKey::new(Provider::GoogleCalendar, None),
            StoredCredential::oauth(Secret::new("access"), None, Some(NOW - Duration::hours(1))),
        );

        let status = service
            .status(Provider::GoogleCalendar, None)
            .await
            .expect("status");
        assert_eq!(status.state, AuthState::Expired);
        assert!(!status.is_usable());

        let error = service
            .token(Provider::GoogleCalendar, None)
            .await
            .expect_err("expired");
        assert!(matches!(error, PortError::Unauthenticated { .. }));
    }

    #[tokio::test]
    async fn an_expired_credential_is_refreshed_before_it_is_handed_out() {
        let (service, store, _) = service();
        let key = CredentialKey::new(Provider::GoogleCalendar, None);
        store.seed(
            &key,
            StoredCredential::oauth(
                Secret::new("stale"),
                Some(Secret::new("refresh")),
                Some(NOW - Duration::hours(1)),
            ),
        );

        let token = service
            .token(Provider::GoogleCalendar, None)
            .await
            .expect("refreshed");

        assert_eq!(token.expose(), "refreshed-access");
        let stored = store.get(&key).await.expect("get").expect("stored");
        assert_eq!(
            stored.token().expose(),
            "refreshed-access",
            "the refreshed credential is written back so the next call is cheap"
        );
    }

    #[tokio::test]
    async fn a_token_nearing_expiry_is_renewed_before_a_long_call() {
        let (service, store, _) = service();
        let key = CredentialKey::new(Provider::Autognomus, None);
        store.seed(
            &key,
            StoredCredential::oauth(
                Secret::new("soon"),
                Some(Secret::new("refresh")),
                Some(NOW + Duration::minutes(2)),
            ),
        );
        assert_eq!(
            service
                .token(Provider::Autognomus, None)
                .await
                .expect("token")
                .expose(),
            "soon",
            "token() still hands out a usable one as it is"
        );
        assert_eq!(
            service
                .renewed_token(Provider::Autognomus, None, false)
                .await
                .expect("renewed")
                .expose(),
            "refreshed-access"
        );
        assert_eq!(
            store
                .get(&key)
                .await
                .expect("get")
                .expect("stored")
                .token()
                .expose(),
            "refreshed-access"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_is_renewed_however_long_it_has_left() {
        let (service, store, _) = service();
        store.seed(
            &CredentialKey::new(Provider::Autognomus, None),
            StoredCredential::oauth(
                Secret::new("valid-but-refused"),
                Some(Secret::new("refresh")),
                Some(NOW + Duration::hours(1)),
            ),
        );
        assert_eq!(
            service
                .renewed_token(Provider::Autognomus, None, true)
                .await
                .expect("renewed")
                .expose(),
            "refreshed-access"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_that_cannot_be_renewed_needs_a_sign_in() {
        let (service, store, _) = service();
        store.seed(
            &CredentialKey::new(Provider::Autognomus, None),
            StoredCredential::oauth(Secret::new("refused"), None, Some(NOW + Duration::hours(1))),
        );
        assert_eq!(
            service
                .renewed_token(Provider::Autognomus, None, true)
                .await
                .expect_err("no refresh token"),
            PortError::Unauthenticated {
                provider: Provider::Autognomus
            }
        );
    }

    #[tokio::test]
    async fn a_device_flow_grants_once_the_developer_approves_it() {
        let (service, store, _) = service();
        let challenge = service
            .begin_device_flow(Provider::GoogleCalendar)
            .await
            .expect("challenge");
        assert_eq!(challenge.user_code, "WXYZ-1234");

        let first = service
            .poll_device_flow(Provider::GoogleCalendar, None, &challenge)
            .await
            .expect("poll");
        assert_eq!(first, DeviceCodePoll::Pending);
        assert!(store.is_empty());

        let second = service
            .poll_device_flow(Provider::GoogleCalendar, None, &challenge)
            .await
            .expect("poll");
        assert!(matches!(second, DeviceCodePoll::Granted(_)));
        assert_eq!(
            service
                .token(Provider::GoogleCalendar, None)
                .await
                .expect("token")
                .expose(),
            "granted-access"
        );
    }

    #[tokio::test]
    async fn a_device_flow_for_a_provider_that_uses_tokens_is_refused() {
        let (service, _, _) = service();
        let error = service
            .begin_device_flow(Provider::GitHub)
            .await
            .expect_err("wrong method");
        assert!(matches!(error, PortError::Unsupported { .. }));
    }

    #[tokio::test]
    async fn an_expired_challenge_is_not_polled_again() {
        let (service, _, clock) = service();
        let challenge = service
            .begin_device_flow(Provider::GoogleCalendar)
            .await
            .expect("challenge");
        clock.advance(Duration::days(1));

        let outcome = service
            .poll_device_flow(Provider::GoogleCalendar, None, &challenge)
            .await
            .expect("poll");

        assert!(matches!(outcome, DeviceCodePoll::Rejected { .. }));
    }

    #[tokio::test]
    async fn forgetting_a_credential_removes_it() {
        let (service, _, _) = service();
        service
            .save_api_token(Provider::Slack, None, Secret::new("xoxb"))
            .await
            .expect("save");
        assert!(service.forget(Provider::Slack, None).await.expect("forget"));
        assert!(!service.forget(Provider::Slack, None).await.expect("forget"));
    }

    #[tokio::test]
    async fn credentials_are_scoped_per_account_so_work_and_home_do_not_collide() {
        let (service, _, _) = service();
        let work = Slug::parse("work").expect("slug");
        let home = Slug::parse("home").expect("slug");
        service
            .save_api_token(
                Provider::GitHub,
                Some(work.clone()),
                Secret::new("work-token"),
            )
            .await
            .expect("save");

        assert_eq!(
            service
                .token(Provider::GitHub, Some(work))
                .await
                .expect("token")
                .expose(),
            "work-token"
        );
        assert!(service.token(Provider::GitHub, Some(home)).await.is_err());
    }

    #[tokio::test]
    async fn the_header_summarises_how_many_providers_are_set_up() {
        let (service, _, _) = service();
        assert_eq!(service.summary(None).await, "Keychain: none configured");

        service
            .save_api_token(Provider::GitHub, None, Secret::new("t"))
            .await
            .expect("save");
        service
            .save_api_token(Provider::Jira, None, Secret::new("t"))
            .await
            .expect("save");

        assert_eq!(service.summary(None).await, "Keychain: 2 authenticated");
    }

    #[tokio::test]
    async fn the_header_calls_out_credentials_that_need_attention() {
        let (service, store, _) = service();
        store.seed(
            &CredentialKey::new(Provider::GoogleCalendar, None),
            StoredCredential::oauth(Secret::new("a"), None, Some(NOW - Duration::hours(1))),
        );
        assert_eq!(
            service.summary(None).await,
            "Keychain: 0 authenticated, 1 needs attention"
        );
    }

    #[tokio::test]
    async fn a_machine_with_no_credential_store_says_that_rather_than_counting_failures() {
        let service = AuthService::new(
            Arc::new(crate::testing::UnavailableCredentialStore),
            Arc::new(FakeDeviceCodeFlow::new(0, LATER)),
            Arc::new(FixedClock::new(NOW)),
        );

        assert_eq!(
            service.summary(None).await,
            "Keychain: unavailable on this machine"
        );
        assert!(
            service
                .statuses(None)
                .await
                .iter()
                .all(|status| status.state == AuthState::Corrupt)
        );
    }

    #[tokio::test]
    async fn every_provider_is_listed_for_the_setup_wizard() {
        let (service, _, _) = service();
        let statuses = service.statuses(None).await;
        assert_eq!(statuses.len(), Provider::ALL.len());
        assert!(
            statuses
                .iter()
                .all(|status| status.state == AuthState::Missing)
        );
        assert!(
            statuses
                .iter()
                .any(|status| status.provider == Provider::Slack)
        );
    }
}
