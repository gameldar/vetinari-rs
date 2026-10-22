//! Start-up: where configuration is looked for, and what `doctor` reports.
//!
//! The parts that can be decided without a terminal live here so they can be
//! tested; the binary only does the terminal mechanics.

use std::path::{Path, PathBuf};

use vetinari_adapters::config::{APP_DIR, candidate_paths};
use vetinari_core::application::auth::ProviderStatus;
use vetinari_core::application::config::{AgentConfig, Config};
use vetinari_core::domain::ids::Slug;
use vetinari_core::domain::profile::ProfileSet;

/// Where to look for configuration, in order.
///
/// An explicit path is used alone: if the developer named a file, failing to
/// find it should be an error rather than a silent fall back to another one.
#[must_use]
pub fn config_candidates(explicit: Option<PathBuf>, config_dir: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path];
    }
    config_dir.map(candidate_paths).unwrap_or_default()
}

/// Where the skill library lives, given a config directory.
#[must_use]
pub fn skills_path(config_dir: &Path) -> PathBuf {
    config_dir.join(APP_DIR).join("skills.toml")
}

/// Which credential account a command-line invocation should read and write.
///
/// The interface scopes credentials by the active profile's account, so
/// `auth` and `doctor` have to agree with it: storing a token the interface
/// then reports as missing is worse than not storing one at all. An explicit
/// `--account` wins, for setting up a profile that is not the active one.
///
/// # Errors
///
/// Returns the name unchanged when it is not a usable account name.
pub fn account_for(explicit: Option<&str>, profiles: &ProfileSet) -> Result<Option<Slug>, String> {
    match explicit {
        Some(name) => Slug::parse(name)
            .map(Some)
            .map_err(|error| format!("{name:?} is not a usable account name: {error}")),
        None => Ok(profiles.active().account().cloned()),
    }
}

/// How to describe the account a credential is filed under.
#[must_use]
pub fn account_label(account: Option<&Slug>) -> &str {
    account.map_or("default", Slug::as_str)
}

