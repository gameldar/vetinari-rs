//! The attention queue as a service: polls go in, decisions go in, and a
//! view of what needs the developer next comes out.

use std::collections::VecDeque;

use time::OffsetDateTime;

use super::attention::{Polled, SourceProblem};
use crate::domain::ids::ItemId;
use crate::domain::queue::rules::QueueRules;
use crate::domain::queue::view::QueueView;
use crate::domain::queue::{Decision, Entry, Happened, Note, Queue, Refusal};

/// Something that moved an item, as the developer would want it recalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentAction {
    /// When.
    pub at: OffsetDateTime,
    /// The item it happened to.
    pub item: ItemId,
    /// The item's title at the time, so it reads the same once the item is gone.
    pub title: String,
    /// What happened.
    pub happened: Happened,
}

/// Owns the queue, what the last poll could not reach, and what recently
/// moved items.
#[derive(Debug, Clone)]
pub struct QueueService {
    queue: Queue,
    problems: Vec<SourceProblem>,
    recent: VecDeque<RecentAction>,
}

impl QueueService {
    /// How many recent actions are kept.
    pub const RECENT_KEPT: usize = 50;

    /// An empty queue under some rules.
    #[must_use]
    pub const fn new(rules: QueueRules) -> Self {
        Self {
            queue: Queue::new(rules),
            problems: Vec::new(),
            recent: VecDeque::new(),
        }
    }

    /// Take in a poll. Only the sources that answered are reconciled, so a
    /// source that is down does not read as everything it reported being
    /// resolved.
    pub fn absorb(&mut self, now: OffsetDateTime, polled: Vec<Polled>) -> Vec<Note> {
        let mut notes = Vec::new();
        self.problems.clear();
        for Polled { source, outcome } in polled {
            match outcome {
                Ok(items) => notes.extend(self.queue.observe(now, &source, items)),
                Err(error) => self.problems.push(SourceProblem { source, error }),
            }
        }
        self.remember(now, &notes);
        notes
    }

    /// Bring back whatever time has caught up with.
    pub fn tick(&mut self, now: OffsetDateTime) -> Vec<Note> {
        let notes = self.queue.tick(now);
        self.remember(now, &notes);
        notes
    }

    /// Apply a decision.
    pub fn decide(
        &mut self,
        now: OffsetDateTime,
        decision: Decision,
    ) -> Result<Vec<Note>, Refusal> {
        let notes = self.queue.decide(now, decision)?;
        self.remember(now, &notes);
        Ok(notes)
    }

    /// What recently moved items, newest first.
    pub fn recent(&self) -> impl Iterator<Item = &RecentAction> {
        self.recent.iter().rev()
    }

    fn remember(&mut self, now: OffsetDateTime, notes: &[Note]) {
        for note in notes.iter().filter(|note| note.happened.moves_it()) {
            let title = self.queue.entry(&note.item).map_or_else(
                || note.item.to_string(),
                |entry| entry.item().title().single_line(),
            );
            self.recent.push_back(RecentAction {
                at: now,
                item: note.item.clone(),
                title,
                happened: note.happened,
            });
            if self.recent.len() > Self::RECENT_KEPT {
                self.recent.pop_front();
            }
        }
    }

    /// The queue, laid out for a client.
    #[must_use]
    pub fn view(&self, now: OffsetDateTime) -> QueueView<'_> {
        self.queue.view(now)
    }

    /// The queue itself.
    #[must_use]
    pub const fn queue(&self) -> &Queue {
        &self.queue
    }

    /// The sources the last poll could not reach.
    #[must_use]
    pub fn problems(&self) -> &[SourceProblem] {
        &self.problems
    }
}

/// Which lane of the queue a row sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    /// What the developer is working on.
    Now,
    /// What needs them, most urgent first.
    Next,
    /// Handed to someone or something else.
    Waiting,
    /// Set aside until something brings it back.
    Parked,
    /// Held back by focus time.
    HeldForFocus,
    /// Needs nothing yet.
    Quiet,
    /// Closed.
    Done,
}

impl Lane {
    /// The lane's heading.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Lane::Now => "NOW",
            Lane::Next => "NEXT",
            Lane::Waiting => "WAITING",
            Lane::Parked => "PARKED",
            Lane::HeldForFocus => "HELD FOR FOCUS",
            Lane::Quiet => "QUIET",
            Lane::Done => "DONE",
        }
    }
}

