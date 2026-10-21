//! Configuration: the parse boundary between files and the domain.
//!
//! [`RawConfig`] mirrors the file format and is deliberately forgiving in
//! type: every field is optional, strings are strings. [`Config::parse`] turns
//! it into domain values, and reports *every* problem it finds rather than the
//! first, because fixing a config file one error per run is miserable.
//!
//! Both TOML and YAML deserialise into `RawConfig`; which one a file is, is
//! the loader's business, not this module's.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::action::Action;
use crate::domain::agent::TaskClass;
use crate::domain::credentials::{AuthMethod, Provider};
use crate::domain::ids::{AgentId, ProfileId, Slug, SourceId};
use crate::domain::keymap::{Keymap, Mode};
use crate::domain::keys::KeySequence;
use crate::domain::profile::{Profile, ProfileSet};
use crate::domain::routing::RoutingTable;
use crate::domain::text::NonEmptyText;
use crate::domain::workspace::TargetKind;

/// One thing wrong with a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProblem {
    /// Where in the file, in dotted form.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// A configuration file that could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError(Vec<ConfigProblem>);

impl ConfigError {
    /// Every problem found.
    #[must_use]
    pub fn problems(&self) -> &[ConfigProblem] {
        &self.0
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self.0.iter().map(ConfigProblem::to_string).collect();
        write!(f, "{}", rendered.join("; "))
    }
}

/// What the parser needs to know about the machine it is running on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseContext {
    /// The home directory, for expanding `~`.
    pub home: Option<PathBuf>,
}

/// Which engine family an agent entry drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentKind {
    /// The Claude Code CLI.
    ClaudeCode,
    /// Gemini through `agy`.
    Agy,
    /// Any other command or script.
    Script,
}

impl AgentKind {
    /// Every kind.
    pub const ALL: &'static [AgentKind] =
        &[AgentKind::ClaudeCode, AgentKind::Agy, AgentKind::Script];

    /// The name used in configuration.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Agy => "agy",
            AgentKind::Script => "script",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.name() == raw.trim())
    }
}

/// A configured agent engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// The engine's id, as routing refers to it.
    pub id: AgentId,
    /// Which family it belongs to.
    pub kind: AgentKind,
    /// The executable to run.
    pub command: String,
    /// Fixed arguments, before the prompt.
    pub args: Vec<String>,
    /// Extra environment.
    pub env: Vec<(String, String)>,
}

/// Settings specific to one kind of workspace target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSettings {
    /// Herdr, over its Unix socket.
    Herdr {
        /// The socket path, or the default when absent.
        socket: Option<PathBuf>,
    },
    /// A dedicated tmux session.
    Tmux {
        /// The session name Vetinari owns.
        session: String,
    },
    /// An external editor.
    Editor {
        /// The executable to run.
        command: String,
        /// Fixed arguments, before the path.
        args: Vec<String>,
    },
}

/// A configured workspace target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetConfig {
    /// Which backend.
    pub kind: TargetKind,
    /// Its settings.
    pub settings: TargetSettings,
}

/// Settings specific to one kind of attention source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSettings {
    /// GitHub pull requests and checks.
    GitHub {
        /// The search query that defines the inbox.
        query: String,
        /// An enterprise API base URL, when not github.com.
        base_url: Option<String>,
    },
    /// GitLab merge requests.
    GitLab {
        /// The project path, e.g. `group/project`.
        project: String,
        /// An API base URL, when not gitlab.com.
        base_url: Option<String>,
    },
    /// Jira issues.
    Jira {
        /// The site URL.
        base_url: String,
        /// The account email the API token belongs to.
        email: String,
        /// The JQL that defines the inbox.
        jql: String,
    },
    /// Slack conversations.
    Slack {
        /// Channel ids to watch; empty means direct messages only.
        channels: Vec<String>,
    },
    /// Discord channels.
    Discord {
        /// Channel ids to watch.
        channels: Vec<String>,
    },
    /// Google Calendar.
    GoogleCalendar {
        /// Which calendar.
        calendar_id: String,
        /// How far ahead to look.
        lookahead_hours: u16,
    },
    /// Outlook calendar.
    Outlook {
        /// How far ahead to look.
        lookahead_hours: u16,
    },
}

/// A configured attention source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    /// The source's id, as profiles refer to it.
    pub id: SourceId,
    /// Its settings.
    pub settings: SourceSettings,
}

/// An OAuth client registered with a provider, for the device-code flow.
///
/// The client id and secret identify the *application*, not the developer, so
/// they live in configuration rather than the keychain; the tokens they obtain
/// are what go in the keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientConfig {
    /// The provider it authenticates against.
    pub provider: Provider,
    /// The client id.
    pub client_id: String,
    /// The client secret, when the provider demands one.
    pub client_secret: Option<String>,
}

/// Behaviour that is not specific to a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct General {
    /// How often the UI redraws and polls, in milliseconds.
    pub tick_rate_ms: u64,
    /// How many log lines to keep for feedback capture.
    pub log_lines: usize,
    /// How often sources are refreshed, in seconds.
    pub refresh_secs: u64,
}