/// What `vetinari doctor` prints.
#[must_use]
pub fn doctor_report(
    config: &Config,
    config_path: Option<&Path>,
    problems: &[String],
    missing_engines: &[AgentConfig],
    statuses: &[ProviderStatus],
    required: &[vetinari_core::domain::credentials::Provider],
    keychain: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(match config_path {
        Some(path) => format!("config: {}", path.display()),
        None => "config: built-in defaults (no config file found)".to_owned(),
    });
    lines.push(format!(
        "profiles: {} (active: {})",
        config.profiles.len(),
        config.profiles.active().id()
    ));
    lines.push(format!(
        "sources: {}",
        if config.sources.is_empty() {
            "none configured".to_owned()
        } else {
            config
                .sources
                .iter()
                .map(|source| source.id.as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    lines.push(format!(
        "targets: {}",
        config
            .targets
            .iter()
            .map(|target| target.kind.name().to_owned())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    if let Some(keychain) = keychain {
        lines.push(format!("keychain: {keychain}"));
    }
    lines.push(format!(
        "credential account: {}",
        account_label(config.profiles.active().account())
    ));

    for engine in &config.agents {
        let state = if missing_engines
            .iter()
            .any(|missing| missing.id == engine.id)
        {
            "NOT FOUND"
        } else {
            "ok"
        };
        lines.push(format!(
            "engine {}: {} ({})",
            engine.id, engine.command, state
        ));
    }

    for provider in required {
        let status = statuses.iter().find(|status| status.provider == *provider);
        let state = status.map_or("unknown", |status| status.state.label());
        lines.push(format!("credential {}: {state}", provider.name()));
    }

    for problem in problems {
        lines.push(format!("problem: {problem}"));
    }

    let healthy = missing_engines.is_empty()
        && problems.is_empty()
        && required.iter().all(|provider| {
            statuses
                .iter()
                .find(|status| status.provider == *provider)
                .is_some_and(ProviderStatus::is_usable)
        });
    lines.push(if healthy {
        "all good".to_owned()
    } else {
        "some things need attention (see above)".to_owned()
    });

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use vetinari_core::application::auth::{AuthState, ProviderStatus};
    use vetinari_core::application::config::{AgentKind, ParseContext, RawConfig};
    use vetinari_core::domain::credentials::{CredentialKey, Provider};
    use vetinari_core::domain::ids::AgentId;

    fn config(toml_text: &str) -> Config {
        let raw: RawConfig = toml::from_str(toml_text).expect("valid TOML");
        Config::parse(raw, &ParseContext::default()).expect("valid config")
    }

    fn status(provider: Provider, state: AuthState) -> ProviderStatus {
        ProviderStatus {
            provider,
            key: CredentialKey::new(provider, None),
            state,
            expires_at: Some(datetime!(2026-10-21 09:00 UTC)),
        }
    }

    #[test]
    fn an_explicit_config_path_is_the_only_candidate() {
        let candidates = config_candidates(
            Some(PathBuf::from("/tmp/mine.toml")),
            Some(Path::new("/home/dev/.config")),
        );
        assert_eq!(candidates, vec![PathBuf::from("/tmp/mine.toml")]);
    }

    #[test]
    fn without_an_explicit_path_the_config_directory_is_searched() {
        let candidates = config_candidates(None, Some(Path::new("/home/dev/.config")));
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0].ends_with("vetinari/config.toml"));
    }

    #[test]
    fn with_no_config_directory_there_is_nothing_to_search() {
        assert!(config_candidates(None, None).is_empty());
    }

    #[test]
    fn the_skill_library_sits_beside_the_config() {
        assert_eq!(
            skills_path(Path::new("/home/dev/.config")),
            PathBuf::from("/home/dev/.config/vetinari/skills.toml")
        );
    }

    #[test]
    fn a_healthy_setup_says_so() {
        let config = config(
            r#"
            [sources.github]
            kind = "github"
            "#,
        );
        let report = doctor_report(
            &config,
            Some(Path::new("/home/dev/.config/vetinari/config.toml")),
            &[],
            &[],
            &[status(Provider::GitHub, AuthState::Valid)],
            &[Provider::GitHub],
            Some("Keychain: 1 authenticated"),
        );

        assert!(report.iter().any(|line| line.contains("config.toml")));
        assert!(
            report
                .iter()
                .any(|line| line == "credential github: authenticated")
        );
        assert!(
            report
                .iter()
                .any(|line| line.contains("engine claude-code"))
        );
        assert_eq!(report.last().expect("a verdict"), "all good");
    }

    #[test]
    fn a_missing_engine_is_called_out_and_the_verdict_changes() {
        let config = config("");
        let report = doctor_report(
            &config,
            None,
            &[],
            &[AgentConfig {
                id: AgentId::parse("claude-code").expect("id"),
                kind: AgentKind::ClaudeCode,
                command: "claude".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            }],
            &[],
            &[],
            None,
        );

        assert!(
            report
                .iter()
                .any(|line| line.contains("claude-code") && line.contains("NOT FOUND"))
        );
        assert!(report.last().expect("a verdict").contains("attention"));
    }

    #[test]
    fn a_credential_that_is_needed_but_missing_changes_the_verdict() {
        let config = config(
            r#"
            [sources.github]
            kind = "github"
            "#,
        );
        let report = doctor_report(
            &config,
            None,
            &[],
            &[],
            &[status(Provider::GitHub, AuthState::Missing)],
            &[Provider::GitHub],
            None,
        );

        assert!(
            report
                .iter()
                .any(|line| line == "credential github: not configured")
        );
        assert!(report.last().expect("a verdict").contains("attention"));
    }

    #[test]
    fn a_source_without_an_adapter_is_listed_as_a_problem() {
        let config = config("");
        let report = doctor_report(
            &config,
            None,
            &["source \"gl\" is configured but its adapter is not implemented yet".to_owned()],
            &[],
            &[],
            &[],
            None,
        );
        assert!(report.iter().any(|line| line.starts_with("problem:")));
        assert!(report.last().expect("a verdict").contains("attention"));
    }

    #[test]
    fn the_report_says_when_no_config_file_was_found() {
        let report = doctor_report(&config(""), None, &[], &[], &[], &[], None);
        assert!(report[0].contains("built-in defaults"));
        assert!(report.iter().any(|line| line.contains("none configured")));
    }
    #[test]
    fn the_command_line_defaults_to_the_active_profiles_account() {
        // The symptom this fixes: `vetinari auth github` stored a credential
        // under "default" while the interface looked under "work" and
        // reported "Keychain: none configured".
        let config = config(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            account = "work"
            "#,
        );

        let account = account_for(None, &config.profiles).expect("an account");

        assert_eq!(account.as_ref().map(Slug::as_str), Some("work"));
    }

    #[test]
    fn a_profile_with_no_account_uses_the_default_one() {
        let config = config(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            "#,
        );

        assert_eq!(account_for(None, &config.profiles).expect("ok"), None);
        assert_eq!(account_label(None), "default");
    }

    #[test]
    fn an_explicit_account_wins_so_another_profile_can_be_set_up() {
        let config = config(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            account = "work"
            "#,
        );

        let account = account_for(Some("home"), &config.profiles).expect("an account");

        assert_eq!(account.as_ref().map(Slug::as_str), Some("home"));
        assert_eq!(account_label(account.as_ref()), "home");
    }

    #[test]
    fn an_account_name_that_cannot_be_a_key_is_refused_rather_than_mangled() {
        let config = config(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            "#,
        );

        let error = account_for(Some("not a slug!"), &config.profiles).expect_err("refused");

        assert!(error.contains("not a slug!"), "{error}");
    }

    #[test]
    fn doctor_says_which_account_it_read() {
        let config = config(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            account = "work"
            "#,
        );

        let report = doctor_report(
            &config,
            None,
            &[],
            &[],
            &[],
            &[],
            Some("Keychain: none configured"),
        );

        assert!(
            report.iter().any(|line| line == "credential account: work"),
            "a keychain line without the account it read is what made this confusing: {report:?}"
        );
    }
}
