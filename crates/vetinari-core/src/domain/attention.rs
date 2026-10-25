//! The attention stream: one prioritised inbox of everything asking for
//! human judgement.
//!
//! Every adapter - GitHub, Jira, Slack, a calendar, a running agent - reduces
//! its own world to [`AttentionItem`]s. Ranking lives here rather than in the
//! adapters so that a failing build and a blocked agent can be compared at
//! all, and so the ordering can be tested without touching a network.

use std::fmt;

use time::{Duration, OffsetDateTime};

use super::action::Action;
use super::credentials::Provider;
use super::devflow::summary::RunSummary;
use super::ids::{ItemId, SessionId, SourceId, SpaceId};
use super::text::NonEmptyText;

/// The kind of signal a source produces, matching the PRD's source
/// categories and the port each one implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceCategory {
    /// Code review and CI, from a code host.
    CodeReview,
    /// Tickets and issues, from an issue tracker.
    IssueTracking,
    /// Chat messages and DMs.
    Messaging,
    /// Calendar events.
    Calendar,
    /// Work being done by agents - the ones Vetinari started, and the ones
    /// it can see running in a workspace target.
    AgentState,
}

impl SourceCategory {
    /// Every category.
    pub const ALL: &'static [SourceCategory] = &[
        SourceCategory::CodeReview,
        SourceCategory::IssueTracking,
        SourceCategory::Messaging,
        SourceCategory::Calendar,
        SourceCategory::AgentState,
    ];

    /// The short tag shown at the start of a feed row.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            SourceCategory::CodeReview => "PR",
            SourceCategory::IssueTracking => "JIRA",
            SourceCategory::Messaging => "SLACK",
            SourceCategory::Calendar => "CAL",
            SourceCategory::AgentState => "AGENT",
        }
    }

    /// The actions that apply to any item in this category.
    #[must_use]
    pub fn actions(&self) -> &'static [Action] {
        match self {
            SourceCategory::CodeReview => &[
                Action::PrReviewSpawn,
                Action::PrApprove,
                Action::PrComment,
                Action::Select,
            ],
            SourceCategory::IssueTracking => &[
                Action::RunFeatureFromTicket,
                Action::IssueUpdateStatus,
                Action::IssueAssign,
                Action::IssueStandupSummary,
                Action::Select,
            ],
            SourceCategory::Messaging => {
                &[Action::ChatReply, Action::ChatConvertToTask, Action::Select]
            }
            SourceCategory::Calendar => {
                &[Action::MeetingPrep, Action::MeetingSchedule, Action::Select]
            }
            SourceCategory::AgentState => &[
                Action::WorkspaceFocusSpace,
                Action::AgentUnblock,
                Action::AgentCancel,
                Action::AgentRerun,
                Action::AgentLogs,
            ],
        }
    }
}

impl fmt::Display for SourceCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where an item stands. The variants that depend on a time carry it, so an
/// item can never claim to be scheduled without saying when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// A review has been requested of you.
    NeedsReview,
    /// A reviewer asked for changes on your work.
    ChangesRequested,
    /// CI is red.
    CiFailed,
    /// CI is still running.
    CiRunning,
    /// An unread message.
    Unread,
    /// Assigned to you and not started.
    Assigned,
    /// Assigned to you and in progress.
    InProgress,
    /// Something external is blocking it.
    Blocked,
    /// An agent is working.
    AgentRunning,
    /// An agent stopped and needs an answer before it can continue.
    AgentWaiting,
    /// An agent finished successfully.
    AgentDone,
    /// An agent failed.
    AgentFailed,
    /// A workflow is waiting on the developer's decision at a gate.
    NeedsDecision,
    /// An event starting at a known time.
    Scheduled {
        /// When it starts.
        starts_at: OffsetDateTime,
    },
    /// Nothing outstanding.
    Resolved,
}

