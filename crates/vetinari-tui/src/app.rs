//! The application state, and every decision the front-end makes.
//!
//! [`App`] holds no I/O. A keypress goes in and an [`Outcome`] comes out:
//! either nothing, a request to quit, or a [`Task`] for the event loop to run
//! against the adapters. Results come back as [`Update`]s. That split is what
//! lets the whole of the front-end's behaviour be tested without a terminal.

use std::collections::VecDeque;
use std::path::PathBuf;

use time::{Duration, OffsetDateTime};
use vetinari_core::application::agents::AgentRegistry;
use vetinari_core::application::attention::Polled;
use vetinari_core::application::config::General;
use vetinari_core::application::devflow::api::RunDetail;
use vetinari_core::application::keymap_engine::{KeymapEngine, Keystroke};
use vetinari_core::application::ports::{DeviceCodeChallenge, PortError};
use vetinari_core::application::queue::{Lane, QueueCursor, QueueService, Row, lanes};
use vetinari_core::application::skills::SkillDraft;
use vetinari_core::domain::action::Action;
use vetinari_core::domain::agent::AgentSession;
use vetinari_core::domain::attention::{
    AttentionItem, EventRef, IssueRef, ItemContext, ItemState, MessageRef, PullRequestRef, humanize,
};
use vetinari_core::domain::credentials::{AuthMethod, Provider, Secret};
use vetinari_core::domain::devflow::ids::Rounds;
use vetinari_core::domain::devflow::ids::{NodeInstance, RunId};
use vetinari_core::domain::devflow::lanes::IterationSelection;
use vetinari_core::domain::devflow::outcome::{GateAnswer, Line};
use vetinari_core::domain::devflow::step::GateKind;
use vetinari_core::domain::devflow::template::{
    DevWorkflow, FeatureFromTicket, RepoTarget, TicketRef,
};
use vetinari_core::domain::feedback::{CapturedContext, FeedbackRequest, FeedbackTarget};
use vetinari_core::domain::ids::{ItemId, SessionId, SkillId, SourceId, SpaceId};
use vetinari_core::domain::keymap::{Keymap, Mode};
use vetinari_core::domain::keys::{KeyChord, KeyCode};
use vetinari_core::domain::profile::{Profile, ProfileSet};
use vetinari_core::domain::queue::Position;
use vetinari_core::domain::queue::lifecycle::{Lifecycle, ParkUntil, Rule, WaitingOn};
use vetinari_core::domain::queue::rules::QueueRules;
use vetinari_core::domain::queue::{Decision, Entry, Happened, Note, ParkFor, Upstream};
use vetinari_core::domain::skill::{Bindings, Skill, SkillLibrary};
use vetinari_core::domain::text::NonEmptyText;
use vetinari_core::domain::workspace::Space;

use crate::overlay::{
    DeviceCodeView, Form, Menu, MenuItem, MenuKind, MenuTarget, Overlay, Prompt, PromptKind,
    RunView, TextView,
};
use crate::text_input::InputOutcome;

/// Vetinari's version, as shown in the header.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

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
    StartPrReview {
        /// The pull request.
        pr: PullRequestRef,
        /// The item the review is for.
        item: ItemId,
    },
    /// Brief an agent on a meeting.
    MeetingPrep {
        /// The item the briefing is for.
        item: ItemId,
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
    /// Answer a run's gate.
    AnswerGate {
        /// The run.
        run: RunId,
        /// The gate.
        instance: NodeInstance,
        /// The answer.
        answer: GateAnswer,
    },
    /// Cancel a run.
    CancelRun {
        /// The run.
        run: RunId,
    },
    /// Start a dev workflow run for an item.
    StartRun {
        /// The item the run is for, which waits on it.
        item: ItemId,
        /// The run to start. Boxed: it is large and most tasks are small.
        workflow: Box<DevWorkflow>,
    },
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
    /// Every source's answer to a poll.
    Polled(Vec<Polled>),
    /// The daemon started a run for an item.
    RunStarted {
        /// The item.
        item: ItemId,
        /// The run's own queue item.
        run: ItemId,
    },
    /// A workflow started an agent on an item.
    AgentStarted {
        /// The item.
        item: ItemId,
        /// The agent's session.
        session: SessionId,
    },
    /// The agent sessions as they now stand.
    Sessions(Vec<SessionRow>),
    /// The workspace spaces as they now stand.
    Spaces(Vec<SpaceRow>),
    /// The keychain summary.
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
    /// The daemon's latest detail of the run whose popup is open. Boxed
    /// because it is large and every other update is small.
    RunDetail(Box<RunDetail>),
    /// Time has passed.
    Tick(OffsetDateTime),
}

/// The front-end's state.
pub struct App {
    keys: KeymapEngine,
    queue: QueueService,
    cursor: QueueCursor,
    profiles: ProfileSet,
    library: SkillLibrary,
    feedback_target: FeedbackTarget,
    overlay: Option<Overlay>,
    which_key: Option<WhichKeyView>,
    status: Option<(StatusMessage, OffsetDateTime)>,
    log: VecDeque<String>,
    log_limit: usize,
    sessions: Vec<SessionRow>,
    spaces: Vec<SpaceRow>,
    keychain: String,
    recent_shown: usize,
    config_path: Option<PathBuf>,
    last_feedback: Option<FeedbackRequest>,
    filter_backup: Option<String>,
    now: OffsetDateTime,
    quit: bool,
}

impl App {
    /// How many rows a page key moves by.
    pub const PAGE: usize = 10;

    /// How long focus time lasts once started.
    pub const FOCUS_LENGTH: Duration = Duration::hours(1);

