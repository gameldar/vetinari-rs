//! The application state, and every decision the front-end makes.
//!
//! [`App`] holds no I/O. A keypress goes in and an [`Outcome`] comes out:
//! either nothing, a request to quit, or a [`Task`] for the event loop to run
//! against the adapters. Results come back as [`Update`]s. That split is what
//! lets the whole of the front-end's behaviour be tested without a terminal.

use std::collections::VecDeque;
use std::path::PathBuf;

use time::{Duration, OffsetDateTime};
use vetinari_core::application::attention::{Feed, FeedView, SourceProblem};
use vetinari_core::application::keymap_engine::{KeymapEngine, Keystroke};
use vetinari_core::application::ports::{DeviceCodeChallenge, PortError};
use vetinari_core::application::skills::SkillDraft;
use vetinari_core::domain::action::Action;
use vetinari_core::domain::agent::AgentSession;
use vetinari_core::domain::attention::{
    AttentionItem, EventRef, IssueRef, ItemContext, ItemState, MessageRef, PullRequestRef,
};
use vetinari_core::domain::credentials::{AuthMethod, Provider, Secret};
use vetinari_core::domain::feedback::{CapturedContext, FeedbackRequest, FeedbackTarget};
use vetinari_core::domain::ids::{SessionId, SkillId, SourceId, SpaceId};
use vetinari_core::domain::keymap::{Keymap, Mode};
use vetinari_core::domain::keys::{KeyChord, KeyCode};
use vetinari_core::domain::profile::{Profile, ProfileSet};
use vetinari_core::domain::skill::{Bindings, Skill, SkillLibrary};
use vetinari_core::domain::text::NonEmptyText;
use vetinari_core::domain::workspace::Space;

use crate::overlay::{
    DeviceCodeView, Form, Menu, MenuItem, MenuKind, MenuTarget, Overlay, Prompt, PromptKind,
    TextView,
};
use crate::text_input::InputOutcome;

/// Vetinari's version, as shown in the header.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The attention feed.
    Feed,
    /// The workspace and agent monitor.
    Monitor,
    /// The details of the selected row.
    Details,
}

impl Pane {
    const ORDER: &'static [Pane] = &[Pane::Feed, Pane::Monitor, Pane::Details];

    /// The pane's title.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Pane::Feed => "ATTENTION FEED",
            Pane::Monitor => "WORKSPACE & AGENT MONITOR",
            Pane::Details => "CONTEXT / DETAILS",
        }
    }

    fn shifted(self, by: isize) -> Self {
        let index = Self::ORDER
            .iter()
            .position(|pane| *pane == self)
            .unwrap_or(0) as isize;
        let next = (index + by).clamp(0, Self::ORDER.len() as isize - 1);
        Self::ORDER[next as usize]
    }

    fn cycled(self, by: isize) -> Self {
        let len = Self::ORDER.len() as isize;
        let index = Self::ORDER
            .iter()
            .position(|pane| *pane == self)
            .unwrap_or(0) as isize;
        Self::ORDER[((index + by).rem_euclid(len)) as usize]
    }
}

/// How loud a status message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something happened.
    Info,
    /// Something did not happen, and the developer should know why.
    Warn,
    /// Something failed.
    Error,
}

/// A line for the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    /// What to say.
    pub text: String,
    /// How loudly.
    pub level: Level,
}

impl StatusMessage {
    /// How long a message stays up before the status line clears itself.
    pub const LIFETIME: Duration = Duration::seconds(6);

    /// An informational message.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: Level::Info,
        }
    }

    /// A warning: the request was understood but not carried out.
    #[must_use]
    pub fn warn(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: Level::Warn,
        }
    }

    /// An error.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: Level::Error,
        }
    }
}

/// The which-key popup's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhichKeyView {
    /// The prefix pressed so far.
    pub prefix: String,
    /// The keys that may follow, and what they do.
    pub entries: Vec<(String, String)>,
}

/// A row of the agent monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session.
    pub id: SessionId,
    /// Which engine is running it.
    pub agent: String,
    /// What it is doing.
    pub summary: String,
    /// Where it has got to.
    pub status: String,
    /// The space it occupies.
    pub space: Option<SpaceId>,
    /// Whether it is waiting on the developer.
    pub blocked: bool,
}

impl SessionRow {
    /// Summarise a session for the monitor.
    #[must_use]
    pub fn from_session(session: &AgentSession) -> Self {
        Self {
            id: session.id().clone(),
            agent: session.agent().to_string(),
            summary: session.summary().to_owned(),
            status: session.status().to_string(),
            space: session.space().cloned(),
            blocked: matches!(
                session.status(),
                vetinari_core::domain::agent::AgentStatus::WaitingForInput { .. }
            ),
        }
    }

    /// One line for the monitor pane.
    #[must_use]
    pub fn label(&self) -> String {
        let space = self
            .space
            .as_ref()
            .map_or_else(|| "-".to_owned(), |space| space.as_str().to_owned());
        format!(
            "[{space}] {}: {} ({})",
            self.agent, self.summary, self.status
        )
    }

    #[cfg(test)]
    fn sample(id: &str, agent: &str, summary: &str, status: &str) -> Self {
        Self {
            id: SessionId::parse(id).expect("id"),
            agent: agent.to_owned(),
            summary: summary.to_owned(),
            status: status.to_owned(),
            space: None,
            blocked: status == "waiting",
        }
    }
}

/// A row of the workspace monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRow {
    /// The space.
    pub id: SpaceId,
    /// How to describe it.
    pub label: String,
}

impl SpaceRow {
    /// Summarise a space for the monitor.
    #[must_use]
    pub fn from_space(space: &Space) -> Self {
        Self {
            id: space.id().clone(),
            label: space.describe(),
        }
    }

    #[cfg(test)]
    fn sample(id: &str, label: &str) -> Self {
        Self {
            id: SpaceId::parse(id).expect("id"),
            label: label.to_owned(),
        }
    }
}

/// Work for the event loop to do against the adapters.
#[derive(Debug, Clone)]
pub enum Task {
    /// Poll every source and every agent.
    Refresh,
    /// Run the dogfooding workflow.
    ///
    /// Boxed because a captured context is large and every other task is
    /// small; the cost belongs to the one task that carries it.
    StartFeedback(Box<FeedbackRequest>),
    /// Run the PR review workflow.
    StartPrReview(PullRequestRef),
    /// Brief an agent on a meeting.
    MeetingPrep {
        /// The event's title.
        title: String,
        /// Its details.
        event: EventRef,
        /// When it starts.
        starts_at: OffsetDateTime,
    },
    /// Run a skill from the library.
    RunSkill {
        /// Which skill.
        id: SkillId,
        /// Its bindings.
        bindings: Bindings,
    },
    /// Approve a pull request.
    ApprovePr {
        /// The source that produced the item.
        source: SourceId,
        /// The pull request.
        pr: PullRequestRef,
    },
    /// Comment on a pull request.
    CommentPr {
        /// The source that produced the item.
        source: SourceId,
        /// The pull request.
        pr: PullRequestRef,
        /// What to say.
        body: String,
    },
    /// Reply to a message.
    ReplyChat {
        /// The source that produced the item.
        source: SourceId,
        /// The message to reply to.
        message: MessageRef,
        /// What to say.
        body: String,
    },
    /// Move an issue to another status.
    TransitionIssue {
        /// The source that produced the item.
        source: SourceId,
        /// The issue.
        issue: IssueRef,
        /// The status to move it to.
        status: String,
    },
    /// Assign an issue.
    AssignIssue {
        /// The source that produced the item.
        source: SourceId,
        /// The issue.
        issue: IssueRef,
        /// Who to assign it to.
        assignee: String,
    },
    /// Bring a space to the foreground.
    FocusSpace(SpaceId),
    /// Close a space.
    CloseSpace(SpaceId),
    /// Stop an agent task.
    CancelAgent(SessionId),
    /// Run an agent task again.
    RerunAgent(SessionId),
    /// Answer what an agent is blocked on.
    AnswerAgent(SessionId, String),
    /// Fetch an agent's output.
    ShowAgentOutput(SessionId),
    /// Run a session's task again inside its own space, beside the
    /// developer's shell, instead of under Vetinari's supervision.
    DelegateToSpace(SessionId),
    /// Save a skill.
    SaveSkill(Skill),
    /// Store a pasted API token.
    SaveToken(Provider, Secret),
    /// Begin a device-code flow.
    BeginDeviceFlow(Provider),
}

/// What a keypress led to.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Nothing for the event loop to do.
    Nothing,
    /// Leave.
    Quit,
    /// Run this.
    Task(Task),
}

/// Something the event loop learned, for the state to absorb.
#[derive(Debug, Clone)]
pub enum Update {
    /// A fresh feed.
    Feed(Feed),
    /// The agent sessions as they now stand.
    Sessions(Vec<SessionRow>),
    /// The workspace spaces as they now stand.
    Spaces(Vec<SpaceRow>),
    /// The keychain summary for the header.
    Keychain(String),
    /// A reloaded skill library.
    Library(SkillLibrary),
    /// Something to say in the status bar.
    Status(StatusMessage),
    /// A line for the log, which feedback capture sends to agents.
    Log(String),
    /// An agent's output, to show.
    AgentOutput(SessionId, Vec<String>),
    /// A device-code challenge to display.
    DeviceCode(Provider, DeviceCodeChallenge),
    /// Progress on the device-code flow being displayed.
    DeviceCodeMessage(String),
    /// Close whatever has the foreground.
    CloseOverlay,
    /// Time has passed.
    Tick(OffsetDateTime),
}