impl ItemState {
    /// The label shown in parentheses on a feed row.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            ItemState::NeedsReview => "needs review",
            ItemState::ChangesRequested => "changes requested",
            ItemState::CiFailed => "CI failed",
            ItemState::CiRunning => "CI running",
            ItemState::Unread => "unread",
            ItemState::Assigned => "assigned",
            ItemState::InProgress => "in progress",
            ItemState::Blocked => "blocked",
            ItemState::AgentRunning => "running",
            ItemState::AgentWaiting => "waiting on you",
            ItemState::AgentDone => "done",
            ItemState::AgentFailed => "failed",
            ItemState::NeedsDecision => "needs a decision",
            ItemState::Scheduled { .. } => "scheduled",
            ItemState::Resolved => "resolved",
        }
    }

    /// Whether anything is wanted from the developer right now. Work still in
    /// flight belongs out of the way until it lands.
    #[must_use]
    pub const fn needs_you(&self) -> bool {
        !matches!(
            self,
            ItemState::CiRunning | ItemState::AgentRunning | ItemState::Resolved
        )
    }

    /// Whether every minute this waits costs someone, so it should cut a
    /// snooze short.
    #[must_use]
    pub const fn is_urgent(&self) -> bool {
        matches!(
            self,
            ItemState::AgentWaiting
                | ItemState::NeedsDecision
                | ItemState::Blocked
                | ItemState::AgentFailed
                | ItemState::CiFailed
                | ItemState::ChangesRequested
        )
    }

    const fn base_score(&self) -> u32 {
        match self {
            ItemState::AgentWaiting => 900,
            ItemState::NeedsDecision => 880,
            ItemState::Blocked => 850,
            ItemState::AgentFailed => 800,
            ItemState::CiFailed => 700,
            ItemState::ChangesRequested => 650,
            ItemState::NeedsReview => 600,
            ItemState::Unread => 500,
            ItemState::Scheduled { .. } => 450,
            ItemState::Assigned => 400,
            ItemState::InProgress => 350,
            ItemState::CiRunning => 300,
            ItemState::AgentRunning => 250,
            ItemState::AgentDone => 100,
            ItemState::Resolved => 50,
        }
    }
}

/// A computed ranking. Higher sorts first in the feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u32);

impl Priority {
    pub(crate) const fn raised(self, by: u32) -> Self {
        Self(self.0.saturating_add(by))
    }
}

/// A pull request or merge request, with everything a review workflow needs
/// to create a worktree for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRef {
    /// `owner/name`, as the host spells it.
    pub repo: String,
    /// The PR or MR number.
    pub number: u64,
    /// The branch under review.
    pub head_ref: String,
    /// The branch it targets.
    pub base_ref: String,
    /// A URL git can clone or fetch from.
    pub clone_url: String,
}

/// A tracker issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    /// The tracker's own key, e.g. `DEV-891`.
    pub key: String,
    /// The current workflow status, as the tracker names it.
    pub status: String,
    /// Statuses this issue may be moved to.
    pub transitions: Vec<String>,
}

/// A chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    /// Channel or conversation id.
    pub channel: String,
    /// Thread root, when the message is in a thread.
    pub thread: Option<String>,
}

/// A calendar event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRef {
    /// When it ends, for working out whether it is still running.
    pub ends_at: Option<OffsetDateTime>,
    /// Who is invited.
    pub attendees: Vec<String>,
    /// Where it is, or the conferencing link.
    pub location: Option<String>,
}

/// A running or finished agent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    /// The session the agent is running in.
    pub session: SessionId,
    /// The workspace space it occupies, when it has one.
    pub space: Option<SpaceId>,
}

/// Source-specific payload, so workflows get typed data instead of parsing
/// strings back out of a title.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ItemContext {
    /// No extra context.
    #[default]
    None,
    /// A pull or merge request.
    PullRequest(PullRequestRef),
    /// A tracker issue.
    Issue(IssueRef),
    /// A chat message.
    Message(MessageRef),
    /// A calendar event.
    Event(EventRef),
    /// An agent task.
    Agent(AgentRef),
    /// A dev workflow run, as the daemon summarised it.
    Run(RunSummary),
    /// A credential only vetinarid holds needs signing in again.
    SignIn(Provider),
}

