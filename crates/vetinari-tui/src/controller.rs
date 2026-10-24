//! Runs the tasks the state asks for against the adapters.
//!
//! Every method answers with [`Update`]s rather than touching the state, so
//! the whole of the front-end's behaviour stays testable: the state decides,
//! the controller performs, and nothing in between holds a terminal.

use vetinari_adapters::Runtime;
use vetinari_core::application::attention::Polled;
use vetinari_core::application::observed::ObservedAgents;
use vetinari_core::application::ports::{DeviceCodeChallenge, DeviceCodePoll, PortResult};
use vetinari_core::application::workflows::WorkflowOutcome;
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{ItemId, SourceId};
use vetinari_core::domain::profile::Profile;
use vetinari_core::domain::skill::SkillLibrary;

use crate::app::{SessionRow, SpaceRow, StatusMessage, Task, Update, describe_failure};

/// Performs work against the adapters.
pub struct Controller {
    runtime: Runtime,
    library: SkillLibrary,
}

impl Controller {
    /// How many lines of agent output to fetch for the output overlay.
    pub const OUTPUT_LINES: usize = 200;

    /// How many finished sessions stay in the monitor.
    pub const FINISHED_SESSIONS_KEPT: usize = 20;

    /// Take ownership of a runtime.
    #[must_use]
    pub fn new(runtime: Runtime, library: SkillLibrary) -> Self {
        Self { runtime, library }
    }

    /// The runtime, for the parts of start-up that need it.
    #[must_use]
    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The skill library as the controller last saw it.
    #[must_use]
    pub const fn library(&self) -> &SkillLibrary {
        &self.library
    }

    /// Load the skill library, falling back to the built-ins.
    pub async fn load_library(&mut self) -> SkillLibrary {
        self.library = self.runtime.skills.load_or_builtin().await;
        self.library.clone()
    }

    /// Poll every source and agent the profile wants.
    pub async fn refresh(&mut self, profile: &Profile) -> Vec<Update> {
        let mut updates = Vec::new();

        for (session, error) in self.runtime.agents.poll().await {
            updates.push(Update::Log(format!("WARN session {session}: {error}")));
        }
        self.runtime
            .agents
            .prune_finished(Self::FINISHED_SESSIONS_KEPT);

        // The agents a target is running on its own: Vetinari cannot control
        // them, but a blocked one is still the most expensive thing in the
        // inbox, so it is ranked alongside the rest.
        let observation = self.runtime.observed.poll().await;
        for (kind, error) in &observation.problems {
            updates.push(Update::Log(format!(
                "WARN target {kind} could not list its agents: {error}"
            )));
        }

        let mut polled = self.runtime.attention.poll(profile).await;
        polled.push(Polled {
            source: self.runtime.agents.source_id().clone(),
            outcome: Ok(self.runtime.agents.attention_items()),
        });
        // A target that could not list its agents has already been logged,
        // and leaving the source out keeps its agents rather than closing
        // them.
        if observation.problems.is_empty() {
            polled.push(Polled {
                source: ObservedAgents::source_id(),
                outcome: Ok(ObservedAgents::attention_items(&observation.agents)),
            });
        }
        updates.push(Update::Polled(polled));

        updates.push(Update::Sessions(
            self.runtime
                .agents
                .sessions()
                .into_iter()
                .map(SessionRow::from_session)
                .collect(),
        ));

        let inventory = self.runtime.workflows.workspaces().inventory().await;
        for (kind, error) in &inventory.problems {
            updates.push(Update::Log(format!("WARN target {kind}: {error}")));
        }
        updates.push(Update::Spaces(
            inventory.spaces.iter().map(SpaceRow::from_space).collect(),
        ));

        updates.push(Update::Keychain(
            self.runtime.auth.summary(profile.account().cloned()).await,
        ));

        updates
    }

