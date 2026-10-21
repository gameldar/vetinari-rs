//! Credentials and the shape of each provider's authentication flow.
//!
//! Secrets live in the OS keychain, so what the domain models is the *key*
//! under which a secret is filed, the metadata needed to tell a usable
//! credential from a stale one, and a description of what the user must supply
//! to obtain one. The secret value itself is wrapped so that it cannot be
//! logged by accident.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use time::OffsetDateTime;

use super::ids::Slug;

/// A secret value. Never printed, never serialised implicitly.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the secret. Call this at the point of use, not before.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// An external service Vetinari authenticates against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provider {
    /// GitHub, for pull requests, issues, and CI status.
    GitHub,
    /// GitLab, for merge requests and pipelines.
    GitLab,
    /// Jira, for issue tracking.
    Jira,
    /// Slack, for messages and DMs.
    Slack,
    /// Discord, for messages.
    Discord,
    /// Google Calendar, for schedule signals.
    GoogleCalendar,
    /// Outlook / Microsoft Graph calendar.
    Outlook,
    /// Anthropic API, for Claude-backed agents.
    Anthropic,
    /// Google AI / Gemini API, for `agy`-backed agents.
    GoogleAi,
}

/// A configuration file named a provider that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider {name:?}")]
pub struct UnknownProvider {
    /// The unrecognised name.
    pub name: String,
}

impl Provider {
    /// Every provider Vetinari can authenticate against.
    pub const ALL: &'static [Provider] = &[
        Provider::GitHub,
        Provider::GitLab,
        Provider::Jira,
        Provider::Slack,
        Provider::Discord,
        Provider::GoogleCalendar,
        Provider::Outlook,
        Provider::Anthropic,
        Provider::GoogleAi,
    ];

    /// The stable name used in configuration and as a keychain namespace.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::GitLab => "gitlab",
            Provider::Jira => "jira",
            Provider::Slack => "slack",
            Provider::Discord => "discord",
            Provider::GoogleCalendar => "google-calendar",
            Provider::Outlook => "outlook",
            Provider::Anthropic => "anthropic",
            Provider::GoogleAi => "google-ai",
        }
    }

    /// The name shown in the TUI.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::GitLab => "GitLab",
            Provider::Jira => "Jira",
            Provider::Slack => "Slack",
            Provider::Discord => "Discord",
            Provider::GoogleCalendar => "Google Calendar",
            Provider::Outlook => "Outlook",
            Provider::Anthropic => "Anthropic",
            Provider::GoogleAi => "Google AI (Gemini)",
        }
    }

    /// How a credential for this provider is obtained.
    #[must_use]
    pub fn auth_spec(&self) -> AuthSpec {
        match self {
            Provider::GitHub => AuthSpec {
                provider: *self,
                instructions: "Create a fine-grained personal access token with read access to pull \
                     requests, issues, and checks.",
                method: AuthMethod::ApiToken {
                    fields: vec![
                        FieldSpec::optional_plain(
                            "base_url",
                            "API base URL (blank for github.com)",
                        ),
                        FieldSpec::secret("token", "Personal access token"),
                    ],
                },
            },
            Provider::GitLab => AuthSpec {
                provider: *self,
                instructions: "Create a project access token with the read_api scope.",
                method: AuthMethod::ApiToken {
                    fields: vec![
                        FieldSpec::optional_plain(
                            "base_url",
                            "API base URL (blank for gitlab.com)",
                        ),
                        FieldSpec::secret("token", "Access token"),
                    ],
                },
            },
            Provider::Jira => AuthSpec {
                provider: *self,
                instructions: "Create an API token at id.atlassian.com and pair it with the \
                               account email it belongs to.",
                method: AuthMethod::ApiToken {
                    fields: vec![
                        FieldSpec::plain("base_url", "Site URL, e.g. https://acme.atlassian.net"),
                        FieldSpec::plain("email", "Account email"),
                        FieldSpec::secret("api_token", "API token"),
                    ],
                },
            },
            Provider::Slack => AuthSpec {
                provider: *self,
                instructions: "Install a Slack app with channels:history, im:history, and \
                               chat:write, then copy its user or bot token.",
                method: AuthMethod::ApiToken {
                    fields: vec![FieldSpec::secret("token", "Slack token (xoxp-/xoxb-)")],
                },
            },
            Provider::Discord => AuthSpec {
                provider: *self,
                instructions: "Create a bot in the Discord developer portal and copy its token.",
                method: AuthMethod::ApiToken {
                    fields: vec![FieldSpec::secret("token", "Bot token")],
                },
            },
            Provider::GoogleCalendar => AuthSpec {
                provider: *self,
                instructions: "Create an OAuth client of type 'TV and limited input' in Google \
                               Cloud, then authorise Vetinari on another device.",
                method: AuthMethod::OAuthDeviceCode {
                    device_authorization_url: "https://oauth2.googleapis.com/device/code",
                    token_url: "https://oauth2.googleapis.com/token",
                    scopes: &["https://www.googleapis.com/auth/calendar.readonly"],
                    needs_client_secret: true,
                },
            },
            Provider::Outlook => AuthSpec {
                provider: *self,
                instructions: "Register an Entra ID application with the Calendars.Read \
                               delegated permission and enable public client flows.",
                method: AuthMethod::OAuthDeviceCode {
                    device_authorization_url: "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode",
                    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                    scopes: &["offline_access", "Calendars.Read"],
                    needs_client_secret: false,
                },
            },
            Provider::Anthropic => AuthSpec {
                provider: *self,
                instructions: "Only needed for direct API agents; the Claude Code CLI carries \
                               its own session.",
                method: AuthMethod::ApiToken {
                    fields: vec![FieldSpec::secret("api_key", "Anthropic API key")],
                },
            },
            Provider::GoogleAi => AuthSpec {
                provider: *self,
                instructions: "Create an API key in Google AI Studio for Gemini-backed agents.",
                method: AuthMethod::ApiToken {
                    fields: vec![FieldSpec::secret("api_key", "Google AI API key")],
                },
            },
        }
    }
}

