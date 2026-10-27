//! vetinarid's sign-in to the autognomus issuer (design 12): the daemon is
//! the credential's only holder, so it runs the device flow, and the claim
//! loops wait on it when the gateway stops accepting the token.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use vetinari_core::application::auth::{AuthService, AuthState};
use vetinari_core::application::ports::{DeviceCodeChallenge, DeviceCodePoll, PortResult};
use vetinari_core::domain::credentials::Provider;

/// How a sign-in ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInOutcome {
    /// Approved; the credential is stored.
    Granted,
    /// Declined, expired, or failed.
    Rejected(String),
}

/// Whether the daemon can reach the gateway, and how it signs in.
pub struct SignIn {
    auth: Arc<AuthService>,
    needed: watch::Sender<bool>,
}

impl SignIn {
    /// Sign in through `auth`, assumed signed in until told otherwise.
    #[must_use]
    pub fn new(auth: Arc<AuthService>) -> Arc<Self> {
        Arc::new(Self {
            auth,
            needed: watch::channel(false).0,
        })
    }

    /// Whether the developer has to sign in.
    #[must_use]
    pub fn needed(&self) -> bool {
        *self.needed.borrow()
    }

    /// Hear whenever that changes.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<bool> {
        self.needed.subscribe()
    }

    /// Read the stored credential's state afresh: one that is missing,
    /// expired past renewal or unreadable needs a sign-in.
    pub async fn check(&self) {
        let usable = self
            .auth
            .status(Provider::Autognomus, None)
            .await
            .is_ok_and(|status| status.is_usable());
        self.needed.send_replace(!usable);
    }

    /// The stored credential's state.
    pub async fn state(&self) -> AuthState {
        self.auth
            .status(Provider::Autognomus, None)
            .await
            .map_or(AuthState::Corrupt, |status| status.state)
    }

    /// The gateway refused the token even after a renewal.
    pub fn lost(&self) {
        self.needed.send_replace(true);
    }

    /// Wait until the daemon is signed in.
    pub async fn signed_in(&self) {
        let mut changes = self.needed.subscribe();
        let _ = changes.wait_for(|needed| !needed).await;
    }

    /// Begin a device flow: the challenge now, and how it ended once the
    /// developer approves or it expires. The flow runs on whether or not
    /// anyone waits for its end.
    pub async fn begin(
        self: &Arc<Self>,
    ) -> PortResult<(DeviceCodeChallenge, oneshot::Receiver<SignInOutcome>)> {
        let challenge = self.auth.begin_device_flow(Provider::Autognomus).await?;
        let (ended, outcome) = oneshot::channel();
        let this = self.clone();
        let polled = challenge.clone();
        tokio::spawn(async move {
            let _ = ended.send(this.poll_until_ended(&polled).await);
        });
        Ok((challenge, outcome))
    }

    async fn poll_until_ended(&self, challenge: &DeviceCodeChallenge) -> SignInOutcome {
        let interval = Duration::from_secs(challenge.interval_secs.max(1));
        loop {
            match self
                .auth
                .poll_device_flow(Provider::Autognomus, None, challenge)
                .await
            {
                Ok(DeviceCodePoll::Pending) => tokio::time::sleep(interval).await,
                Ok(DeviceCodePoll::Granted(_)) => {
                    self.needed.send_replace(false);
                    return SignInOutcome::Granted;
                }
                Ok(DeviceCodePoll::Rejected { reason }) => return SignInOutcome::Rejected(reason),
                Err(error) if error.is_transient() => {
                    tracing::warn!(%error, "cannot poll the sign-in; trying again");
                    tokio::time::sleep(interval).await;
                }
                Err(error) => return SignInOutcome::Rejected(error.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use time::macros::datetime;
    use vetinari_core::domain::credentials::{CredentialKey, Secret, StoredCredential};
    use vetinari_core::testing::{FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore};

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-10-25 09:00 UTC);

    fn sign_in(expiry: OffsetDateTime) -> (Arc<SignIn>, Arc<MemoryCredentialStore>) {
        let store = Arc::new(MemoryCredentialStore::default());
        let auth = Arc::new(AuthService::new(
            store.clone(),
            Arc::new(FakeDeviceCodeFlow::new(1, expiry)),
            Arc::new(FixedClock::new(NOW)),
        ));
        (SignIn::new(auth), store)
    }

    #[tokio::test]
    async fn a_daemon_with_no_credential_needs_a_sign_in() {
        let (sign_in, store) = sign_in(NOW + time::Duration::minutes(10));
        sign_in.check().await;
        assert!(sign_in.needed());

        store.seed(
            &CredentialKey::new(Provider::Autognomus, None),
            StoredCredential::oauth(Secret::new("jwt"), Some(Secret::new("r")), None),
        );
        sign_in.check().await;
        assert!(!sign_in.needed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_granted_sign_in_is_stored_and_wakes_whoever_waits_on_it() {
        let (sign_in, store) = sign_in(NOW + time::Duration::minutes(10));
        sign_in.lost();
        assert!(sign_in.needed());
        let waiting = tokio::spawn({
            let sign_in = sign_in.clone();
            async move { sign_in.signed_in().await }
        });

        let (challenge, outcome) = sign_in.begin().await.expect("challenge");
        assert_eq!(challenge.user_code.as_deref(), Some("WXYZ-1234"));
        let outcome = tokio::time::timeout(Duration::from_secs(60), outcome)
            .await
            .expect("ends within a minute")
            .expect("an outcome");

        assert_eq!(outcome, SignInOutcome::Granted);
        assert!(!sign_in.needed());
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the waiter wakes")
            .expect("joined");
        assert_eq!(store.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_challenge_that_expires_is_rejected_and_still_needs_a_sign_in() {
        let (sign_in, _) = sign_in(NOW);
        sign_in.lost();
        let (_, outcome) = sign_in.begin().await.expect("challenge");
        let outcome = tokio::time::timeout(Duration::from_secs(60), outcome)
            .await
            .expect("ends")
            .expect("an outcome");
        assert!(matches!(outcome, SignInOutcome::Rejected(_)), "{outcome:?}");
        assert!(sign_in.needed());
    }
}