    /// Build the state.
    #[must_use]
    pub fn new(
        keymap: Keymap,
        profiles: ProfileSet,
        library: SkillLibrary,
        feedback_target: FeedbackTarget,
        rules: QueueRules,
        log_limit: usize,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            keys: KeymapEngine::new(keymap),
            queue: QueueService::new(rules),
            cursor: QueueCursor::default(),
            profiles,
            library,
            feedback_target,
            overlay: None,
            which_key: None,
            status: None,
            log: VecDeque::new(),
            log_limit: log_limit.max(1),
            sessions: Vec::new(),
            spaces: Vec::new(),
            keychain: "Keychain: unknown".to_owned(),
            recent_shown: General::DEFAULT_RECENT_ACTIONS,
            config_path: None,
            last_feedback: None,
            filter_backup: None,
            now,
            quit: false,
        }
    }

    /// How many recent queue actions to show.
    pub fn set_recent_actions(&mut self, shown: usize) {
        self.recent_shown = shown;
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

    /// The attention queue.
    #[must_use]
    pub const fn queue(&self) -> &QueueService {
        &self.queue
    }

    /// The queue's rows, as the filter admits them.
    #[must_use]
    pub fn rows(&self) -> Vec<Row<'_>> {
        self.cursor.rows(&self.queue.view(self.now))
    }

    /// The selected row.
    #[must_use]
    pub fn selected(&self) -> Option<Row<'_>> {
        self.cursor.selected(&self.queue.view(self.now))
    }

    /// The selected item.
    #[must_use]
    pub fn selected_item(&self) -> Option<&AttentionItem> {
        self.selected().map(|row| row.entry.item())
    }

    /// Which row is selected; the top when there are none.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor.index(&self.queue.view(self.now)).unwrap_or(0)
    }

    /// The filter in force.
    #[must_use]
    pub fn filter(&self) -> &str {
        self.cursor.filter()
    }

    /// What a row's lane alone does not say: when it comes back, what it
    /// waits on, or why it is back.
    #[must_use]
    pub fn row_note(&self, row: &Row<'_>) -> Option<String> {
        let id = row.entry.item().id();
        let when = |at: OffsetDateTime| humanize(at - self.now);
        let note = match self.queue.queue().position(id)? {
            Position::Held(Lifecycle::Parked {
                until: ParkUntil::Time(at),
                ..
            }) => Some(format!("back in {}", when(*at))),
            Position::Held(Lifecycle::Parked {
                until: ParkUntil::Changed,
                ..
            }) => Some("until it changes".to_owned()),
            Position::Held(Lifecycle::Waiting(WaitingOn::Agent)) => Some("on its agent".to_owned()),
            Position::Held(Lifecycle::Waiting(WaitingOn::Reply { follow_up_at })) => {
                Some(format!("on a reply, follow up in {}", when(*follow_up_at)))
            }
            Position::Held(Lifecycle::Inbox) => row
                .entry
                .returned()
                .map(|returned| format!("back: {}", rule_label(returned.rule))),
            Position::Held(Lifecycle::Done { .. }) | Position::Active { .. } => None,
        };
        match gone_from(row.entry).filter(|_| row.lane != Lane::Done) {
            Some(gone) => Some(note.map_or_else(|| gone.clone(), |note| format!("{gone}, {note}"))),
            None => note,
        }
    }

    /// How many rows each lane has, for the queue's title.
    #[must_use]
    pub fn lane_counts(&self) -> Vec<(Lane, usize)> {
        let mut counts: Vec<(Lane, usize)> = Vec::new();
        for row in self.rows() {
            match counts.last_mut() {
                Some((lane, count)) if *lane == row.lane => *count += 1,
                _ => counts.push((row.lane, 1)),
            }
        }
        counts
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

    /// The session behind the selected row: the agent it is, or the agent
    /// it was handed to.
    #[must_use]
    pub fn selected_session(&self) -> Option<&SessionRow> {
        let row = self.selected()?;
        let agent = row.entry.agent().unwrap_or_else(|| row.entry.item().id());
        self.sessions
            .iter()
            .find(|session| AgentRegistry::item_id(&session.id) == *agent)
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

    /// The queue's title: whose it is, what is in it, and whether focus
    /// time is holding anything back.
    #[must_use]
    pub fn title(&self) -> String {
        let mut title = format!("Vetinari · {}", self.profile().name());
        if self.filter().is_empty() {
            let counts = self
                .lane_counts()
                .iter()
                .map(|(lane, count)| format!("{} {count}", lane.title().to_lowercase()))
                .collect::<Vec<_>>();
            if !counts.is_empty() {
                title.push_str(&format!(" ({})", counts.join(", ")));
            }
        } else {
            title.push_str(&format!(" (/{} - {})", self.filter(), self.rows().len()));
        }
        if let Some(focus) = self.queue.queue().focus() {
            title.push_str(&format!(
                " · focus until {:02}:{:02}",
                focus.until.hour(),
                focus.until.minute()
            ));
        }
        title
    }

    /// What recently moved items, newest first, as many as are shown.
    #[must_use]
    pub fn recent_actions(&self) -> Vec<String> {
        self.queue
            .recent()
            .take(self.recent_shown)
            .map(|action| {
                format!(
                    "{:02}:{:02} {}: {}",
                    action.at.hour(),
                    action.at.minute(),
                    action_label(action.happened),
                    action.title
                )
            })
            .collect()
    }

    /// The footer's key hints, spelled as the keymap actually binds them.
    #[must_use]
    pub fn footer(&self) -> String {
        const HINTS: &[(Action, &str)] = &[
            (Action::QueueTakeNext, "Next"),
            (Action::Select, "Details"),
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
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        let item = row.entry.item();
        let mut rows = vec![
            ("Title".to_owned(), item.title().single_line()),
            ("Source".to_owned(), item.source().to_string()),
            ("Status".to_owned(), item.state().label().to_owned()),
            ("Queue".to_owned(), row.lane.title().to_lowercase()),
        ];
        if let Some(returned) = row.entry.returned() {
            rows.push(("Came back".to_owned(), rule_label(returned.rule).to_owned()));
        }
        if let Some(gone) = gone_from(row.entry) {
            rows.push(("Upstream".to_owned(), gone));
        }
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
            Update::Polled(polled) => {
                let notes = self.queue.absorb(self.now, polled);
                self.note(&notes);
                let problems = self.queue.problems().to_vec();
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
            Update::Sessions(sessions) => self.sessions = sessions,
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
            Update::AgentStarted { item, session } => {
                let agent = AgentRegistry::item_id(&session);
                self.decide(Decision::Delegate { item, agent });
            }
            Update::RunStarted { item, run } => {
                self.decide(Decision::Delegate { item, agent: run });
            }
            Update::CloseOverlay => self.close_overlay(),
            Update::RunDetail(detail) => {
                if let Some(Overlay::Run(view)) = self.overlay.as_mut()
                    && view.run == detail.item.summary.run
                {
                    view.detail = Some(*detail);
                }
            }
            Update::Tick(now) => {
                self.now = now;
                let notes = self.queue.tick(now);
                self.note(&notes);
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
            Action::QueueTakeNext => {
                let next = self
                    .queue
                    .view(self.now)
                    .next
                    .first()
                    .map(|ranked| ranked.entry.item().id().clone());
                match next {
                    Some(id) => {
                        self.decide(Decision::Take(id.clone()));
                        self.cursor.select_item(&self.queue.view(self.now), &id);
                        Outcome::Nothing
                    }
                    None => self.info("nothing needs you right now"),
                }
            }
            Action::QueueTake => self.decide_selected(Decision::Take, true),
            Action::QueueFinish => self.decide_selected(Decision::Finish, false),
            Action::QueueSilence => self.decide_selected(Decision::Silence, false),
            Action::QueueParkHour => {
                let until = self.now + Duration::hours(1);
                self.decide_selected(|id| Decision::Park(id, ParkFor::Until(until)), false)
            }
            Action::QueueParkTomorrow => {
                let until = tomorrow_morning(self.now);
                self.decide_selected(|id| Decision::Park(id, ParkFor::Until(until)), false)
            }
            Action::QueueParkChanged => {
                self.decide_selected(|id| Decision::Park(id, ParkFor::Changed), false)
            }
            Action::QueueWaitAgent => self.decide_selected(Decision::WaitOnAgent, false),
            Action::QueueWaitReply => self.decide_selected(Decision::WaitOnReply, false),
            Action::FocusToggle => {
                if self.queue.queue().focus().is_some() {
                    if self.decide(Decision::EndFocus) {
                        self.info("focus time is over")
                    } else {
                        Outcome::Nothing
                    }
                } else {
                    let until = self.now + Self::FOCUS_LENGTH;
                    if self.decide(Decision::StartFocus { until }) {
                        self.info(format!(
                            "focus time for {} minutes",
                            Self::FOCUS_LENGTH.whole_minutes()
                        ))
                    } else {
                        Outcome::Nothing
                    }
                }
            }

            Action::FilterStart => {
                self.filter_backup = Some(self.cursor.filter().to_owned());
                let mut prompt = Prompt::line(
                    PromptKind::Filter,
                    "Filter",
                    "narrow the queue; RET keeps it, ESC restores it",
                );
                prompt.input =
                    crate::text_input::TextInput::single_line().with_value(self.cursor.filter());
                self.overlay = Some(Overlay::Prompt(prompt));
                Outcome::Nothing
            }
            Action::FilterClear => {
                self.cursor.clear_filter();
                Outcome::Nothing
            }
            Action::Select => {
                if let Some(item) = self.selected_item() {
                    match item.context() {
                        ItemContext::Run(summary) => {
                            self.overlay = Some(Overlay::Run(RunView::new(
                                summary.run.clone(),
                                item.title().single_line(),
                            )));
                            return Outcome::Nothing;
                        }
                        ItemContext::SignIn(provider) => {
                            return Outcome::Task(Task::BeginDeviceFlow(*provider));
                        }
                        _ => {}
                    }
                }
                if self.selected().is_some() {
                    self.overlay = Some(Overlay::Details);
                }
                Outcome::Nothing
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

            Action::PrReviewSpawn => match (self.selected_pull_request(), self.selected_id()) {
                (Some((_, pr)), Some(item)) => Outcome::Task(Task::StartPrReview { pr, item }),
                _ => self.warn("select a pull request row first"),
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
            Action::RunFeatureFromTicket => match self.selected_ticket() {
                Some((item, ticket, repo)) => {
                    self.overlay = Some(Overlay::Prompt(Self::repo_prompt(item, ticket, &repo)));
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
            Action::MeetingPrep => match (self.selected_event(), self.selected_id()) {
                (Some((title, event, starts_at)), Some(item)) => Outcome::Task(Task::MeetingPrep {
                    item,
                    title,
                    event,
                    starts_at,
                }),
                _ => self.warn("select a calendar row first"),
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
        self.cursor.move_by(&self.queue.view(self.now), by);
        Outcome::Nothing
    }

    fn navigate_to_end(&mut self, bottom: bool) -> Outcome {
        let view = self.queue.view(self.now);
        if bottom {
            self.cursor.select_last(&view);
        } else {
            self.cursor.select_first(&view);
        }
        Outcome::Nothing
    }

    fn overlay_key(&mut self, chord: KeyChord) -> Outcome {
        match self.overlay.take() {
            Some(Overlay::Prompt(prompt)) => self.prompt_key(prompt, chord),
            Some(Overlay::Menu(menu)) => self.menu_key(menu, chord),
            Some(Overlay::Form(form)) => self.form_key(form, chord),
            Some(Overlay::Text(view)) => self.text_key(view, chord),
            Some(Overlay::Details) => {
                if !matches!(
                    chord.code(),
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')
                ) {
                    self.overlay = Some(Overlay::Details);
                }
                Outcome::Nothing
            }
            Some(Overlay::Run(view)) => self.run_key(view, chord),
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
                    self.cursor.set_filter(previous);
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
                    self.cursor.set_filter(prompt.input.value());
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
            PromptKind::FeatureRepo { item, ticket } => {
                match RepoTarget::named(&text, self.profiles.active()) {
                    Ok(repo) => Outcome::Task(Task::StartRun {
                        item,
                        workflow: Box::new(DevWorkflow::FeatureFromTicket(FeatureFromTicket {
                            ticket,
                            repo,
                            profile: self.profiles.active().id().clone(),
                            base: None,
                            max_review_rounds: Rounds::DEFAULT,
                            max_intake_rounds: Rounds::DEFAULT,
                            agent: None,
                        })),
                    }),
                    Err(error) => {
                        let outcome = self.warn(error.to_string());
                        self.overlay =
                            Some(Overlay::Prompt(Self::repo_prompt(item, ticket, &text)));
                        outcome
                    }
                }
            }
            PromptKind::GateNote { run, instance } => match Line::parse(text.trim()) {
                Ok(note) => Outcome::Task(Task::AnswerGate {
                    run,
                    instance,
                    answer: GateAnswer::Recheck { note: Some(note) },
                }),
                Err(error) => self.warn(format!("that note will not do: {error}")),
            },
        }
    }

    /// Keys in a run's popup: the arrows pick a round, and a gate's choices
    /// answer it (design 8.2).
    fn run_key(&mut self, mut view: RunView, chord: KeyChord) -> Outcome {
        let gate = view.gate();
        let outcome = match (chord.code(), &gate) {
            (KeyCode::Esc | KeyCode::Char('q'), _) => return Outcome::Nothing,
            (KeyCode::Left | KeyCode::Char('h'), _) => {
                view.step_round(false);
                Outcome::Nothing
            }
            (KeyCode::Right | KeyCode::Char('l'), _) => {
                view.step_round(true);
                Outcome::Nothing
            }
            (KeyCode::Char('x'), _) => Outcome::Task(Task::CancelRun {
                run: view.run.clone(),
            }),
            (KeyCode::Char('r'), Some((instance, GateKind::InfoNeeded))) => {
                Outcome::Task(Task::AnswerGate {
                    run: view.run.clone(),
                    instance: instance.clone(),
                    answer: GateAnswer::Recheck { note: None },
                })
            }
            (KeyCode::Char('n'), Some((instance, GateKind::InfoNeeded))) => {
                self.overlay = Some(Overlay::Prompt(Prompt::line(
                    PromptKind::GateNote {
                        run: view.run.clone(),
                        instance: instance.clone(),
                    },
                    format!("Recheck - {}", view.title),
                    "a note for preflight; RET sends it, ESC cancels",
                )));
                return Outcome::Nothing;
            }
            (KeyCode::Char('o'), Some((instance, GateKind::ReviewStuck))) => {
                Outcome::Task(Task::AnswerGate {
                    run: view.run.clone(),
                    instance: instance.clone(),
                    answer: GateAnswer::OpenAnyway,
                })
            }
            _ => Outcome::Nothing,
        };
        self.overlay = Some(Overlay::Run(view));
        outcome
    }

    /// The run whose popup is open, and the rounds it looks at: the event
    /// loop keeps a `run.watch` open for exactly this.
    #[must_use]
    pub fn watched_run(&self) -> Option<(RunId, IterationSelection)> {
        match &self.overlay {
            Some(Overlay::Run(view)) => Some((view.run.clone(), view.selection.clone())),
            _ => None,
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
                let agent = AgentRegistry::item_id(&id);
                let row = self
                    .rows()
                    .into_iter()
                    .find(|row| {
                        row.entry.item().id() == &agent || row.entry.agent() == Some(&agent)
                    })
                    .map(|row| row.entry.item().id().clone());
                match row {
                    Some(item) => {
                        self.cursor.select_item(&self.queue.view(self.now), &item);
                        Outcome::Nothing
                    }
                    None => self.warn(format!("{id} is not in the queue")),
                }
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
                    AuthMethod::OAuthDeviceCode { .. }
                    | AuthMethod::OAuthLoopback { .. }
                    | AuthMethod::OidcDeviceCode => Outcome::Task(Task::BeginDeviceFlow(provider)),
                }
            }
            (MenuKind::Meetings, MenuTarget::Item(id)) => {
                let selected = self
                    .queue
                    .queue()
                    .entry(&id)
                    .map(Entry::item)
                    .and_then(event_of);
                match selected {
                    Some((title, event, starts_at)) => Outcome::Task(Task::MeetingPrep {
                        item: id,
                        title,
                        event,
                        starts_at,
                    }),
                    None => self.warn("that meeting is no longer in the queue"),
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
            .scheduled()
            .into_iter()
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
                    AuthMethod::OAuthLoopback { .. } => "browser sign-in".to_owned(),
                    AuthMethod::OidcDeviceCode => "device code, held by vetinarid".to_owned(),
                },
                target: MenuTarget::Provider(*provider),
            })
            .collect();
        self.overlay = Some(Overlay::Menu(Menu::new(
            MenuKind::Providers,
            format!("Authenticate - {}", self.keychain),
            items,
        )));
    }

    fn open_meeting_menu(&mut self) {
        let items: Vec<MenuItem> = self
            .scheduled()
            .into_iter()
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
            self.cursor.set_filter(previous);
        }
        self.overlay = None;
    }

    fn capture_context(&self) -> CapturedContext {
        let mut captured = CapturedContext::new(self.profile().id().clone(), self.keys.mode());
        if let Some(item) = self.selected_item() {
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
        let Some(item) = self.selected_item() else {
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
            ItemContext::Run(summary) => bindings.with("run", summary.run.to_string()),
            ItemContext::SignIn(_) | ItemContext::None => {
                bindings.with("issues", item.title().single_line())
            }
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
        let item = self.selected_item()?;
        match item.context() {
            ItemContext::PullRequest(pr) => Some((item.source().clone(), pr.clone())),
            _ => None,
        }
    }

    fn selected_issue(&self) -> Option<(SourceId, IssueRef)> {
        let item = self.selected_item()?;
        match item.context() {
            ItemContext::Issue(issue) => Some((item.source().clone(), issue.clone())),
            _ => None,
        }
    }

    /// The selected issue as a ticket to run, and the repository its key
    /// names, if any: a GitHub issue's key is `owner/name#n`.
    fn selected_ticket(&self) -> Option<(ItemId, TicketRef, String)> {
        let item = self.selected_item()?;
        let ItemContext::Issue(issue) = item.context() else {
            return None;
        };
        let ticket = TicketRef {
            source: item.source().clone(),
            key: NonEmptyText::parse(&issue.key).ok()?,
            title: item.title().clone(),
            url: item.url().map(str::to_owned),
        };
        let repo = issue
            .key
            .split_once('#')
            .map(|(repo, _)| repo.to_owned())
            .unwrap_or_default();
        Some((item.id().clone(), ticket, repo))
    }

    /// Ask which repository a ticket's run works in.
    fn repo_prompt(item: ItemId, ticket: TicketRef, repo: &str) -> Prompt {
        let title = format!("Run {} to a pull request", ticket.key);
        let mut prompt = Prompt::line(
            PromptKind::FeatureRepo { item, ticket },
            title,
            "the repository, as owner/name; RET starts the run",
        );
        prompt.input = crate::text_input::TextInput::single_line().with_value(repo);
        prompt
    }

    fn selected_message(&self) -> Option<(SourceId, MessageRef)> {
        let item = self.selected_item()?;
        match item.context() {
            ItemContext::Message(message) => Some((item.source().clone(), message.clone())),
            _ => None,
        }
    }

    fn selected_event(&self) -> Option<(String, EventRef, OffsetDateTime)> {
        event_of(self.selected_item()?)
    }

    fn focusable_space(&self) -> Option<SpaceId> {
        if let Some(item) = self.selected_item()
            && let ItemContext::Agent(agent) = item.context()
            && let Some(space) = &agent.space
        {
            return Some(space.clone());
        }
        self.selected_session()
            .and_then(|session| session.space.clone())
            .or_else(|| self.spaces.first().map(|space| space.id.clone()))
    }

    fn selected_id(&self) -> Option<ItemId> {
        self.selected_item().map(|item| item.id().clone())
    }

    /// Open meetings, soonest first.
    fn scheduled(&self) -> Vec<&AttentionItem> {
        let mut meetings: Vec<&AttentionItem> = lanes(&self.queue.view(self.now))
            .into_iter()
            .filter(|row| row.lane != Lane::Done)
            .map(|row| row.entry.item())
            .filter(|item| matches!(item.state(), ItemState::Scheduled { .. }))
            .collect();
        meetings.sort_by_key(|item| item.occurred_at());
        meetings
    }

    /// Apply a decision, warning when the queue refuses it.
    fn decide(&mut self, decision: Decision) -> bool {
        match self.queue.decide(self.now, decision) {
            Ok(notes) => {
                self.note(&notes);
                true
            }
            Err(refusal) => {
                self.set_status(StatusMessage::warn(refusal.to_string()));
                false
            }
        }
    }

    /// Decide about the selected item. The selection either goes with the
    /// item or stays where it was, moving on to whatever follows.
    fn decide_selected(
        &mut self,
        decision: impl FnOnce(ItemId) -> Decision,
        follow: bool,
    ) -> Outcome {
        let Some(id) = self.selected_id() else {
            return self.warn("select an item first");
        };
        if !follow {
            self.cursor.keep_place(&self.queue.view(self.now));
        }
        self.decide(decision(id));
        Outcome::Nothing
    }

    /// Log what the queue brought back, and say so.
    fn note(&mut self, notes: &[Note]) {
        for note in notes {
            if let Happened::Resurfaced(rule) = note.happened {
                self.push_log(format!("INFO queue: {} came back ({rule:?})", note.item));
                let title = self.queue.queue().entry(&note.item).map_or_else(
                    || note.item.to_string(),
                    |entry| entry.item().title().single_line(),
                );
                self.set_status(StatusMessage::info(format!(
                    "{title} is back: {}",
                    rule_label(rule)
                )));
            }
        }
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

const fn rule_label(rule: Rule) -> &'static str {
    match rule {
        Rule::SwitchedAway => "you switched away from it",
        Rule::SnoozeEnded => "its park ran out",
        Rule::Changed => "it changed",
        Rule::UrgentChange => "it turned urgent",
        Rule::Reopened => "it needs you again",
        Rule::ReplyArrived => "the reply arrived",
        Rule::FollowUpDue => "time to follow up",
        Rule::ArrivedDuringFocus => "focus time is over",
        Rule::AgentNeedsYou => "its agent needs you",
        Rule::AgentClosed => "its agent went away",
    }
}

fn gone_from(entry: &Entry) -> Option<String> {
    (entry.upstream() == Upstream::Gone).then(|| format!("gone from {}", entry.item().source()))
}

fn action_label(happened: Happened) -> String {
    match happened {
        Happened::Taken => "taken".to_owned(),
        Happened::SwitchedAway => "switched away".to_owned(),
        Happened::Parked => "parked".to_owned(),
        Happened::Finished => "finished".to_owned(),
        Happened::Silenced => "silenced".to_owned(),
        Happened::WaitingOnReply => "waiting on a reply".to_owned(),
        Happened::HandedToAgent => "handed to an agent".to_owned(),
        Happened::WaitingOnAgent => "waiting on its agent".to_owned(),
        Happened::HeldForFocus => "held for focus".to_owned(),
        Happened::Resurfaced(rule) => format!("back, {}", rule_label(rule)),
        Happened::GoneWhileActive => "gone from its source".to_owned(),
        Happened::ClosedUpstream => "closed at its source".to_owned(),
        Happened::AgentNeedsYouWhileActive => "its agent needs you".to_owned(),
        Happened::AgentClosed => "its agent went away".to_owned(),
        Happened::Arrived
        | Happened::Updated
        | Happened::StayedParked
        | Happened::IgnoredSilenced
        | Happened::LeftClosed
        | Happened::KeptAfterGone
        | Happened::ClosedWithItem => "updated".to_owned(),
    }
}

/// Nine the next morning, in the clock's own offset.
fn tomorrow_morning(now: OffsetDateTime) -> OffsetDateTime {
    now.date()
        .next_day()
        .map_or(now + Duration::days(1), |day| {
            day.with_time(time::macros::time!(09:00))
                .assume_offset(now.offset())
        })
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
    use vetinari_core::application::attention::Polled;
    use vetinari_core::domain::attention::{
        AttentionItem, EventRef, IssueRef, ItemContext, ItemState, MessageRef, PullRequestRef,
        SourceCategory,
    };
    use vetinari_core::domain::ids::SourceId;
    use vetinari_core::domain::queue::lifecycle::{Closure, Lifecycle, ParkUntil, WaitingOn};
    use vetinari_core::testing::polled;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source(name: &str) -> SourceId {
        SourceId::parse(name).expect("id")
    }

    fn agent_item(session: &str, state: ItemState) -> AttentionItem {
        AttentionItem::builder(
            &source("agents"),
            session,
            SourceCategory::AgentState,
            NonEmptyText::parse(&format!("agent {session}")).expect("title"),
        )
        .state(state)
        .occurred_at(NOW)
        .build()
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

    fn empty_app() -> App {
        app_with_rules(QueueRules::default())
    }

    fn app_with_rules(rules: QueueRules) -> App {
        App::new(
            Keymap::doom_defaults(),
            ProfileSet::defaults(),
            SkillLibrary::builtin(),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            rules,
            200,
            NOW,
        )
    }

    fn app() -> App {
        let mut app = empty_app();
        app.apply(Update::Polled(polled(vec![
            pr_item(),
            issue_item(),
            message_item(),
            event_item(),
        ])));
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
        press(app, "g g");
        for _ in 0..10 {
            if app
                .selected_item()
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
        assert!(app.overlay().is_none());
        assert_eq!(
            app.selected_item().expect("a row").title().as_str(),
            "Sprint planning",
            "a meeting in fifteen minutes outranks a failing build"
        );
        assert_eq!(app.profile().id().as_str(), "work");
    }

    fn position_of(app: &App, title: &str) -> String {
        let row = app
            .queue()
            .view(app.now())
            .now
            .into_iter()
            .chain(lanes_of(app))
            .find(|entry| entry.item().title().as_str() == title)
            .map(|entry| entry.item().id().clone())
            .unwrap_or_else(|| panic!("no item titled {title:?}"));
        format!("{:?}", app.queue().queue().position(&row))
    }

    fn lanes_of(app: &App) -> Vec<&vetinari_core::domain::queue::Entry> {
        app.rows().into_iter().map(|row| row.entry).collect()
    }

    fn lane_of(app: &App, title: &str) -> Lane {
        app.rows()
            .into_iter()
            .find(|row| row.entry.item().title().as_str() == title)
            .map(|row| row.lane)
            .unwrap_or_else(|| panic!("no row titled {title:?}"))
    }

    #[test]
    fn spc_n_takes_what_needs_you_next() {
        let mut app = app();

        press(&mut app, "SPC n");

        assert_eq!(lane_of(&app, "Sprint planning"), Lane::Now);
        assert_eq!(
            app.selected_item().expect("a row").title().as_str(),
            "Sprint planning",
            "the selection goes with what you took"
        );
    }

    #[test]
    fn taking_something_else_parks_what_you_were_on() {
        let mut app = app();
        press(&mut app, "SPC n");
        select_row(&mut app, "Review spec");

        press(&mut app, "SPC d t");

        assert_eq!(lane_of(&app, "Review spec"), Lane::Now);
        assert_eq!(lane_of(&app, "Sprint planning"), Lane::Parked);
    }

    #[test]
    fn finishing_moves_on_to_the_row_that_followed() {
        let mut app = app();
        let second = app.rows()[1].entry.item().title().as_str().to_owned();

        press(&mut app, "SPC d d");

        assert_eq!(lane_of(&app, "Sprint planning"), Lane::Done);
        assert_eq!(app.selected_item().expect("a row").title().as_str(), second);
    }

    #[test]
    fn silencing_closes_an_item_for_good() {
        let mut app = app();
        select_row(&mut app, "Message from sarah");

        press(&mut app, "SPC d x");

        assert!(
            position_of(&app, "Message from sarah").contains(&format!("{:?}", Closure::Silenced))
        );
    }

    #[test]
    fn parking_for_an_hour_brings_it_back_when_the_hour_is_up() {
        let mut app = app();
        select_row(&mut app, "Review spec");

        press(&mut app, "SPC d p h");
        assert_eq!(lane_of(&app, "Review spec"), Lane::Parked);
        app.apply(Update::Tick(NOW + Duration::hours(1)));

        assert_eq!(lane_of(&app, "Review spec"), Lane::Next);
        assert!(
            app.log()
                .iter()
                .any(|line| line.contains("jira:DEV-891") && line.contains("SnoozeEnded")),
            "a return is logged with the rule that caused it: {:?}",
            app.log()
        );
    }

    #[test]
    fn an_item_that_came_back_says_why_until_it_is_acted_on() {
        let mut app = app();
        select_row(&mut app, "Review spec");
        press(&mut app, "SPC d p h");
        app.apply(Update::Tick(NOW + Duration::hours(1)));
        select_row(&mut app, "Review spec");

        let row = app.selected().expect("a row");

        assert_eq!(
            app.row_note(&row).as_deref(),
            Some("back: its park ran out")
        );
        assert!(
            app.details()
                .iter()
                .any(|(key, value)| key == "Came back" && value == "its park ran out")
        );
    }

    #[test]
    fn parking_until_tomorrow_brings_it_back_at_nine() {
        let mut app = app();
        select_row(&mut app, "Review spec");

        press(&mut app, "SPC d p t");

        assert!(position_of(&app, "Review spec").contains(&format!(
            "{:?}",
            ParkUntil::Time(datetime!(2026-10-22 09:00 UTC))
        )));
    }

    #[test]
    fn parking_until_it_changes_waits_on_its_source() {
        let mut app = app();
        select_row(&mut app, "Review spec");

        press(&mut app, "SPC d p c");

        assert!(position_of(&app, "Review spec").contains(&format!("{:?}", ParkUntil::Changed)));
    }

    #[test]
    fn waiting_on_a_reply_follows_up_later() {
        let mut app = app();
        select_row(&mut app, "Message from sarah");

        press(&mut app, "SPC d w r");

        assert_eq!(lane_of(&app, "Message from sarah"), Lane::Waiting);
        assert!(position_of(&app, "Message from sarah").contains(&format!(
            "{:?}",
            WaitingOn::Reply {
                follow_up_at: NOW + Duration::hours(2)
            }
        )));
    }

    #[test]
    fn a_decision_the_queue_refuses_is_a_warning() {
        let mut app = app();

        press(&mut app, "SPC d w a");

        let status = app.status().expect("a status");
        assert_eq!(status.level, Level::Warn);
        assert!(status.text.contains("no agent"), "{}", status.text);
    }

    #[test]
    fn focus_time_holds_messages_back_until_it_is_ended() {
        let mut app = app();

        press(&mut app, "SPC t f");
        assert_eq!(lane_of(&app, "Message from sarah"), Lane::HeldForFocus);
        press(&mut app, "SPC t f");

        assert_eq!(lane_of(&app, "Message from sarah"), Lane::Next);
    }

    #[test]
    fn a_workflow_started_for_an_item_hands_it_to_the_agent() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        let Outcome::Task(Task::StartPrReview { item, .. }) = press(&mut app, "SPC p r") else {
            panic!("expected a review task");
        };

        app.apply(Update::AgentStarted {
            item,
            session: SessionId::parse("s1").expect("session"),
        });

        assert_eq!(
            lane_of(&app, "Fix memory leak in auth parser"),
            Lane::Waiting
        );
        assert!(
            position_of(&app, "Fix memory leak in auth parser")
                .contains(&format!("{:?}", Lifecycle::Waiting(WaitingOn::Agent)))
        );
    }

    fn github_issue_item() -> AttentionItem {
        AttentionItem::builder(
            &source("github"),
            "acme/web#17",
            SourceCategory::IssueTracking,
            NonEmptyText::parse("Retry on 503").expect("title"),
        )
        .state(ItemState::Assigned)
        .url("https://github.test/acme/web/issues/17")
        .occurred_at(NOW)
        .context(ItemContext::Issue(IssueRef {
            key: "acme/web#17".to_owned(),
            status: "open".to_owned(),
            transitions: Vec::new(),
        }))
        .build()
    }

    fn start_run(
        outcome: Outcome,
    ) -> (
        ItemId,
        vetinari_core::domain::devflow::template::FeatureFromTicket,
    ) {
        let Outcome::Task(Task::StartRun { item, workflow }) = outcome else {
            panic!("expected a run to start, got {outcome:?}");
        };
        let DevWorkflow::FeatureFromTicket(feature) = *workflow;
        (item, feature)
    }

    #[test]
    fn a_ticket_is_run_to_a_pull_request_in_the_repository_named() {
        let mut app = app();
        select_row(&mut app, "Review spec");

        press(&mut app, "SPC w f");
        let Some(Overlay::Prompt(prompt)) = app.overlay() else {
            panic!("expected the repository prompt");
        };
        assert!(matches!(prompt.kind, PromptKind::FeatureRepo { .. }));
        type_text(&mut app, "acme/web");
        let (item, feature) = start_run(press(&mut app, "RET"));

        assert_eq!(item, issue_item().id().clone());
        assert_eq!(feature.ticket.source.as_str(), "jira");
        assert_eq!(feature.ticket.key.as_str(), "DEV-891");
        assert_eq!(feature.ticket.title.as_str(), "Review spec");
        assert_eq!(feature.repo.remote.as_str(), "acme/web");
        assert_eq!(feature.repo.path, app.profile().repo_path("acme/web"));
        assert_eq!(&feature.profile, app.profile().id());
    }

    #[test]
    fn a_github_issue_offers_its_own_repository() {
        let mut app = empty_app();
        app.apply(Update::Polled(polled(vec![github_issue_item()])));
        select_row(&mut app, "Retry on 503");

        press(&mut app, "SPC w f");
        let (_, feature) = start_run(press(&mut app, "RET"));
        assert_eq!(feature.repo.remote.as_str(), "acme/web");
        assert_eq!(
            feature.ticket.url.as_deref(),
            Some("https://github.test/acme/web/issues/17")
        );
    }

    #[test]
    fn a_repository_that_is_not_owner_and_name_is_asked_for_again() {
        let mut app = app();
        select_row(&mut app, "Review spec");
        press(&mut app, "SPC w f");
        type_text(&mut app, "web");

        assert!(matches!(press(&mut app, "RET"), Outcome::Nothing));
        assert_eq!(app.status().expect("a status").level, Level::Warn);
        assert!(matches!(app.overlay(), Some(Overlay::Prompt(_))));
    }

    #[test]
    fn running_a_ticket_needs_an_issue_row() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        assert!(matches!(press(&mut app, "SPC w f"), Outcome::Nothing));
        assert_eq!(app.status().expect("a status").level, Level::Warn);
        assert!(app.overlay().is_none());
    }

    #[test]
    fn a_started_run_holds_its_ticket_until_the_run_needs_you() {
        let mut app = app();
        app.apply(Update::RunStarted {
            item: issue_item().id().clone(),
            run: ItemId::from_parts(&source("vetinarid"), "01JB80"),
        });
        assert_eq!(lane_of(&app, "Review spec"), Lane::Waiting);
        assert!(
            position_of(&app, "Review spec")
                .contains(&format!("{:?}", Lifecycle::Waiting(WaitingOn::Agent)))
        );
    }

    #[test]
    fn a_source_that_is_down_keeps_its_rows() {
        let mut app = app();

        app.apply(Update::Polled(vec![Polled {
            source: source("jira"),
            outcome: Err(PortError::unavailable("jira", "503")),
        }]));

        assert_eq!(lane_of(&app, "Review spec"), Lane::Next);
    }

    #[test]
    fn an_item_being_worked_on_says_when_its_source_drops_it() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        press(&mut app, "SPC d t");

        app.apply(Update::Polled(vec![Polled {
            source: source("github"),
            outcome: Ok(Vec::new()),
        }]));
        select_row(&mut app, "Fix memory leak in auth parser");
        let row = app.selected().expect("a row");

        assert_eq!(row.lane, Lane::Now);
        assert_eq!(app.row_note(&row).as_deref(), Some("gone from github"));
        assert!(
            app.details()
                .iter()
                .any(|(key, value)| key == "Upstream" && value == "gone from github")
        );
    }

    #[test]
    fn an_item_its_source_dropped_keeps_its_other_note() {
        let mut app = app_with_rules(QueueRules {
            close_when_gone: false,
            ..QueueRules::default()
        });
        app.apply(Update::Polled(polled(vec![issue_item()])));
        select_row(&mut app, "Review spec");
        press(&mut app, "SPC d p h");

        app.apply(Update::Polled(vec![Polled {
            source: source("jira"),
            outcome: Ok(Vec::new()),
        }]));
        let row = app
            .rows()
            .into_iter()
            .find(|row| row.entry.item().title().as_str() == "Review spec")
            .expect("a row");

        assert_eq!(
            app.row_note(&row).as_deref(),
            Some("gone from jira, back in 1h")
        );
    }

    #[test]
    fn a_row_closed_because_its_source_dropped_it_needs_no_note() {
        let mut app = app();

        app.apply(Update::Polled(vec![Polled {
            source: source("jira"),
            outcome: Ok(Vec::new()),
        }]));
        let row = app
            .rows()
            .into_iter()
            .find(|row| row.entry.item().title().as_str() == "Review spec")
            .expect("a row");

        assert_eq!(row.lane, Lane::Done);
        assert_eq!(app.row_note(&row), None);
    }

    #[test]
    fn the_title_names_the_profile_and_when_focus_time_ends() {
        let mut app = empty_app();
        assert_eq!(app.title(), "Vetinari · Work");

        press(&mut app, "SPC t f");

        assert_eq!(app.title(), "Vetinari · Work · focus until 10:00");
    }

    #[test]
    fn the_title_counts_each_lane() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");

        press(&mut app, "SPC d t");

        assert!(
            app.title().starts_with("Vetinari · Work (now 1, next 3)"),
            "{}",
            app.title()
        );
    }

    #[test]
    fn return_opens_the_details_of_the_selected_row_and_escape_closes_them() {
        let mut app = app();

        press(&mut app, "RET");
        assert!(matches!(app.overlay(), Some(Overlay::Details)));

        press(&mut app, "<Esc>");
        assert!(app.overlay().is_none());
    }

    #[test]
    fn recent_actions_say_when_and_what_happened_to_which_item() {
        let mut app = app();
        select_row(&mut app, "Fix memory leak in auth parser");
        press(&mut app, "SPC d p h");

        app.apply(Update::Tick(NOW + Duration::hours(1)));

        assert_eq!(
            app.recent_actions(),
            vec![
                "10:00 back, its park ran out: Fix memory leak in auth parser".to_owned(),
                "09:00 parked: Fix memory leak in auth parser".to_owned(),
            ]
        );
    }

    #[test]
    fn only_as_many_recent_actions_as_configured_are_shown() {
        let mut app = app();
        app.set_recent_actions(1);

        press(&mut app, "SPC n");
        press(&mut app, "SPC n");

        assert_eq!(app.recent_actions().len(), 1);
    }

    #[test]
    fn vim_keys_move_the_feed_selection() {
        let mut app = app();
        press(&mut app, "j");
        assert_eq!(app.cursor(), 1);
        press(&mut app, "G");
        assert_eq!(app.cursor(), 3);
        press(&mut app, "g g");
        assert_eq!(app.cursor(), 0);
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

        assert_eq!(app.rows().len(), 1);
        assert_eq!(
            app.selected_item().expect("a row").title().as_str(),
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
        assert_eq!(app.filter(), "memory");

        press(&mut app, "/");
        type_text(&mut app, "zzz");
        press(&mut app, "<Esc>");
        assert!(app.overlay().is_none());
        assert_eq!(app.filter(), "memory", "the kept filter survives");
    }

    #[test]
    fn escape_in_normal_mode_clears_the_filter() {
        let mut app = app();
        press(&mut app, "/");
        type_text(&mut app, "memory");
        press(&mut app, "RET");
        press(&mut app, "<Esc>");
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 4);
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

        let Outcome::Task(Task::StartPrReview { pr, .. }) = outcome else {
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
    fn the_agent_menu_lists_sessions_and_choosing_one_selects_its_row() {
        let mut app = app();
        app.apply(Update::Sessions(vec![
            SessionRow::sample("s1", "claude-code", "writing tests", "running"),
            SessionRow::sample("s2", "agy-gemini", "summarising", "waiting"),
        ]));
        app.apply(Update::Polled(polled(vec![
            agent_item("s1", ItemState::AgentRunning),
            agent_item("s2", ItemState::AgentWaiting),
        ])));

        press(&mut app, "SPC a a");
        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the agent menu");
        };
        assert_eq!(menu.items.len(), 2);
        assert!(menu.items[0].label.contains("claude-code"));

        press(&mut app, "j");
        press(&mut app, "RET");

        assert_eq!(
            app.selected_item()
                .map(|item| item.title().as_str().to_owned()),
            Some("agent s2".to_owned())
        );
    }

    #[test]
    fn agent_actions_act_on_the_agent_of_the_selected_row() {
        let mut app = app();
        app.apply(Update::Sessions(vec![
            SessionRow::sample("s1", "claude-code", "writing tests", "waiting"),
            SessionRow::sample("s2", "agy-gemini", "summarising", "running"),
        ]));
        app.apply(Update::Polled(polled(vec![
            agent_item("s1", ItemState::AgentWaiting),
            agent_item("s2", ItemState::AgentWaiting),
        ])));
        select_row(&mut app, "agent s1");

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
    fn agent_actions_follow_an_item_to_the_agent_working_on_it() {
        let mut app = app();
        app.apply(Update::Sessions(vec![SessionRow::sample(
            "s1",
            "claude-code",
            "reviewing",
            "running",
        )]));
        select_row(&mut app, "Fix memory leak in auth parser");
        let Outcome::Task(Task::StartPrReview { item, .. }) = press(&mut app, "SPC p r") else {
            panic!("expected a review task");
        };
        app.apply(Update::AgentStarted {
            item,
            session: SessionId::parse("s1").expect("session"),
        });
        select_row(&mut app, "Fix memory leak in auth parser");

        let outcome = press(&mut app, "SPC a l");

        let Outcome::Task(Task::ShowAgentOutput(session)) = outcome else {
            panic!("expected the agent's output, got {outcome:?}");
        };
        assert_eq!(session.as_str(), "s1");
    }

    #[test]
    fn a_session_can_be_run_beside_the_developer_in_its_own_space() {
        let mut app = app();
        let mut row = SessionRow::sample("s1", "claude-code", "writing tests", "running");
        row.space = Some(SpaceId::parse("space-7").expect("id"));
        app.apply(Update::Sessions(vec![row]));
        app.apply(Update::Polled(polled(vec![agent_item(
            "s1",
            ItemState::AgentWaiting,
        )])));
        select_row(&mut app, "agent s1");

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
        app.apply(Update::Polled(polled(vec![agent_item(
            "s1",
            ItemState::AgentWaiting,
        )])));
        select_row(&mut app, "agent s1");

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
        app.apply(Update::Polled(polled(vec![agent_item(
            "s1",
            ItemState::AgentWaiting,
        )])));
        select_row(&mut app, "agent s1");

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
                user_code: Some("WXYZ-1234".to_owned()),
                verification_uri: "https://google.test/device".to_owned(),
                device_code: "dev".to_owned(),
                interval_secs: 5,
                expires_at: NOW + Duration::minutes(10),
            },
        ));

        let Some(Overlay::DeviceCode(view)) = app.overlay() else {
            panic!("expected the device-code overlay");
        };
        assert_eq!(view.challenge.user_code.as_deref(), Some("WXYZ-1234"));
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
            QueueRules::default(),
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
        app.apply(Update::Polled(vec![Polled {
            source: source("jira"),
            outcome: Err(PortError::unavailable("jira", "503")),
        }]));

        let status = app.status().expect("status");
        assert_eq!(status.level, Level::Error);
        assert!(status.text.contains("jira"));
        assert!(app.log().iter().any(|line| line.contains("jira")));
    }

    #[test]
    fn the_authenticate_menu_says_what_the_keychain_holds() {
        let mut app = app();
        app.apply(Update::Keychain("Keychain: 2 authenticated".to_owned()));

        press(&mut app, "SPC h a");

        let Some(Overlay::Menu(menu)) = app.overlay() else {
            panic!("expected the provider menu");
        };
        assert!(
            menu.title.contains("Keychain: 2 authenticated"),
            "{}",
            menu.title
        );
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
            assert!(app.rows().len() <= 4);
        }
    }

    #[test]
    fn a_tick_moves_the_clock_the_relative_times_are_shown_against() {
        let mut app = app();
        app.apply(Update::Tick(NOW + Duration::minutes(10)));
        assert_eq!(app.now(), NOW + Duration::minutes(10));
        assert_eq!(
            app.selected_item()
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
    fn the_details_pane_is_empty_when_the_queue_is() {
        assert!(empty_app().details().is_empty());
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
        assert!(footer.contains("SPC n: Next"), "{footer}");
        assert!(footer.contains("RET: Details"), "{footer}");
        assert!(footer.contains("SPC h k: Help"), "{footer}");
    }

    mod runs {
        use std::num::NonZeroU32;

        use vetinari_core::domain::devflow::ids::NodeId;
        use vetinari_core::domain::devflow::lanes::IterationSelection;
        use vetinari_core::domain::devflow::outcome::GateAnswer;

        use super::*;
        use crate::fixtures::{ask_info, detail, item, run};

        fn app_with_a_run() -> App {
            let mut app = app();
            let source = SourceId::parse("vetinarid").expect("source");
            app.apply(Update::Polled(vec![Polled {
                source: source.clone(),
                outcome: Ok(vec![item().to_attention(&source, NOW)]),
            }]));
            select_row(&mut app, "ENG-398 Paginate audit export");
            app
        }

        fn opened() -> App {
            let mut app = app_with_a_run();
            press(&mut app, "RET");
            app.apply(Update::RunDetail(Box::new(detail())));
            app
        }

        #[test]
        fn ret_on_a_run_opens_its_popup_and_watches_it() {
            let mut app = app_with_a_run();
            press(&mut app, "RET");
            assert!(matches!(app.overlay(), Some(Overlay::Run(view)) if view.run == run()));
            assert_eq!(
                app.watched_run(),
                Some((run(), IterationSelection::default()))
            );
        }

        #[test]
        fn the_popup_takes_the_detail_the_daemon_sends() {
            let app = opened();
            let Some(Overlay::Run(view)) = app.overlay() else {
                panic!("the popup is open");
            };
            assert_eq!(view.detail.as_ref(), Some(&detail()));
        }

        #[test]
        fn r_rechecks_and_n_rechecks_with_a_note() {
            let mut app = opened();
            assert!(matches!(
                press(&mut app, "r"),
                Outcome::Task(Task::AnswerGate { run: answered, instance, answer: GateAnswer::Recheck { note: None } })
                    if answered == run() && instance == ask_info()
            ));

            press(&mut app, "n");
            assert!(matches!(
                app.overlay(),
                Some(Overlay::Prompt(prompt)) if matches!(prompt.kind, PromptKind::GateNote { .. })
            ));
            assert_eq!(app.watched_run(), None, "the popup gave way to the prompt");
            type_text(&mut app, "Page size is 50");
            let outcome = press(&mut app, "RET");
            assert!(matches!(
                outcome,
                Outcome::Task(Task::AnswerGate { answer: GateAnswer::Recheck { note: Some(note) }, .. })
                    if note.as_str() == "Page size is 50"
            ));
        }

        #[test]
        fn x_abandons_the_run_and_esc_closes_the_popup() {
            let mut app = opened();
            assert!(matches!(
                press(&mut app, "x"),
                Outcome::Task(Task::CancelRun { run: cancelled }) if cancelled == run()
            ));
            press(&mut app, "ESC");
            assert!(app.overlay().is_none());
            assert_eq!(app.watched_run(), None);
        }

        #[test]
        fn the_arrows_look_at_another_round_of_the_loop() {
            let mut app = opened();
            press(&mut app, "<left>");
            assert_eq!(
                app.watched_run(),
                Some((
                    run(),
                    IterationSelection::default()
                        .with(NodeId::parse("intake").expect("node"), NonZeroU32::MIN)
                ))
            );
            press(&mut app, "<left>");
            assert_eq!(
                app.watched_run().map(|(_, selection)| selection),
                Some(
                    IterationSelection::default()
                        .with(NodeId::parse("intake").expect("node"), NonZeroU32::MIN)
                ),
                "there is no round before the first"
            );
            press(&mut app, "<right>");
            assert_eq!(
                app.watched_run().map(|(_, selection)| selection),
                Some(IterationSelection::default().with(
                    NodeId::parse("intake").expect("node"),
                    NonZeroU32::new(2).expect("two")
                ))
            );
        }

        #[test]
        fn ret_on_a_sign_in_item_begins_the_daemons_sign_in() {
            let mut app = app();
            let source = SourceId::parse("vetinarid").expect("source");
            let item = AttentionItem::builder(
                &source,
                "sign-in",
                SourceCategory::AgentState,
                NonEmptyText::parse("autognomus needs sign-in").expect("title"),
            )
            .state(ItemState::NeedsDecision)
            .occurred_at(NOW)
            .context(ItemContext::SignIn(Provider::Autognomus))
            .build();
            app.apply(Update::Polled(vec![Polled {
                source,
                outcome: Ok(vec![item]),
            }]));
            select_row(&mut app, "autognomus needs sign-in");
            assert!(matches!(
                press(&mut app, "RET"),
                Outcome::Task(Task::BeginDeviceFlow(Provider::Autognomus))
            ));
        }
    }
}
