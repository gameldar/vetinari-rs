//! Assembles the concrete adapters a configuration asks for.
//!
//! This is the only place that knows both what the configuration says and
//! which types implement each port, which is what keeps that knowledge out of
//! the core and out of the TUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use vetinari_core::application::agents::AgentRegistry;
use vetinari_core::application::attention::AttentionEngine;
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::config::{
    AgentConfig, Config, SourceConfig, SourceSettings, TargetConfig, TargetSettings,
};
use vetinari_core::application::observed::ObservedAgents;
use vetinari_core::application::ports::{
    ChatSource, Clock, CodeHostSource, CredentialStore, DeviceCodeFlow, ExternalAgentSource,
    IssueTrackerSource, PortResult, ScheduleSource, SkillStore, WorkspaceTarget, WorktreeManager,
};
use vetinari_core::application::skills::SkillService;
use vetinari_core::application::workflows::WorkflowEngine;
use vetinari_core::application::workspaces::WorkspaceDirectory;
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::SourceId;

use crate::HttpClient;
use crate::agent::CliAgent;
use crate::clock::SystemClock;
use crate::credentials::KeyringStore;
use crate::git::GitWorktreeManager;
use crate::oauth::HttpDeviceCodeFlow;
use crate::skills::FileSkillStore;
use crate::sources::{
    GitHubSettings, GitHubSource, GoogleCalendarSettings, GoogleCalendarSource, JiraSettings,
    JiraSource, SlackSettings, SlackSource,
};
use crate::workspace::herdr::SocketLocation;
use crate::workspace::{EditorTarget, HerdrSocketTarget, TmuxTarget};

/// Where Vetinari keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Where configuration and the skill library live.
    pub config_dir: PathBuf,
    /// Where sockets and other runtime state live.
    pub state_dir: PathBuf,
}

/// Everything the TUI needs, wired up.
pub struct Runtime {
    /// The parsed configuration.
    pub config: Config,
    /// The clock every service reads.
    pub clock: Arc<dyn Clock>,
    /// Credentials and renewal.
    pub auth: Arc<AuthService>,
    /// The attention sources.
    pub attention: AttentionEngine,
    /// The agent engines and their sessions.
    pub agents: AgentRegistry,
    /// The agents running in workspace targets that Vetinari did not start.
    pub observed: ObservedAgents,
    /// Worktrees, spaces, and the workflows that compose them.
    pub workflows: WorkflowEngine,
    /// The skill library's store.
    pub skills: SkillService,
    /// The code hosts, by source id, for approving and commenting.
    pub code_hosts: BTreeMap<SourceId, Arc<dyn CodeHostSource>>,
    /// The issue trackers, by source id, for transitions and assignment.
    pub issue_trackers: BTreeMap<SourceId, Arc<dyn IssueTrackerSource>>,
    /// The chat systems, by source id, for replies.
    pub chats: BTreeMap<SourceId, Arc<dyn ChatSource>>,
    /// The calendars, by source id, for agendas.
    pub schedules: BTreeMap<SourceId, Arc<dyn ScheduleSource>>,
    /// Where files were looked for.
    pub paths: Paths,
    /// Things the configuration asked for that could not be provided.
    ///
    /// Collected rather than fatal: one unsupported source should not stop
    /// Vetinari starting.
    pub problems: Vec<String>,
}

impl Runtime {
    /// How many lines of agent output to keep per session.
    pub const AGENT_OUTPUT_LINES: usize = 500;

    /// Build the runtime from a configuration, using the real adapters.
    pub fn build(config: Config, paths: Paths) -> PortResult<Self> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let store: Arc<dyn CredentialStore> = Arc::new(KeyringStore);
        let http = HttpClient::new()?;
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(HttpDeviceCodeFlow::new(
            http.clone(),
            config.oauth_clients.clone(),
            clock.clone(),
        ));
        let skills: Arc<dyn SkillStore> = Arc::new(FileSkillStore::new(
            FileSkillStore::default_path(&paths.config_dir),
        ));
        let worktrees: Arc<dyn WorktreeManager> = Arc::new(GitWorktreeManager::default());