    /// Run one task.
    pub async fn run(&mut self, task: Task, profile: &Profile) -> Vec<Update> {
        match task {
            Task::Refresh => self.refresh(profile).await,

            Task::StartFeedback(request) => {
                let outcome = self
                    .runtime
                    .workflows
                    .start_feedback(&request, &self.library, profile, &mut self.runtime.agents)
                    .await;
                self.after_workflow("feedback", outcome, None).await
            }

            Task::StartPrReview { pr, item } => {
                let outcome = self
                    .runtime
                    .workflows
                    .start_pr_review(&pr, &self.library, profile, &mut self.runtime.agents)
                    .await;
                self.after_workflow(&format!("review of #{}", pr.number), outcome, Some(item))
                    .await
            }

            Task::MeetingPrep {
                item,
                title,
                event,
                starts_at,
            } => {
                let outcome = self
                    .runtime
                    .workflows
                    .start_meeting_prep(
                        &title,
                        &event,
                        starts_at,
                        &self.library,
                        profile,
                        &mut self.runtime.agents,
                    )
                    .await;
                self.after_workflow(&format!("preparation for {title}"), outcome, Some(item))
                    .await
            }

            Task::RunSkill { id, bindings } => {
                let outcome = self
                    .runtime
                    .workflows
                    .start_quick_task(
                        &id,
                        bindings,
                        &self.library,
                        profile,
                        &mut self.runtime.agents,
                    )
                    .await;
                self.after_workflow(&format!("skill {id}"), outcome, None)
                    .await
            }

            Task::ApprovePr { source, pr } => {
                let result = match self.runtime.code_hosts.get(&source) {
                    Some(host) => host.approve(profile.account(), &pr, None).await,
                    None => Err(missing(&source, "code host")),
                };
                self.report(&format!("approving #{}", pr.number), result, profile)
                    .await
            }

            Task::CommentPr { source, pr, body } => {
                let result = match self.runtime.code_hosts.get(&source) {
                    Some(host) => host.comment(profile.account(), &pr, &body).await,
                    None => Err(missing(&source, "code host")),
                };
                self.report(&format!("commenting on #{}", pr.number), result, profile)
                    .await
            }

            Task::ReplyChat {
                source,
                message,
                body,
            } => {
                let result = match self.runtime.chats.get(&source) {
                    Some(chat) => chat.reply(profile.account(), &message, &body).await,
                    None => Err(missing(&source, "chat source")),
                };
                self.report(&format!("replying in {}", message.channel), result, profile)
                    .await
            }

            Task::TransitionIssue {
                source,
                issue,
                status,
            } => {
                let result = match self.runtime.issue_trackers.get(&source) {
                    Some(tracker) => tracker.transition(profile.account(), &issue, &status).await,
                    None => Err(missing(&source, "issue tracker")),
                };
                self.report(
                    &format!("moving {} to {status}", issue.key),
                    result,
                    profile,
                )
                .await
            }

            Task::AssignIssue {
                source,
                issue,
                assignee,
            } => {
                let result = match self.runtime.issue_trackers.get(&source) {
                    Some(tracker) => tracker.assign(profile.account(), &issue, &assignee).await,
                    None => Err(missing(&source, "issue tracker")),
                };
                self.report(&format!("assigning {}", issue.key), result, profile)
                    .await
            }

            Task::FocusSpace(space) => {
                let result = self
                    .runtime
                    .workflows
                    .workspaces()
                    .focus_space(&space)
                    .await;
                self.report(&format!("focusing {space}"), result, profile)
                    .await
            }

            Task::CloseSpace(space) => {
                let result = self
                    .runtime
                    .workflows
                    .workspaces()
                    .close_space(&space)
                    .await;
                let mut updates = self
                    .report(&format!("closing {space}"), result, profile)
                    .await;
                updates.extend(self.space_list().await);
                updates
            }

            Task::CancelAgent(session) => {
                let result = self.runtime.agents.cancel(&session).await;
                let mut updates = self
                    .report(&format!("cancelling {session}"), result, profile)
                    .await;
                updates.push(self.session_list());
                updates
            }

            Task::RerunAgent(session) => match self.runtime.agents.rerun(&session).await {
                Ok(new_session) => vec![
                    Update::Status(StatusMessage::info(format!(
                        "re-running {session} as {new_session}"
                    ))),
                    self.session_list(),
                ],
                Err(error) => vec![Update::Status(describe_failure(
                    &format!("re-running {session}"),
                    &error,
                ))],
            },

            Task::AnswerAgent(session, answer) => {
                let result = self.runtime.agents.answer(&session, &answer).await;
                let mut updates = self
                    .report(&format!("answering {session}"), result, profile)
                    .await;
                updates.push(self.session_list());
                updates
            }

            Task::ShowAgentOutput(session) => {
                match self.runtime.agents.logs(&session, Self::OUTPUT_LINES).await {
                    Ok(lines) => vec![Update::AgentOutput(session, lines)],
                    Err(error) => vec![Update::Status(describe_failure(
                        &format!("reading the output of {session}"),
                        &error,
                    ))],
                }
            }

            Task::SaveSkill(skill) => {
                let id = skill.id().clone();
                match self.runtime.skills.save(&mut self.library, skill).await {
                    Ok(()) => vec![
                        Update::Library(self.library.clone()),
                        Update::Status(StatusMessage::info(format!("saved skill {id}"))),
                    ],
                    Err(error) => vec![Update::Status(describe_failure(
                        &format!("saving skill {id}"),
                        &error,
                    ))],
                }
            }

            Task::SaveToken(provider, secret) => {
                let account = profile.account().cloned();
                match self
                    .runtime
                    .auth
                    .save_api_token(provider, account.clone(), secret)
                    .await
                {
                    Ok(()) => vec![
                        Update::Keychain(self.runtime.auth.summary(account).await),
                        Update::CloseOverlay,
                        Update::Status(StatusMessage::info(format!(
                            "stored the {} credential",
                            provider.display_name()
                        ))),
                    ],
                    Err(error) => vec![Update::Status(describe_failure(
                        &format!("storing the {} credential", provider.display_name()),
                        &error,
                    ))],
                }
            }

            Task::DelegateToSpace(session) => {
                let placed = match (
                    self.runtime.agents.session(&session),
                    self.runtime.agents.spec_for(&session),
                ) {
                    (Some(tracked), Some(spec)) => tracked
                        .space()
                        .cloned()
                        .map(|space| (space, tracked.agent().clone(), spec.clone())),
                    _ => None,
                };
                let Some((space, agent, spec)) = placed else {
                    return vec![Update::Status(StatusMessage::warn(format!(
                        "{session} has no space to run in"
                    )))];
                };
                match self
                    .runtime
                    .workflows
                    .delegate_to_space(&space, &agent, &self.runtime.agents, &spec)
                    .await
                {
                    Ok(command) => vec![Update::Status(StatusMessage::info(format!(
                        "running {} in {space}",
                        command.join(" ")
                    )))],
                    Err(error) => vec![Update::Status(describe_failure(
                        &format!("running {agent} in {space}"),
                        &error,
                    ))],
                }
            }

            Task::BeginDeviceFlow(provider) => {
                match self.runtime.auth.begin_device_flow(provider).await {
                    Ok(challenge) => vec![Update::DeviceCode(provider, challenge)],
                    Err(error) => vec![Update::Status(describe_failure(
                        &format!("authorising {}", provider.display_name()),
                        &error,
                    ))],
                }
            }
        }
    }