impl FromStr for Provider {
    type Err = UnknownProvider;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Provider::ALL
            .iter()
            .copied()
            .find(|provider| provider.name() == raw.trim())
            .ok_or_else(|| UnknownProvider {
                name: raw.trim().to_owned(),
            })
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One value the user must supply during an API-token flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    /// The settings key this value populates.
    pub key: &'static str,
    /// The prompt shown in the wizard.
    pub prompt: &'static str,
    /// Whether the value belongs in the keychain rather than the config file.
    pub secret: bool,
    /// Whether the flow can proceed without it.
    pub required: bool,
}

impl FieldSpec {
    /// A required, non-secret field.
    #[must_use]
    pub const fn plain(key: &'static str, prompt: &'static str) -> Self {
        Self {
            key,
            prompt,
            secret: false,
            required: true,
        }
    }

    /// An optional, non-secret field.
    #[must_use]
    pub const fn optional_plain(key: &'static str, prompt: &'static str) -> Self {
        Self {
            key,
            prompt,
            secret: false,
            required: false,
        }
    }

    /// A required field whose value is stored in the keychain.
    #[must_use]
    pub const fn secret(key: &'static str, prompt: &'static str) -> Self {
        Self {
            key,
            prompt,
            secret: true,
            required: true,
        }
    }
}

/// How a provider's credential is obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// The user pastes a token, optionally alongside plain settings.
    ApiToken {
        /// The values to collect, in the order to prompt for them.
        fields: Vec<FieldSpec>,
    },
    /// RFC 8628 device authorisation: Vetinari shows a code, the user
    /// approves it elsewhere, and Vetinari polls for the token.
    OAuthDeviceCode {
        /// Endpoint that issues the device and user codes.
        device_authorization_url: &'static str,
        /// Endpoint polled for the access token.
        token_url: &'static str,
        /// Scopes to request.
        scopes: &'static [&'static str],
        /// Whether the provider also demands a client secret.
        needs_client_secret: bool,
    },
}

/// A provider's authentication requirements, ready to drive a wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSpec {
    /// The provider being authenticated.
    pub provider: Provider,
    /// Human-facing guidance shown above the prompts.
    pub instructions: &'static str,
    /// The mechanism to run.
    pub method: AuthMethod,
}

/// Where a credential is filed in the OS keychain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialKey {
    provider: Provider,
    account: Option<Slug>,
}

impl CredentialKey {
    /// Build a key, optionally scoped to a named account so that a work and a
    /// home profile can hold separate credentials for one provider.
    #[must_use]
    pub const fn new(provider: Provider, account: Option<Slug>) -> Self {
        Self { provider, account }
    }

    /// The provider this key belongs to.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// The keychain service name.
    #[must_use]
    pub fn service(&self) -> String {
        format!("vetinari.{}", self.provider.name())
    }

    /// The keychain account name.
    #[must_use]
    pub fn account(&self) -> &str {
        self.account.as_ref().map_or("default", Slug::as_str)
    }
}

impl fmt::Display for CredentialKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider.name(), self.account())
    }
}

/// Whether a stored credential can still be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// Usable, with time to spare.
    Valid,
    /// Usable, but due for renewal.
    NearingExpiry,
    /// Past its expiry; renew before use.
    Expired,
}