        Self::assemble(config, paths, clock, store, flow, http, skills, worktrees)
    }

    /// Build the runtime from supplied ports, for testing.
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        config: Config,
        paths: Paths,
        clock: Arc<dyn Clock>,
        store: Arc<dyn CredentialStore>,
        flow: Arc<dyn DeviceCodeFlow>,
        http: HttpClient,
        skills: Arc<dyn SkillStore>,
        worktrees: Arc<dyn WorktreeManager>,
    ) -> PortResult<Self> {
        let mut problems = Vec::new();
        let auth = Arc::new(AuthService::new(store, flow, clock.clone()));

        let mut agents = AgentRegistry::new(clock.clone());
        for agent in &config.agents {
            agents.register(Arc::new(CliAgent::new(
                agent.clone(),
                Self::AGENT_OUTPUT_LINES,
            )));
        }

        let mut workspaces = WorkspaceDirectory::default();
        let mut observed = ObservedAgents::default();
        for target in &config.targets {
            let built = build_target(target, &paths);
            // A trait object cannot be widened after the fact, so whether a
            // target can report its own agents is decided here, where the
            // concrete type is still known.
            if let Some(watcher) = built.observer {
                observed.register(watcher);
            }
            workspaces.register(built.target);
        }

        // A profile that opens spaces on a target nobody configured fails only
        // when a workflow reaches for it, which is too late to be useful.
        for profile in config.profiles.iter() {
            if !workspaces.has(profile.target()) {
                problems.push(format!(
                    "profile {} opens spaces on {}, which is not configured",
                    profile.id(),
                    profile.target().name()
                ));
            }
        }

        let mut attention = AttentionEngine::default();
        let mut code_hosts: BTreeMap<SourceId, Arc<dyn CodeHostSource>> = BTreeMap::new();
        let mut issue_trackers: BTreeMap<SourceId, Arc<dyn IssueTrackerSource>> = BTreeMap::new();
        let mut chats: BTreeMap<SourceId, Arc<dyn ChatSource>> = BTreeMap::new();
        let mut schedules: BTreeMap<SourceId, Arc<dyn ScheduleSource>> = BTreeMap::new();
        for source in &config.sources {
            match build_source(source, &http, &auth, &clock) {
                Ok(built) => {
                    let id = source.id.clone();
                    match built {
                        BuiltSource::CodeHost(host) => {
                            attention.register(host.clone());
                            code_hosts.insert(id, host);
                        }
                        BuiltSource::IssueTracker(tracker) => {
                            attention.register(tracker.clone());
                            issue_trackers.insert(id, tracker);
                        }
                        BuiltSource::Chat(chat) => {
                            attention.register(chat.clone());
                            chats.insert(id, chat);
                        }
                        BuiltSource::Schedule(schedule) => {
                            attention.register(schedule.clone());
                            schedules.insert(id, schedule);
                        }
                    }
                }
                Err(problem) => problems.push(problem),
            }
        }

        Ok(Self {
            config,
            clock,
            auth,
            attention,
            agents,
            observed,
            workflows: WorkflowEngine::new(worktrees, workspaces),
            skills: SkillService::new(skills),
            code_hosts,
            issue_trackers,
            chats,
            schedules,
            paths,
            problems,
        })
    }

    /// The providers whose credentials this configuration needs.
    #[must_use]
    pub fn required_providers(&self) -> Vec<Provider> {
        let mut providers: Vec<Provider> = self
            .config
            .sources
            .iter()
            .map(|source| match source.settings {
                SourceSettings::GitHub { .. } => Provider::GitHub,
                SourceSettings::GitLab { .. } => Provider::GitLab,
                SourceSettings::Jira { .. } => Provider::Jira,
                SourceSettings::Slack { .. } => Provider::Slack,
                SourceSettings::Discord { .. } => Provider::Discord,
                SourceSettings::GoogleCalendar { .. } => Provider::GoogleCalendar,
                SourceSettings::Outlook { .. } => Provider::Outlook,
            })
            .collect();
        providers.sort_unstable();
        providers.dedup();
        providers
    }

    /// The agent engines that are not installed on this machine.
    pub async fn missing_engines(&self) -> Vec<AgentConfig> {
        let mut missing = Vec::new();
        for agent in &self.config.agents {
            if let Some(engine) = self.agents.engine(&agent.id)
                && !engine.is_available().await
            {
                missing.push(agent.clone());
            }
        }
        missing
    }
}

/// A target, and whether it can also report the agents running inside it.
struct BuiltTarget {
    target: Arc<dyn WorkspaceTarget>,
    observer: Option<Arc<dyn ExternalAgentSource>>,
}

fn build_target(target: &TargetConfig, paths: &Paths) -> BuiltTarget {
    match &target.settings {
        TargetSettings::Herdr { socket } => {
            let herdr = Arc::new(HerdrSocketTarget::at(
                &SocketLocation::from_env(Some(&paths.config_dir)).configured(socket.clone()),
            ));
            BuiltTarget {
                target: herdr.clone(),
                observer: Some(herdr),
            }
        }
        TargetSettings::Tmux { session } => BuiltTarget {
            target: Arc::new(TmuxTarget::new(session.clone())),
            observer: None,
        },
        TargetSettings::Editor { command, args } => BuiltTarget {
            target: Arc::new(EditorTarget::new(command.clone(), args.clone())),
            observer: None,
        },
    }
}