    /// Poll a device-code flow once, for the event loop's timer.
    pub async fn poll_device_flow(
        &mut self,
        provider: Provider,
        challenge: &DeviceCodeChallenge,
        profile: &Profile,
    ) -> Vec<Update> {
        let account = profile.account().cloned();
        match self
            .runtime
            .auth
            .poll_device_flow(provider, account.clone(), challenge)
            .await
        {
            Ok(DeviceCodePoll::Pending) => {
                vec![Update::DeviceCodeMessage(
                    "Waiting for approval...".to_owned(),
                )]
            }
            Ok(DeviceCodePoll::Granted(_)) => vec![
                Update::CloseOverlay,
                Update::Keychain(self.runtime.auth.summary(account).await),
                Update::Status(StatusMessage::info(format!(
                    "{} is authenticated",
                    provider.display_name()
                ))),
            ],
            Ok(DeviceCodePoll::Rejected { reason }) => vec![
                Update::CloseOverlay,
                Update::Status(StatusMessage::warn(format!(
                    "{} was not authorised: {reason}",
                    provider.display_name()
                ))),
            ],
            Err(error) => vec![Update::Status(describe_failure(
                &format!("authorising {}", provider.display_name()),
                &error,
            ))],
        }
    }

    async fn after_workflow(
        &mut self,
        what: &str,
        outcome: PortResult<WorkflowOutcome>,
        for_item: Option<ItemId>,
    ) -> Vec<Update> {
        match outcome {
            Ok(outcome) => {
                let mut updates = Vec::new();
                if let Some(worktree) = &outcome.worktree {
                    updates.push(Update::Log(format!(
                        "INFO worktree {}",
                        worktree.describe()
                    )));
                }
                for warning in &outcome.warnings {
                    updates.push(Update::Log(format!("WARN {what}: {warning}")));
                }
                let where_it_runs = match &outcome.space {
                    Some(space) => format!(" in {space}"),
                    None => String::new(),
                };
                let message = if outcome.warnings.is_empty() {
                    StatusMessage::info(format!(
                        "{what} started on {}{where_it_runs}",
                        outcome.agent
                    ))
                } else {
                    StatusMessage::warn(format!(
                        "{what} started on {} but {} thing(s) did not work",
                        outcome.agent,
                        outcome.warnings.len()
                    ))
                };
                updates.push(Update::Status(message));
                if let Some(item) = for_item {
                    updates.push(Update::AgentStarted {
                        item,
                        session: outcome.session.clone(),
                    });
                }
                updates.push(self.session_list());
                updates.extend(self.space_list().await);
                updates
            }
            Err(error) => vec![Update::Status(describe_failure(what, &error))],
        }
    }

