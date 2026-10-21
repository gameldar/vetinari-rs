//! Agent tasks and sessions.
//!
//! Vetinari is agent-agnostic: a task says what needs doing and which class of
//! work it is, and the router decides which engine runs it. A session is the
//! record of one such run, whichever engine produced it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use time::{Duration, OffsetDateTime};

use super::attention::ItemState;
use super::ids::{AgentId, SessionId, SkillId, SpaceId};
use super::text::NonEmptyText;

/// The kind of work a task represents. Routing rules are written against
/// these rather than against free text, so a rule cannot name work that does
/// not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskClass {
    /// Deep, interactive modification of a codebase.
    DeepCodeChange,
    /// Reviewing a pull request.
    PrReview,
    /// Summarising a pull request.
    PrSummary,
    /// Summarising or triaging a tracker issue.
    IssueSummary,
    /// Producing a standup summary.
    StandupSummary,
    /// Preparing for a meeting.
    MeetingPrep,
    /// Implementing feedback about Vetinari itself or the active project.
    Enhancement,
    /// Answering a question about a large codebase or document.
    LongContextQuery,
}

/// A configuration file named a task class that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown task class {name:?}")]
pub struct UnknownTaskClass {
    /// The unrecognised name.
    pub name: String,
}

impl TaskClass {
    /// Every task class.
    pub const ALL: &'static [TaskClass] = &[
        TaskClass::DeepCodeChange,
        TaskClass::PrReview,
        TaskClass::PrSummary,
        TaskClass::IssueSummary,
        TaskClass::StandupSummary,
        TaskClass::MeetingPrep,
        TaskClass::Enhancement,
        TaskClass::LongContextQuery,
    ];

    /// The name used in configuration files.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            TaskClass::DeepCodeChange => "deep-code-change",
            TaskClass::PrReview => "pr-review",
            TaskClass::PrSummary => "pr-summary",
            TaskClass::IssueSummary => "issue-summary",
            TaskClass::StandupSummary => "standup-summary",
            TaskClass::MeetingPrep => "meeting-prep",
            TaskClass::Enhancement => "enhancement",
            TaskClass::LongContextQuery => "long-context-query",
        }
    }

    /// Whether this class of work wants its own isolated worktree.
    #[must_use]
    pub const fn wants_worktree(&self) -> bool {
        matches!(
            self,
            TaskClass::DeepCodeChange | TaskClass::PrReview | TaskClass::Enhancement
        )
    }
}

impl FromStr for TaskClass {
    type Err = UnknownTaskClass;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        TaskClass::ALL
            .iter()
            .copied()
            .find(|class| class.name() == raw.trim())
            .ok_or_else(|| UnknownTaskClass {
                name: raw.trim().to_owned(),
            })
    }
}

impl fmt::Display for TaskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl serde::Serialize for TaskClass {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> serde::Deserialize<'de> for TaskClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Everything an engine needs to start a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    prompt: NonEmptyText,
    class: TaskClass,
    working_dir: PathBuf,
    context_paths: Vec<PathBuf>,
    skill: Option<SkillId>,
    env: Vec<(String, String)>,
}

impl TaskSpec {
    /// Build a task from the three things it cannot do without.
    #[must_use]
    pub fn new(prompt: NonEmptyText, class: TaskClass, working_dir: PathBuf) -> Self {
        Self {
            prompt,
            class,
            working_dir,
            context_paths: Vec::new(),
            skill: None,
            env: Vec::new(),
        }
    }

    /// The prompt to send.
    #[must_use]
    pub const fn prompt(&self) -> &NonEmptyText {
        &self.prompt
    }

    /// The class of work, used for routing.
    #[must_use]
    pub const fn class(&self) -> TaskClass {
        self.class
    }

    /// Where the engine should run.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Files and directories worth putting in front of the agent.
    #[must_use]
    pub fn context_paths(&self) -> &[PathBuf] {
        &self.context_paths
    }

    /// The skill whose template produced this prompt, when one did.
    #[must_use]
    pub const fn skill(&self) -> Option<&SkillId> {
        self.skill.as_ref()
    }

    /// Extra environment for the spawned process.
    #[must_use]
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// Add a context path.
    #[must_use]
    pub fn with_context_path(mut self, path: PathBuf) -> Self {
        self.context_paths.push(path);
        self
    }

    /// Add several context paths.
    #[must_use]
    pub fn with_context_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.context_paths.extend(paths);
        self
    }

    /// Record the skill that produced the prompt.
    #[must_use]
    pub fn with_skill(mut self, skill: SkillId) -> Self {
        self.skill = Some(skill);
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Move the task to a different working directory, e.g. a fresh worktree.
    #[must_use]
    pub fn in_directory(mut self, working_dir: PathBuf) -> Self {
        self.working_dir = working_dir;
        self
    }
}

