//! Credentials in the OS keychain.
//!
//! The keychain calls are synchronous and may block on a platform service, so
//! each one runs on a blocking thread rather than on the UI runtime. A missing
//! entry is absence, not failure; everything else is reported in terms the
//! auth wizard can act on.

use async_trait::async_trait;
use keyring_core::error::Error as KeyringError;
use vetinari_core::application::ports::{CredentialStore, PortError, PortResult};
use vetinari_core::domain::credentials::{CredentialKey, Provider, Secret, StoredCredential};

/// Reads and writes credentials in the OS keychain.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// There is no such entry; that is an answer, not a problem.
    Absent,
    /// Something went wrong.
    Failed,
}

impl KeyringStore {
    /// Whether this machine has a credential store Vetinari can use.
    ///
    /// Reported so the TUI can say "keychain unavailable" rather than failing
    /// every provider one at a time.
    pub fn availability() -> PortResult<()> {
        match keyring::Entry::store_status() {
            Ok(()) => Ok(()),
            Err(error) => Err(PortError::unavailable(
                "keychain",
                format!("no usable credential store: {error}"),
            )),
        }
    }

    fn entry(key: &CredentialKey) -> PortResult<keyring::Entry> {
        keyring::Entry::new(&key.service(), key.account())
            .map_err(|error| map_error(key.provider(), &error))
    }
}

fn classify(error: &KeyringError) -> Outcome {
    match error {
        KeyringError::NoEntry => Outcome::Absent,
        _ => Outcome::Failed,
    }
}

fn map_error(provider: Provider, error: &KeyringError) -> PortError {
    match error {
        KeyringError::NoEntry => PortError::not_found("credential", provider.name()),
        KeyringError::NoDefaultStore | KeyringError::NoStorageAccess(_) => PortError::unavailable(
            "keychain",
            format!(
                "cannot reach the credential store for {}: {error}",
                provider.name()
            ),
        ),
        KeyringError::PlatformFailure(_) => PortError::unavailable(
            "keychain",
            format!(
                "the credential store failed for {}: {error}",
                provider.name()
            ),
        ),
        KeyringError::Ambiguous(_) => PortError::protocol(
            "keychain",
            format!(
                "more than one credential is stored for {}; remove the duplicates",
                provider.name()
            ),
        ),
        KeyringError::BadEncoding(_)
        | KeyringError::BadDataFormat(_, _)
        | KeyringError::BadStoreFormat(_) => PortError::protocol(
            "keychain",
            format!(
                "the stored credential for {} is unreadable",
                provider.name()
            ),
        ),
        other => PortError::Refused {
            what: format!("keychain ({})", provider.name()),
            detail: other.to_string(),
        },
    }
}

#[async_trait]
impl CredentialStore for KeyringStore {
    async fn get(&self, key: &CredentialKey) -> PortResult<Option<StoredCredential>> {
        let key = key.clone();
        let provider = key.provider();
        let read = tokio::task::spawn_blocking(move || {
            let entry = KeyringStore::entry(&key)?;
            match entry.get_password() {
                Ok(password) => Ok(Some(password)),
                Err(error) => match classify(&error) {
                    Outcome::Absent => Ok(None),
                    Outcome::Failed => Err(map_error(provider, &error)),
                },
            }
        })
        .await
        .map_err(|error| PortError::unavailable("keychain", error.to_string()))??;

        match read {
            None => Ok(None),
            Some(raw) => StoredCredential::decode(&raw).map(Some).map_err(|error| {
                PortError::protocol(
                    "keychain",
                    format!(
                        "the stored credential for {} is unreadable: {error}",
                        provider.name()
                    ),
                )
            }),
        }
    }

    async fn set(&self, key: &CredentialKey, credential: &StoredCredential) -> PortResult<()> {
        let key = key.clone();
        let provider = key.provider();
        let encoded = Secret::new(credential.encode());
        tokio::task::spawn_blocking(move || {
            let entry = KeyringStore::entry(&key)?;
            entry
                .set_password(encoded.expose())
                .map_err(|error| map_error(provider, &error))
        })
        .await
        .map_err(|error| PortError::unavailable("keychain", error.to_string()))?
    }

    async fn delete(&self, key: &CredentialKey) -> PortResult<bool> {
        let key = key.clone();
        let provider = key.provider();
        tokio::task::spawn_blocking(move || {
            let entry = KeyringStore::entry(&key)?;
            match entry.delete_credential() {
                Ok(()) => Ok(true),
                Err(error) => match classify(&error) {
                    Outcome::Absent => Ok(false),
                    Outcome::Failed => Err(map_error(provider, &error)),
                },
            }
        })
        .await
        .map_err(|error| PortError::unavailable("keychain", error.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring_core::error::Error as KeyringError;

    #[test]
    fn a_missing_entry_is_absence_rather_than_failure() {
        assert_eq!(classify(&KeyringError::NoEntry), Outcome::Absent);
    }

    #[test]
    fn a_keychain_that_cannot_be_reached_is_transient() {
        let error = map_error(Provider::GitHub, &KeyringError::NoDefaultStore);
        assert!(error.is_transient(), "got {error:?}");
        assert!(error.to_string().contains("keychain"));
    }

    #[test]
    fn a_platform_refusal_names_the_provider_so_the_wizard_can_offer_to_retry() {
        let error = map_error(
            Provider::Jira,
            &KeyringError::NotSupportedByStore("secrets".to_owned()),
        );
        assert!(error.to_string().contains("jira"), "got {error}");
    }

    #[test]
    fn an_unreadable_entry_is_reported_as_a_protocol_error_not_as_absence() {
        let error = map_error(Provider::Slack, &KeyringError::BadEncoding(vec![0xff]));
        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[test]
    fn an_ambiguous_entry_is_reported_rather_than_picked_at_random() {
        let error = map_error(Provider::GitHub, &KeyringError::Ambiguous(Vec::new()));
        assert!(matches!(error, PortError::Protocol { .. }));
        assert!(error.to_string().contains("more than one"));
    }

    #[test]
    fn the_store_reports_whether_the_platform_can_keep_secrets_at_all() {
        // Whatever this machine says, it must answer rather than panic, and
        // the answer must explain itself when it is no.
        match KeyringStore::availability() {
            Ok(()) => {}
            Err(error) => assert!(!error.to_string().is_empty()),
        }
    }
}