/// A source, in the richest role it can play.
///
/// The concrete adapters implement both [`AttentionSource`] and the trait for
/// their category, and a trait object cannot be widened after the fact, so the
/// role is decided here, where the concrete type is still known.
enum BuiltSource {
    CodeHost(Arc<dyn CodeHostSource>),
    IssueTracker(Arc<dyn IssueTrackerSource>),
    Chat(Arc<dyn ChatSource>),
    Schedule(Arc<dyn ScheduleSource>),
}

fn build_source(
    source: &SourceConfig,
    http: &HttpClient,
    auth: &Arc<AuthService>,
    clock: &Arc<dyn Clock>,
) -> Result<BuiltSource, String> {
    let id: SourceId = source.id.clone();
    match &source.settings {
        SourceSettings::GitHub { query, base_url } => {
            Ok(BuiltSource::CodeHost(Arc::new(GitHubSource::new(
                id,
                GitHubSettings {
                    query: query.clone(),
                    base_url: base_url.clone(),
                },
                http.clone(),
                auth.clone(),
            ))))
        }
        SourceSettings::Jira {
            base_url,
            email,
            jql,
        } => Ok(BuiltSource::IssueTracker(Arc::new(JiraSource::new(
            id,
            JiraSettings {
                base_url: base_url.clone(),
                email: email.clone(),
                jql: jql.clone(),
            },
            http.clone(),
            auth.clone(),
        )))),
        SourceSettings::Slack { channels } => Ok(BuiltSource::Chat(Arc::new(SlackSource::new(
            id,
            SlackSettings {
                channels: channels.clone(),
                base_url: None,
            },
            http.clone(),
            auth.clone(),
        )))),
        SourceSettings::GoogleCalendar {
            calendar_id,
            lookahead_hours,
        } => Ok(BuiltSource::Schedule(Arc::new(GoogleCalendarSource::new(
            id,
            GoogleCalendarSettings {
                calendar_id: calendar_id.clone(),
                lookahead_hours: *lookahead_hours,
                base_url: None,
            },
            http.clone(),
            auth.clone(),
            clock.clone(),
        )))),
        SourceSettings::GitLab { .. }
        | SourceSettings::Discord { .. }
        | SourceSettings::Outlook { .. } => Err(format!(
            "source {:?} is configured but its adapter is not implemented yet; \
                 GitHub, Jira, Slack, and Google Calendar are available",
            source.id.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetinari_core::application::config::{ParseContext, RawConfig};
    use vetinari_core::domain::workspace::TargetKind;
    use vetinari_core::testing::{
        FakeDeviceCodeFlow, FakeWorktreeManager, FixedClock, MemoryCredentialStore,
        MemorySkillStore,
    };

    fn runtime(toml_text: &str) -> Runtime {
        let raw: RawConfig = toml::from_str(toml_text).expect("valid TOML");
        let config = Config::parse(raw, &ParseContext::default()).expect("valid config");
        let clock = Arc::new(FixedClock::new(time::macros::datetime!(
            2026-10-21 09:00 UTC
        )));
        Runtime::assemble(
            config,
            Paths {
                config_dir: PathBuf::from("/home/dev/.config"),
                state_dir: PathBuf::from("/home/dev/.local/state"),
            },
            clock.clone(),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeDeviceCodeFlow::new(0, clock.now())),
            HttpClient::new().expect("client"),
            Arc::new(MemorySkillStore::default()),
            Arc::new(FakeWorktreeManager::default()),
        )
        .expect("runtime")
    }

    #[test]
    fn a_profile_whose_target_is_not_configured_is_reported_at_start_up() {
        // Otherwise it fails only when a workflow reaches for the target, and
        // the monitor just stays empty.
        let runtime = runtime(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "herdr"

            [targets.tmux]
            session = "vetinari"
            "#,
        );

        assert!(
            runtime
                .problems
                .iter()
                .any(|problem| problem.contains("work") && problem.contains("herdr")),
            "got {:?}",
            runtime.problems
        );
    }

    #[test]
    fn an_empty_configuration_still_produces_a_working_runtime() {
        let runtime = runtime("");

        let mut engines: Vec<String> = runtime
            .agents
            .engine_ids()
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        engines.sort();
        assert_eq!(engines, vec!["agy-gemini", "claude-code", "script"]);
        assert_eq!(
            runtime.workflows.workspaces().kinds(),
            vec![TargetKind::Herdr, TargetKind::Tmux],
            "the built-in profiles target both, so both have to be there"
        );
        assert_eq!(
            runtime.observed.targets(),
            vec![TargetKind::Herdr],
            "herdr is the only default target that reports its own agents"
        );
        assert!(
            runtime.problems.is_empty(),
            "no profile should be left without its target: {:?}",
            runtime.problems
        );
        assert!(runtime.attention.source_ids().is_empty());
        assert!(runtime.problems.is_empty());
    }

    #[test]
    fn the_configured_sources_are_registered() {
        let runtime = runtime(
            r#"
            [sources.github]
            kind = "github"

            [sources.jira]
            kind = "jira"
            base_url = "https://acme.atlassian.net"
            email = "dev@acme.test"

            [sources.slack]
            kind = "slack"

            [sources.calendar]
            kind = "google-calendar"
            "#,
        );

        let mut ids: Vec<String> = runtime
            .attention
            .source_ids()
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["calendar", "github", "jira", "slack"]);
        assert!(runtime.problems.is_empty());
    }

    #[test]
    fn sources_are_kept_in_the_registry_their_actions_need() {
        let runtime = runtime(
            r#"
            [sources.github]
            kind = "github"

            [sources.jira]
            kind = "jira"
            base_url = "https://acme.atlassian.net"
            email = "dev@acme.test"

            [sources.slack]
            kind = "slack"

            [sources.calendar]
            kind = "google-calendar"
            "#,
        );

        assert!(
            runtime
                .code_hosts
                .contains_key(&SourceId::parse("github").expect("id"))
        );
        assert!(
            runtime
                .issue_trackers
                .contains_key(&SourceId::parse("jira").expect("id"))
        );
        assert!(
            runtime
                .chats
                .contains_key(&SourceId::parse("slack").expect("id"))
        );
        assert!(
            runtime
                .schedules
                .contains_key(&SourceId::parse("calendar").expect("id"))
        );
        assert_eq!(runtime.code_hosts.len(), 1);
    }

    #[test]
    fn a_source_without_an_adapter_is_reported_but_does_not_stop_startup() {
        let runtime = runtime(
            r#"
            [sources.github]
            kind = "github"

            [sources.gl]
            kind = "gitlab"
            project = "group/project"
            "#,
        );

        assert_eq!(runtime.attention.source_ids().len(), 1);
        assert_eq!(runtime.problems.len(), 1);
        assert!(runtime.problems[0].contains("gl"));
        assert!(runtime.problems[0].contains("not implemented"));
    }

    #[test]
    fn every_configured_target_kind_is_registered() {
        let runtime = runtime(
            r#"
            [targets.herdr]
            socket = "/run/herdr.sock"

            [targets.tmux]
            session = "vetinari"

            [targets.editor]
            command = "zed"
            "#,
        );

        let kinds = runtime.workflows.workspaces().kinds();
        assert_eq!(kinds.len(), 3);
        assert!(kinds.contains(&TargetKind::Herdr));
        assert!(kinds.contains(&TargetKind::Editor));
    }

    #[test]
    fn a_herdr_target_without_a_socket_falls_back_to_the_state_directory() {
        let runtime = runtime("[targets.herdr]\n");
        assert!(runtime.workflows.workspaces().has(TargetKind::Herdr));
    }

    #[test]
    fn the_providers_the_configuration_needs_are_reported_for_the_wizard() {
        let runtime = runtime(
            r#"
            [sources.github]
            kind = "github"

            [sources.calendar]
            kind = "google-calendar"
            "#,
        );

        assert_eq!(
            runtime.required_providers(),
            vec![Provider::GitHub, Provider::GoogleCalendar]
        );
    }

    #[test]
    fn two_sources_of_one_kind_need_that_provider_once() {
        let runtime = runtime(
            r#"
            [sources.team]
            kind = "github"
            query = "org:acme"

            [sources.mine]
            kind = "github"
            query = "author:@me"
            "#,
        );
        assert_eq!(runtime.required_providers(), vec![Provider::GitHub]);
    }

    #[tokio::test]
    async fn engines_that_are_not_installed_are_reported_rather_than_failing_later() {
        let runtime = runtime(
            r#"
            [agents.real]
            kind = "script"
            command = "sh"

            [agents.imaginary]
            kind = "claude-code"
            command = "claude-that-does-not-exist"
            "#,
        );

        let missing = runtime.missing_engines().await;

        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id.as_str(), "imaginary");
    }

    #[test]
    fn the_profiles_from_the_configuration_are_carried_through() {
        let runtime = runtime(
            r#"
            [[profiles]]
            id = "oss"
            name = "Open source"
            target = "tmux"
            "#,
        );
        assert_eq!(runtime.config.profiles.active().id().as_str(), "oss");
    }
}
