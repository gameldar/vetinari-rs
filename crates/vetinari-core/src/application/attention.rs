//! The attention engine: poll every source the active profile wants, merge
//! the results into one prioritised feed, and keep a view over it.
//!
//! Sources are polled concurrently and failures are collected rather than
//! propagated, because one rate-limited tracker must not cost the developer
//! sight of a failing build.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::join_all;
use time::OffsetDateTime;

use super::ports::{AttentionSource, Clock, PortError, PortResult};
use crate::domain::attention::{AttentionItem, SourceCategory};
use crate::domain::ids::{ItemId, SourceId};
use crate::domain::profile::Profile;

/// A source that could not be polled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProblem {
    /// Which source failed.
    pub source: SourceId,
    /// Why.
    pub error: PortError,
}

/// One source's answer to a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    /// Which source.
    pub source: SourceId,
    /// Everything it reports, or why it could not be asked.
    pub outcome: PortResult<Vec<AttentionItem>>,
}

/// Everything asking for attention, most urgent first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    items: Vec<AttentionItem>,
    problems: Vec<SourceProblem>,
    refreshed_at: OffsetDateTime,
}

impl Feed {
    /// Build a feed, sorting the items by priority.
    #[must_use]
    pub fn new(
        mut items: Vec<AttentionItem>,
        problems: Vec<SourceProblem>,
        refreshed_at: OffsetDateTime,
    ) -> Self {
        items.sort_by(|left, right| {
            right
                .priority(refreshed_at)
                .cmp(&left.priority(refreshed_at))
                .then_with(|| left.id().cmp(right.id()))
        });
        Self {
            items,
            problems,
            refreshed_at,
        }
    }

    /// A feed with nothing in it.
    #[must_use]
    pub fn empty(refreshed_at: OffsetDateTime) -> Self {
        Self::new(Vec::new(), Vec::new(), refreshed_at)
    }

    /// The items, in priority order.
    #[must_use]
    pub fn items(&self) -> &[AttentionItem] {
        &self.items
    }

    /// The sources that failed on the last refresh.
    #[must_use]
    pub fn problems(&self) -> &[SourceProblem] {
        &self.problems
    }

    /// When the feed was built.
    #[must_use]
    pub const fn refreshed_at(&self) -> OffsetDateTime {
        self.refreshed_at
    }

    /// How many items there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the feed is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many items still want something from the developer.
    #[must_use]
    pub fn actionable_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.state().is_actionable())
            .count()
    }

    /// Item counts per category, for the header.
    #[must_use]
    pub fn counts_by_category(&self) -> BTreeMap<SourceCategory, usize> {
        let mut counts = BTreeMap::new();
        for item in &self.items {
            *counts.entry(item.category()).or_insert(0) += 1;
        }
        counts
    }

    /// Find an item by id.
    #[must_use]
    pub fn get(&self, id: &ItemId) -> Option<&AttentionItem> {
        self.items.iter().find(|item| item.id() == id)
    }
}

/// Polls the registered sources and builds feeds.
pub struct AttentionEngine {
    sources: Vec<Arc<dyn AttentionSource>>,
    clock: Arc<dyn Clock>,
}

impl AttentionEngine {
    /// Build an engine with no sources.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            sources: Vec::new(),
            clock,
        }
    }

    /// Add a source.
    pub fn register(&mut self, source: Arc<dyn AttentionSource>) {
        self.sources.push(source);
    }

    /// Every registered source id.
    #[must_use]
    pub fn source_ids(&self) -> Vec<SourceId> {
        self.sources
            .iter()
            .map(|source| source.id().clone())
            .collect()
    }

    /// Poll the sources the profile wants, concurrently, keeping each
    /// source's answer apart.
    pub async fn poll(&self, profile: &Profile) -> Vec<Polled> {
        let account = profile.account();
        join_all(
            self.sources
                .iter()
                .filter(|source| profile.wants_source(source.id()))
                .map(|source| async {
                    Polled {
                        source: source.id().clone(),
                        outcome: source.fetch(account).await,
                    }
                }),
        )
        .await
    }

    /// Poll the sources the profile wants and merge them into one feed.
    pub async fn refresh(&self, profile: &Profile) -> Feed {
        let mut items = Vec::new();
        let mut problems = Vec::new();
        for Polled { source, outcome } in self.poll(profile).await {
            match outcome {
                Ok(fetched) => items.extend(fetched),
                Err(error) => problems.push(SourceProblem { source, error }),
            }
        }

        Feed::new(items, problems, self.clock.now())
    }
}