/// The front-end's state.
pub struct App {
    keys: KeymapEngine,
    feed: FeedView,
    profiles: ProfileSet,
    library: SkillLibrary,
    feedback_target: FeedbackTarget,
    focus: Pane,
    overlay: Option<Overlay>,
    which_key: Option<WhichKeyView>,
    status: Option<(StatusMessage, OffsetDateTime)>,
    log: VecDeque<String>,
    log_limit: usize,
    sessions: Vec<SessionRow>,
    spaces: Vec<SpaceRow>,
    monitor_cursor: usize,
    keychain: String,
    config_path: Option<PathBuf>,
    last_feedback: Option<FeedbackRequest>,
    filter_backup: Option<String>,
    now: OffsetDateTime,
    quit: bool,
}

impl App {
    /// How many rows a page key moves by.
    pub const PAGE: usize = 10;

    /// Build the state.
    #[must_use]
    pub fn new(
        keymap: Keymap,
        profiles: ProfileSet,
        library: SkillLibrary,
        feedback_target: FeedbackTarget,
        log_limit: usize,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            keys: KeymapEngine::new(keymap),
            feed: FeedView::new(Feed::empty(now)),
            profiles,
            library,
            feedback_target,
            focus: Pane::Feed,
            overlay: None,
            which_key: None,
            status: None,
            log: VecDeque::new(),
            log_limit: log_limit.max(1),
            sessions: Vec::new(),
            spaces: Vec::new(),
            monitor_cursor: 0,
            keychain: "Keychain: unknown".to_owned(),
            config_path: None,
            last_feedback: None,
            filter_backup: None,
            now,
            quit: false,
        }
    }

    /// Record which configuration file is in force, for feedback capture.
    pub fn set_config_path(&mut self, path: Option<PathBuf>) {
        self.config_path = path;
    }

    /// The keymap in force.
    #[must_use]
    pub fn keymap(&self) -> &Keymap {
        self.keys.keymap()
    }

    /// The feed and its selection.
    #[must_use]
    pub const fn feed(&self) -> &FeedView {
        &self.feed
    }

    /// The active profile.
    #[must_use]
    pub fn profile(&self) -> &Profile {
        self.profiles.active()
    }

    /// Every profile.
    #[must_use]
    pub const fn profiles(&self) -> &ProfileSet {
        &self.profiles
    }

    /// The skill library.
    #[must_use]
    pub const fn library(&self) -> &SkillLibrary {
        &self.library
    }

    /// Which pane has the keyboard.
    #[must_use]
    pub const fn focus(&self) -> Pane {
        self.focus
    }

    /// Whatever has the foreground.
    #[must_use]
    pub const fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// The which-key popup, when a prefix is pending.
    #[must_use]
    pub const fn which_key(&self) -> Option<&WhichKeyView> {
        self.which_key.as_ref()
    }

    /// The status line.
    #[must_use]
    pub fn status(&self) -> Option<&StatusMessage> {
        self.status.as_ref().map(|(message, _)| message)
    }

    /// The captured log.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.iter().cloned().collect()
    }

    /// The agent sessions.
    #[must_use]
    pub fn sessions(&self) -> &[SessionRow] {
        &self.sessions
    }

    /// The workspace spaces.
    #[must_use]
    pub fn spaces(&self) -> &[SpaceRow] {
        &self.spaces
    }

    /// Which monitor row is highlighted.
    #[must_use]
    pub const fn monitor_cursor(&self) -> usize {
        self.monitor_cursor
    }

    /// The highlighted session.
    #[must_use]
    pub fn selected_session(&self) -> Option<&SessionRow> {
        self.sessions.get(self.monitor_cursor)
    }

    /// The clock the relative times are shown against.
    #[must_use]
    pub const fn now(&self) -> OffsetDateTime {
        self.now
    }

    /// Whether the developer asked to leave.
    #[must_use]
    pub const fn should_quit(&self) -> bool {
        self.quit
    }

    /// The left of the header.
    #[must_use]
    pub fn header_left(&self) -> String {
        format!(
            "VETINARI v{VERSION} - [ Profile: {} ]",
            self.profile().name()
        )
    }

    /// The right of the header.
    #[must_use]
    pub fn header_right(&self) -> String {
        self.keychain.clone()
    }

    /// The footer's key hints, spelled as the keymap actually binds them.
    #[must_use]
    pub fn footer(&self) -> String {
        const HINTS: &[(Action, &str)] = &[
            (Action::PrReviewSpawn, "PR Review"),
            (Action::FeedbackOpen, "Feedback"),
            (Action::SkillEditor, "Edit Skills"),
            (Action::AgentMenu, "Agents"),
            (Action::Help, "Help"),
            (Action::Quit, "Quit"),
        ];
        HINTS
            .iter()
            .filter_map(|(action, label)| {
                let sequence = self.keys.keymap().sequence_for(Mode::Normal, *action)?;
                Some(format!("{sequence}: {label}"))
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// The details of the selected row.
    #[must_use]
    pub fn details(&self) -> Vec<(String, String)> {
        let Some(item) = self.feed.selected() else {
            return Vec::new();
        };
        let mut rows = vec![
            ("Title".to_owned(), item.title().single_line()),
            ("Source".to_owned(), item.source().to_string()),
            ("Status".to_owned(), item.state().label().to_owned()),
        ];
        if let Some(author) = item.author() {
            rows.push(("Author".to_owned(), author.to_owned()));
        }
        if let Some(when) = item.relative_time(self.now) {
            rows.push(("When".to_owned(), when));
        }
        for (key, value) in item.details() {
            rows.push((key.clone(), value.clone()));
        }
        if let Some(url) = item.url() {
            rows.push(("URL".to_owned(), url.to_owned()));
        }
        rows
    }

    /// Absorb something the event loop learned.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Feed(feed) => {
                let problems: Vec<SourceProblem> = feed.problems().to_vec();
                self.feed.replace_feed(feed);
                for problem in &problems {
                    self.push_log(format!(
                        "ERROR source {}: {}",
                        problem.source, problem.error
                    ));
                }
                if let Some(problem) = problems.first() {
                    let extra = problems.len() - 1;
                    let text = if extra == 0 {
                        format!("{} is unavailable: {}", problem.source, problem.error)
                    } else {
                        format!(
                            "{} is unavailable: {} (and {extra} more)",
                            problem.source, problem.error
                        )
                    };
                    self.set_status(StatusMessage::error(text));
                }
            }
            Update::Sessions(sessions) => {
                self.sessions = sessions;
                self.monitor_cursor = self
                    .monitor_cursor
                    .min(self.sessions.len().saturating_sub(1));
            }
            Update::Spaces(spaces) => self.spaces = spaces,
            Update::Keychain(summary) => self.keychain = summary,
            Update::Library(library) => self.library = library,
            Update::Status(message) => self.set_status(message),
            Update::Log(line) => self.push_log(line),
            Update::AgentOutput(session, lines) => {
                self.overlay = Some(Overlay::Text(TextView::new(
                    format!("Agent output - {session}"),
                    if lines.is_empty() {
                        vec!["(no output yet)".to_owned()]
                    } else {
                        lines
                    },
                )));
            }
            Update::DeviceCode(provider, challenge) => {
                self.overlay = Some(Overlay::DeviceCode(DeviceCodeView {
                    provider,
                    challenge,
                    message: "Waiting for approval...".to_owned(),
                }));
            }
            Update::DeviceCodeMessage(message) => {
                if let Some(Overlay::DeviceCode(view)) = self.overlay.as_mut() {
                    view.message = message;
                }
            }
            Update::CloseOverlay => self.close_overlay(),
            Update::Tick(now) => {
                self.now = now;
                if let Some((_, shown_at)) = &self.status
                    && now - *shown_at >= StatusMessage::LIFETIME
                {
                    self.status = None;
                }
            }
        }
    }

    /// Feed a keypress through the state.
    pub fn on_key(&mut self, chord: KeyChord) -> Outcome {
        if self.overlay.is_some() {
            return self.overlay_key(chord);
        }
        match self.keys.press(chord) {
            Keystroke::Dispatch(action) => {
                self.which_key = None;
                self.dispatch(action)
            }
            Keystroke::Pending {
                sequence,
                candidates,
            } => {
                self.which_key = Some(WhichKeyView {
                    prefix: sequence.to_string(),
                    entries: candidates
                        .into_iter()
                        .map(|candidate| (candidate.chord.to_string(), candidate.label))
                        .collect(),
                });
                Outcome::Nothing
            }
            Keystroke::Unbound(sequence) => {
                self.which_key = None;
                self.warn(format!("{sequence} is not bound"))
            }
            Keystroke::Cancelled => {
                self.which_key = None;
                Outcome::Nothing
            }
            Keystroke::Text(_) | Keystroke::Ignored => Outcome::Nothing,
        }
    }

    /// Run an action directly, as a command would.
    pub fn dispatch(&mut self, action: Action) -> Outcome {
        match action {
            Action::Quit => {
                self.quit = true;
                Outcome::Quit
            }
            Action::Refresh => Outcome::Task(Task::Refresh),
            Action::Help => {
                self.open_help();
                Outcome::Nothing
            }
            Action::CancelPending => {
                self.close_overlay();
                self.which_key = None;
                Outcome::Nothing
            }

            Action::NavDown => self.navigate(1),
            Action::NavUp => self.navigate(-1),
            Action::NavTop => self.navigate_to_end(false),
            Action::NavBottom => self.navigate_to_end(true),
            Action::NavPageDown => self.navigate(Self::PAGE as isize),
            Action::NavPageUp => self.navigate(-(Self::PAGE as isize)),
            Action::PaneLeft => {
                self.focus = self.focus.shifted(-1);
                Outcome::Nothing
            }
            Action::PaneRight => {
                self.focus = self.focus.shifted(1);
                Outcome::Nothing
            }
            Action::PaneNext => {
                self.focus = self.focus.cycled(1);
                Outcome::Nothing
            }
            Action::PanePrev => {
                self.focus = self.focus.cycled(-1);
                Outcome::Nothing
            }
            Action::FilterStart => {
                self.filter_backup = Some(self.feed.filter().to_owned());
                let mut prompt = Prompt::line(
                    PromptKind::Filter,
                    "Filter",
                    "narrow the feed; RET keeps it, ESC restores it",
                );
                prompt.input =
                    crate::text_input::TextInput::single_line().with_value(self.feed.filter());
                self.overlay = Some(Overlay::Prompt(prompt));
                Outcome::Nothing
            }
            Action::FilterClear => {
                self.feed.clear_filter();
                Outcome::Nothing
            }
            Action::Select => {
                self.focus = Pane::Details;
                match self.feed.selected() {
                    Some(item) => {
                        let label = item.row_label();
                        self.info(label)
                    }
                    None => Outcome::Nothing,
                }
            }

            Action::FeedbackOpen => {
                self.overlay = Some(Overlay::Prompt(Prompt::paragraph(
                    PromptKind::Feedback,
                    format!("Feedback - {}", self.feedback_target.name()),
                    "describe the change; C-s submits, ESC cancels",
                )));
                Outcome::Nothing
            }
            Action::FeedbackSubmitLast => match self.last_feedback.clone() {
                Some(request) => Outcome::Task(Task::StartFeedback(Box::new(request))),
                None => self.warn("no feedback request has been made yet"),
            },

            Action::PrReviewSpawn => match self.selected_pull_request() {
                Some((_, pr)) => Outcome::Task(Task::StartPrReview(pr)),
                None => self.warn("select a pull request row first"),
            },
            Action::PrApprove => match self.selected_pull_request() {
                Some((source, pr)) => Outcome::Task(Task::ApprovePr { source, pr }),
                None => self.warn("select a pull request row first"),
            },
            Action::PrComment => match self.selected_pull_request() {
                Some((source, pr)) => {
                    self.open_prompt(
                        PromptKind::PrComment { source, pr },
                        "Comment on pull request",
                        "RET posts the comment",
                    );
                    Outcome::Nothing
                }
                None => self.warn("select a pull request row first"),
            },

            Action::IssueUpdateStatus => match self.selected_issue() {
                Some((source, issue)) => {
                    let hint = if issue.transitions.is_empty() {
                        "RET moves the issue".to_owned()
                    } else {
                        format!("one of: {}", issue.transitions.join(", "))
                    };
                    self.open_prompt(
                        PromptKind::IssueStatus { source, issue },
                        "New status",
                        hint,
                    );
                    Outcome::Nothing
                }
                None => self.warn("select an issue row first"),
            },
            Action::IssueAssign => match self.selected_issue() {
                Some((source, issue)) => {
                    self.open_prompt(
                        PromptKind::IssueAssignee { source, issue },
                        "Assign issue",
                        "account id or username",
                    );
                    Outcome::Nothing
                }
                None => self.warn("select an issue row first"),
            },
            Action::IssueStandupSummary => Outcome::Task(Task::RunSkill {
                id: Self::skill_id("standup-summary"),
                bindings: self.bindings_for_selection(),
            }),

            Action::ChatReply => match self.selected_message() {
                Some((source, message)) => {
                    self.open_prompt(
                        PromptKind::ChatReply { source, message },
                        "Reply",
                        "RET sends the reply",
                    );
                    Outcome::Nothing
                }
                None => self.warn("select a message row first"),
            },
            Action::ChatConvertToTask => self.warn(
                "converting a message into a ticket is not implemented yet; \
                 reply, or run a skill to draft one",
            ),

            Action::MeetingActions => {
                self.open_meeting_menu();
                Outcome::Nothing
            }
            Action::MeetingPrep => match self.selected_event() {
                Some((title, event, starts_at)) => Outcome::Task(Task::MeetingPrep {
                    title,
                    event,
                    starts_at,
                }),
                None => self.warn("select a calendar row first"),
            },
            Action::MeetingSchedule => {
                self.open_schedule();
                Outcome::Nothing
            }

            Action::AgentMenu => {
                self.open_agent_menu();
                Outcome::Nothing
            }
            Action::AgentUnblock => match self.selected_session().cloned() {
                Some(session) => {
                    self.open_prompt(
                        PromptKind::AgentAnswer(session.id.clone()),
                        format!("Answer {}", session.agent),
                        "RET sends the answer to the agent",
                    );
                    Outcome::Nothing
                }
                None => self.warn("no agent session is selected"),
            },
            Action::AgentCancel => match self.selected_session() {
                Some(session) => Outcome::Task(Task::CancelAgent(session.id.clone())),
                None => self.warn("no agent session is selected"),
            },
            Action::AgentRerun => match self.selected_session() {
                Some(session) => Outcome::Task(Task::RerunAgent(session.id.clone())),
                None => self.warn("no agent session is selected"),
            },
            Action::AgentLogs => match self.selected_session() {
                Some(session) => Outcome::Task(Task::ShowAgentOutput(session.id.clone())),
                None => self.warn("no agent session is selected"),
            },

            Action::WorkspaceNavigate => {
                self.open_space_menu();
                Outcome::Nothing
            }
            Action::WorkspaceFocusSpace => match self.focusable_space() {
                Some(space) => Outcome::Task(Task::FocusSpace(space)),
                None => self.warn("nothing selected has a workspace space"),
            },
            Action::WorkspaceCloseSpace => match self.focusable_space() {
                Some(space) => Outcome::Task(Task::CloseSpace(space)),
                None => self.warn("nothing selected has a workspace space"),
            },
            Action::WorkspaceRunAgent => match self.selected_session() {
                Some(session) if session.space.is_some() => {
                    Outcome::Task(Task::DelegateToSpace(session.id.clone()))
                }
                Some(_) => self.warn("that session has no space to run in"),
                None => self.warn("no agent session is selected"),
            },

            Action::ProfileCycle => {
                let name = self.profiles.cycle().name().to_string();
                self.set_status(StatusMessage::info(format!("Profile: {name}")));
                Outcome::Task(Task::Refresh)
            }
            Action::ProfileMenu => {
                self.open_profile_menu();
                Outcome::Nothing
            }

            Action::SkillEditor => {
                self.open_skill_menu(MenuKind::SkillsToEdit);
                Outcome::Nothing
            }
            Action::SkillRun => {
                self.open_skill_menu(MenuKind::Skills);
                Outcome::Nothing
            }

            Action::AuthWizard => {
                self.open_provider_menu();
                Outcome::Nothing
            }
        }
    }

    fn navigate(&mut self, by: isize) -> Outcome {
        match self.focus {
            Pane::Feed | Pane::Details => {
                if by > 0 {
                    #[allow(clippy::cast_sign_loss)]
                    let step = by as usize;
                    if step == 1 {
                        self.feed.select_next();
                    } else {
                        self.feed.page_down(step);
                    }
                } else {
                    #[allow(clippy::cast_sign_loss)]
                    let step = (-by) as usize;
                    if step == 1 {
                        self.feed.select_previous();
                    } else {
                        self.feed.page_up(step);
                    }
                }
            }
            Pane::Monitor => {
                let last = self.sessions.len().saturating_sub(1);
                self.monitor_cursor = if by > 0 {
                    #[allow(clippy::cast_sign_loss)]
                    self.monitor_cursor.saturating_add(by as usize).min(last)
                } else {
                    #[allow(clippy::cast_sign_loss)]
                    self.monitor_cursor.saturating_sub((-by) as usize)
                };
            }
        }
        Outcome::Nothing
    }

    fn navigate_to_end(&mut self, bottom: bool) -> Outcome {
        match self.focus {
            Pane::Feed | Pane::Details => {
                if bottom {
                    self.feed.select_last();
                } else {
                    self.feed.select_first();
                }
            }
            Pane::Monitor => {
                self.monitor_cursor = if bottom {
                    self.sessions.len().saturating_sub(1)
                } else {
                    0
                };
            }
        }
        Outcome::Nothing
    }

    fn overlay_key(&mut self, chord: KeyChord) -> Outcome {
        match self.overlay.take() {
            Some(Overlay::Prompt(prompt)) => self.prompt_key(prompt, chord),
            Some(Overlay::Menu(menu)) => self.menu_key(menu, chord),
            Some(Overlay::Form(form)) => self.form_key(form, chord),
            Some(Overlay::Text(view)) => self.text_key(view, chord),
            Some(Overlay::DeviceCode(view)) => {
                if chord.code() == KeyCode::Esc {
                    return Outcome::Nothing;
                }
                self.overlay = Some(Overlay::DeviceCode(view));
                Outcome::Nothing
            }
            None => Outcome::Nothing,
        }
    }

    fn prompt_key(&mut self, mut prompt: Prompt, chord: KeyChord) -> Outcome {
        match prompt.input.press(chord) {
            InputOutcome::Cancelled => {
                if prompt.kind == PromptKind::Filter
                    && let Some(previous) = self.filter_backup.take()
                {
                    self.feed.set_filter(previous);
                }
                Outcome::Nothing
            }
            InputOutcome::Submitted => {
                if prompt.kind == PromptKind::Filter {
                    self.filter_backup = None;
                    return Outcome::Nothing;
                }
                if prompt.input.is_blank() {
                    let outcome = self.warn("nothing to submit");
                    self.overlay = Some(Overlay::Prompt(prompt));
                    return outcome;
                }
                let text = prompt.input.value().to_owned();
                self.submit_prompt(prompt.kind, text)
            }
            InputOutcome::Changed | InputOutcome::Ignored => {
                if prompt.kind == PromptKind::Filter {
                    self.feed.set_filter(prompt.input.value());
                }
                self.overlay = Some(Overlay::Prompt(prompt));
                Outcome::Nothing
            }
        }
    }

    fn submit_prompt(&mut self, kind: PromptKind, text: String) -> Outcome {
        match kind {
            PromptKind::Filter => Outcome::Nothing,
            PromptKind::Feedback => {
                let Ok(request) = NonEmptyText::parse(&text) else {
                    return self.warn("nothing to submit");
                };
                let captured = FeedbackRequest::new(
                    request,
                    self.feedback_target.clone(),
                    self.capture_context(),
                    self.now,
                );
                self.last_feedback = Some(captured.clone());
                Outcome::Task(Task::StartFeedback(Box::new(captured)))
            }
            PromptKind::PrComment { source, pr } => Outcome::Task(Task::CommentPr {
                source,
                pr,
                body: text,
            }),
            PromptKind::ChatReply { source, message } => Outcome::Task(Task::ReplyChat {
                source,
                message,
                body: text,
            }),
            PromptKind::AgentAnswer(session) => Outcome::Task(Task::AnswerAgent(session, text)),
            PromptKind::IssueStatus { source, issue } => Outcome::Task(Task::TransitionIssue {
                source,
                issue,
                status: text,
            }),
            PromptKind::IssueAssignee { source, issue } => Outcome::Task(Task::AssignIssue {
                source,
                issue,
                assignee: text,
            }),
            PromptKind::ApiToken(provider) => {
                Outcome::Task(Task::SaveToken(provider, Secret::new(text.trim())))
            }
        }
    }

    fn menu_key(&mut self, mut menu: Menu, chord: KeyChord) -> Outcome {
        match chord.code() {
            KeyCode::Esc => Outcome::Nothing,
            KeyCode::Enter => match menu.selected().cloned() {
                Some(item) => self.choose(menu.kind, item),
                None => Outcome::Nothing,
            },
            KeyCode::Char('j') | KeyCode::Down => {
                menu.next();
                self.overlay = Some(Overlay::Menu(menu));
                Outcome::Nothing
            }
            KeyCode::Char('k') | KeyCode::Up => {
                menu.previous();
                self.overlay = Some(Overlay::Menu(menu));
                Outcome::Nothing
            }
            KeyCode::Char('G') | KeyCode::End => {
                menu.last();
                self.overlay = Some(Overlay::Menu(menu));
                Outcome::Nothing
            }
            KeyCode::Home => {
                menu.first();
                self.overlay = Some(Overlay::Menu(menu));
                Outcome::Nothing
            }
            _ => {
                self.overlay = Some(Overlay::Menu(menu));
                Outcome::Nothing
            }
        }
    }

    fn choose(&mut self, kind: MenuKind, item: MenuItem) -> Outcome {
        match (kind, item.target) {
            (MenuKind::Profiles, MenuTarget::Profile(id)) => {
                if self.profiles.activate(&id) {
                    let name = self.profile().name().to_string();
                    self.set_status(StatusMessage::info(format!("Profile: {name}")));
                    Outcome::Task(Task::Refresh)
                } else {
                    self.warn(format!("no profile named {id}"))
                }
            }
            (MenuKind::Agents, MenuTarget::Session(id)) => {
                self.focus = Pane::Monitor;
                if let Some(index) = self.sessions.iter().position(|row| row.id == id) {
                    self.monitor_cursor = index;
                }
                Outcome::Nothing
            }
            (MenuKind::Spaces, MenuTarget::Space(id)) => Outcome::Task(Task::FocusSpace(id)),
            (MenuKind::Skills, MenuTarget::Skill(id)) => Outcome::Task(Task::RunSkill {
                id,
                bindings: self.bindings_for_selection(),
            }),
            (MenuKind::SkillsToEdit, MenuTarget::NewSkill) => {
                self.overlay = Some(Overlay::Form(Form::skill(&SkillDraft::blank(), None)));
                Outcome::Nothing
            }
            (MenuKind::SkillsToEdit, MenuTarget::Skill(id)) => match self.library.get(&id) {
                Some(skill) => {
                    self.overlay = Some(Overlay::Form(Form::skill(
                        &SkillDraft::from_skill(skill),
                        Some(id),
                    )));
                    Outcome::Nothing
                }
                None => self.warn(format!("no skill named {id}")),
            },
            (MenuKind::Providers, MenuTarget::Provider(provider)) => {
                match provider.auth_spec().method {
                    AuthMethod::ApiToken { .. } => {
                        self.open_prompt(
                            PromptKind::ApiToken(provider),
                            format!("{} token", provider.display_name()),
                            provider.auth_spec().instructions,
                        );
                        Outcome::Nothing
                    }
                    AuthMethod::OAuthDeviceCode { .. } => {
                        Outcome::Task(Task::BeginDeviceFlow(provider))
                    }
                }
            }
            (MenuKind::Meetings, MenuTarget::Item(id)) => {
                let selected = self.feed.feed().get(&id).and_then(event_of);
                match selected {
                    Some((title, event, starts_at)) => Outcome::Task(Task::MeetingPrep {
                        title,
                        event,
                        starts_at,
                    }),
                    None => self.warn("that meeting is no longer in the feed"),
                }
            }
            _ => Outcome::Nothing,
        }
    }

    fn form_key(&mut self, mut form: Form, chord: KeyChord) -> Outcome {
        let modifiers = chord.modifiers();
        match chord.code() {
            KeyCode::Esc => Outcome::Nothing,
            KeyCode::Tab if modifiers.shift => {
                form.previous_field();
                self.overlay = Some(Overlay::Form(form));
                Outcome::Nothing
            }
            KeyCode::Tab => {
                form.next_field();
                self.overlay = Some(Overlay::Form(form));
                Outcome::Nothing
            }
            KeyCode::Char('s') if modifiers.ctrl => match form.parse() {
                Some(skill) => Outcome::Task(Task::SaveSkill(skill)),
                None => {
                    let problems = form.problems.len();
                    self.overlay = Some(Overlay::Form(form));
                    self.warn(format!("the skill has {problems} problem(s) to fix"))
                }
            },
            _ => {
                if let Some(field) = form.focused() {
                    match field.input.press(chord) {
                        InputOutcome::Submitted => {
                            form.next_field();
                        }
                        InputOutcome::Cancelled => {
                            self.overlay = None;
                            return Outcome::Nothing;
                        }
                        InputOutcome::Changed | InputOutcome::Ignored => {}
                    }
                }
                self.overlay = Some(Overlay::Form(form));
                Outcome::Nothing
            }
        }
    }

    fn text_key(&mut self, mut view: TextView, chord: KeyChord) -> Outcome {
        match chord.code() {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Outcome::Nothing,
            KeyCode::Char('j') | KeyCode::Down => {
                view.scroll_down(1);
                self.overlay = Some(Overlay::Text(view));
                Outcome::Nothing
            }
            KeyCode::Char('k') | KeyCode::Up => {
                view.scroll_up(1);
                self.overlay = Some(Overlay::Text(view));
                Outcome::Nothing
            }
            KeyCode::Char('d') if chord.modifiers().ctrl => {
                view.scroll_down(Self::PAGE);
                self.overlay = Some(Overlay::Text(view));
                Outcome::Nothing
            }
            KeyCode::Char('u') if chord.modifiers().ctrl => {
                view.scroll_up(Self::PAGE);
                self.overlay = Some(Overlay::Text(view));
                Outcome::Nothing
            }
            _ => {
                self.overlay = Some(Overlay::Text(view));
                Outcome::Nothing
            }
        }
    }

    fn open_prompt(&mut self, kind: PromptKind, title: impl Into<String>, hint: impl Into<String>) {
        self.overlay = Some(Overlay::Prompt(Prompt::line(kind, title, hint)));
    }

    fn open_help(&mut self) {
        let lines = self
            .keys
            .keymap()
            .bindings(Mode::Normal)
            .into_iter()
            .map(|(sequence, action)| {
                format!("{:<12} {}", sequence.to_string(), action.description())
            })
            .collect();
        self.overlay = Some(Overlay::Text(TextView::new("Keybindings", lines)));
    }

    fn open_schedule(&mut self) {
        let lines: Vec<String> = self
            .feed
            .feed()
            .items()
            .iter()
            .filter(|item| matches!(item.state(), ItemState::Scheduled { .. }))
            .map(|item| {
                format!(
                    "{:<10} {}",
                    item.relative_time(self.now).unwrap_or_default(),
                    item.title().single_line()
                )
            })
            .collect();
        self.overlay = Some(Overlay::Text(TextView::new(
            "Schedule",
            if lines.is_empty() {
                vec!["(nothing scheduled)".to_owned()]
            } else {
                lines
            },
        )));
    }

    fn open_profile_menu(&mut self) {
        let items = self
            .profiles
            .iter()
            .map(|profile| MenuItem {
                label: profile.name().to_string(),
                detail: profile.target().display_name().to_owned(),
                target: MenuTarget::Profile(profile.id().clone()),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Profiles,
            "Context profile",
            items,
        )));
    }

    fn open_agent_menu(&mut self) {
        let items = self
            .sessions
            .iter()
            .map(|row| MenuItem {
                label: row.label(),
                detail: row.status.clone(),
                target: MenuTarget::Session(row.id.clone()),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Agents,
            "Agent sessions",
            items,
        )));
    }

    fn open_space_menu(&mut self) {
        let items = self
            .spaces
            .iter()
            .map(|row| MenuItem {
                label: row.label.clone(),
                detail: String::new(),
                target: MenuTarget::Space(row.id.clone()),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Spaces,
            "Workspace spaces",
            items,
        )));
    }

    fn open_skill_menu(&mut self, kind: MenuKind) {
        let mut items = Vec::new();
        if kind == MenuKind::SkillsToEdit {
            items.push(MenuItem {
                label: "+ new skill".to_owned(),
                detail: String::new(),
                target: MenuTarget::NewSkill,
            });
        }
        items.extend(self.library.iter().map(|skill| MenuItem {
            label: format!("{} ({})", skill.name(), skill.id()),
            detail: skill.class().name().to_owned(),
            target: MenuTarget::Skill(skill.id().clone()),
        }));
        let title = if kind == MenuKind::SkillsToEdit {
            "Edit skill"
        } else {
            "Run skill"
        };
        self.overlay = Some(Overlay::Menu(Menu::new(kind, title, items)));
    }

    fn open_provider_menu(&mut self) {
        let items = Provider::ALL
            .iter()
            .map(|provider| MenuItem {
                label: provider.display_name().to_owned(),
                detail: match provider.auth_spec().method {
                    AuthMethod::ApiToken { .. } => "token".to_owned(),
                    AuthMethod::OAuthDeviceCode { .. } => "device code".to_owned(),
                },
                target: MenuTarget::Provider(*provider),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Providers,
            "Authenticate",
            items,
        )));
    }

    fn open_meeting_menu(&mut self) {
        let items: Vec<MenuItem> = self
            .feed
            .feed()
            .items()
            .iter()
            .filter(|item| matches!(item.state(), ItemState::Scheduled { .. }))
            .map(|item| MenuItem {
                label: item.title().single_line(),
                detail: item.relative_time(self.now).unwrap_or_default(),
                target: MenuTarget::Item(item.id().clone()),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Meetings,
            "Prepare for a meeting",
            items,
        )));
    }

    fn close_overlay(&mut self) {
        if let Some(Overlay::Prompt(prompt)) = &self.overlay
            && prompt.kind == PromptKind::Filter
            && let Some(previous) = self.filter_backup.take()
        {
            self.feed.set_filter(previous);
        }
        self.overlay = None;
    }

    fn capture_context(&self) -> CapturedContext {
        let mut captured = CapturedContext::new(self.profile().id().clone(), self.keys.mode());
        if let Some(item) = self.feed.selected() {
            captured = captured.with_selected_item(item.row_label());
        }
        if let Some(pending) = self.keys.pending_display() {
            captured = captured.with_pending_keys(pending);
        }
        if let Some(path) = &self.config_path {
            captured = captured.with_config_path(path.clone());
        }
        for space in &self.spaces {
            captured = captured.with_space(space.label.clone());
        }
        for session in &self.sessions {
            captured = captured.with_session(session.label());
        }
        for line in &self.log {
            captured = captured.with_log_line(line.clone());
        }
        captured
    }

    /// The placeholders a skill can be filled from, taken from the selected
    /// row, so running a skill rarely needs anything typed.
    #[must_use]
    pub fn bindings_for_selection(&self) -> Bindings {
        let mut bindings = Bindings::new().with("profile", self.profile().id().as_str());
        let Some(item) = self.feed.selected() else {
            return bindings;
        };
        bindings = bindings
            .with("title", item.title().single_line())
            .with("description", item.row_label())
            .with("context", self.describe_selection(item));

        match item.context() {
            ItemContext::PullRequest(pr) => bindings
                .with("number", pr.number.to_string())
                .with("repo", pr.repo.clone())
                .with("head_ref", pr.head_ref.clone())
                .with("base_ref", pr.base_ref.clone()),
            ItemContext::Issue(issue) => bindings
                .with("key", issue.key.clone())
                .with("status", issue.status.clone())
                .with("issues", issue.key.clone()),
            ItemContext::Message(message) => bindings.with("channel", message.channel.clone()),
            ItemContext::Event(event) => {
                bindings.with("attendees", event.attendees.join(", ")).with(
                    "starts_at",
                    match item.state() {
                        ItemState::Scheduled { starts_at } => starts_at.to_string(),
                        _ => self.now.to_string(),
                    },
                )
            }
            ItemContext::Agent(agent) => bindings.with("session", agent.session.to_string()),
            ItemContext::None => bindings.with("issues", item.title().single_line()),
        }
    }

    fn describe_selection(&self, item: &AttentionItem) -> String {
        let mut described = format!("{}\n", item.row_label());
        for (key, value) in item.details() {
            described.push_str(&format!("- {key}: {value}\n"));
        }
        if let Some(url) = item.url() {
            described.push_str(&format!("- URL: {url}\n"));
        }
        described
    }

    fn selected_pull_request(&self) -> Option<(SourceId, PullRequestRef)> {
        let item = self.feed.selected()?;
        match item.context() {
            ItemContext::PullRequest(pr) => Some((item.source().clone(), pr.clone())),
            _ => None,
        }
    }

    fn selected_issue(&self) -> Option<(SourceId, IssueRef)> {
        let item = self.feed.selected()?;
        match item.context() {
            ItemContext::Issue(issue) => Some((item.source().clone(), issue.clone())),
            _ => None,
        }
    }

    fn selected_message(&self) -> Option<(SourceId, MessageRef)> {
        let item = self.feed.selected()?;
        match item.context() {
            ItemContext::Message(message) => Some((item.source().clone(), message.clone())),
            _ => None,
        }
    }

    fn selected_event(&self) -> Option<(String, EventRef, OffsetDateTime)> {
        event_of(self.feed.selected()?)
    }

    fn focusable_space(&self) -> Option<SpaceId> {
        if let Some(item) = self.feed.selected()
            && let ItemContext::Agent(agent) = item.context()
            && let Some(space) = &agent.space
        {
            return Some(space.clone());
        }
        self.selected_session()
            .and_then(|session| session.space.clone())
            .or_else(|| self.spaces.first().map(|space| space.id.clone()))
    }

    fn skill_id(name: &str) -> SkillId {
        SkillId::parse(name).expect("built-in skill ids are valid")
    }

    fn set_status(&mut self, message: StatusMessage) {
        self.status = Some((message, self.now));
    }

    fn warn(&mut self, text: impl Into<String>) -> Outcome {
        self.set_status(StatusMessage::warn(text));
        Outcome::Nothing
    }

    fn info(&mut self, text: impl Into<String>) -> Outcome {
        self.set_status(StatusMessage::info(text));
        Outcome::Nothing
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() == self.log_limit {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }
}