/// One item, in its lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<'a> {
    /// The lane.
    pub lane: Lane,
    /// The item and the queue's bookkeeping for it.
    pub entry: &'a Entry,
}

/// Every item in the view, lane by lane, with nothing left out.
#[must_use]
pub fn lanes<'a>(view: &QueueView<'a>) -> Vec<Row<'a>> {
    let row = |lane| move |entry| Row { lane, entry };
    view.now
        .into_iter()
        .map(row(Lane::Now))
        .chain(
            view.next
                .iter()
                .map(|ranked| ranked.entry)
                .map(row(Lane::Next)),
        )
        .chain(view.waiting.iter().copied().map(row(Lane::Waiting)))
        .chain(view.parked.iter().copied().map(row(Lane::Parked)))
        .chain(
            view.held_for_focus
                .iter()
                .copied()
                .map(row(Lane::HeldForFocus)),
        )
        .chain(view.quiet.iter().copied().map(row(Lane::Quiet)))
        .chain(view.done.iter().copied().map(row(Lane::Done)))
        .collect()
}

/// A filter and a selection over the queue's rows. The selection is held
/// by item, so it follows the item from lane to lane as the queue moves.
#[derive(Debug, Clone, Default)]
pub struct QueueCursor {
    filter: String,
    selected: Option<ItemId>,
    place: usize,
}

impl QueueCursor {
    /// How many closed items are shown.
    pub const DONE_SHOWN: usize = 5;