/// A filtered, navigable view over a feed.
///
/// The selection is remembered by item id rather than by row, so a refresh
/// that reorders the feed does not move the developer off what they were
/// reading.
#[derive(Debug, Clone)]
pub struct FeedView {
    feed: Feed,
    filter: String,
    cursor: usize,
}

impl FeedView {
    /// Open a view on a feed.
    #[must_use]
    pub fn new(feed: Feed) -> Self {
        Self {
            feed,
            filter: String::new(),
            cursor: 0,
        }
    }

    /// The underlying feed.
    #[must_use]
    pub const fn feed(&self) -> &Feed {
        &self.feed
    }

    /// The active filter.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The row the cursor is on.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The rows the filter admits.
    #[must_use]
    pub fn visible(&self) -> Vec<&AttentionItem> {
        self.feed
            .items()
            .iter()
            .filter(|item| item.matches(&self.filter))
            .collect()
    }

    /// The selected item, if there is one.
    #[must_use]
    pub fn selected(&self) -> Option<&AttentionItem> {
        self.visible().get(self.cursor).copied()
    }

    /// The selected item's id.
    #[must_use]
    pub fn selected_id(&self) -> Option<ItemId> {
        self.selected().map(|item| item.id().clone())
    }

    /// Replace the filter, keeping the selection where possible.
    pub fn set_filter(&mut self, filter: impl Into<String>) {
        let previous = self.selected_id();
        self.filter = filter.into();
        self.restore_selection(previous.as_ref());
    }

    /// Drop the filter.
    pub fn clear_filter(&mut self) {
        self.set_filter(String::new());
    }

    /// Replace the feed, keeping the selection where possible.
    pub fn replace_feed(&mut self, feed: Feed) {
        let previous = self.selected_id();
        self.feed = feed;
        self.restore_selection(previous.as_ref());
    }

    /// Move down a row.
    pub fn select_next(&mut self) {
        let last = self.last_row();
        self.cursor = self.cursor.saturating_add(1).min(last);
    }

    /// Move up a row.
    pub fn select_previous(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Jump to the top.
    pub fn select_first(&mut self) {
        self.cursor = 0;
    }

    /// Jump to the bottom.
    pub fn select_last(&mut self) {
        self.cursor = self.last_row();
    }

    /// Move down a page.
    pub fn page_down(&mut self, page: usize) {
        let last = self.last_row();
        self.cursor = self.cursor.saturating_add(page).min(last);
    }

    /// Move up a page.
    pub fn page_up(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_sub(page);
    }

    fn last_row(&self) -> usize {
        self.visible().len().saturating_sub(1)
    }

    fn restore_selection(&mut self, previous: Option<&ItemId>) {
        let visible = self.visible();
        if visible.is_empty() {
            self.cursor = 0;
            return;
        }
        if let Some(previous) = previous
            && let Some(index) = visible.iter().position(|item| item.id() == previous)
        {
            self.cursor = index;
            return;
        }
        self.cursor = self.cursor.min(visible.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attention::{ItemState, SourceCategory};
    use crate::domain::ids::{ProfileId, Slug};
    use crate::domain::profile::Profile;
    use crate::domain::text::NonEmptyText;
    use crate::domain::workspace::TargetKind;
    use crate::testing::{FakeSource, FixedClock};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source_id(name: &str) -> SourceId {
        SourceId::parse(name).expect("slug")
    }

    fn item(
        source: &str,
        native: &str,
        category: SourceCategory,
        state: ItemState,
    ) -> AttentionItem {
        AttentionItem::builder(
            &source_id(source),
            native,
            category,
            NonEmptyText::parse(&format!("{source} {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(NOW)
        .build()
    }

    fn profile(sources: &[&str]) -> Profile {
        let mut profile = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Tmux,
        );
        for source in sources {
            profile = profile.with_source(source_id(source));
        }
        profile
    }

    fn engine(sources: Vec<Arc<dyn AttentionSource>>) -> AttentionEngine {
        let mut engine = AttentionEngine::new(Arc::new(FixedClock::new(NOW)));
        for source in sources {
            engine.register(source);
        }
        engine
    }

    #[tokio::test]
    async fn every_source_is_polled_as_the_active_profiles_account() {
        // The profile is what scopes credentials, and it changes while
        // Vetinari is running, so the account has to travel with the call.
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            Vec::new(),
        ));
        let engine = engine(vec![github.clone()]);
        let work = profile(&["github"]).with_account(Slug::parse("work").expect("account"));
        let home = profile(&["github"]).with_account(Slug::parse("home").expect("account"));

        engine.refresh(&work).await;
        engine.refresh(&home).await;

        let seen: Vec<Option<String>> = github
            .accounts()
            .into_iter()
            .map(|account| account.map(|slug| slug.as_str().to_owned()))
            .collect();
        assert_eq!(
            seen,
            vec![Some("work".to_owned()), Some("home".to_owned())],
            "switching profile must switch which credential is used"
        );
    }

    #[tokio::test]
    async fn a_profile_with_no_account_polls_without_one() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            Vec::new(),
        ));
        let engine = engine(vec![github.clone()]);

        engine.refresh(&profile(&["github"])).await;

        assert_eq!(github.accounts(), vec![None]);
    }