fn event_of(item: &AttentionItem) -> Option<(String, EventRef, OffsetDateTime)> {
    let ItemContext::Event(event) = item.context() else {
        return None;
    };
    let starts_at = match item.state() {
        ItemState::Scheduled { starts_at } => starts_at,
        _ => item.occurred_at(),
    };
    Some((item.title().single_line(), event.clone(), starts_at))
}

/// The errors a task can report, rendered for the status line.
#[must_use]
pub fn describe_failure(what: &str, error: &PortError) -> StatusMessage {
    match error {
        PortError::Unauthenticated { provider } => StatusMessage::warn(format!(
            "{what}: {} needs authentication (SPC h a)",
            provider.display_name()
        )),
        other => StatusMessage::error(format!("{what}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::FormKind;
    use time::macros::datetime;
    use vetinari_core::domain::attention::{
        AttentionItem, EventRef, IssueRef, ItemContext, ItemState, MessageRef, PullRequestRef,
        SourceCategory,
    };
    use vetinari_core::domain::ids::SourceId;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source(name: &str) -> SourceId {
        SourceId::parse(name).expect("id")
    }

    fn pr_item() -> AttentionItem {
        AttentionItem::builder(
            &source("github"),
            "acme/web#142",
            SourceCategory::CodeReview,
            NonEmptyText::parse("Fix memory leak in auth parser").expect("title"),
        )
        .state(ItemState::CiFailed)
        .author("developer")
        .occurred_at(NOW)
        .context(ItemContext::PullRequest(PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/memory-leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "https://github.test/acme/web.git".to_owned(),
        }))
        .build()
    }

    fn issue_item() -> AttentionItem {
        AttentionItem::builder(
            &source("jira"),
            "DEV-891",
            SourceCategory::IssueTracking,
            NonEmptyText::parse("Review spec").expect("title"),
        )
        .state(ItemState::Assigned)
        .occurred_at(NOW)
        .context(ItemContext::Issue(IssueRef {
            key: "DEV-891".to_owned(),
            status: "To Do".to_owned(),
            transitions: Vec::new(),
        }))
        .build()
    }

    fn message_item() -> AttentionItem {
        AttentionItem::builder(
            &source("slack"),
            "D123",
            SourceCategory::Messaging,
            NonEmptyText::parse("Message from sarah").expect("title"),
        )
        .state(ItemState::Unread)
        .occurred_at(NOW)
        .context(ItemContext::Message(MessageRef {
            channel: "D123".to_owned(),
            thread: None,
        }))
        .build()
    }

    fn event_item() -> AttentionItem {
        AttentionItem::builder(
            &source("calendar"),
            "evt-1",
            SourceCategory::Calendar,
            NonEmptyText::parse("Sprint planning").expect("title"),
        )
        .state(ItemState::Scheduled {
            starts_at: NOW + Duration::minutes(15),
        })
        .occurred_at(NOW)
        .context(ItemContext::Event(EventRef {
            ends_at: None,
            attendees: vec!["sarah".to_owned()],
            location: None,
        }))
        .build()
    }

    fn app() -> App {
        let mut app = App::new(
            Keymap::doom_defaults(),
            ProfileSet::defaults(),
            SkillLibrary::builtin(),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            200,
            NOW,
        );
        app.apply(Update::Feed(Feed::new(
            vec![pr_item(), issue_item(), message_item(), event_item()],
            Vec::new(),
            NOW,
        )));
        app
    }

    fn press(app: &mut App, spelling: &str) -> Outcome {
        let mut last = Outcome::Nothing;
        for part in spelling.split_whitespace() {
            last = app.on_key(KeyChord::parse(part).expect("chord"));
        }
        last
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_key(KeyChord::plain(KeyCode::Char(ch)));
        }
    }

    fn select_row(app: &mut App, title: &str) {
        for _ in 0..10 {
            if app
                .feed()
                .selected()
                .is_some_and(|item| item.title().as_str() == title)
            {
                return;
            }
            press(app, "j");
        }
        panic!("no row titled {title:?}");
    }

    #[test]
    fn the_app_starts_on_the_most_urgent_row_with_no_overlay() {
        let app = app();
        assert_eq!(app.focus(), Pane::Feed);
        assert!(app.overlay().is_none());
        assert_eq!(
            app.feed().selected().expect("a row").title().as_str(),
            "Sprint planning",
            "a meeting in fifteen minutes outranks a failing build"
        );
        assert_eq!(app.profile().id().as_str(), "work");
    }

    #[test]
    fn vim_keys_move_the_feed_selection() {
        let mut app = app();
        press(&mut app, "j");
        assert_eq!(app.feed().cursor(), 1);
        press(&mut app, "G");
        assert_eq!(app.feed().cursor(), 3);
        press(&mut app, "g g");
        assert_eq!(app.feed().cursor(), 0);
    }

    #[test]
    fn panes_are_switched_with_h_and_l_and_tab() {
        let mut app = app();
        press(&mut app, "l");
        assert_eq!(app.focus(), Pane::Monitor);
        press(&mut app, "l");
        assert_eq!(app.focus(), Pane::Details);
        press(&mut app, "h");
        assert_eq!(app.focus(), Pane::Monitor);
        press(&mut app, "TAB");
        assert_eq!(app.focus(), Pane::Details);
    }

    #[test]
    fn with_the_monitor_focused_the_arrows_move_between_sessions() {
        let mut app = app();
        app.apply(Update::Sessions(vec![
            SessionRow::sample("s1", "claude-code", "writing tests", "running"),
            SessionRow::sample("s2", "agy-gemini", "summarising", "done"),
        ]));
        press(&mut app, "l");
        press(&mut app, "j");
        assert_eq!(app.monitor_cursor(), 1);
        assert_eq!(
            app.selected_session().map(|row| row.id.as_str().to_owned()),
            Some("s2".to_owned())
        );
        assert_eq!(app.feed().cursor(), 0, "the feed selection is left alone");
    }

    #[test]
    fn a_leader_prefix_shows_what_can_follow_it() {
        let mut app = app();
        press(&mut app, "SPC");
        let which_key = app.which_key().expect("which-key");
        assert_eq!(which_key.prefix, "SPC");
        assert!(
            which_key
                .entries
                .iter()
                .any(|(key, label)| key == "f" && label == "+feedback")
        );
        press(&mut app, "<Esc>");
        assert!(app.which_key().is_none());
    }

    #[test]
    fn an_unbound_sequence_says_so_rather_than_doing_nothing() {
        let mut app = app();
        press(&mut app, "SPC z");
        let status = app.status().expect("status");
        assert!(status.text.contains("SPC z"));
        assert_eq!(status.level, Level::Warn);
        assert!(app.which_key().is_none());
    }

    #[test]
    fn slash_opens_a_filter_that_narrows_the_feed_as_it_is_typed() {
        let mut app = app();
        press(&mut app, "/");
        assert!(matches!(app.overlay(), Some(Overlay::Prompt(_))));

        type_text(&mut app, "memory");

        assert_eq!(app.feed().visible().len(), 1);
        assert_eq!(
            app.feed().selected().expect("a row").title().as_str(),
            "Fix memory leak in auth parser"
        );
    }

    #[test]
    fn enter_keeps_the_filter_and_escape_abandons_it() {
        let mut app = app();
        press(&mut app, "/");
        type_text(&mut app, "memory");
        press(&mut app, "RET");
        assert!(app.overlay().is_none());
        assert_eq!(app.feed().filter(), "memory");

        press(&mut app, "/");
        type_text(&mut app, "zzz");
        press(&mut app, "<Esc>");
        assert!(app.overlay().is_none());
        assert_eq!(app.feed().filter(), "memory", "the kept filter survives");
    }

    #[test]
    fn escape_in_normal_mode_clears_the_filter() {
        let mut app = app();
        press(&mut app, "/");
        type_text(&mut app, "memory");
        press(&mut app, "RET");
        press(&mut app, "<Esc>");
        assert_eq!(app.feed().filter(), "");
        assert_eq!(app.feed().visible().len(), 4);
    }

    #[test]
    fn the_feedback_overlay_captures_what_the_developer_was_looking_at() {
        let mut app = app();
        app.apply(Update::Log("WARN source github: rate limited".to_owned()));
        app.apply(Update::Spaces(vec![SpaceRow::sample(
            "space-1",
            "[herdr] pr-142 (/trees/pr-142)",
        )]));
        select_row(&mut app, "Fix memory leak in auth parser");

        press(&mut app, "SPC f f");
        let Some(Overlay::Prompt(prompt)) = app.overlay() else {
            panic!("expected the feedback prompt");
        };
        assert_eq!(prompt.kind, PromptKind::Feedback);
        assert!(prompt.input.is_multiline());

        type_text(&mut app, "show group labels in which-key");
        let outcome = press(&mut app, "C-s");

        let Outcome::Task(Task::StartFeedback(request)) = outcome else {
            panic!("expected a feedback task, got {outcome:?}");
        };
        assert_eq!(request.request().as_str(), "show group labels in which-key");
        assert_eq!(request.target().name().as_str(), "vetinari");
        let context = request.context().render();
        assert!(context.contains("Profile: work"));
        assert!(context.contains("Fix memory leak in auth parser"));
        assert!(context.contains("rate limited"));
        assert!(context.contains("pr-142"));
        assert!(app.overlay().is_none());
    }

    #[test]
    fn an_empty_feedback_request_is_not_submitted() {
        let mut app = app();
        press(&mut app, "SPC f f");
        let outcome = press(&mut app, "C-s");
        assert!(matches!(outcome, Outcome::Nothing));
        assert!(
            app.overlay().is_some(),
            "the overlay stays open to be filled in"
        );
        assert_eq!(app.status().expect("status").level, Level::Warn);
    }

    #[test]
    fn a_pr_review_is_started_from_a_pull_request_row() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");

        let outcome = press(&mut app, "SPC p r");

        let Outcome::Task(Task::StartPrReview(pr)) = outcome else {
            panic!("expected a review task, got {outcome:?}");
        };
        assert_eq!(pr.number, 142);
        assert_eq!(pr.head_ref, "fix/memory-leak");
    }

    #[test]
    fn a_pr_review_on_a_row_that_is_not_a_pull_request_explains_itself() {
        let mut app = app();
        select_row(&mut app, "Review spec");

        let outcome = press(&mut app, "SPC p r");

        assert!(matches!(outcome, Outcome::Nothing));
        let status = app.status().expect("status");
        assert_eq!(status.level, Level::Warn);
        assert!(status.text.to_lowercase().contains("pull request"));
    }

    #[test]
    fn approving_acts_on_the_selected_pull_request() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        let outcome = press(&mut app, "SPC p a");
        let Outcome::Task(Task::ApprovePr { source, pr }) = outcome else {
            panic!("expected an approval task, got {outcome:?}");
        };
        assert_eq!(source.as_str(), "github");
        assert_eq!(pr.number, 142);
    }

    #[test]
    fn commenting_asks_for_the_comment_first() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        press(&mut app, "SPC p c");
        type_text(&mut app, "one question");

        let outcome = press(&mut app, "RET");

        let Outcome::Task(Task::CommentPr { source, pr, body }) = outcome else {
            panic!("expected a comment task, got {outcome:?}");
        };
        assert_eq!(source.as_str(), "github");
        assert_eq!(pr.number, 142);
        assert_eq!(body, "one question");
    }

    #[test]
    fn an_issue_status_change_asks_for_the_status() {
        let mut app = app();
        select_row(&mut app, "Review spec");
        press(&mut app, "SPC i u");
        type_text(&mut app, "In Progress");

        let outcome = press(&mut app, "RET");

        let Outcome::Task(Task::TransitionIssue {
            source,
            issue,
            status,
        }) = outcome
        else {
            panic!("expected a transition task, got {outcome:?}");
        };
        assert_eq!(source.as_str(), "jira");
        assert_eq!(issue.key, "DEV-891");
        assert_eq!(status, "In Progress");
    }

    #[test]
    fn replying_to_a_message_asks_for_the_reply() {
        let mut app = app();
        select_row(&mut app, "Message from sarah");
        press(&mut app, "SPC c r");
        type_text(&mut app, "on it");

        let outcome = press(&mut app, "RET");

        let Outcome::Task(Task::ReplyChat {
            source,
            message,
            body,
        }) = outcome
        else {
            panic!("expected a reply task, got {outcome:?}");
        };
        assert_eq!(source.as_str(), "slack");
        assert_eq!(message.channel, "D123");
        assert_eq!(body, "on it");
    }

    #[test]
    fn meeting_prep_is_started_from_a_calendar_row() {
        let mut app = app();
        select_row(&mut app, "Sprint planning");

        let outcome = press(&mut app, "SPC m p");

        let Outcome::Task(Task::MeetingPrep {
            title, starts_at, ..
        }) = outcome
        else {
            panic!("expected a meeting prep task, got {outcome:?}");
        };
        assert_eq!(title, "Sprint planning");
        assert_eq!(starts_at, NOW + Duration::minutes(15));
    }

    #[test]
    fn the_profile_switch_takes_effect_and_asks_for_a_refresh() {
        let mut app = app();
        let outcome = press(&mut app, "SPC p p");
        assert_eq!(app.profile().id().as_str(), "home");
        assert!(matches!(outcome, Outcome::Task(Task::Refresh)));
        assert!(app.status().expect("status").text.contains("Home"));
    }

    #[test]
    fn the_profile_menu_activates_the_profile_that_is_chosen() {
        let mut app = app();
        press(&mut app, "SPC p l");
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the profile menu");
        };
        assert_eq!(menu.kind, MenuKind::Profiles);
        assert_eq!(menu.items.len(), 2);

        press(&mut app, "j");
        let outcome = press(&mut app, "RET");

        assert_eq!(app.profile().id().as_str(), "home");
        assert!(matches!(outcome, Outcome::Task(Task::Refresh)));
        assert!(app.overlay().is_none());
    }

    #[test]
    fn the_agent_menu_lists_sessions_and_choosing_one_focuses_it_in_the_monitor() {
        let mut app = app();
        app.apply(Update::Sessions(vec![
            SessionRow::sample("s1", "claude-code", "writing tests", "running"),
            SessionRow::sample("s2", "agy-gemini", "summarising", "waiting"),
        ]));

        press(&mut app, "SPC a a");
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the agent menu");
        };
        assert_eq!(menu.items.len(), 2);
        assert!(menu.items[0].label.contains("claude-code"));

        press(&mut app, "j");
        press(&mut app, "RET");

        assert_eq!(app.focus(), Pane::Monitor);
        assert_eq!(app.monitor_cursor(), 1);
    }

    #[test]
    fn an_agent_can_be_cancelled_rerun_and_unblocked_from_the_monitor() {
        let mut app = app();
        app.apply(Update::Sessions(vec![SessionRow::sample(
            "s1",
            "claude-code",
            "writing tests",
            "waiting",
        )]));

        assert!(matches!(
            press(&mut app, "SPC a c"),
            Outcome::Task(Task::CancelAgent(_))
        ));
        assert!(matches!(
            press(&mut app, "SPC a r"),
            Outcome::Task(Task::RerunAgent(_))
        ));
        assert!(matches!(
            press(&mut app, "SPC a l"),
            Outcome::Task(Task::ShowAgentOutput(_))
        ));

        press(&mut app, "SPC a u");
        type_text(&mut app, "main");
        let outcome = press(&mut app, "RET");
        let Outcome::Task(Task::AnswerAgent(session, answer)) = outcome else {
            panic!("expected an answer task, got {outcome:?}");
        };
        assert_eq!(session.as_str(), "s1");
        assert_eq!(answer, "main");
    }

    #[test]
    fn a_session_can_be_run_beside_the_developer_in_its_own_space() {
        let mut app = app();
        let mut row = SessionRow::sample("s1", "claude-code", "writing tests", "running");
        row.space = Some(SpaceId::parse("space-7").expect("id"));
        app.apply(Update::Sessions(vec![row]));

        let outcome = press(&mut app, "SPC w r");

        let Outcome::Task(Task::DelegateToSpace(session)) = outcome else {
            panic!("expected a delegation task, got {outcome:?}");
        };
        assert_eq!(session.as_str(), "s1");
    }

    #[test]
    fn a_session_with_no_space_cannot_be_run_beside_the_developer() {
        let mut app = app();
        app.apply(Update::Sessions(vec![SessionRow::sample(
            "s1",
            "claude-code",
            "writing tests",
            "running",
        )]));

        let outcome = press(&mut app, "SPC w r");

        assert!(matches!(outcome, Outcome::Nothing));
        assert!(app.status().expect("status").text.contains("no space"));
    }

    #[test]
    fn agent_actions_with_no_session_selected_explain_themselves() {
        let mut app = app();
        let outcome = press(&mut app, "SPC a c");
        assert!(matches!(outcome, Outcome::Nothing));
        assert!(
            app.status()
                .expect("status")
                .text
                .to_lowercase()
                .contains("agent")
        );
    }

    #[test]
    fn the_workspace_menu_focuses_the_space_that_is_chosen() {
        let mut app = app();
        app.apply(Update::Spaces(vec![
            SpaceRow::sample("space-1", "[herdr] pr-142 (/trees/pr-142)"),
            SpaceRow::sample("space-2", "[tmux] feedback (/trees/feedback)"),
        ]));

        press(&mut app, "SPC w w");
        press(&mut app, "j");
        let outcome = press(&mut app, "RET");

        let Outcome::Task(Task::FocusSpace(space)) = outcome else {
            panic!("expected a focus task, got {outcome:?}");
        };
        assert_eq!(space.as_str(), "space-2");
    }

    #[test]
    fn a_session_with_a_space_can_be_focused_directly() {
        let mut app = app();
        let mut row = SessionRow::sample("s1", "claude-code", "writing tests", "running");
        row.space = Some(SpaceId::parse("space-7").expect("id"));
        app.apply(Update::Sessions(vec![row]));

        let outcome = press(&mut app, "SPC w s");

        let Outcome::Task(Task::FocusSpace(space)) = outcome else {
            panic!("expected a focus task, got {outcome:?}");
        };
        assert_eq!(space.as_str(), "space-7");
    }

    #[test]
    fn the_skill_editor_opens_on_a_chosen_skill_and_saves_it() {
        let mut app = app();
        press(&mut app, "SPC s e");
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the skill menu");
        };
        assert_eq!(menu.kind, MenuKind::SkillsToEdit);
        assert!(
            menu.items[0].label.contains("new"),
            "a new skill can be started from the same menu"
        );

        press(&mut app, "j");
        press(&mut app, "RET");

        let Some(Overlay::Form(form)) = app.overlay() else {
            panic!("expected the skill editor");
        };
        assert!(matches!(form.kind, FormKind::Skill(Some(_))));

        let outcome = press(&mut app, "C-s");
        assert!(matches!(outcome, Outcome::Task(Task::SaveSkill(_))));
        assert!(app.overlay().is_none());
    }

    #[test]
    fn the_skill_editor_keeps_the_form_open_when_it_cannot_be_saved() {
        let mut app = app();
        press(&mut app, "SPC s e");
        press(&mut app, "RET");

        let outcome = press(&mut app, "C-s");

        assert!(matches!(outcome, Outcome::Nothing));
        let Some(Overlay::Form(form)) = app.overlay() else {
            panic!("the editor stays open so the problems can be fixed");
        };
        assert!(!form.problems.is_empty());
    }

    #[test]
    fn running_a_skill_binds_what_the_selected_row_knows() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        press(&mut app, "SPC s r");

        // The built-in library lists deep-fix before pr-review.
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the skill menu");
        };
        let index = menu
            .items
            .iter()
            .position(|item| item.label.contains("pr-summary"))
            .expect("pr-summary is offered");
        for _ in 0..index {
            press(&mut app, "j");
        }
        let outcome = press(&mut app, "RET");

        let Outcome::Task(Task::RunSkill { id, bindings }) = outcome else {
            panic!("expected a skill task, got {outcome:?}");
        };
        assert_eq!(id.as_str(), "pr-summary");
        assert_eq!(bindings.get("number"), Some("142"));
        assert_eq!(bindings.get("repo"), Some("acme/web"));
    }

    #[test]
    fn help_lists_the_bindings_in_force() {
        let mut app = app();
        press(&mut app, "?");
        let Some(Overlay::Text(view)) = app.overlay() else {
            panic!("expected the help overlay");
        };
        assert!(view.lines.iter().any(|line| line.contains("SPC f f")));
        assert!(view.lines.iter().any(|line| line.contains("Feedback")));
    }

    #[test]
    fn the_auth_wizard_asks_for_a_token_for_a_token_based_provider() {
        let mut app = app();
        press(&mut app, "SPC h a");
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the provider menu");
        };
        assert_eq!(menu.kind, MenuKind::Providers);
        assert!(menu.items[0].label.contains("GitHub"));

        press(&mut app, "RET");
        let Some(Overlay::Prompt(prompt)) = app.overlay() else {
            panic!("expected a token prompt");
        };
        assert_eq!(prompt.kind, PromptKind::ApiToken(Provider::GitHub));
        assert!(
            prompt.hint.contains("token"),
            "the wizard explains what to paste"
        );

        type_text(&mut app, "ghp_pasted");
        let outcome = press(&mut app, "RET");
        let Outcome::Task(Task::SaveToken(provider, secret)) = outcome else {
            panic!("expected a save task, got {outcome:?}");
        };
        assert_eq!(provider, Provider::GitHub);
        assert_eq!(secret.expose(), "ghp_pasted");
    }

    #[test]
    fn the_auth_wizard_starts_a_device_flow_for_a_provider_that_needs_one() {
        let mut app = app();
        press(&mut app, "SPC h a");
        let index = {
            let Some(Overlay::Menu(menu)) = app.overlay() else {
                panic!("expected the provider menu");
            };
            menu.items
                .iter()
                .position(|item| item.label.contains("Google Calendar"))
                .expect("Google Calendar is offered")
        };
        for _ in 0..index {
            press(&mut app, "j");
        }

        let outcome = press(&mut app, "RET");

        assert!(matches!(
            outcome,
            Outcome::Task(Task::BeginDeviceFlow(Provider::GoogleCalendar))
        ));
    }

    #[test]
    fn a_device_code_challenge_is_shown_while_vetinari_waits() {
        let mut app = app();
        app.apply(Update::DeviceCode(
            Provider::GoogleCalendar,
            DeviceCodeChallenge {
                user_code: "WXYZ-1234".to_owned(),
                verification_uri: "https://google.test/device".to_owned(),
                device_code: "dev".to_owned(),
                interval_secs: 5,
                expires_at: NOW + Duration::minutes(10),
            },
        ));

        let Some(Overlay::DeviceCode(view)) = app.overlay() else {
            panic!("expected the device-code overlay");
        };
        assert_eq!(view.challenge.user_code, "WXYZ-1234");
        assert_eq!(view.provider, Provider::GoogleCalendar);
    }

    #[test]
    fn agent_output_is_shown_in_a_scrollable_overlay() {
        let mut app = app();
        app.apply(Update::AgentOutput(
            SessionId::parse("s1").expect("id"),
            vec!["running tests".to_owned(), "all green".to_owned()],
        ));

        let Some(Overlay::Text(view)) = app.overlay() else {
            panic!("expected the output overlay");
        };
        assert!(view.title.contains("s1"));
        assert_eq!(view.lines.len(), 2);

        press(&mut app, "j");
        let Some(Overlay::Text(view)) = app.overlay() else {
            panic!("expected the output overlay");
        };
        assert_eq!(view.scroll, 1);
    }

    #[test]
    fn refreshing_is_asked_for_by_both_its_bindings() {
        let mut app = app();
        assert!(matches!(press(&mut app, "r"), Outcome::Task(Task::Refresh)));
        assert!(matches!(
            press(&mut app, "SPC r r"),
            Outcome::Task(Task::Refresh)
        ));
    }

    #[test]
    fn quitting_is_reported_once_and_remembered() {
        let mut app = app();
        assert!(matches!(press(&mut app, "SPC q"), Outcome::Quit));
        assert!(app.should_quit());
    }

    #[test]
    fn the_log_keeps_only_as_much_as_it_was_told_to() {
        let mut app = App::new(
            Keymap::doom_defaults(),
            ProfileSet::defaults(),
            SkillLibrary::builtin(),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/repo"),
            ),
            3,
            NOW,
        );
        for index in 0..10 {
            app.apply(Update::Log(format!("line {index}")));
        }
        assert_eq!(app.log().len(), 3);
        assert_eq!(app.log().last().expect("a line"), "line 9");
    }

    #[test]
    fn problems_reported_by_a_refresh_reach_the_status_line_and_the_log() {
        let mut app = app();
        app.apply(Update::Feed(Feed::new(
            Vec::new(),
            vec![SourceProblem {
                source: source("jira"),
                error: PortError::unavailable("jira", "503"),
            }],
            NOW,
        )));

        let status = app.status().expect("status");
        assert_eq!(status.level, Level::Error);
        assert!(status.text.contains("jira"));
        assert!(app.log().iter().any(|line| line.contains("jira")));
    }

    #[test]
    fn the_header_reports_the_profile_and_the_keychain() {
        let mut app = app();
        app.apply(Update::Keychain("Keychain: 2 authenticated".to_owned()));
        assert_eq!(app.header_left(), "VETINARI v0.1 - [ Profile: Work ]");
        assert_eq!(app.header_right(), "Keychain: 2 authenticated");
    }

    #[test]
    fn every_default_binding_is_handled_without_panicking() {
        let keymap = Keymap::doom_defaults();
        for (sequence, action) in keymap.bindings(Mode::Normal) {
            let mut app = app();
            app.apply(Update::Sessions(vec![SessionRow::sample(
                "s1",
                "claude-code",
                "working",
                "waiting",
            )]));
            app.apply(Update::Spaces(vec![SpaceRow::sample(
                "space-1",
                "[tmux] x (/x)",
            )]));
            let outcome = press(&mut app, &sequence.to_string());
            if action == Action::Quit {
                assert!(matches!(outcome, Outcome::Quit));
            }
            // Anything else is acceptable, so long as it did not panic and
            // the app is still usable.
            assert!(app.feed().visible().len() <= 4);
        }
    }

    #[test]
    fn a_tick_moves_the_clock_the_relative_times_are_shown_against() {
        let mut app = app();
        app.apply(Update::Tick(NOW + Duration::minutes(10)));
        assert_eq!(app.now(), NOW + Duration::minutes(10));
        assert_eq!(
            app.feed()
                .selected()
                .expect("a row")
                .relative_time(app.now())
                .as_deref(),
            Some("in 5m")
        );
    }

    #[test]
    fn a_status_message_is_cleared_once_it_has_been_up_long_enough() {
        let mut app = app();
        app.apply(Update::Status(StatusMessage::info("saved")));
        assert!(app.status().is_some());
        app.apply(Update::Tick(NOW + Duration::seconds(10)));
        assert!(
            app.status().is_none(),
            "the status line does not stay stale"
        );
    }

    #[test]
    fn the_details_pane_describes_the_selected_row() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        let details = app.details();
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Title" && value == "Fix memory leak in auth parser")
        );
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Author" && value == "developer")
        );
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Status" && value == "CI failed")
        );
    }

    #[test]
    fn the_details_pane_is_empty_when_the_feed_is() {
        let mut app = app();
        app.apply(Update::Feed(Feed::empty(NOW)));
        assert!(app.details().is_empty());
    }

    #[test]
    fn a_failure_that_needs_authentication_says_which_key_fixes_it() {
        let message = describe_failure(
            "refresh",
            &PortError::Unauthenticated {
                provider: Provider::GitHub,
            },
        );
        assert_eq!(message.level, Level::Warn);
        assert!(message.text.contains("GitHub"));
        assert!(message.text.contains("SPC h a"));
    }

    #[test]
    fn any_other_failure_is_reported_as_an_error_with_its_detail() {
        let message = describe_failure("refresh", &PortError::unavailable("jira", "503"));
        assert_eq!(message.level, Level::Error);
        assert!(message.text.contains("503"));
    }

    #[test]
    fn the_footer_shows_the_headline_bindings_as_the_keymap_defines_them() {
        let app = app();
        let footer = app.footer();
        assert!(footer.contains("SPC p r"));
        assert!(footer.contains("SPC f f"));
        assert!(footer.contains("SPC q"));
    }
}