    /// The rows the filter admits, with only the latest closed items.
    #[must_use]
    pub fn rows<'a>(&self, view: &QueueView<'a>) -> Vec<Row<'a>> {
        let mut done = 0;
        lanes(view)
            .into_iter()
            .filter(|row| row.entry.item().matches(&self.filter))
            .filter(|row| {
                if row.lane != Lane::Done {
                    return true;
                }
                done += 1;
                done <= Self::DONE_SHOWN
            })
            .collect()
    }

    /// Which row is selected.
    #[must_use]
    pub fn index(&self, view: &QueueView<'_>) -> Option<usize> {
        self.index_in(&self.rows(view))
    }

    /// The selected row.
    #[must_use]
    pub fn selected<'a>(&self, view: &QueueView<'a>) -> Option<Row<'a>> {
        let rows = self.rows(view);
        self.index_in(&rows).map(|index| rows[index])
    }

    /// Move the selection by some rows, stopping at either end.
    pub fn move_by(&mut self, view: &QueueView<'_>, by: isize) {
        let rows = self.rows(view);
        let Some(index) = self.index_in(&rows) else {
            return;
        };
        self.select_in(&rows, index.saturating_add_signed(by));
    }

    /// Select the first row.
    pub fn select_first(&mut self, view: &QueueView<'_>) {
        self.select_in(&self.rows(view), 0);
    }

    /// Select the last row.
    pub fn select_last(&mut self, view: &QueueView<'_>) {
        self.select_in(&self.rows(view), usize::MAX);
    }

    /// Select an item, if it is among the rows.
    pub fn select_item(&mut self, view: &QueueView<'_>, id: &ItemId) {
        let rows = self.rows(view);
        if let Some(index) = rows.iter().position(|row| row.entry.item().id() == id) {
            self.select_in(&rows, index);
        }
    }

    /// Stop following the selected item and stay on its row instead, so
    /// that acting on an item moves on to the one after it.
    pub fn keep_place(&mut self, view: &QueueView<'_>) {
        if let Some(index) = self.index(view) {
            self.place = index;
        }
        self.selected = None;
    }

    /// The filter in force.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Replace the filter.
    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
    }

    /// Drop the filter.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
    }

    fn index_in(&self, rows: &[Row<'_>]) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        self.selected
            .as_ref()
            .and_then(|id| rows.iter().position(|row| row.entry.item().id() == id))
            .or(Some(self.place.min(rows.len() - 1)))
    }

    fn select_in(&mut self, rows: &[Row<'_>], index: usize) {
        let Some(last) = rows.len().checked_sub(1) else {
            return;
        };
        self.place = index.min(last);
        self.selected = Some(rows[self.place].entry.item().id().clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::PortError;
    use crate::domain::attention::{AttentionItem, ItemState, SourceCategory};
    use crate::domain::ids::{ItemId, SourceId};
    use crate::domain::queue::lifecycle::{Closure, Lifecycle};
    use crate::domain::queue::{ParkFor, Position};
    use crate::domain::text::NonEmptyText;
    use time::Duration;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-24 09:00 UTC);

    fn source(name: &str) -> SourceId {
        SourceId::parse(name).expect("slug")
    }

    fn pr(native: &str) -> AttentionItem {
        AttentionItem::builder(
            &source("github"),
            native,
            SourceCategory::CodeReview,
            NonEmptyText::parse(&format!("PR {native}")).expect("title"),
        )
        .state(ItemState::NeedsReview)
        .occurred_at(NOW)
        .build()
    }

    fn answered(name: &str, items: Vec<AttentionItem>) -> Polled {
        Polled {
            source: source(name),
            outcome: Ok(items),
        }
    }

    fn down(name: &str) -> Polled {
        Polled {
            source: source(name),
            outcome: Err(PortError::unavailable(name, "503")),
        }
    }

    fn github(native: &str) -> ItemId {
        ItemId::from_parts(&source("github"), native)
    }

    fn parked_for(service: &mut QueueService, native: &str, minutes: i64) {
        service
            .decide(
                NOW,
                Decision::Park(
                    github(native),
                    ParkFor::Until(NOW + Duration::minutes(minutes)),
                ),
            )
            .expect("parked");
    }

    fn recent(service: &QueueService) -> Vec<(OffsetDateTime, String, Happened)> {
        service
            .recent()
            .map(|action| (action.at, action.title.clone(), action.happened))
            .collect()
    }

    #[test]
    fn what_moved_items_is_kept_newest_first() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1"), pr("2")])]);
        let later = NOW + Duration::minutes(5);

        service
            .decide(NOW, Decision::Take(github("1")))
            .expect("taken");
        service
            .decide(later, Decision::Take(github("2")))
            .expect("taken");

        assert_eq!(
            recent(&service),
            vec![
                (later, "PR 2".to_owned(), Happened::Taken),
                (later, "PR 1".to_owned(), Happened::SwitchedAway),
                (NOW, "PR 1".to_owned(), Happened::Taken),
            ]
        );
    }

    #[test]
    fn arrivals_and_changes_that_move_nothing_are_not_kept() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1")])]);
        parked_for(&mut service, "1", 60);
        let changed = AttentionItem::builder(
            &source("github"),
            "1",
            SourceCategory::CodeReview,
            NonEmptyText::parse("PR 1").expect("title"),
        )
        .state(ItemState::NeedsReview)
        .occurred_at(NOW + Duration::minutes(1))
        .build();

        service.absorb(
            NOW + Duration::minutes(1),
            vec![answered("github", vec![changed])],
        );

        assert_eq!(
            recent(&service),
            vec![(NOW, "PR 1".to_owned(), Happened::Parked)]
        );
    }

    #[test]
    fn only_the_latest_actions_are_kept() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1")])]);

        for minute in 0..=i64::try_from(QueueService::RECENT_KEPT).expect("small") {
            service
                .decide(
                    NOW + Duration::minutes(minute),
                    Decision::Park(github("1"), ParkFor::Changed),
                )
                .expect("parked");
        }

        assert_eq!(service.recent().count(), QueueService::RECENT_KEPT);
    }

    fn row_ids(rows: &[Row<'_>]) -> Vec<(Lane, String)> {
        rows.iter()
            .map(|row| (row.lane, row.entry.item().id().as_str().to_owned()))
            .collect()
    }

    #[test]
    fn the_rows_run_lane_by_lane_from_what_you_are_on() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(
            NOW,
            vec![answered("github", vec![pr("1"), pr("2"), pr("3")])],
        );
        service
            .decide(NOW, Decision::Take(github("2")))
            .expect("taken");
        parked_for(&mut service, "3", 60);

        let rows = QueueCursor::default().rows(&service.view(NOW));

        assert_eq!(
            row_ids(&rows),
            vec![
                (Lane::Now, "github:2".to_owned()),
                (Lane::Next, "github:1".to_owned()),
                (Lane::Parked, "github:3".to_owned()),
            ]
        );
    }

    #[test]
    fn only_the_latest_closed_items_are_rows() {
        let natives: Vec<String> = (0..QueueCursor::DONE_SHOWN + 2)
            .map(|n| n.to_string())
            .collect();
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(
            NOW,
            vec![answered(
                "github",
                natives.iter().map(|native| pr(native)).collect(),
            )],
        );
        for native in &natives {
            service
                .decide(NOW, Decision::Finish(github(native)))
                .expect("finished");
        }

        let rows = QueueCursor::default().rows(&service.view(NOW));

        assert_eq!(rows.len(), QueueCursor::DONE_SHOWN);
    }

    #[test]
    fn the_selection_follows_its_item_as_the_queue_moves() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1"), pr("2")])]);
        let mut cursor = QueueCursor::default();
        cursor.move_by(&service.view(NOW), 1);

        service
            .decide(NOW, Decision::Take(github("2")))
            .expect("taken");

        let view = service.view(NOW);
        let selected = cursor.selected(&view).expect("a selection");
        assert_eq!(selected.entry.item().id(), &github("2"));
        assert_eq!(selected.lane, Lane::Now);
    }

    #[test]
    fn letting_go_of_the_item_keeps_the_place_instead() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(
            NOW,
            vec![answered("github", vec![pr("1"), pr("2"), pr("3")])],
        );
        let mut cursor = QueueCursor::default();
        cursor.move_by(&service.view(NOW), 1);

        cursor.keep_place(&service.view(NOW));
        service
            .decide(NOW, Decision::Finish(github("2")))
            .expect("finished");

        let view = service.view(NOW);
        let selected = cursor.selected(&view).expect("a selection");
        assert_eq!(selected.entry.item().id(), &github("3"));
    }

    #[test]
    fn moving_stops_at_either_end() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1"), pr("2")])]);
        let view = service.view(NOW);
        let mut cursor = QueueCursor::default();

        cursor.move_by(&view, 10);
        assert_eq!(cursor.index(&view), Some(1));
        cursor.move_by(&view, -10);
        assert_eq!(cursor.index(&view), Some(0));
        cursor.select_last(&view);
        assert_eq!(cursor.index(&view), Some(1));
    }

    #[test]
    fn a_filter_narrows_the_rows_and_keeps_the_selection_when_it_can() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1"), pr("22")])]);
        let mut cursor = QueueCursor::default();
        cursor.select_last(&service.view(NOW));

        cursor.set_filter("PR 22");

        let view = service.view(NOW);
        assert_eq!(cursor.rows(&view).len(), 1);
        assert_eq!(
            cursor
                .selected(&view)
                .expect("a selection")
                .entry
                .item()
                .id(),
            &github("22")
        );
        assert_eq!(cursor.filter(), "PR 22");
    }

    #[test]
    fn an_empty_queue_has_no_selection() {
        let service = QueueService::new(QueueRules::default());

        assert!(
            QueueCursor::default()
                .selected(&service.view(NOW))
                .is_none()
        );
    }

    #[test]
    fn what_a_source_reports_joins_the_inbox() {
        let mut service = QueueService::new(QueueRules::default());

        service.absorb(NOW, vec![answered("github", vec![pr("1"), pr("2")])]);

        assert_eq!(service.view(NOW).next.len(), 2);
    }

    #[test]
    fn a_source_that_is_down_keeps_what_it_last_reported() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1")])]);

        service.absorb(NOW + Duration::minutes(1), vec![down("github")]);

        assert_eq!(
            service.queue().position(&github("1")),
            Some(Position::Held(&Lifecycle::Inbox))
        );
    }

    #[test]
    fn a_source_that_stops_reporting_an_item_closes_it() {
        let mut service = QueueService::new(QueueRules::default());
        service.absorb(NOW, vec![answered("github", vec![pr("1")])]);
        let later = NOW + Duration::minutes(1);

        service.absorb(later, vec![answered("github", Vec::new())]);

        assert_eq!(
            service.queue().position(&github("1")),
            Some(Position::Held(&Lifecycle::Done {
                closure: Closure::Upstream,
                at: later
            }))
        );
    }

    #[test]
    fn the_problems_are_those_of_the_latest_poll() {
        let mut service = QueueService::new(QueueRules::default());

        service.absorb(NOW, vec![down("jira"), answered("github", vec![pr("1")])]);
        let first: Vec<_> = service
            .problems()
            .iter()
            .map(|problem| problem.source.as_str().to_owned())
            .collect();
        service.absorb(NOW, vec![answered("jira", Vec::new())]);

        assert_eq!(first, vec!["jira"]);
        assert!(service.problems().is_empty());
    }
}