/// Where an agent run has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Accepted but not yet started.
    Pending,
    /// Working.
    Running,
    /// Stopped, waiting for the developer to answer something.
    WaitingForInput {
        /// What the agent asked.
        question: String,
    },
    /// Finished successfully.
    Succeeded,
    /// Finished unsuccessfully.
    Failed {
        /// What went wrong.
        message: String,
    },
    /// Stopped on request.
    Cancelled,
}

impl AgentStatus {
    /// Whether no further change is expected.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentStatus::Succeeded | AgentStatus::Failed { .. } | AgentStatus::Cancelled
        )
    }

    /// The short label for the monitor pane.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            AgentStatus::Pending => "pending",
            AgentStatus::Running => "running",
            AgentStatus::WaitingForInput { .. } => "waiting",
            AgentStatus::Succeeded => "done",
            AgentStatus::Failed { .. } => "failed",
            AgentStatus::Cancelled => "cancelled",
        }
    }

    /// The question or error carried by the status, when it carries one.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            AgentStatus::WaitingForInput { question } => Some(question.clone()),
            AgentStatus::Failed { message } => Some(message.clone()),
            _ => None,
        }
    }

    /// How this run should appear in the attention feed.
    #[must_use]
    pub const fn as_item_state(&self) -> ItemState {
        match self {
            AgentStatus::Pending | AgentStatus::Running => ItemState::AgentRunning,
            AgentStatus::WaitingForInput { .. } => ItemState::AgentWaiting,
            AgentStatus::Succeeded => ItemState::AgentDone,
            AgentStatus::Failed { .. } => ItemState::AgentFailed,
            AgentStatus::Cancelled => ItemState::Resolved,
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.detail() {
            Some(detail) => write!(f, "{}: {detail}", self.label()),
            None => f.write_str(self.label()),
        }
    }
}

/// One agent run, as Vetinari tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    id: SessionId,
    agent: AgentId,
    class: TaskClass,
    summary: String,
    working_dir: PathBuf,
    status: AgentStatus,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    space: Option<SpaceId>,
}

impl AgentSession {
    /// Record a newly accepted run.
    #[must_use]
    pub fn new(id: SessionId, agent: AgentId, spec: &TaskSpec, started_at: OffsetDateTime) -> Self {
        Self {
            id,
            agent,
            class: spec.class(),
            summary: spec.prompt().single_line(),
            working_dir: spec.working_dir().to_path_buf(),
            status: AgentStatus::Pending,
            started_at,
            finished_at: None,
            space: None,
        }
    }

