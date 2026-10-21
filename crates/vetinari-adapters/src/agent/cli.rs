//! The CLI-backed agent engines: Claude Code, Gemini through `agy`, and any
//! configured script.
//!
//! All three differ only in how the prompt reaches the process, so they share
//! one implementation rather than three near-identical ones. Claude Code takes
//! it as an argument in print mode; `agy` takes it as its last argument; a
//! script gets it on standard input, and in the environment too so that a
//! script can read it without consuming its input.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use vetinari_core::application::config::{AgentConfig, AgentKind};
use vetinari_core::application::ports::{AgentEngine, PortResult};
use vetinari_core::domain::agent::{AgentSession, AgentStatus, TaskSpec};
use vetinari_core::domain::ids::{AgentId, SessionId};

use super::process::ProcessRegistry;

/// An agent engine that runs a command-line tool.
pub struct CliAgent {
    config: AgentConfig,
    processes: ProcessRegistry,
    counter: Mutex<u64>,
}

impl CliAgent {
    /// Build an engine from its configuration.
    #[must_use]
    pub fn new(config: AgentConfig, max_output_lines: usize) -> Self {
        Self {
            config,
            processes: ProcessRegistry::new(max_output_lines),
            counter: Mutex::new(0),
        }
    }

    /// Its configuration.
    #[must_use]
    pub const fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// The command and standard input for a managed run: the engine works on
    /// its own and Vetinari watches it.
    fn managed_command(&self, spec: &TaskSpec) -> (Vec<String>, Option<String>) {
        let mut command = vec![self.config.command.clone()];
        command.extend(self.config.args.iter().cloned());

        match self.config.kind {
            AgentKind::ClaudeCode => {
                for path in spec.context_paths() {
                    command.push("--add-dir".to_owned());
                    command.push(path.display().to_string());
                }
                command.push("--print".to_owned());
                command.push(spec.prompt().as_str().to_owned());
                (command, None)
            }
            AgentKind::Agy => {
                for path in spec.context_paths() {
                    command.push("--include".to_owned());
                    command.push(path.display().to_string());
                }
                command.push(spec.prompt().as_str().to_owned());
                (command, None)
            }
            AgentKind::Script => (command, Some(spec.prompt().as_str().to_owned())),
        }
    }

    /// The environment a task runs with.
    fn environment(&self, spec: &TaskSpec) -> Vec<(String, String)> {
        let mut env = self.config.env.clone();
        env.push((
            "VETINARI_TASK_CLASS".to_owned(),
            spec.class().name().to_owned(),
        ));
        if let Some(skill) = spec.skill() {
            env.push(("VETINARI_SKILL".to_owned(), skill.as_str().to_owned()));
        }
        if self.config.kind == AgentKind::Script {
            env.push((
                "VETINARI_PROMPT".to_owned(),
                spec.prompt().as_str().to_owned(),
            ));
        }
        env.extend(spec.env().iter().cloned());
        env
    }

    fn next_session_id(&self) -> SessionId {
        let mut counter = self.counter.lock().expect("counter lock");
        *counter += 1;
        SessionId::parse(&format!("{}-{}", self.config.id, *counter))
            .expect("a session id built from a slug and a number is valid")
    }
}

#[async_trait]
impl AgentEngine for CliAgent {
    fn id(&self) -> &AgentId {
        &self.config.id
    }

    async fn is_available(&self) -> bool {
        resolve_binary(&self.config.command).is_some()
    }