/// One entry in the attention feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    id: ItemId,
    source: SourceId,
    category: SourceCategory,
    title: NonEmptyText,
    state: ItemState,
    author: Option<String>,
    url: Option<String>,
    occurred_at: OffsetDateTime,
    due_at: Option<OffsetDateTime>,
    details: Vec<(String, String)>,
    context: ItemContext,
}

impl AttentionItem {
    /// Start building an item. The id is derived from the source so two
    /// adapters cannot collide on a native key.
    #[must_use]
    pub fn builder(
        source: &SourceId,
        native_key: &str,
        category: SourceCategory,
        title: NonEmptyText,
    ) -> AttentionItemBuilder {
        AttentionItemBuilder {
            item: AttentionItem {
                id: ItemId::from_parts(source, native_key),
                source: source.clone(),
                category,
                title,
                state: ItemState::Unread,
                author: None,
                url: None,
                occurred_at: OffsetDateTime::UNIX_EPOCH,
                due_at: None,
                details: Vec::new(),
                context: ItemContext::None,
            },
        }
    }

    /// The feed-unique id.
    #[must_use]
    pub const fn id(&self) -> &ItemId {
        &self.id
    }

    /// The adapter that produced it.
    #[must_use]
    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    /// Its category.
    #[must_use]
    pub const fn category(&self) -> SourceCategory {
        self.category
    }

    /// Its title.
    #[must_use]
    pub const fn title(&self) -> &NonEmptyText {
        &self.title
    }

    /// Where it stands.
    #[must_use]
    pub const fn state(&self) -> ItemState {
        self.state
    }

    /// Who raised it.
    #[must_use]
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    /// Where to open it.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// When it happened.
    #[must_use]
    pub const fn occurred_at(&self) -> OffsetDateTime {
        self.occurred_at
    }

    /// When it is due, if it is.
    #[must_use]
    pub const fn due_at(&self) -> Option<OffsetDateTime> {
        self.due_at
    }

    /// Extra rows for the details pane.
    #[must_use]
    pub fn details(&self) -> &[(String, String)] {
        &self.details
    }

    /// Typed, source-specific context.
    #[must_use]
    pub const fn context(&self) -> &ItemContext {
        &self.context
    }

    /// The actions offered for this item.
    #[must_use]
    pub fn available_actions(&self) -> &'static [Action] {
        self.category.actions()
    }

    /// How urgently this wants attention, as of `now`.
    #[must_use]
    pub fn priority(&self, now: OffsetDateTime) -> Priority {
        let mut score = self.state.base_score();

        if let ItemState::Scheduled { starts_at } = self.state {
            let until = starts_at - now;
            score += if until.is_negative() {
                0
            } else if until <= Duration::minutes(15) {
                600
            } else if until <= Duration::hours(1) {
                300
            } else if until <= Duration::hours(4) {
                100
            } else {
                0
            };
        }

        if let Some(due) = self.due_at {
            let until = due - now;
            score += if until.is_negative() {
                300
            } else if until <= Duration::days(1) {
                150
            } else if until <= Duration::days(3) {
                50
            } else {
                0
            };
        }

        // A nudge for staleness, capped so it can never outweigh a state.
        let age_minutes = (now - self.occurred_at).whole_minutes().clamp(0, 120);
        score += u32::try_from(age_minutes / 10).unwrap_or(0);

        Priority(score)
    }

    /// Whether the item matches a free-text filter.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        let mut haystack = format!(
            "{} {} {} {}",
            self.title.as_str(),
            self.source,
            self.category.label(),
            self.state.label()
        );
        if let Some(author) = &self.author {
            haystack.push(' ');
            haystack.push_str(author);
        }
        if let Some(url) = &self.url {
            haystack.push(' ');
            haystack.push_str(url);
        }
        for (key, value) in &self.details {
            haystack.push(' ');
            haystack.push_str(key);
            haystack.push(' ');
            haystack.push_str(value);
        }
        haystack.to_lowercase().contains(&needle)
    }

    /// One line for the feed.
    #[must_use]
    pub fn row_label(&self) -> String {
        if let ItemContext::Run(summary) = &self.context {
            return format!("[run] {}  {}", self.title.single_line(), summary.strip());
        }
        format!(
            "[{}] {} ({})",
            self.category.label(),
            self.title.single_line(),
            self.state.label()
        )
    }

    /// How far away the item's own moment is, phrased for a feed row.
    #[must_use]
    pub fn relative_time(&self, now: OffsetDateTime) -> Option<String> {
        let moment = match self.state {
            ItemState::Scheduled { starts_at } => Some(starts_at),
            _ => self.due_at,
        }?;
        let delta = moment - now;
        let magnitude = humanize(delta.abs());
        Some(if delta.is_negative() {
            format!("{magnitude} ago")
        } else {
            format!("in {magnitude}")
        })
    }
}