/// A credential as held in the keychain: the token, an optional refresh
/// token, and an optional expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCredential {
    token: Secret,
    refresh_token: Option<Secret>,
    expires_at: Option<OffsetDateTime>,
}

/// A keychain entry could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialDecodeError {
    /// The entry was blank.
    #[error("credential entry is empty")]
    Empty,
    /// The entry used the versioned format but its expiry was unreadable.
    #[error("credential expiry {found:?} is not an RFC 3339 timestamp")]
    BadExpiry {
        /// The unparseable timestamp.
        found: String,
    },
    /// The entry used the versioned format but was truncated.
    #[error("credential entry is truncated")]
    Truncated,
}

impl StoredCredential {
    /// The renewal window: a credential inside it still works but should be
    /// refreshed before the next long-running task is started with it.
    pub const RENEWAL_WINDOW: Duration = Duration::from_secs(5 * 60);

    const MAGIC: &'static str = "vetinari1";

    /// A credential with no expiry, as pasted by the user.
    #[must_use]
    pub const fn api_token(token: Secret) -> Self {
        Self {
            token,
            refresh_token: None,
            expires_at: None,
        }
    }

    /// A credential obtained from an OAuth flow.
    #[must_use]
    pub const fn oauth(
        token: Secret,
        refresh_token: Option<Secret>,
        expires_at: Option<OffsetDateTime>,
    ) -> Self {
        Self {
            token,
            refresh_token,
            expires_at,
        }
    }

    /// The access token or API key.
    #[must_use]
    pub const fn token(&self) -> &Secret {
        &self.token
    }

    /// The refresh token, when the provider issued one.
    #[must_use]
    pub const fn refresh_token(&self) -> Option<&Secret> {
        self.refresh_token.as_ref()
    }

    /// When the token stops working, when the provider said.
    #[must_use]
    pub const fn expires_at(&self) -> Option<OffsetDateTime> {
        self.expires_at
    }

    /// Whether the credential is usable at `now`.
    #[must_use]
    pub fn status(&self, now: OffsetDateTime) -> CredentialStatus {
        match self.expires_at {
            None => CredentialStatus::Valid,
            Some(expiry) if now >= expiry => CredentialStatus::Expired,
            Some(expiry) => {
                let remaining = expiry - now;
                if remaining
                    <= time::Duration::try_from(Self::RENEWAL_WINDOW)
                        .expect("the renewal window fits in a time::Duration")
                {
                    CredentialStatus::NearingExpiry
                } else {
                    CredentialStatus::Valid
                }
            }
        }
    }