    /// The session id.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.id
    }

    /// Which engine is running it.
    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The class of work.
    #[must_use]
    pub const fn class(&self) -> TaskClass {
        self.class
    }

    /// A one-line description of the task.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Where it is running.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Its current status.
    #[must_use]
    pub const fn status(&self) -> &AgentStatus {
        &self.status
    }

    /// When it started.
    #[must_use]
    pub const fn started_at(&self) -> OffsetDateTime {
        self.started_at
    }

    /// When it finished, if it has.
    #[must_use]
    pub const fn finished_at(&self) -> Option<OffsetDateTime> {
        self.finished_at
    }

    /// The space hosting it, if any.
    #[must_use]
    pub const fn space(&self) -> Option<&SpaceId> {
        self.space.as_ref()
    }

    /// Update the status of a run still in flight.
    pub fn set_status(&mut self, status: AgentStatus) {
        self.status = status;
    }

    /// Record a terminal status and stop the clock.
    pub fn finish(&mut self, status: AgentStatus, at: OffsetDateTime) {
        self.status = status;
        self.finished_at = Some(at);
    }

    /// Tie the run to the workspace space it occupies.
    pub fn attach_space(&mut self, space: SpaceId) {
        self.space = Some(space);
    }

    /// How long it has been running, or ran for.
    #[must_use]
    pub fn elapsed(&self, now: OffsetDateTime) -> Duration {
        self.finished_at.unwrap_or(now) - self.started_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const START: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn spec() -> TaskSpec {
        TaskSpec::new(
            NonEmptyText::parse("fix the leak").expect("prompt"),
            TaskClass::DeepCodeChange,
            PathBuf::from("/work/repo"),
        )
    }

    #[test]
    fn a_task_spec_carries_its_prompt_class_and_working_directory() {
        let spec = spec();
        assert_eq!(spec.prompt().as_str(), "fix the leak");
        assert_eq!(spec.class(), TaskClass::DeepCodeChange);
        assert_eq!(spec.working_dir(), Path::new("/work/repo"));
        assert!(spec.context_paths().is_empty());
        assert!(spec.skill().is_none());
    }

    #[test]
    fn a_task_spec_collects_context_paths_and_environment() {
        let spec = spec()
            .with_context_path(PathBuf::from("src/auth.rs"))
            .with_context_path(PathBuf::from("src/lib.rs"))
            .with_env("VETINARI_PROFILE", "work");
        assert_eq!(spec.context_paths().len(), 2);
        assert_eq!(
            spec.env(),
            &[("VETINARI_PROFILE".to_owned(), "work".to_owned())]
        );
    }

    #[test]
    fn task_classes_round_trip_through_their_config_names() {
        for class in TaskClass::ALL {
            assert_eq!(
                class.name().parse::<TaskClass>().expect("known class"),
                *class
            );
        }
        assert!("summon-dragon".parse::<TaskClass>().is_err());
    }

    #[test]
    fn a_session_starts_pending_and_is_not_finished() {
        let session = AgentSession::new(
            SessionId::parse("s1").expect("id"),
            AgentId::parse("claude-code").expect("id"),
            &spec(),
            START,
        );
        assert_eq!(session.status(), &AgentStatus::Pending);
        assert!(!session.status().is_terminal());
        assert_eq!(session.summary(), "fix the leak");
    }

    #[test]
    fn a_session_records_where_it_got_to() {
        let mut session = AgentSession::new(
            SessionId::parse("s1").expect("id"),
            AgentId::parse("claude-code").expect("id"),
            &spec(),
            START,
        );
        session.set_status(AgentStatus::Running);
        assert_eq!(session.status(), &AgentStatus::Running);

        session.set_status(AgentStatus::Failed {
            message: "compile error".to_owned(),
        });
        assert!(session.status().is_terminal());
        assert_eq!(session.status().detail(), Some("compile error".to_owned()));
    }

    #[test]
    fn a_session_knows_how_long_it_has_been_going() {
        let session = AgentSession::new(
            SessionId::parse("s1").expect("id"),
            AgentId::parse("claude-code").expect("id"),
            &spec(),
            START,
        );
        assert_eq!(
            session.elapsed(START + Duration::minutes(3)),
            Duration::minutes(3)
        );
    }

    #[test]
    fn a_finished_session_stops_the_clock() {
        let mut session = AgentSession::new(
            SessionId::parse("s1").expect("id"),
            AgentId::parse("claude-code").expect("id"),
            &spec(),
            START,
        );
        session.finish(AgentStatus::Succeeded, START + Duration::minutes(2));
        assert_eq!(
            session.elapsed(START + Duration::minutes(9)),
            Duration::minutes(2)
        );
        assert_eq!(session.finished_at(), Some(START + Duration::minutes(2)));
    }

    #[test]
    fn agent_status_maps_onto_the_attention_feed() {
        assert_eq!(
            AgentStatus::Pending.as_item_state(),
            ItemState::AgentRunning
        );
        assert_eq!(
            AgentStatus::Running.as_item_state(),
            ItemState::AgentRunning
        );
        assert_eq!(
            AgentStatus::WaitingForInput {
                question: "which branch?".to_owned()
            }
            .as_item_state(),
            ItemState::AgentWaiting
        );
        assert_eq!(AgentStatus::Succeeded.as_item_state(), ItemState::AgentDone);
        assert_eq!(
            AgentStatus::Failed {
                message: "boom".to_owned()
            }
            .as_item_state(),
            ItemState::AgentFailed
        );
        assert_eq!(AgentStatus::Cancelled.as_item_state(), ItemState::Resolved);
    }

    #[test]
    fn a_waiting_session_surfaces_the_question_it_is_stuck_on() {
        let status = AgentStatus::WaitingForInput {
            question: "which branch?".to_owned(),
        };
        assert_eq!(status.detail(), Some("which branch?".to_owned()));
        assert!(!status.is_terminal());
        assert_eq!(status.label(), "waiting");
    }

    #[test]
    fn a_session_can_be_tied_to_the_space_it_runs_in() {
        let mut session = AgentSession::new(
            SessionId::parse("s1").expect("id"),
            AgentId::parse("claude-code").expect("id"),
            &spec(),
            START,
        );
        session.attach_space(SpaceId::parse("space-3").expect("id"));
        assert_eq!(session.space().map(SpaceId::as_str), Some("space-3"));
    }
}