impl General {
    /// The default redraw interval.
    pub const DEFAULT_TICK_RATE_MS: u64 = 250;
    /// The fastest redraw interval accepted.
    pub const MIN_TICK_RATE_MS: u64 = 16;
    /// The default number of log lines kept.
    pub const DEFAULT_LOG_LINES: usize = 200;
    /// The fewest log lines accepted.
    pub const MIN_LOG_LINES: usize = 20;
    /// The default source refresh interval.
    pub const DEFAULT_REFRESH_SECS: u64 = 60;
    /// The shortest source refresh interval accepted.
    pub const MIN_REFRESH_SECS: u64 = 5;
}

impl Default for General {
    fn default() -> Self {
        Self {
            tick_rate_ms: Self::DEFAULT_TICK_RATE_MS,
            log_lines: Self::DEFAULT_LOG_LINES,
            refresh_secs: Self::DEFAULT_REFRESH_SECS,
        }
    }
}

/// Vetinari's configuration, parsed and ready to use.
#[derive(Debug)]
pub struct Config {
    /// General behaviour.
    pub general: General,
    /// The context profiles, with one active.
    pub profiles: ProfileSet,
    /// The keymap, defaults plus overrides.
    pub keymap: Keymap,
    /// The agent engines.
    pub agents: Vec<AgentConfig>,
    /// The workspace targets.
    pub targets: Vec<TargetConfig>,
    /// The attention sources.
    pub sources: Vec<SourceConfig>,
    /// The OAuth clients for device-code flows.
    pub oauth_clients: Vec<OAuthClientConfig>,
}

impl Config {
    /// Parse a raw configuration into domain values.
    pub fn parse(raw: RawConfig, context: &ParseContext) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();

        let general = parse_general(raw.general.clone());
        let profiles = parse_profiles(&raw.profiles, context, &mut problems);
        let keymap = parse_keymap(&raw.keymap, &mut problems);
        let agents = parse_agents(&raw.agents, &mut problems);
        let targets = parse_targets(&raw.targets, context, &mut problems);
        let sources = parse_sources(&raw.sources, &mut problems);
        let oauth_clients = parse_oauth(&raw.oauth, &mut problems);

        let profiles = match profiles {
            Some(mut set) => {
                if let Some(active) = &raw.general.profile {
                    match ProfileId::parse(active) {
                        Ok(id) if set.activate(&id) => {}
                        Ok(id) => problems.push(ConfigProblem {
                            path: "general.profile".to_owned(),
                            message: format!("no profile named {id} is defined"),
                        }),
                        Err(error) => problems.push(ConfigProblem {
                            path: "general.profile".to_owned(),
                            message: error.to_string(),
                        }),
                    }
                }
                Some(set)
            }
            None => None,
        };

        if !problems.is_empty() {
            return Err(ConfigError(problems));
        }

        Ok(Self {
            general,
            profiles: profiles.expect("profiles parse or report a problem"),
            keymap,
            agents,
            targets,
            sources,
            oauth_clients,
        })
    }

    /// The OAuth client registered for a provider, if any.
    #[must_use]
    pub fn oauth_client(&self, provider: Provider) -> Option<&OAuthClientConfig> {
        self.oauth_clients
            .iter()
            .find(|client| client.provider == provider)
    }

    /// The agent ids every routing table may refer to.
    #[must_use]
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.iter().map(|agent| agent.id.clone()).collect()
    }

    /// The agents Vetinari assumes when a file names none.
    #[must_use]
    pub fn default_agents() -> Vec<AgentConfig> {
        vec![
            AgentConfig {
                id: AgentId::parse("claude-code").expect("built-in agent id is valid"),
                kind: AgentKind::ClaudeCode,
                command: "claude".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            },
            AgentConfig {
                id: AgentId::parse("agy-gemini").expect("built-in agent id is valid"),
                kind: AgentKind::Agy,
                command: "agy".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            },
            AgentConfig {
                id: AgentId::parse("script").expect("built-in agent id is valid"),
                kind: AgentKind::Script,
                command: "sh".to_owned(),
                args: vec!["-c".to_owned()],
                env: Vec::new(),
            },
        ]
    }

    /// The workspace target Vetinari assumes when a file names none: a tmux
    /// session of its own, which needs nothing installed beyond tmux.
    #[must_use]
    pub fn default_targets() -> Vec<TargetConfig> {
        vec![TargetConfig {
            kind: TargetKind::Tmux,
            settings: TargetSettings::Tmux {
                session: "vetinari".to_owned(),
            },
        }]
    }
}

fn parse_general(raw: RawGeneral) -> General {
    let defaults = General::default();
    General {
        tick_rate_ms: raw
            .tick_rate_ms
            .unwrap_or(defaults.tick_rate_ms)
            .max(General::MIN_TICK_RATE_MS),
        log_lines: raw
            .log_lines
            .unwrap_or(defaults.log_lines)
            .max(General::MIN_LOG_LINES),
        refresh_secs: raw
            .refresh_secs
            .unwrap_or(defaults.refresh_secs)
            .max(General::MIN_REFRESH_SECS),
    }
}