    #[tokio::test]
    async fn a_refresh_merges_every_source_in_priority_order() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "142",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
        ));
        let slack = Arc::new(FakeSource::new(
            source_id("slack"),
            SourceCategory::Messaging,
            vec![item(
                "slack",
                "D1",
                SourceCategory::Messaging,
                ItemState::Unread,
            )],
        ));
        let herdr = Arc::new(FakeSource::new(
            source_id("herdr"),
            SourceCategory::AgentState,
            vec![item(
                "herdr",
                "s1",
                SourceCategory::AgentState,
                ItemState::AgentWaiting,
            )],
        ));

        let feed = engine(vec![github, slack, herdr])
            .refresh(&profile(&[]))
            .await;

        let ids: Vec<_> = feed
            .items()
            .iter()
            .map(|i| i.id().as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["herdr:s1", "github:142", "slack:D1"]);
        assert_eq!(feed.refreshed_at(), NOW);
        assert!(feed.problems().is_empty());
    }

    #[tokio::test]
    async fn one_broken_source_does_not_cost_us_the_others() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "142",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
        ));
        let jira = Arc::new(FakeSource::failing(
            source_id("jira"),
            SourceCategory::IssueTracking,
            PortError::unavailable("jira", "503"),
        ));

        let feed = engine(vec![github, jira]).refresh(&profile(&[])).await;

        assert_eq!(feed.items().len(), 1);
        assert_eq!(feed.problems().len(), 1);
        assert_eq!(feed.problems()[0].source.as_str(), "jira");
        assert!(feed.problems()[0].error.is_transient());
    }

    #[tokio::test]
    async fn a_poll_keeps_each_sources_answer_apart() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "142",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
        ));
        let jira = Arc::new(FakeSource::failing(
            source_id("jira"),
            SourceCategory::IssueTracking,
            PortError::unavailable("jira", "503"),
        ));

        let polled = engine(vec![github, jira]).poll(&profile(&[])).await;

        let answers: Vec<_> = polled
            .iter()
            .map(|polled| {
                (
                    polled.source.as_str().to_owned(),
                    polled.outcome.as_ref().map(Vec::len).ok(),
                )
            })
            .collect();
        assert_eq!(
            answers,
            vec![("github".to_owned(), Some(1)), ("jira".to_owned(), None)]
        );
    }

    #[tokio::test]
    async fn a_profile_only_polls_the_sources_it_wants() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "1",
                SourceCategory::CodeReview,
                ItemState::NeedsReview,
            )],
        ));
        let slack = Arc::new(FakeSource::new(
            source_id("slack"),
            SourceCategory::Messaging,
            vec![item(
                "slack",
                "D1",
                SourceCategory::Messaging,
                ItemState::Unread,
            )],
        ));

        let feed = engine(vec![github.clone(), slack.clone()])
            .refresh(&profile(&["github"]))
            .await;

        assert_eq!(feed.items().len(), 1);
        assert_eq!(github.fetch_count(), 1);
        assert_eq!(slack.fetch_count(), 0);
    }

    #[test]
    fn a_feed_view_starts_on_the_most_urgent_row() {
        let view = FeedView::new(Feed::new(
            vec![
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));
        assert_eq!(
            view.selected().expect("selection").id().as_str(),
            "github:1"
        );
        assert_eq!(view.cursor(), 0);
        assert_eq!(view.visible().len(), 2);
    }

    #[test]
    fn an_empty_feed_has_no_selection() {
        let view = FeedView::new(Feed::empty(NOW));
        assert!(view.selected().is_none());
        assert!(view.visible().is_empty());
    }

    #[test]
    fn navigation_moves_the_cursor_and_stops_at_the_ends() {
        let mut view = FeedView::new(Feed::new(
            vec![
                item("a", "1", SourceCategory::CodeReview, ItemState::CiFailed),
                item("b", "2", SourceCategory::CodeReview, ItemState::NeedsReview),
                item("c", "3", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));
        view.select_next();
        assert_eq!(view.cursor(), 1);
        view.select_last();
        assert_eq!(view.cursor(), 2);
        view.select_next();
        assert_eq!(view.cursor(), 2, "the cursor does not run off the end");
        view.select_previous();
        assert_eq!(view.cursor(), 1);
        view.select_first();
        assert_eq!(view.cursor(), 0);
        view.select_previous();
        assert_eq!(view.cursor(), 0, "the cursor does not run off the start");
    }

    #[test]
    fn paging_moves_by_a_page_and_clamps() {
        let items: Vec<_> = (0..25)
            .map(|index| {
                item(
                    "github",
                    &index.to_string(),
                    SourceCategory::CodeReview,
                    ItemState::NeedsReview,
                )
            })
            .collect();
        let mut view = FeedView::new(Feed::new(items, Vec::new(), NOW));
        view.page_down(10);
        assert_eq!(view.cursor(), 10);
        view.page_down(100);
        assert_eq!(view.cursor(), 24);
        view.page_up(5);
        assert_eq!(view.cursor(), 19);
        view.page_up(100);
        assert_eq!(view.cursor(), 0);
    }

    #[test]
    fn a_filter_narrows_the_visible_rows() {
        let mut view = FeedView::new(Feed::new(
            vec![
                item(
                    "github",
                    "142",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));
        view.set_filter("slack");
        assert_eq!(view.visible().len(), 1);
        assert_eq!(
            view.selected().expect("selection").id().as_str(),
            "slack:D1"
        );
        assert_eq!(view.filter(), "slack");

        view.clear_filter();
        assert_eq!(view.visible().len(), 2);
    }

    #[test]
    fn a_filter_that_matches_nothing_leaves_no_selection() {
        let mut view = FeedView::new(Feed::new(
            vec![item(
                "github",
                "1",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
            Vec::new(),
            NOW,
        ));
        view.set_filter("nothing here");
        assert!(view.visible().is_empty());
        assert!(view.selected().is_none());
    }

    #[test]
    fn a_refresh_keeps_the_developer_on_the_row_they_were_reading() {
        let mut view = FeedView::new(Feed::new(
            vec![
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));
        view.select_next();
        let before = view.selected().expect("selection").id().clone();

        view.replace_feed(Feed::new(
            vec![
                item(
                    "herdr",
                    "s1",
                    SourceCategory::AgentState,
                    ItemState::AgentWaiting,
                ),
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));

        assert_eq!(view.selected().expect("selection").id(), &before);
        assert_eq!(view.cursor(), 2);
    }

    #[test]
    fn a_refresh_that_drops_the_selected_row_falls_back_to_the_top() {
        let mut view = FeedView::new(Feed::new(
            vec![
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        ));
        view.select_last();
        view.replace_feed(Feed::new(
            vec![item(
                "github",
                "1",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
            Vec::new(),
            NOW,
        ));
        assert_eq!(view.cursor(), 0);
        assert_eq!(
            view.selected().expect("selection").id().as_str(),
            "github:1"
        );
    }

    #[test]
    fn a_feed_counts_what_still_wants_attention() {
        let feed = Feed::new(
            vec![
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item(
                    "herdr",
                    "s1",
                    SourceCategory::AgentState,
                    ItemState::AgentDone,
                ),
            ],
            Vec::new(),
            NOW,
        );
        assert_eq!(feed.actionable_count(), 1);
        assert_eq!(feed.len(), 2);
        assert!(!feed.is_empty());
    }

    #[test]
    fn a_feed_groups_counts_by_category_for_the_header() {
        let feed = Feed::new(
            vec![
                item(
                    "github",
                    "1",
                    SourceCategory::CodeReview,
                    ItemState::CiFailed,
                ),
                item(
                    "github",
                    "2",
                    SourceCategory::CodeReview,
                    ItemState::NeedsReview,
                ),
                item("slack", "D1", SourceCategory::Messaging, ItemState::Unread),
            ],
            Vec::new(),
            NOW,
        );
        let counts = feed.counts_by_category();
        assert_eq!(counts.get(&SourceCategory::CodeReview), Some(&2));
        assert_eq!(counts.get(&SourceCategory::Messaging), Some(&1));
        assert_eq!(counts.get(&SourceCategory::Calendar), None);
    }
}