    async fn spawn_task(&self, spec: &TaskSpec) -> PortResult<AgentSession> {
        let id = self.next_session_id();
        let (command, stdin) = self.managed_command(spec);
        self.processes
            .spawn(
                &id,
                &command,
                spec.working_dir(),
                &self.environment(spec),
                stdin.as_deref(),
            )
            .await?;
        let mut session = AgentSession::new(
            id,
            self.config.id.clone(),
            spec,
            OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()),
        );
        session.set_status(AgentStatus::Running);
        Ok(session)
    }

    async fn check_status(&self, id: &SessionId) -> PortResult<AgentStatus> {
        self.processes.status(id)
    }

    async fn cancel(&self, id: &SessionId) -> PortResult<()> {
        self.processes.cancel(id).await
    }

    async fn answer(&self, id: &SessionId, answer: &str) -> PortResult<()> {
        self.processes.answer(id, answer).await
    }

    async fn output(&self, id: &SessionId, max_lines: usize) -> PortResult<Vec<String>> {
        self.processes.output(id, max_lines)
    }

    fn command_line(&self, spec: &TaskSpec) -> PortResult<Vec<String>> {
        // Run beside the developer, so leave the tool interactive: no print
        // mode, and the prompt as its opening instruction.
        let mut command = vec![self.config.command.clone()];
        command.extend(self.config.args.iter().cloned());
        match self.config.kind {
            AgentKind::ClaudeCode | AgentKind::Agy => {
                for path in spec.context_paths() {
                    command.push(match self.config.kind {
                        AgentKind::ClaudeCode => "--add-dir".to_owned(),
                        _ => "--include".to_owned(),
                    });
                    command.push(path.display().to_string());
                }
                command.push(spec.prompt().as_str().to_owned());
            }
            AgentKind::Script => {}
        }
        Ok(command)
    }
}