fn parse_profiles(
    raw: &[RawProfile],
    context: &ParseContext,
    problems: &mut Vec<ConfigProblem>,
) -> Option<ProfileSet> {
    if raw.is_empty() {
        return Some(ProfileSet::defaults());
    }

    let mut parsed = Vec::with_capacity(raw.len());
    for (index, entry) in raw.iter().enumerate() {
        let at = |field: &str| format!("profiles[{index}].{field}");
        let id = match ProfileId::parse(&entry.id) {
            Ok(id) => id,
            Err(error) => {
                problems.push(ConfigProblem {
                    path: at("id"),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let name = match NonEmptyText::parse(entry.name.as_deref().unwrap_or(&entry.id)) {
            Ok(name) => name,
            Err(error) => {
                problems.push(ConfigProblem {
                    path: at("name"),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let target = match entry.target.as_deref() {
            None => TargetKind::Tmux,
            Some(raw_kind) => match raw_kind.parse::<TargetKind>() {
                Ok(kind) => kind,
                Err(error) => {
                    problems.push(ConfigProblem {
                        path: at("target"),
                        message: error.to_string(),
                    });
                    continue;
                }
            },
        };

        let mut profile = Profile::new(id, name, target);
        for (source_index, source) in entry.sources.iter().enumerate() {
            match SourceId::parse(source) {
                Ok(source) => profile = profile.with_source(source),
                Err(error) => problems.push(ConfigProblem {
                    path: at(&format!("sources[{source_index}]")),
                    message: error.to_string(),
                }),
            }
        }
        for root in &entry.repo_roots {
            profile = profile.with_repo_root(expand(root, context));
        }
        if let Some(root) = &entry.worktree_root {
            profile = profile.with_worktree_root(expand(root, context));
        }
        if let Some(account) = &entry.account {
            match Slug::parse(account) {
                Ok(account) => profile = profile.with_account(account),
                Err(error) => problems.push(ConfigProblem {
                    path: at("account"),
                    message: error.to_string(),
                }),
            }
        }
        if let Some(routing) = parse_routing(&entry.routing, index, problems) {
            profile = profile.with_routing(routing);
        }
        parsed.push(profile);
    }

    let mut iter = parsed.into_iter();
    let first = iter.next()?;
    match ProfileSet::new(first, iter.collect()) {
        Ok(set) => Some(set),
        Err(error) => {
            problems.push(ConfigProblem {
                path: "profiles".to_owned(),
                message: error.to_string(),
            });
            None
        }
    }
}

fn parse_routing(
    raw: &BTreeMap<String, String>,
    profile_index: usize,
    problems: &mut Vec<ConfigProblem>,
) -> Option<RoutingTable> {
    if raw.is_empty() {
        return None;
    }
    let mut fallback = AgentId::parse("claude-code").expect("built-in agent id is valid");
    if let Some(default) = raw.get("default") {
        match AgentId::parse(default) {
            Ok(agent) => fallback = agent,
            Err(error) => problems.push(ConfigProblem {
                path: format!("profiles[{profile_index}].routing.default"),
                message: error.to_string(),
            }),
        }
    }
    let mut table = RoutingTable::new(fallback);
    for (key, value) in raw {
        if key == "default" {
            continue;
        }
        let path = format!("profiles[{profile_index}].routing.{key}");
        let class = match key.parse::<TaskClass>() {
            Ok(class) => class,
            Err(error) => {
                problems.push(ConfigProblem {
                    path,
                    message: error.to_string(),
                });
                continue;
            }
        };
        match AgentId::parse(value) {
            Ok(agent) => table = table.with_rule(class, agent),
            Err(error) => problems.push(ConfigProblem {
                path,
                message: error.to_string(),
            }),
        }
    }
    Some(table)
}

fn parse_keymap(raw: &RawKeymap, problems: &mut Vec<ConfigProblem>) -> Keymap {
    let mut keymap = if raw.defaults.unwrap_or(true) {
        Keymap::doom_defaults()
    } else {
        Keymap::empty()
    };

    for (mode, spellings) in [
        (Mode::Normal, &raw.unbind.normal),
        (Mode::Insert, &raw.unbind.insert),
    ] {
        for spelling in spellings {
            match KeySequence::parse(spelling) {
                Ok(sequence) => {
                    keymap.unbind(mode, &sequence);
                }
                Err(error) => problems.push(ConfigProblem {
                    path: format!("keymap.unbind.{}.{spelling}", mode.name()),
                    message: error.to_string(),
                }),
            }
        }
    }

    for (mode, bindings) in [(Mode::Normal, &raw.normal), (Mode::Insert, &raw.insert)] {
        for (spelling, action_name) in bindings {
            let path = format!("keymap.{}.{spelling}", mode.name());
            let sequence = match KeySequence::parse(spelling) {
                Ok(sequence) => sequence,
                Err(error) => {
                    problems.push(ConfigProblem {
                        path,
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let action = match action_name.parse::<Action>() {
                Ok(action) => action,
                Err(error) => {
                    problems.push(ConfigProblem {
                        path,
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            // Rebinding an exact sequence is intended; replace it quietly.
            keymap.unbind(mode, &sequence);
            if let Err(error) = keymap.bind(mode, sequence, action) {
                problems.push(ConfigProblem {
                    path,
                    message: error.to_string(),
                });
            }
        }
    }

    keymap
}

fn parse_agents(
    raw: &BTreeMap<String, RawAgent>,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<AgentConfig> {
    if raw.is_empty() {
        return Config::default_agents();
    }
    let mut agents = Vec::with_capacity(raw.len());
    for (name, entry) in raw {
        let id = match AgentId::parse(name) {
            Ok(id) => id,
            Err(error) => {
                problems.push(ConfigProblem {
                    path: format!("agents.{name}"),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let Some(kind) = AgentKind::parse(&entry.kind) else {
            problems.push(ConfigProblem {
                path: format!("agents.{name}.kind"),
                message: format!(
                    "unknown agent kind {:?}; expected one of claude-code, agy, script",
                    entry.kind
                ),
            });
            continue;
        };
        agents.push(AgentConfig {
            id,
            kind,
            command: entry.command.clone(),
            args: entry.args.clone(),
            env: entry
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        });
    }
    agents
}

fn parse_targets(
    raw: &BTreeMap<String, RawTarget>,
    context: &ParseContext,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<TargetConfig> {
    if raw.is_empty() {
        return Config::default_targets();
    }
    let mut targets = Vec::with_capacity(raw.len());
    for (name, entry) in raw {
        let kind = match name.parse::<TargetKind>() {
            Ok(kind) => kind,
            Err(error) => {
                problems.push(ConfigProblem {
                    path: format!("targets.{name}"),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let settings = match kind {
            TargetKind::Herdr => TargetSettings::Herdr {
                socket: entry.socket.as_ref().map(|path| expand(path, context)),
            },
            TargetKind::Tmux => TargetSettings::Tmux {
                session: entry
                    .session
                    .clone()
                    .unwrap_or_else(|| "vetinari".to_owned()),
            },
            TargetKind::Editor => match &entry.command {
                Some(command) => TargetSettings::Editor {
                    command: command.clone(),
                    args: entry.args.clone(),
                },
                None => {
                    problems.push(ConfigProblem {
                        path: format!("targets.{name}.command"),
                        message: "an editor target needs the command to run".to_owned(),
                    });
                    continue;
                }
            },
        };
        targets.push(TargetConfig { kind, settings });
    }
    targets
}

fn parse_sources(
    raw: &BTreeMap<String, RawSource>,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<SourceConfig> {
    let mut sources = Vec::with_capacity(raw.len());
    for (name, entry) in raw {
        let id = match SourceId::parse(name) {
            Ok(id) => id,
            Err(error) => {
                problems.push(ConfigProblem {
                    path: format!("sources.{name}"),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let at = |field: &str| format!("sources.{name}.{field}");
        let required = |value: &Option<String>| -> Option<String> {
            match value {
                Some(value) if !value.trim().is_empty() => Some(value.clone()),
                _ => None,
            }
        };

        let settings = match entry.kind.as_str() {
            "github" => SourceSettings::GitHub {
                query: entry
                    .query
                    .clone()
                    .unwrap_or_else(|| "is:open is:pr review-requested:@me".to_owned()),
                base_url: entry.base_url.clone(),
            },
            "gitlab" => match required(&entry.project) {
                Some(project) => SourceSettings::GitLab {
                    project,
                    base_url: entry.base_url.clone(),
                },
                None => {
                    problems.push(ConfigProblem {
                        path: at("project"),
                        message: "a GitLab source needs the project path".to_owned(),
                    });
                    continue;
                }
            },
            "jira" => {
                let base_url = required(&entry.base_url);
                let email = required(&entry.email);
                if base_url.is_none() {
                    problems.push(ConfigProblem {
                        path: at("base_url"),
                        message: "a Jira source needs the site URL".to_owned(),
                    });
                }
                if email.is_none() {
                    problems.push(ConfigProblem {
                        path: at("email"),
                        message: "a Jira source needs the account email the token belongs to"
                            .to_owned(),
                    });
                }
                match (base_url, email) {
                    (Some(base_url), Some(email)) => SourceSettings::Jira {
                        base_url,
                        email,
                        jql: entry.jql.clone().unwrap_or_else(|| {
                            "assignee = currentUser() AND statusCategory != Done".to_owned()
                        }),
                    },
                    _ => continue,
                }
            }
            "slack" => SourceSettings::Slack {
                channels: entry.channels.clone(),
            },
            "discord" => SourceSettings::Discord {
                channels: entry.channels.clone(),
            },
            "google-calendar" => SourceSettings::GoogleCalendar {
                calendar_id: entry
                    .calendar_id
                    .clone()
                    .unwrap_or_else(|| "primary".to_owned()),
                lookahead_hours: entry.lookahead_hours.unwrap_or(12),
            },
            "outlook" => SourceSettings::Outlook {
                lookahead_hours: entry.lookahead_hours.unwrap_or(12),
            },
            other => {
                problems.push(ConfigProblem {
                    path: at("kind"),
                    message: format!("unknown source kind {other:?}"),
                });
                continue;
            }
        };
        sources.push(SourceConfig { id, settings });
    }
    sources
}

fn parse_oauth(
    raw: &BTreeMap<String, RawOAuthClient>,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<OAuthClientConfig> {
    let mut clients = Vec::with_capacity(raw.len());
    for (name, entry) in raw {
        let Ok(provider) = name.parse::<Provider>() else {
            problems.push(ConfigProblem {
                path: format!("oauth.{name}"),
                message: format!("unknown provider {name:?}"),
            });
            continue;
        };
        if !matches!(
            provider.auth_spec().method,
            AuthMethod::OAuthDeviceCode { .. }
        ) {
            problems.push(ConfigProblem {
                path: format!("oauth.{name}"),
                message: format!(
                    "{} does not use the device-code flow; paste an API token instead",
                    provider.name()
                ),
            });
            continue;
        }
        let Some(client_id) = entry
            .client_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .cloned()
        else {
            problems.push(ConfigProblem {
                path: format!("oauth.{name}.client_id"),
                message: "an OAuth client needs its client id".to_owned(),
            });
            continue;
        };
        clients.push(OAuthClientConfig {
            provider,
            client_id,
            client_secret: entry
                .client_secret
                .as_ref()
                .filter(|secret| !secret.trim().is_empty())
                .cloned(),
        });
    }
    clients
}

fn expand(path: &str, context: &ParseContext) -> PathBuf {
    let trimmed = path.trim();
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = &context.home
    {
        return home.join(rest);
    }
    if trimmed == "~"
        && let Some(home) = &context.home
    {
        return home.clone();
    }
    PathBuf::from(trimmed)
}

/// The configuration file, as written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    /// General behaviour.
    pub general: RawGeneral,
    /// Context profiles.
    pub profiles: Vec<RawProfile>,
    /// Keybindings.
    pub keymap: RawKeymap,
    /// Agent engines, keyed by id.
    pub agents: BTreeMap<String, RawAgent>,
    /// Workspace targets, keyed by kind.
    pub targets: BTreeMap<String, RawTarget>,
    /// Attention sources, keyed by id.
    pub sources: BTreeMap<String, RawSource>,
    /// OAuth clients, keyed by provider.
    pub oauth: BTreeMap<String, RawOAuthClient>,
}

/// One `[oauth.<provider>]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawOAuthClient {
    /// The OAuth client id.
    pub client_id: Option<String>,
    /// The OAuth client secret, when the provider needs one.
    pub client_secret: Option<String>,
}

/// The `[general]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawGeneral {
    /// Which profile to start in.
    pub profile: Option<String>,
    /// Redraw interval in milliseconds.
    pub tick_rate_ms: Option<u64>,
    /// How many log lines to keep.
    pub log_lines: Option<usize>,
    /// Source refresh interval in seconds.
    pub refresh_secs: Option<u64>,
}

/// One `[[profiles]]` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawProfile {
    /// The profile id.
    pub id: String,
    /// Its display name; defaults to the id.
    pub name: Option<String>,
    /// Its workspace target.
    pub target: Option<String>,
    /// The sources it enables; empty means all of them.
    pub sources: Vec<String>,
    /// Where its repositories live.
    pub repo_roots: Vec<String>,
    /// Where its worktrees are created.
    pub worktree_root: Option<String>,
    /// The credential account it uses.
    pub account: Option<String>,
    /// Task class to engine, plus an optional `default`.
    pub routing: BTreeMap<String, String>,
}

/// The `[keymap]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawKeymap {
    /// Whether to start from the built-in Doom keymap.
    pub defaults: Option<bool>,
    /// Sequences to remove before binding.
    pub unbind: RawUnbind,
    /// Normal-mode bindings.
    pub normal: BTreeMap<String, String>,
    /// Insert-mode bindings.
    pub insert: BTreeMap<String, String>,
}

/// The `[keymap.unbind]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawUnbind {
    /// Normal-mode sequences to remove.
    pub normal: Vec<String>,
    /// Insert-mode sequences to remove.
    pub insert: Vec<String>,
}

/// One `[agents.<id>]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawAgent {
    /// Which engine family.
    pub kind: String,
    /// The executable to run.
    pub command: String,
    /// Fixed arguments.
    pub args: Vec<String>,
    /// Extra environment.
    pub env: BTreeMap<String, String>,
}

/// One `[targets.<kind>]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawTarget {
    /// Herdr's socket path.
    pub socket: Option<String>,
    /// The tmux session name.
    pub session: Option<String>,
    /// An editor's executable.
    pub command: Option<String>,
    /// An editor's fixed arguments.
    pub args: Vec<String>,
}

/// One `[sources.<id>]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawSource {
    /// Which kind of source.
    pub kind: String,
    /// An API base URL, for self-hosted instances.
    pub base_url: Option<String>,
    /// A code host search query.
    pub query: Option<String>,
    /// A GitLab project path.
    pub project: Option<String>,
    /// A Jira account email.
    pub email: Option<String>,
    /// A Jira query.
    pub jql: Option<String>,
    /// Chat channels to watch.
    pub channels: Vec<String>,
    /// Which calendar to read.
    pub calendar_id: Option<String>,
    /// How far ahead to look, in hours.
    pub lookahead_hours: Option<u16>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::domain::action::Action;
    use crate::domain::keymap::{Lookup, Mode};
    use crate::domain::keys::KeySequence;

    fn parse(toml_text: &str) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(toml_text).expect("the fixture is valid TOML");
        Config::parse(raw, &ParseContext::default())
    }

    fn lookup(config: &Config, sequence: &str) -> Lookup {
        config.keymap.lookup(
            Mode::Normal,
            KeySequence::parse(sequence).expect("sequence").chords(),
        )
    }

    #[test]
    fn an_empty_config_is_usable_out_of_the_box() {
        let config = parse("").expect("defaults");

        assert_eq!(config.profiles.active().id().as_str(), "work");
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(
            lookup(&config, "SPC f f"),
            Lookup::Action(Action::FeedbackOpen)
        );
        assert_eq!(config.general.tick_rate_ms, General::DEFAULT_TICK_RATE_MS);
        assert!(
            config
                .agents
                .iter()
                .any(|agent| agent.id.as_str() == "claude-code"),
            "the built-in engines are available without configuration"
        );
        assert!(
            config
                .targets
                .iter()
                .any(|target| target.kind == TargetKind::Tmux)
        );
        assert!(
            config.sources.is_empty(),
            "sources need credentials, so none are assumed"
        );
    }

    #[test]
    fn the_startup_profile_can_be_chosen() {
        let config = parse(
            r#"
            [general]
            profile = "home"
            "#,
        )
        .expect("config");
        assert_eq!(config.profiles.active().id().as_str(), "home");
    }

    #[test]
    fn naming_a_profile_that_does_not_exist_is_reported() {
        let error = parse(
            r#"
            [general]
            profile = "mars"
            "#,
        )
        .expect_err("unknown profile");
        assert!(error.to_string().contains("mars"));
        assert_eq!(error.problems()[0].path, "general.profile");
    }

    #[test]
    fn profiles_replace_the_defaults_when_any_are_declared() {
        let config = parse(
            r#"
            [[profiles]]
            id = "oss"
            name = "Open source"
            target = "tmux"
            sources = ["github"]
            repo_roots = ["/src"]
            worktree_root = "/src/.trees"
            account = "home"
            "#,
        )
        .expect("config");

        assert_eq!(config.profiles.len(), 1);
        let profile = config.profiles.active();
        assert_eq!(profile.id().as_str(), "oss");
        assert_eq!(profile.target(), TargetKind::Tmux);
        assert_eq!(profile.sources().len(), 1);
        assert_eq!(profile.repo_roots(), &[PathBuf::from("/src")]);
        assert_eq!(profile.worktree_root(), Some(&PathBuf::from("/src/.trees")));
        assert_eq!(profile.account().map(Slug::as_str), Some("home"));
    }

    #[test]
    fn a_profile_can_override_task_routing() {
        let config = parse(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"

            [profiles.routing]
            default = "script"
            deep-code-change = "agy-gemini"
            "#,
        )
        .expect("config");

        let routing = config.profiles.active().routing();
        assert_eq!(routing.fallback().as_str(), "script");
        assert_eq!(
            routing.agent_for(TaskClass::DeepCodeChange).as_str(),
            "agy-gemini"
        );
        assert_eq!(routing.agent_for(TaskClass::PrSummary).as_str(), "script");
    }

    #[test]
    fn an_unknown_task_class_in_routing_names_the_offending_key() {
        let error = parse(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"

            [profiles.routing]
            sorcery = "claude-code"
            "#,
        )
        .expect_err("bad routing");
        assert_eq!(error.problems()[0].path, "profiles[0].routing.sorcery");
    }

    #[test]
    fn two_profiles_with_one_id_are_reported() {
        let error = parse(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"

            [[profiles]]
            id = "work"
            name = "Work again"
            target = "tmux"
            "#,
        )
        .expect_err("duplicate");
        assert_eq!(error.problems()[0].path, "profiles");
    }

    #[test]
    fn keybindings_are_layered_on_top_of_the_defaults() {
        let config = parse(
            r#"
            [keymap.normal]
            "SPC o o" = "agent-logs"
            "#,
        )
        .expect("config");

        assert_eq!(
            lookup(&config, "SPC o o"),
            Lookup::Action(Action::AgentLogs)
        );
        assert_eq!(
            lookup(&config, "SPC f f"),
            Lookup::Action(Action::FeedbackOpen)
        );
    }

    #[test]
    fn the_defaults_can_be_abandoned_entirely() {
        let config = parse(
            r#"
            [keymap]
            defaults = false

            [keymap.normal]
            "x" = "quit"
            "#,
        )
        .expect("config");

        assert_eq!(lookup(&config, "x"), Lookup::Action(Action::Quit));
        assert_eq!(lookup(&config, "SPC f f"), Lookup::Unbound);
    }

    #[test]
    fn a_default_binding_can_be_rebound_to_something_else() {
        let config = parse(
            r#"
            [keymap.normal]
            "r" = "agent-rerun"
            "#,
        )
        .expect("config");
        assert_eq!(lookup(&config, "r"), Lookup::Action(Action::AgentRerun));
    }

    #[test]
    fn a_default_binding_can_be_removed_to_free_its_prefix() {
        let config = parse(
            r#"
            [keymap]
            unbind = { normal = ["r"] }

            [keymap.normal]
            "r r" = "refresh"
            "#,
        )
        .expect("config");
        assert_eq!(lookup(&config, "r r"), Lookup::Action(Action::Refresh));
    }

    #[test]
    fn a_binding_that_shadows_another_is_reported_rather_than_silently_lost() {
        let error = parse(
            r#"
            [keymap.normal]
            "SPC f" = "quit"
            "#,
        )
        .expect_err("conflict");
        assert_eq!(error.problems()[0].path, "keymap.normal.SPC f");
        assert!(error.problems()[0].message.contains("prefix"));
    }

    #[test]
    fn an_unknown_action_name_is_reported_with_its_binding() {
        let error = parse(
            r#"
            [keymap.normal]
            "SPC z" = "summon-dragon"
            "#,
        )
        .expect_err("unknown action");
        assert_eq!(error.problems()[0].path, "keymap.normal.SPC z");
        assert!(error.problems()[0].message.contains("summon-dragon"));
    }

    #[test]
    fn an_unparseable_key_sequence_is_reported() {
        let error = parse(
            r#"
            [keymap.normal]
            "<Nope>" = "quit"
            "#,
        )
        .expect_err("bad key");
        assert_eq!(error.problems()[0].path, "keymap.normal.<Nope>");
    }

    #[test]
    fn insert_mode_bindings_are_kept_separate() {
        let config = parse(
            r#"
            [keymap.insert]
            "C-c" = "cancel-pending"
            "#,
        )
        .expect("config");
        assert_eq!(
            config.keymap.lookup(
                Mode::Insert,
                KeySequence::parse("C-c").expect("sequence").chords()
            ),
            Lookup::Action(Action::CancelPending)
        );
    }

    #[test]
    fn agents_are_configured_with_their_command_line() {
        let config = parse(
            r#"
            [agents.claude-code]
            kind = "claude-code"
            command = "claude"
            args = ["--permission-mode", "acceptEdits"]
            env = { CLAUDE_NO_COLOR = "1" }

            [agents.summariser]
            kind = "script"
            command = "/usr/local/bin/summarise"
            "#,
        )
        .expect("config");

        let claude = config
            .agents
            .iter()
            .find(|agent| agent.id.as_str() == "claude-code")
            .expect("claude");
        assert_eq!(claude.kind, AgentKind::ClaudeCode);
        assert_eq!(claude.command, "claude");
        assert_eq!(claude.args, vec!["--permission-mode", "acceptEdits"]);
        assert_eq!(
            claude.env,
            vec![("CLAUDE_NO_COLOR".to_owned(), "1".to_owned())]
        );

        assert!(
            config
                .agents
                .iter()
                .any(|agent| agent.kind == AgentKind::Script)
        );
        assert_eq!(
            config.agents.len(),
            2,
            "declaring agents replaces the built-in set"
        );
    }

    #[test]
    fn an_unknown_agent_kind_is_reported() {
        let error = parse(
            r#"
            [agents.weird]
            kind = "telepathy"
            command = "think"
            "#,
        )
        .expect_err("unknown kind");
        assert_eq!(error.problems()[0].path, "agents.weird.kind");
    }

    #[test]
    fn workspace_targets_carry_their_own_settings() {
        let config = parse(
            r#"
            [targets.herdr]
            socket = "/run/herdr.sock"

            [targets.tmux]
            session = "vetinari"

            [targets.editor]
            command = "zed"
            args = ["--new"]
            "#,
        )
        .expect("config");

        assert_eq!(config.targets.len(), 3);
        for target in &config.targets {
            match (&target.kind, &target.settings) {
                (TargetKind::Herdr, TargetSettings::Herdr { socket }) => {
                    assert_eq!(socket.as_deref(), Some(Path::new("/run/herdr.sock")));
                }
                (TargetKind::Tmux, TargetSettings::Tmux { session }) => {
                    assert_eq!(session, "vetinari");
                }
                (TargetKind::Editor, TargetSettings::Editor { command, args }) => {
                    assert_eq!(command, "zed");
                    assert_eq!(args, &["--new".to_owned()]);
                }
                other => panic!("mismatched target settings: {other:?}"),
            }
        }
    }

    #[test]
    fn attention_sources_are_configured_per_kind() {
        let config = parse(
            r#"
            [sources.github]
            kind = "github"
            query = "is:open review-requested:@me"

            [sources.jira]
            kind = "jira"
            base_url = "https://acme.atlassian.net"
            email = "dev@acme.test"
            jql = "assignee = currentUser()"

            [sources.calendar]
            kind = "google-calendar"
            calendar_id = "primary"
            lookahead_hours = 12
            "#,
        )
        .expect("config");

        assert_eq!(config.sources.len(), 3);
        let github = config
            .sources
            .iter()
            .find(|source| source.id.as_str() == "github")
            .expect("github");
        match &github.settings {
            SourceSettings::GitHub { query, base_url } => {
                assert_eq!(query, "is:open review-requested:@me");
                assert!(base_url.is_none());
            }
            other => panic!("expected github settings, got {other:?}"),
        }
        let calendar = config
            .sources
            .iter()
            .find(|source| source.id.as_str() == "calendar")
            .expect("calendar");
        match &calendar.settings {
            SourceSettings::GoogleCalendar {
                calendar_id,
                lookahead_hours,
            } => {
                assert_eq!(calendar_id, "primary");
                assert_eq!(*lookahead_hours, 12);
            }
            other => panic!("expected calendar settings, got {other:?}"),
        }
    }

    #[test]
    fn a_jira_source_without_a_site_url_cannot_work_and_says_so() {
        let error = parse(
            r#"
            [sources.jira]
            kind = "jira"
            "#,
        )
        .expect_err("incomplete");
        assert_eq!(error.problems()[0].path, "sources.jira.base_url");
    }

    #[test]
    fn every_problem_is_reported_in_one_pass() {
        let error = parse(
            r#"
            [general]
            profile = "mars"

            [keymap.normal]
            "SPC z" = "summon-dragon"

            [agents.weird]
            kind = "telepathy"
            command = "think"
            "#,
        )
        .expect_err("several problems");
        assert_eq!(error.problems().len(), 3);
    }

    #[test]
    fn oauth_clients_are_configured_per_provider() {
        let config = parse(
            r#"
            [oauth.google-calendar]
            client_id = "123.apps.googleusercontent.com"
            client_secret = "GOCSPX-secret"
            "#,
        )
        .expect("config");

        assert_eq!(config.oauth_clients.len(), 1);
        let client = &config.oauth_clients[0];
        assert_eq!(client.provider, Provider::GoogleCalendar);
        assert_eq!(client.client_id, "123.apps.googleusercontent.com");
        assert_eq!(client.client_secret.as_deref(), Some("GOCSPX-secret"));
        assert_eq!(
            config
                .oauth_client(Provider::GoogleCalendar)
                .map(|c| c.client_id.as_str()),
            Some("123.apps.googleusercontent.com")
        );
        assert!(config.oauth_client(Provider::Outlook).is_none());
    }

    #[test]
    fn an_oauth_client_for_a_provider_that_uses_tokens_is_reported() {
        let error = parse(
            r#"
            [oauth.github]
            client_id = "abc"
            "#,
        )
        .expect_err("wrong method");
        assert_eq!(error.problems()[0].path, "oauth.github");
        assert!(error.problems()[0].message.contains("device"));
    }

    #[test]
    fn an_oauth_client_without_an_id_is_reported() {
        let error = parse(
            r#"
            [oauth.google-calendar]
            client_secret = "only-a-secret"
            "#,
        )
        .expect_err("incomplete");
        assert_eq!(error.problems()[0].path, "oauth.google-calendar.client_id");
    }

    #[test]
    fn an_unknown_provider_in_the_oauth_table_is_reported() {
        let error = parse(
            r#"
            [oauth.telepathy]
            client_id = "abc"
            "#,
        )
        .expect_err("unknown provider");
        assert_eq!(error.problems()[0].path, "oauth.telepathy");
    }

    #[test]
    fn paths_beginning_with_a_tilde_are_expanded_against_home() {
        let raw: RawConfig = toml::from_str(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            repo_roots = ["~/code"]
            worktree_root = "~/code/.trees"
            "#,
        )
        .expect("valid TOML");
        let config = Config::parse(
            raw,
            &ParseContext {
                home: Some(PathBuf::from("/home/dev")),
            },
        )
        .expect("config");

        assert_eq!(
            config.profiles.active().repo_roots(),
            &[PathBuf::from("/home/dev/code")]
        );
        assert_eq!(
            config.profiles.active().worktree_root(),
            Some(&PathBuf::from("/home/dev/code/.trees"))
        );
    }

    #[test]
    fn a_tilde_path_with_no_home_is_left_alone_rather_than_guessed_at() {
        let raw: RawConfig = toml::from_str(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "tmux"
            repo_roots = ["~/code"]
            "#,
        )
        .expect("valid TOML");
        let config = Config::parse(raw, &ParseContext { home: None }).expect("config");
        assert_eq!(
            config.profiles.active().repo_roots(),
            &[PathBuf::from("~/code")]
        );
    }

    #[test]
    fn the_general_section_bounds_the_values_it_accepts() {
        let config = parse(
            r#"
            [general]
            tick_rate_ms = 1
            log_lines = 0
            "#,
        )
        .expect("config");
        assert_eq!(config.general.tick_rate_ms, General::MIN_TICK_RATE_MS);
        assert_eq!(config.general.log_lines, General::MIN_LOG_LINES);
    }
}