    async fn report(
        &mut self,
        what: &str,
        result: PortResult<()>,
        profile: &Profile,
    ) -> Vec<Update> {
        let _ = profile;
        match result {
            Ok(()) => vec![Update::Status(StatusMessage::info(format!("{what}: done")))],
            Err(error) => vec![Update::Status(describe_failure(what, &error))],
        }
    }

    fn session_list(&self) -> Update {
        Update::Sessions(
            self.runtime
                .agents
                .sessions()
                .into_iter()
                .map(SessionRow::from_session)
                .collect(),
        )
    }

    async fn space_list(&self) -> Vec<Update> {
        let inventory = self.runtime.workflows.workspaces().inventory().await;
        vec![Update::Spaces(
            inventory.spaces.iter().map(SpaceRow::from_space).collect(),
        )]
    }
}

fn missing(source: &SourceId, what: &str) -> vetinari_core::application::ports::PortError {
    vetinari_core::application::ports::PortError::not_configured(
        source.as_str(),
        format!("no {what} with that id is configured"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;
    use time::macros::datetime;
    use vetinari_adapters::git::GitWorktreeManager;
    use vetinari_adapters::{HttpClient, Paths};
    use vetinari_core::application::agents::AgentRegistry;
    use vetinari_core::application::config::{Config, ParseContext, RawConfig};
    use vetinari_core::application::queue::QueueService;
    use vetinari_core::domain::attention::{IssueRef, MessageRef, PullRequestRef};
    use vetinari_core::domain::credentials::Secret;
    use vetinari_core::domain::feedback::{CapturedContext, FeedbackRequest, FeedbackTarget};
    use vetinari_core::domain::ids::{ProfileId, SessionId, SkillId};
    use vetinari_core::domain::keymap::Mode;
    use vetinari_core::domain::queue::rules::QueueRules;
    use vetinari_core::domain::skill::Bindings;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::testing::{
        FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore, MemorySkillStore,
    };

    use super::*;

    const NOW: time::OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    struct Harness {
        controller: Controller,
        profile: Profile,
        repo: PathBuf,
        store: Arc<MemoryCredentialStore>,
        skills: Arc<MemorySkillStore>,
        _trees: TempDir,
        _repos: TempDir,
    }

    /// A real repository, so the worktrees the workflows cut are real and the
    /// agents they start have somewhere to run.
    fn repo_with_a_commit(parent: &std::path::Path) -> PathBuf {
        let repo = parent.join("web");
        let run = |args: &[&str], dir: &std::path::Path| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::create_dir_all(&repo).expect("create repo dir");
        run(&["init", "-b", "main", "."], &repo);
        run(&["config", "user.email", "dev@example.test"], &repo);
        run(&["config", "user.name", "Dev"], &repo);
        std::fs::write(repo.join("README.md"), "hello").expect("write");
        run(&["add", "."], &repo);
        run(&["commit", "-m", "initial"], &repo);
        repo
    }

    fn harness(extra_toml: &str) -> Harness {
        let trees = tempfile::tempdir().expect("tempdir");
        let repos = tempfile::tempdir().expect("tempdir");
        let repo = repo_with_a_commit(repos.path());
        let toml_text = format!(
            r#"
            [[profiles]]
            id = "work"
            name = "Work"
            target = "editor"
            worktree_root = "{}"
            repo_roots = ["{}"]

            [agents.claude-code]
            kind = "script"
            command = "true"

            [agents.agy-gemini]
            kind = "script"
            command = "true"

            [agents.script]
            kind = "script"
            command = "true"

            [targets.editor]
            command = "true"
            {extra_toml}
            "#,
            trees.path().display(),
            repos.path().display()
        );
        let raw: RawConfig = toml::from_str(&toml_text).expect("valid TOML");
        let config = Config::parse(raw, &ParseContext::default()).expect("valid config");
        let profile = config.profiles.active().clone();

        let clock = Arc::new(FixedClock::new(NOW));
        let store = Arc::new(MemoryCredentialStore::default());
        let skills = Arc::new(MemorySkillStore::default());
        let runtime = Runtime::assemble(
            config,
            Paths {
                config_dir: PathBuf::from("/tmp/vetinari-config"),
                state_dir: PathBuf::from("/tmp/vetinari-state"),
            },
            clock,
            store.clone(),
            Arc::new(FakeDeviceCodeFlow::new(
                1,
                NOW + time::Duration::minutes(10),
            )),
            HttpClient::new().expect("client"),
            skills.clone(),
            Arc::new(GitWorktreeManager::default()),
        )
        .expect("runtime");

        Harness {
            controller: Controller::new(runtime, SkillLibrary::builtin()),
            profile,
            repo,
            store,
            skills,
            _trees: trees,
            _repos: repos,
        }
    }

    fn feedback_request(repo: &std::path::Path) -> FeedbackRequest {
        FeedbackRequest::new(
            NonEmptyText::parse("show group labels in which-key").expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("web").expect("name"),
                repo.to_path_buf(),
            ),
            CapturedContext::new(ProfileId::parse("work").expect("id"), Mode::Normal),
            NOW,
        )
    }

    fn branch_of(worktree: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(worktree)
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn status_of(updates: &[Update]) -> StatusMessage {
        updates
            .iter()
            .find_map(|update| match update {
                Update::Status(message) => Some(message.clone()),
                _ => None,
            })
            .expect("a status update")
    }

    /// A minimal stand-in for Herdr, so the wiring from the socket to the
    /// monitor is exercised rather than assumed.
    fn fake_herdr(socket: std::path::PathBuf) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut lines = BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let id = field(&line, "id");
                        let result = match field(&line, "method").as_str() {
                            "workspace.list" => concat!(
                                r#"{"workspaces":[{"workspace_id":"w1","#,
                                r#""label":"api","cwd":"/src/api"}]}"#
                            )
                            .to_owned(),
                            "agent.list" => concat!(
                                r#"{"agents":[{"pane_id":"w1:p1","workspace_id":"w1","#,
                                r#""agent":"claude","agent_status":"blocked"}]}"#
                            )
                            .to_owned(),
                            _ => "{}".to_owned(),
                        };
                        let reply = format!(r#"{{"id":"{id}","result":{result}}}"#);
                        let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = write_half.flush().await;
                    }
                });
            }
        });
    }

    /// Pull a string field out of a request, so the test needs no JSON parser.
    fn field(line: &str, name: &str) -> String {
        let key = format!("\"{name}\":\"");
        let start = line.find(&key).map(|at| at + key.len()).unwrap_or_default();
        line[start..]
            .find('"')
            .map(|end| line[start..start + end].to_owned())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_refresh_queues_the_agents_herdr_is_running_and_ranks_a_blocked_one_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("herdr.sock");
        fake_herdr(socket.clone());
        let mut h = harness(&format!(
            "\n[targets.herdr]\nsocket = \"{}\"\n",
            socket.display()
        ));

        let updates = h.controller.refresh(&h.profile).await;

        let mut queue = QueueService::new(QueueRules::default());
        queue.absorb(NOW, polled_of(&updates));
        let view = queue.view(NOW);
        let first = view.next.first().expect("at least one item").entry.item();
        assert_eq!(
            first.state(),
            vetinari_core::domain::attention::ItemState::AgentWaiting,
            "a blocked agent is the most expensive thing in the inbox"
        );
        assert!(first.title().as_str().contains("claude in w1"));

        let spaces = updates
            .iter()
            .find_map(|update| match update {
                Update::Spaces(spaces) => Some(spaces.clone()),
                _ => None,
            })
            .expect("spaces");
        assert!(
            spaces.iter().any(|space| space.id.as_str() == "w1"),
            "herdr's workspaces are its spaces: {spaces:?}"
        );
    }

    #[tokio::test]
    async fn a_refresh_reports_the_feed_the_sessions_the_spaces_and_the_keychain() {
        let mut h = harness("");

        let updates = h.controller.refresh(&h.profile).await;

        assert!(updates.iter().any(|u| matches!(u, Update::Polled(_))));
        assert!(updates.iter().any(|u| matches!(u, Update::Sessions(_))));
        assert!(updates.iter().any(|u| matches!(u, Update::Spaces(_))));
        assert!(updates.iter().any(|u| matches!(u, Update::Keychain(_))));
    }

    #[tokio::test]
    async fn the_feedback_workflow_cuts_a_worktree_opens_a_space_and_starts_an_agent() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(
                Task::StartFeedback(Box::new(feedback_request(&h.repo))),
                &h.profile,
            )
            .await;

        let session_dir = h
            .controller
            .runtime()
            .agents
            .sessions()
            .first()
            .expect("a session")
            .working_dir()
            .to_path_buf();
        assert!(
            session_dir.join("README.md").is_file(),
            "the agent runs in the worktree"
        );
        assert_eq!(
            branch_of(&session_dir),
            "vetinari/feedback/show-group-labels-in-which-key"
        );
        let status = status_of(&updates);
        assert_eq!(status.level, crate::app::Level::Info, "got {}", status.text);
        assert!(status.text.contains("claude-code"), "got {}", status.text);
        assert!(status.text.contains("feedback"));

        let Some(Update::Sessions(sessions)) = updates
            .iter()
            .find(|u| matches!(u, Update::Sessions(_)))
            .cloned()
        else {
            panic!("expected the session list");
        };
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].space.is_some(), "the session knows its space");
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, Update::Log(line) if line.contains("worktree")))
        );
    }

    #[tokio::test]
    async fn a_pr_review_starts_from_a_pull_request() {
        let mut h = harness("");
        let pr = PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "https://github.test/acme/web.git".to_owned(),
        };

        let item = ItemId::from_parts(&SourceId::parse("github").expect("id"), "acme/web#142");

        let updates = h
            .controller
            .run(
                Task::StartPrReview {
                    pr,
                    item: item.clone(),
                },
                &h.profile,
            )
            .await;

        let session = h
            .controller
            .runtime()
            .agents
            .sessions()
            .first()
            .expect("a session")
            .id()
            .clone();
        assert!(
            updates.iter().any(|update| matches!(
                update,
                Update::AgentStarted { item: for_item, session: started }
                    if *for_item == item && *started == session
            )),
            "the item is handed to the agent reviewing it"
        );

        let session_dir = h
            .controller
            .runtime()
            .agents
            .sessions()
            .first()
            .expect("a session")
            .working_dir()
            .to_path_buf();
        assert_eq!(branch_of(&session_dir), "vetinari/review/pr-142");
        let status = status_of(&updates);
        assert!(status.text.contains("#142"));
        assert!(
            status.text.contains("did not work"),
            "the test repository has no remote, so the fetch is reported: {}",
            status.text
        );
    }

    #[tokio::test]
    async fn a_workflow_that_cannot_cut_a_worktree_reports_why() {
        let mut h = harness("");
        let not_a_repo = tempfile::tempdir().expect("tempdir");

        let updates = h
            .controller
            .run(
                Task::StartFeedback(Box::new(feedback_request(not_a_repo.path()))),
                &h.profile,
            )
            .await;

        let status = status_of(&updates);
        assert_eq!(status.level, crate::app::Level::Error);
        assert!(status.text.contains("git"), "got {}", status.text);
        assert!(
            h.controller.runtime().agents.sessions().is_empty(),
            "nothing is started without isolation"
        );
    }

    #[tokio::test]
    async fn a_skill_missing_a_binding_is_reported_rather_than_run() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(
                Task::RunSkill {
                    id: SkillId::parse("meeting-prep").expect("id"),
                    bindings: Bindings::new(),
                },
                &h.profile,
            )
            .await;

        let status = status_of(&updates);
        assert_eq!(status.level, crate::app::Level::Error);
        assert!(status.text.contains("skill meeting-prep"));
    }

    #[tokio::test]
    async fn a_skill_with_its_bindings_runs_on_the_engine_its_class_routes_to() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(
                Task::RunSkill {
                    id: SkillId::parse("pr-summary").expect("id"),
                    bindings: Bindings::new()
                        .with("number", "142")
                        .with("repo", "acme/web"),
                },
                &h.profile,
            )
            .await;

        let status = status_of(&updates);
        assert_eq!(status.level, crate::app::Level::Info);
        assert!(status.text.contains("agy-gemini"), "got {}", status.text);
    }

    #[tokio::test]
    async fn acting_on_a_source_that_is_not_configured_says_so() {
        let mut h = harness("");
        let source = SourceId::parse("github").expect("id");
        let pr = PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 1,
            head_ref: "h".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "u".to_owned(),
        };

        let updates = h
            .controller
            .run(
                Task::ApprovePr {
                    source: source.clone(),
                    pr,
                },
                &h.profile,
            )
            .await;
        assert!(status_of(&updates).text.contains("code host"));

        let updates = h
            .controller
            .run(
                Task::TransitionIssue {
                    source: source.clone(),
                    issue: IssueRef {
                        key: "DEV-1".to_owned(),
                        status: "To Do".to_owned(),
                        transitions: Vec::new(),
                    },
                    status: "Done".to_owned(),
                },
                &h.profile,
            )
            .await;
        assert!(status_of(&updates).text.contains("issue tracker"));

        let updates = h
            .controller
            .run(
                Task::ReplyChat {
                    source,
                    message: MessageRef {
                        channel: "D1".to_owned(),
                        thread: None,
                    },
                    body: "hello".to_owned(),
                },
                &h.profile,
            )
            .await;
        assert!(status_of(&updates).text.contains("chat source"));
    }

    #[tokio::test]
    async fn saving_a_skill_persists_it_and_reports_the_new_library() {
        let mut h = harness("");
        let skill = vetinari_core::domain::skill::Skill::new(
            SkillId::parse("triage").expect("id"),
            NonEmptyText::parse("Triage").expect("name"),
            vetinari_core::domain::skill::PromptTemplate::parse("Triage {{number}}")
                .expect("template"),
            vetinari_core::domain::agent::TaskClass::PrSummary,
        );

        let updates = h.controller.run(Task::SaveSkill(skill), &h.profile).await;

        assert!(updates.iter().any(|u| matches!(u, Update::Library(_))));
        assert!(
            h.skills
                .library()
                .get(&SkillId::parse("triage").expect("id"))
                .is_some()
        );
        assert!(status_of(&updates).text.contains("triage"));
    }

    #[tokio::test]
    async fn storing_a_token_puts_it_in_the_keychain_and_closes_the_prompt() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(
                Task::SaveToken(Provider::GitHub, Secret::new("ghp_test")),
                &h.profile,
            )
            .await;

        assert_eq!(h.store.len(), 1);
        assert!(updates.iter().any(|u| matches!(u, Update::CloseOverlay)));
        let Some(Update::Keychain(summary)) = updates
            .iter()
            .find(|u| matches!(u, Update::Keychain(_)))
            .cloned()
        else {
            panic!("expected the keychain summary");
        };
        assert!(summary.contains('1'), "got {summary}");
    }

    #[tokio::test]
    async fn a_device_flow_shows_its_challenge_then_grants() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(Task::BeginDeviceFlow(Provider::GoogleCalendar), &h.profile)
            .await;
        let Some(Update::DeviceCode(provider, challenge)) = updates.into_iter().next() else {
            panic!("expected a device-code challenge");
        };
        assert_eq!(provider, Provider::GoogleCalendar);

        let first = h
            .controller
            .poll_device_flow(provider, &challenge, &h.profile)
            .await;
        assert!(
            first
                .iter()
                .any(|u| matches!(u, Update::DeviceCodeMessage(_)))
        );

        let second = h
            .controller
            .poll_device_flow(provider, &challenge, &h.profile)
            .await;
        assert!(second.iter().any(|u| matches!(u, Update::CloseOverlay)));
        assert_eq!(h.store.len(), 1);
        assert!(status_of(&second).text.contains("authenticated"));
    }

    #[tokio::test]
    async fn a_device_flow_for_a_token_provider_is_refused_with_an_explanation() {
        let mut h = harness("");

        let updates = h
            .controller
            .run(Task::BeginDeviceFlow(Provider::GitHub), &h.profile)
            .await;

        let status = status_of(&updates);
        assert!(status.text.contains("GitHub"));
        assert!(status.text.to_lowercase().contains("api token"));
    }

    #[tokio::test]
    async fn reading_the_output_of_an_unknown_session_is_reported() {
        let mut h = harness("");
        let updates = h
            .controller
            .run(
                Task::ShowAgentOutput(SessionId::parse("ghost").expect("id")),
                &h.profile,
            )
            .await;
        assert_eq!(status_of(&updates).level, crate::app::Level::Error);
    }

    #[tokio::test]
    async fn cancelling_and_answering_an_agent_report_what_happened() {
        let mut h = harness("");
        h.controller
            .run(
                Task::StartFeedback(Box::new(feedback_request(&h.repo))),
                &h.profile,
            )
            .await;
        let session = h
            .controller
            .runtime()
            .agents
            .sessions()
            .first()
            .expect("a session")
            .id()
            .clone();

        let updates = h
            .controller
            .run(Task::CancelAgent(session.clone()), &h.profile)
            .await;
        assert!(status_of(&updates).text.contains("cancelling"));
        assert!(updates.iter().any(|u| matches!(u, Update::Sessions(_))));

        let updates = h
            .controller
            .run(Task::RerunAgent(session), &h.profile)
            .await;
        assert!(status_of(&updates).text.contains("re-running"));
        assert_eq!(h.controller.runtime().agents.sessions().len(), 2);
    }

    #[tokio::test]
    async fn a_session_can_be_run_in_its_space_beside_the_developer() {
        let mut h = harness("");
        h.controller
            .run(
                Task::StartFeedback(Box::new(feedback_request(&h.repo))),
                &h.profile,
            )
            .await;
        let session = h
            .controller
            .runtime()
            .agents
            .sessions()
            .first()
            .expect("a session")
            .id()
            .clone();

        let updates = h
            .controller
            .run(Task::DelegateToSpace(session), &h.profile)
            .await;

        let status = status_of(&updates);
        assert_eq!(
            status.level,
            crate::app::Level::Error,
            "got {}",
            status.text
        );
        assert!(
            status.text.contains("editor target") || status.text.contains("does not support"),
            "an editor cannot host a command, and says so: {}",
            status.text
        );
    }

    #[tokio::test]
    async fn delegating_a_session_with_no_space_says_so() {
        let mut h = harness("");
        let updates = h
            .controller
            .run(
                Task::DelegateToSpace(SessionId::parse("ghost").expect("id")),
                &h.profile,
            )
            .await;
        assert!(status_of(&updates).text.contains("no space"));
    }

    #[tokio::test]
    async fn focusing_a_space_that_no_target_owns_is_reported() {
        let mut h = harness("");
        let updates = h
            .controller
            .run(
                Task::FocusSpace(vetinari_core::domain::ids::SpaceId::parse("ghost").expect("id")),
                &h.profile,
            )
            .await;
        assert_eq!(status_of(&updates).level, crate::app::Level::Error);
    }

    #[tokio::test]
    async fn the_library_is_loaded_from_the_store_at_start_up() {
        let mut h = harness("");
        let library = h.controller.load_library().await;
        assert!(
            library
                .get(&SkillId::parse("pr-review").expect("id"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn agent_work_appears_in_the_feed_alongside_the_sources() {
        let mut h = harness("");
        h.controller
            .run(
                Task::StartFeedback(Box::new(feedback_request(&h.repo))),
                &h.profile,
            )
            .await;

        let updates = h.controller.refresh(&h.profile).await;

        let agents = polled_of(&updates)
            .into_iter()
            .find(|polled| polled.source.as_str() == AgentRegistry::SOURCE_NAME)
            .expect("the agents answer every poll");
        let items = agents.outcome.expect("agents always answer");
        assert_eq!(items.len(), 1, "the running agent is an item of its own");
        assert_eq!(
            items[0].category(),
            vetinari_core::domain::attention::SourceCategory::AgentState
        );
    }

    fn polled_of(updates: &[Update]) -> Vec<Polled> {
        updates
            .iter()
            .find_map(|update| match update {
                Update::Polled(polled) => Some(polled.clone()),
                _ => None,
            })
            .expect("a poll")
    }
}