    /// Render for storage in a single keychain entry.
    ///
    /// The token is last so that it may contain anything, including newlines.
    #[must_use]
    pub fn encode(&self) -> String {
        let expiry = self
            .expires_at
            .and_then(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .unwrap_or_else(|| "-".to_owned());
        let refresh = self
            .refresh_token
            .as_ref()
            .map_or("-", Secret::expose)
            .to_owned();
        format!(
            "{}\n{}\n{}\n{}",
            Self::MAGIC,
            expiry,
            refresh,
            self.token.expose()
        )
    }

    /// Read back an entry written by [`Self::encode`].
    ///
    /// An entry that does not use the versioned format is taken to be a bare
    /// token, so a value put in the keychain by hand still works.
    pub fn decode(raw: &str) -> Result<Self, CredentialDecodeError> {
        if raw.trim().is_empty() {
            return Err(CredentialDecodeError::Empty);
        }
        let Some(body) = raw
            .strip_prefix(Self::MAGIC)
            .and_then(|r| r.strip_prefix('\n'))
        else {
            return Ok(Self::api_token(Secret::new(raw.trim())));
        };
        let mut parts = body.splitn(3, '\n');
        let expiry = parts.next().ok_or(CredentialDecodeError::Truncated)?;
        let refresh = parts.next().ok_or(CredentialDecodeError::Truncated)?;
        let token = parts.next().ok_or(CredentialDecodeError::Truncated)?;
        let expires_at = match expiry {
            "-" => None,
            other => Some(
                OffsetDateTime::parse(other, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| CredentialDecodeError::BadExpiry {
                        found: other.to_owned(),
                    })?,
            ),
        };
        Ok(Self {
            token: Secret::new(token),
            refresh_token: match refresh {
                "-" => None,
                other => Some(Secret::new(other)),
            },
            expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("slug")
    }

    #[test]
    fn a_secret_never_prints_its_contents() {
        let secret = Secret::new("ghp_supersecret");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(secret.expose(), "ghp_supersecret");
    }

    #[test]
    fn secrets_compare_without_revealing_themselves() {
        assert_eq!(Secret::new("a"), Secret::new("a"));
        assert_ne!(Secret::new("a"), Secret::new("b"));
    }

    #[test]
    fn a_credential_key_maps_to_a_namespaced_keyring_entry() {
        let key = CredentialKey::new(Provider::GitHub, None);
        assert_eq!(key.service(), "vetinari.github");
        assert_eq!(key.account(), "default");

        let scoped = CredentialKey::new(Provider::Jira, Some(slug("work")));
        assert_eq!(scoped.service(), "vetinari.jira");
        assert_eq!(scoped.account(), "work");
    }

    #[test]
    fn providers_round_trip_through_their_config_names() {
        for provider in Provider::ALL {
            assert_eq!(
                provider.name().parse::<Provider>().expect("known provider"),
                *provider
            );
            assert!(!provider.display_name().is_empty());
        }
    }

    #[test]
    fn the_prd_providers_are_all_known() {
        for name in [
            "github",
            "gitlab",
            "jira",
            "slack",
            "discord",
            "google-calendar",
            "outlook",
            "anthropic",
            "google-ai",
        ] {
            assert!(name.parse::<Provider>().is_ok(), "{name} is missing");
        }
    }

    #[test]
    fn a_credential_without_an_expiry_never_expires() {
        let credential = StoredCredential::api_token(Secret::new("t"));
        assert_eq!(
            credential.status(datetime!(2026-10-21 09:00 UTC)),
            CredentialStatus::Valid
        );
        assert!(credential.refresh_token().is_none());
    }

    #[test]
    fn an_expired_credential_reports_itself_expired() {
        let credential = StoredCredential::oauth(
            Secret::new("access"),
            Some(Secret::new("refresh")),
            Some(datetime!(2026-10-21 09:00 UTC)),
        );
        assert_eq!(
            credential.status(datetime!(2026-10-21 08:00 UTC)),
            CredentialStatus::Valid
        );
        assert_eq!(
            credential.status(datetime!(2026-10-21 09:00 UTC)),
            CredentialStatus::Expired
        );
        assert_eq!(
            credential.status(datetime!(2026-10-21 09:30 UTC)),
            CredentialStatus::Expired
        );
        assert!(credential.refresh_token().is_some());
    }

    #[test]
    fn a_credential_close_to_expiry_is_flagged_for_renewal() {
        let credential = StoredCredential::oauth(
            Secret::new("access"),
            Some(Secret::new("refresh")),
            Some(datetime!(2026-10-21 09:00 UTC)),
        );
        assert_eq!(
            credential.status(datetime!(2026-10-21 08:56 UTC)),
            CredentialStatus::NearingExpiry
        );
    }

    #[test]
    fn credentials_serialise_for_storage_and_come_back_intact() {
        let credential = StoredCredential::oauth(
            Secret::new("access"),
            Some(Secret::new("refresh")),
            Some(datetime!(2026-10-21 09:00 UTC)),
        );
        let encoded = credential.encode();
        let decoded = StoredCredential::decode(&encoded).expect("decodes");
        assert_eq!(decoded, credential);
    }

    #[test]
    fn a_bare_token_decodes_as_an_api_token_for_hand_written_entries() {
        let decoded = StoredCredential::decode("ghp_pasted_by_hand").expect("decodes");
        assert_eq!(decoded.token().expose(), "ghp_pasted_by_hand");
        assert_eq!(
            decoded.status(datetime!(2026-10-21 09:00 UTC)),
            CredentialStatus::Valid
        );
    }

    #[test]
    fn every_provider_declares_how_to_authenticate_with_it() {
        for provider in Provider::ALL {
            let spec = provider.auth_spec();
            match &spec.method {
                AuthMethod::ApiToken { fields } => {
                    assert!(!fields.is_empty(), "{} has no fields", provider.name());
                    assert!(
                        fields.iter().any(|field| field.secret),
                        "{} has no secret field",
                        provider.name()
                    );
                }
                AuthMethod::OAuthDeviceCode { scopes, .. } => {
                    assert!(!scopes.is_empty(), "{} requests no scopes", provider.name());
                }
            }
            assert!(!spec.instructions.is_empty());
        }
    }

    #[test]
    fn field_specs_name_the_setting_they_populate() {
        let spec = Provider::Jira.auth_spec();
        let AuthMethod::ApiToken { fields } = &spec.method else {
            panic!("Jira uses an API token");
        };
        let keys: Vec<_> = fields.iter().map(|field| field.key).collect();
        assert_eq!(keys, vec!["base_url", "email", "api_token"]);
        assert!(!fields[0].secret);
        assert!(fields[2].secret);
    }
}