/// A span of time, as short as a feed row wants it: `3d 2h`, `1h 5m`, `4m`.
#[must_use]
pub fn humanize(delta: Duration) -> String {
    let total_minutes = delta.whole_minutes();
    let days = total_minutes / (60 * 24);
    let hours = (total_minutes % (60 * 24)) / 60;
    let minutes = total_minutes % 60;
    if days > 0 {
        if hours > 0 {
            return format!("{days}d {hours}h");
        }
        return format!("{days}d");
    }
    if hours > 0 {
        if minutes > 0 {
            return format!("{hours}h {minutes}m");
        }
        return format!("{hours}h");
    }
    format!("{minutes}m")
}

/// Builds an [`AttentionItem`]; adapters set only the fields they have.
#[derive(Debug, Clone)]
pub struct AttentionItemBuilder {
    item: AttentionItem,
}

impl AttentionItemBuilder {
    /// Set where the item stands.
    #[must_use]
    pub fn state(mut self, state: ItemState) -> Self {
        self.item.state = state;
        self
    }

    /// Set who raised it.
    #[must_use]
    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.item.author = Some(author.into());
        self
    }

    /// Set where to open it.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.item.url = Some(url.into());
        self
    }

    /// Set when it happened.
    #[must_use]
    pub fn occurred_at(mut self, at: OffsetDateTime) -> Self {
        self.item.occurred_at = at;
        self
    }

    /// Set when it is due.
    #[must_use]
    pub fn due_at(mut self, at: OffsetDateTime) -> Self {
        self.item.due_at = Some(at);
        self
    }

    /// Add a row to the details pane.
    #[must_use]
    pub fn detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.item.details.push((key.into(), value.into()));
        self
    }

    /// Attach typed, source-specific context.
    #[must_use]
    pub fn context(mut self, context: ItemContext) -> Self {
        self.item.context = context;
        self
    }

    /// Finish the item.
    #[must_use]
    pub fn build(self) -> AttentionItem {
        self.item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source(name: &str) -> SourceId {
        SourceId::parse(name).expect("slug")
    }

    fn item(name: &str, native: &str, category: SourceCategory, state: ItemState) -> AttentionItem {
        AttentionItem::builder(
            &source(name),
            native,
            category,
            NonEmptyText::parse("a thing that needs you").expect("title"),
        )
        .state(state)
        .occurred_at(NOW - Duration::minutes(10))
        .build()
    }

    #[test]
    fn an_item_derives_its_feed_id_from_its_source() {
        let pr = item(
            "github",
            "142",
            SourceCategory::CodeReview,
            ItemState::CiFailed,
        );
        assert_eq!(pr.id().as_str(), "github:142");
        assert_eq!(pr.source(), &source("github"));
    }

    #[test]
    fn categories_carry_the_prd_labels_and_port_names() {
        assert_eq!(SourceCategory::CodeReview.label(), "PR");
        assert_eq!(SourceCategory::IssueTracking.label(), "JIRA");
        assert_eq!(SourceCategory::Messaging.label(), "SLACK");
        assert_eq!(SourceCategory::Calendar.label(), "CAL");
        assert_eq!(SourceCategory::AgentState.label(), "AGENT");
        assert_eq!(SourceCategory::ALL.len(), 5);
    }

    #[test]
    fn a_blocked_agent_outranks_a_failing_build() {
        let blocked = item(
            "herdr",
            "space-3",
            SourceCategory::AgentState,
            ItemState::AgentWaiting,
        );
        let ci = item(
            "github",
            "142",
            SourceCategory::CodeReview,
            ItemState::CiFailed,
        );
        assert!(blocked.priority(NOW) > ci.priority(NOW));
    }

    #[test]
    fn a_failing_build_outranks_an_unread_message() {
        let ci = item(
            "github",
            "142",
            SourceCategory::CodeReview,
            ItemState::CiFailed,
        );
        let dm = item(
            "slack",
            "D123",
            SourceCategory::Messaging,
            ItemState::Unread,
        );
        assert!(ci.priority(NOW) > dm.priority(NOW));
    }

    #[test]
    fn an_imminent_meeting_beats_everything_else() {
        let meeting = item(
            "google-calendar",
            "evt-1",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW + Duration::minutes(15),
            },
        );
        let blocked = item(
            "herdr",
            "space-3",
            SourceCategory::AgentState,
            ItemState::AgentWaiting,
        );
        assert!(meeting.priority(NOW) > blocked.priority(NOW));
    }

    #[test]
    fn a_distant_meeting_does_not_crowd_out_live_work() {
        let meeting = item(
            "google-calendar",
            "evt-2",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW + Duration::hours(6),
            },
        );
        let ci = item(
            "github",
            "142",
            SourceCategory::CodeReview,
            ItemState::CiFailed,
        );
        assert!(meeting.priority(NOW) < ci.priority(NOW));
    }

    #[test]
    fn a_meeting_already_under_way_is_not_urgent_any_more() {
        let started = item(
            "google-calendar",
            "evt-3",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW - Duration::minutes(5),
            },
        );
        let upcoming = item(
            "google-calendar",
            "evt-4",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW + Duration::minutes(5),
            },
        );
        assert!(started.priority(NOW) < upcoming.priority(NOW));
    }

    #[test]
    fn an_overdue_issue_outranks_one_due_next_week() {
        let overdue = AttentionItem::builder(
            &source("jira"),
            "DEV-891",
            SourceCategory::IssueTracking,
            NonEmptyText::parse("Review spec").expect("title"),
        )
        .state(ItemState::Assigned)
        .due_at(NOW - Duration::days(1))
        .build();
        let later = AttentionItem::builder(
            &source("jira"),
            "DEV-892",
            SourceCategory::IssueTracking,
            NonEmptyText::parse("Review spec").expect("title"),
        )
        .state(ItemState::Assigned)
        .due_at(NOW + Duration::days(7))
        .build();
        assert!(overdue.priority(NOW) > later.priority(NOW));
    }

    #[test]
    fn finished_work_ranks_below_anything_still_open() {
        let done = item(
            "herdr",
            "space-2",
            SourceCategory::AgentState,
            ItemState::AgentDone,
        );
        let unread = item("slack", "D1", SourceCategory::Messaging, ItemState::Unread);
        assert!(done.priority(NOW) < unread.priority(NOW));
    }

    #[test]
    fn items_expose_the_actions_their_category_supports() {
        let pr = item(
            "github",
            "1",
            SourceCategory::CodeReview,
            ItemState::NeedsReview,
        );
        assert!(pr.available_actions().contains(&Action::PrReviewSpawn));
        assert!(pr.available_actions().contains(&Action::PrApprove));

        let issue = item(
            "jira",
            "ENG-412",
            SourceCategory::IssueTracking,
            ItemState::Assigned,
        );
        assert!(
            issue
                .available_actions()
                .contains(&Action::RunFeatureFromTicket)
        );

        let event = item(
            "google-calendar",
            "e",
            SourceCategory::Calendar,
            ItemState::Scheduled { starts_at: NOW },
        );
        assert!(event.available_actions().contains(&Action::MeetingPrep));
        assert!(!event.available_actions().contains(&Action::PrApprove));
    }

    #[test]
    fn items_match_a_filter_on_title_source_state_and_detail() {
        let pr = AttentionItem::builder(
            &source("github"),
            "142",
            SourceCategory::CodeReview,
            NonEmptyText::parse("Fix memory leak in auth parser").expect("title"),
        )
        .state(ItemState::CiFailed)
        .author("developer")
        .detail("Branch", "fix/leak")
        .build();

        assert!(pr.matches("memory"));
        assert!(pr.matches("MEMORY"));
        assert!(pr.matches("github"));
        assert!(pr.matches("ci failed"));
        assert!(pr.matches("developer"));
        assert!(pr.matches("fix/leak"));
        assert!(!pr.matches("sprint planning"));
    }

    #[test]
    fn an_item_summarises_itself_for_one_feed_row() {
        let pr = AttentionItem::builder(
            &source("github"),
            "142",
            SourceCategory::CodeReview,
            NonEmptyText::parse("Fix memory leak").expect("title"),
        )
        .state(ItemState::CiFailed)
        .build();
        assert_eq!(pr.row_label(), "[PR] Fix memory leak (CI failed)");
    }

    #[test]
    fn a_scheduled_item_says_how_long_until_it_starts() {
        let soon = item(
            "google-calendar",
            "e",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW + Duration::minutes(15),
            },
        );
        assert_eq!(soon.relative_time(NOW).as_deref(), Some("in 15m"));

        let past = item(
            "google-calendar",
            "e2",
            SourceCategory::Calendar,
            ItemState::Scheduled {
                starts_at: NOW - Duration::minutes(90),
            },
        );
        assert_eq!(past.relative_time(NOW).as_deref(), Some("1h 30m ago"));
    }

    #[test]
    fn work_in_flight_needs_nothing_from_you() {
        for state in [
            ItemState::CiRunning,
            ItemState::AgentRunning,
            ItemState::Resolved,
        ] {
            assert!(!state.needs_you(), "{state:?}");
        }
    }

    #[test]
    fn finished_agent_output_still_needs_you_to_look() {
        assert!(ItemState::AgentDone.needs_you());
        assert!(ItemState::Unread.needs_you());
        assert!(
            ItemState::Scheduled {
                starts_at: NOW + Duration::hours(3)
            }
            .needs_you()
        );
    }

    #[test]
    fn only_states_that_cost_time_now_are_urgent() {
        for state in [
            ItemState::AgentWaiting,
            ItemState::Blocked,
            ItemState::AgentFailed,
            ItemState::CiFailed,
            ItemState::ChangesRequested,
        ] {
            assert!(state.is_urgent(), "{state:?}");
        }
        for state in [
            ItemState::NeedsReview,
            ItemState::Unread,
            ItemState::Assigned,
            ItemState::AgentDone,
        ] {
            assert!(!state.is_urgent(), "{state:?}");
        }
    }

    #[test]
    fn a_decision_needs_you_and_cuts_a_snooze_short() {
        assert!(ItemState::NeedsDecision.needs_you());
        assert!(ItemState::NeedsDecision.is_urgent());
        assert!(ItemState::NeedsDecision.base_score() > ItemState::AgentFailed.base_score());
        assert_eq!(ItemState::NeedsDecision.label(), "needs a decision");
    }

    #[test]
    fn a_run_row_shows_its_strip_rather_than_a_state() {
        use crate::domain::devflow::ids::RunId;
        use crate::domain::devflow::run_view::RunStatus;
        use crate::domain::devflow::summary::{Mark, Phase, PhaseCell, RunSummary};

        let summary = RunSummary {
            run: RunId::parse("01JB7Z").expect("run"),
            status: RunStatus::Active,
            phases: vec![
                PhaseCell {
                    phase: Phase::Pre,
                    mark: Mark::Done,
                    round: None,
                },
                PhaseCell {
                    phase: Phase::Impl,
                    mark: Mark::RunningHere,
                    round: None,
                },
            ],
            needs_you: None,
        };
        let item = AttentionItem::builder(
            &SourceId::parse("vetinarid").expect("source"),
            "01JB7Z",
            SourceCategory::AgentState,
            NonEmptyText::parse("ENG-412 Rate-limit retries").expect("title"),
        )
        .state(ItemState::AgentRunning)
        .context(ItemContext::Run(summary))
        .build();
        assert_eq!(
            item.row_label(),
            "[run] ENG-412 Rate-limit retries  pre✓ impl●"
        );
    }
}