/// Find an executable the way a shell would: an absolute or relative path is
/// checked as given, a bare name is looked for on `PATH`.
fn resolve_binary(command: &str) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if command.contains('/') {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vetinari_core::domain::agent::TaskClass;
    use vetinari_core::domain::ids::SkillId;
    use vetinari_core::domain::text::NonEmptyText;

    fn config(kind: AgentKind, command: &str, args: &[&str]) -> AgentConfig {
        AgentConfig {
            id: AgentId::parse(kind.name()).expect("id"),
            kind,
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: Vec::new(),
        }
    }

    fn agent(kind: AgentKind, command: &str, args: &[&str]) -> CliAgent {
        CliAgent::new(config(kind, command, args), 100)
    }

    fn spec() -> TaskSpec {
        TaskSpec::new(
            NonEmptyText::parse("fix the memory leak").expect("prompt"),
            TaskClass::DeepCodeChange,
            PathBuf::from("."),
        )
    }

    async fn wait_for_terminal(agent: &CliAgent, id: &SessionId) -> AgentStatus {
        for _ in 0..200 {
            let status = agent.check_status(id).await.expect("status");
            if status.is_terminal() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the process never finished");
    }

    #[test]
    fn claude_code_is_asked_to_print_rather_than_sit_at_a_prompt() {
        let agent = agent(
            AgentKind::ClaudeCode,
            "claude",
            &["--permission-mode", "acceptEdits"],
        );
        let (command, stdin) = agent.managed_command(&spec());

        assert_eq!(
            command,
            vec![
                "claude".to_owned(),
                "--permission-mode".to_owned(),
                "acceptEdits".to_owned(),
                "--print".to_owned(),
                "fix the memory leak".to_owned(),
            ]
        );
        assert!(stdin.is_none(), "the prompt is an argument, not input");
    }

    #[test]
    fn claude_code_run_in_a_space_is_left_interactive() {
        let agent = agent(AgentKind::ClaudeCode, "claude", &[]);
        assert_eq!(
            agent.command_line(&spec()).expect("command"),
            vec!["claude".to_owned(), "fix the memory leak".to_owned()]
        );
    }

    #[test]
    fn claude_code_is_told_about_context_directories() {
        let agent = agent(AgentKind::ClaudeCode, "claude", &[]);
        let spec = spec()
            .with_context_path(PathBuf::from("/work/repo/src"))
            .with_context_path(PathBuf::from("/work/repo/docs"));
        let (command, _) = agent.managed_command(&spec);

        assert_eq!(
            command,
            vec![
                "claude".to_owned(),
                "--add-dir".to_owned(),
                "/work/repo/src".to_owned(),
                "--add-dir".to_owned(),
                "/work/repo/docs".to_owned(),
                "--print".to_owned(),
                "fix the memory leak".to_owned(),
            ]
        );
    }

    #[test]
    fn agy_takes_the_prompt_as_its_last_argument() {
        let agent = agent(AgentKind::Agy, "agy", &["--model", "gemini-2.5-pro"]);
        let spec = spec().with_context_path(PathBuf::from("src/auth.rs"));
        let (command, stdin) = agent.managed_command(&spec);

        assert_eq!(
            command,
            vec![
                "agy".to_owned(),
                "--model".to_owned(),
                "gemini-2.5-pro".to_owned(),
                "--include".to_owned(),
                "src/auth.rs".to_owned(),
                "fix the memory leak".to_owned(),
            ]
        );
        assert!(stdin.is_none());
    }

    #[test]
    fn a_script_receives_the_prompt_on_its_input() {
        let agent = agent(AgentKind::Script, "/usr/local/bin/summarise", &["--json"]);
        let (command, stdin) = agent.managed_command(&spec());

        assert_eq!(
            command,
            vec!["/usr/local/bin/summarise".to_owned(), "--json".to_owned()]
        );
        assert_eq!(stdin.as_deref(), Some("fix the memory leak"));
    }

    #[test]
    fn the_task_class_and_prompt_are_offered_to_a_script_as_environment() {
        let agent = agent(AgentKind::Script, "summarise", &[]);
        let env = agent.environment(&spec());

        assert!(env.contains(&(
            "VETINARI_TASK_CLASS".to_owned(),
            "deep-code-change".to_owned()
        )));
        assert!(
            env.contains(&(
                "VETINARI_PROMPT".to_owned(),
                "fix the memory leak".to_owned()
            )),
            "a script can read the prompt without consuming its input"
        );
    }

    #[test]
    fn a_cli_agent_does_not_put_the_prompt_in_the_environment() {
        let agent = agent(AgentKind::ClaudeCode, "claude", &[]);
        let env = agent.environment(&spec());
        assert!(!env.iter().any(|(key, _)| key == "VETINARI_PROMPT"));
    }

    #[test]
    fn configured_environment_is_passed_through() {
        let mut config = config(AgentKind::ClaudeCode, "claude", &[]);
        config.env = vec![("CLAUDE_NO_COLOR".to_owned(), "1".to_owned())];
        let agent = CliAgent::new(config, 100);
        assert!(
            agent
                .environment(&spec())
                .contains(&("CLAUDE_NO_COLOR".to_owned(), "1".to_owned()))
        );
    }

    #[test]
    fn a_skill_name_travels_with_the_task() {
        let agent = agent(AgentKind::Script, "summarise", &[]);
        let spec = spec().with_skill(SkillId::parse("deep-fix").expect("id"));
        assert!(
            agent
                .environment(&spec)
                .contains(&("VETINARI_SKILL".to_owned(), "deep-fix".to_owned()))
        );
    }

    #[tokio::test]
    async fn an_engine_whose_binary_is_on_the_path_is_available() {
        assert!(agent(AgentKind::Script, "sh", &[]).is_available().await);
    }

    #[tokio::test]
    async fn an_engine_whose_binary_is_missing_is_not_available() {
        assert!(
            !agent(AgentKind::Script, "definitely-not-a-real-binary", &[])
                .is_available()
                .await
        );
    }

    #[tokio::test]
    async fn an_engine_configured_with_an_absolute_path_checks_that_path() {
        assert!(
            agent(AgentKind::Script, "/bin/sh", &[])
                .is_available()
                .await
        );
        assert!(
            !agent(AgentKind::Script, "/bin/no-such-binary", &[])
                .is_available()
                .await
        );
    }

    #[tokio::test]
    async fn spawning_a_task_runs_it_and_tracks_the_session() {
        let agent = agent(AgentKind::Script, "cat", &[]);
        let session = agent.spawn_task(&spec()).await.expect("spawn");

        assert_eq!(session.agent().as_str(), "script");
        assert_eq!(session.summary(), "fix the memory leak");
        assert_eq!(
            wait_for_terminal(&agent, session.id()).await,
            AgentStatus::Succeeded
        );
        assert_eq!(
            agent.output(session.id(), 10).await.expect("output"),
            vec!["fix the memory leak".to_owned()]
        );
    }

    #[tokio::test]
    async fn every_session_gets_its_own_id() {
        let agent = agent(AgentKind::Script, "true", &[]);
        let first = agent.spawn_task(&spec()).await.expect("spawn");
        let second = agent.spawn_task(&spec()).await.expect("spawn");
        assert_ne!(first.id(), second.id());
        assert!(first.id().as_str().starts_with("script-"));
    }

    #[tokio::test]
    async fn a_task_runs_in_its_own_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = CliAgent::new(
            AgentConfig {
                args: vec!["-c".to_owned(), "pwd".to_owned()],
                ..config(AgentKind::Script, "sh", &[])
            },
            100,
        );
        let spec = spec().in_directory(dir.path().to_path_buf());

        let session = agent.spawn_task(&spec).await.expect("spawn");
        wait_for_terminal(&agent, session.id()).await;

        let output = agent.output(session.id(), 1).await.expect("output");
        assert!(
            output[0].ends_with(
                dir.path()
                    .file_name()
                    .expect("name")
                    .to_str()
                    .expect("utf-8")
            )
        );
    }

    #[tokio::test]
    async fn a_running_task_can_be_cancelled_through_the_port() {
        let agent = CliAgent::new(
            AgentConfig {
                args: vec!["-c".to_owned(), "sleep 60".to_owned()],
                ..config(AgentKind::Script, "sh", &[])
            },
            100,
        );
        let session = agent.spawn_task(&spec()).await.expect("spawn");

        agent.cancel(session.id()).await.expect("cancel");

        assert_eq!(
            wait_for_terminal(&agent, session.id()).await,
            AgentStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn a_task_that_reads_more_input_can_be_answered() {
        let agent = CliAgent::new(
            AgentConfig {
                args: vec![
                    "-c".to_owned(),
                    "read answer; echo \"answered $answer\"".to_owned(),
                ],
                ..config(AgentKind::Script, "sh", &[])
            },
            100,
        );
        // An empty prompt is not possible, so this task is given one it
        // ignores; what matters is that stdin stays open for the answer.
        let session = agent
            .spawn_task(&TaskSpec::new(
                NonEmptyText::parse("question").expect("prompt"),
                TaskClass::DeepCodeChange,
                PathBuf::from("."),
            ))
            .await
            .expect("spawn");

        wait_for_terminal(&agent, session.id()).await;

        assert_eq!(
            agent.output(session.id(), 10).await.expect("output"),
            vec!["answered question".to_owned()],
            "the prompt itself is the first thing the task reads"
        );
    }

    #[tokio::test]
    async fn asking_about_an_unknown_session_is_an_error() {
        let agent = agent(AgentKind::Script, "true", &[]);
        let ghost = SessionId::parse("ghost").expect("id");
        assert!(agent.check_status(&ghost).await.is_err());
        assert!(agent.output(&ghost, 1).await.is_err());
        assert!(agent.cancel(&ghost).await.is_err());
        assert!(agent.answer(&ghost, "x").await.is_err());
    }

    #[test]
    fn a_binary_is_resolved_against_the_path() {
        assert!(resolve_binary("sh").is_some());
        assert!(resolve_binary("definitely-not-a-real-binary").is_none());
        assert_eq!(
            resolve_binary("/bin/sh").as_deref(),
            Some(Path::new("/bin/sh"))
        );
    }
}
