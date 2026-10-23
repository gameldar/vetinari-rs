//! The queue as the developer reads it: what they are on, what to do next,
//! and what is held back and why.

use time::OffsetDateTime;

use super::lifecycle::{Lifecycle, ParkUntil};
use super::{Entry, Queue, mutes};
use crate::domain::attention::Priority;

/// How far an item that came back is lifted: enough to pass others in the
/// same state, never enough to pass a state a tier more urgent.
const RETURN_BOOST: u32 = 200;

/// An inbox item with the ranking that placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ranked<'a> {
    /// The item.
    pub entry: &'a Entry,
    /// Where it ranks; higher comes first.
    pub priority: Priority,
}

/// Every item, in the lane it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView<'a> {
    /// What the developer is working on.
    pub now: Option<&'a Entry>,
    /// What needs them, most urgent first. The top is what to do next.
    pub next: Vec<Ranked<'a>>,
    /// Inbox items that need nothing yet, such as work still in flight.
    pub quiet: Vec<&'a Entry>,
    /// Inbox items focus time is holding back.
    pub held_for_focus: Vec<&'a Entry>,
    /// Items waiting on someone or something.
    pub waiting: Vec<&'a Entry>,
    /// Parked items, soonest back first.
    pub parked: Vec<&'a Entry>,
    /// Closed items, most recent first.
    pub done: Vec<&'a Entry>,
}

impl Queue {
    /// Sort every item into its lane, as of `now`.
    #[must_use]
    pub fn view(&self, now: OffsetDateTime) -> QueueView<'_> {
        let mut view = QueueView {
            now: self.active.as_ref().map(|active| &active.entry),
            next: Vec::new(),
            quiet: Vec::new(),
            held_for_focus: Vec::new(),
            waiting: Vec::new(),
            parked: Vec::new(),
            done: Vec::new(),
        };
        let mut parked = Vec::new();
        let mut done = Vec::new();

        for (entry, lifecycle) in self.held.values() {
            match lifecycle {
                Lifecycle::Inbox if mutes(self.focus.as_ref(), entry) => {
                    view.held_for_focus.push(entry);
                }
                Lifecycle::Inbox if entry.item.state().needs_you() => view.next.push(Ranked {
                    entry,
                    priority: rank(entry, now),
                }),
                Lifecycle::Inbox => view.quiet.push(entry),
                Lifecycle::Waiting(_) => view.waiting.push(entry),
                Lifecycle::Parked { until, .. } => {
                    let back = match until {
                        ParkUntil::Time(at) => (false, *at),
                        ParkUntil::Changed => (true, OffsetDateTime::UNIX_EPOCH),
                    };
                    parked.push((back, entry));
                }
                Lifecycle::Done { at, .. } => done.push((*at, entry)),
            }
        }

        view.next.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.entry.item.id().cmp(right.entry.item.id()))
        });
        parked.sort_by_key(|(back, _)| *back);
        view.parked = parked.into_iter().map(|(_, entry)| entry).collect();
        done.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        view.done = done.into_iter().map(|(_, entry)| entry).collect();
        view
    }
}

fn rank(entry: &Entry, now: OffsetDateTime) -> Priority {
    let priority = entry.item.priority(now);
    if entry.returned.is_some() {
        priority.raised(RETURN_BOOST)
    } else {
        priority
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attention::{AttentionItem, ItemState, SourceCategory};
    use crate::domain::ids::{ItemId, SourceId};
    use crate::domain::queue::rules::QueueRules;
    use crate::domain::queue::{Decision, ParkFor};
    use crate::domain::text::NonEmptyText;
    use time::Duration;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-22 09:00 UTC);

    fn source(name: &str) -> SourceId {
        SourceId::parse(name).expect("slug")
    }

    fn report(
        source_name: &str,
        native: &str,
        category: SourceCategory,
        state: ItemState,
    ) -> AttentionItem {
        AttentionItem::builder(
            &source(source_name),
            native,
            category,
            NonEmptyText::parse(&format!("item {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(NOW - Duration::hours(1))
        .build()
    }

    fn pr(native: &str, state: ItemState) -> AttentionItem {
        report("github", native, SourceCategory::CodeReview, state)
    }

    fn id(source_name: &str, native: &str) -> ItemId {
        ItemId::from_parts(&source(source_name), native)
    }

    fn ids(entries: &[&Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.item().id().to_string())
            .collect()
    }

    fn next_ids(view: &QueueView<'_>) -> Vec<String> {
        view.next
            .iter()
            .map(|ranked| ranked.entry.item().id().to_string())
            .collect()
    }

    fn queue_of(items: Vec<AttentionItem>) -> Queue {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(NOW, &source("github"), items);
        queue
    }

    fn decide(queue: &mut Queue, decision: Decision) {
        queue.decide(NOW, decision).expect("decision accepted");
    }

    #[test]
    fn next_is_ranked_most_urgent_first() {
        let queue = queue_of(vec![
            pr("review", ItemState::NeedsReview),
            pr("ci", ItemState::CiFailed),
            pr("assigned", ItemState::Assigned),
        ]);

        assert_eq!(
            next_ids(&queue.view(NOW)),
            ["github:ci", "github:review", "github:assigned"]
        );
    }

    #[test]
    fn an_item_that_came_back_outranks_one_in_the_same_state() {
        let mut queue = queue_of(vec![
            pr("a", ItemState::NeedsReview),
            pr("b", ItemState::NeedsReview),
        ]);
        decide(&mut queue, Decision::Finish(id("github", "b")));
        let pushed = AttentionItem::builder(
            &source("github"),
            "b",
            SourceCategory::CodeReview,
            NonEmptyText::parse("item b").expect("title"),
        )
        .state(ItemState::NeedsReview)
        .occurred_at(NOW)
        .build();
        queue.observe(
            NOW,
            &source("github"),
            vec![pr("a", ItemState::NeedsReview), pushed],
        );

        assert_eq!(next_ids(&queue.view(NOW)), ["github:b", "github:a"]);
    }

    #[test]
    fn items_that_need_nothing_yet_are_quiet() {
        let queue = queue_of(vec![
            pr("ci", ItemState::CiRunning),
            pr("review", ItemState::NeedsReview),
        ]);
        let view = queue.view(NOW);

        assert_eq!(next_ids(&view), ["github:review"]);
        assert_eq!(ids(&view.quiet), ["github:ci"]);
    }

    #[test]
    fn the_active_item_is_shown_on_its_own() {
        let mut queue = queue_of(vec![
            pr("review", ItemState::NeedsReview),
            pr("ci", ItemState::CiFailed),
        ]);
        decide(&mut queue, Decision::Take(id("github", "review")));
        let view = queue.view(NOW);

        assert_eq!(
            view.now
                .map(|entry| entry.item().id().to_string())
                .as_deref(),
            Some("github:review")
        );
        assert_eq!(next_ids(&view), ["github:ci"]);
    }

    #[test]
    fn held_items_sit_in_their_own_lanes() {
        let mut queue = queue_of(vec![
            pr("waiting", ItemState::NeedsReview),
            pr("parked", ItemState::NeedsReview),
            pr("done", ItemState::NeedsReview),
        ]);
        decide(&mut queue, Decision::WaitOnReply(id("github", "waiting")));
        decide(
            &mut queue,
            Decision::Park(id("github", "parked"), ParkFor::Changed),
        );
        decide(&mut queue, Decision::Finish(id("github", "done")));
        let view = queue.view(NOW);

        assert!(view.next.is_empty());
        assert_eq!(ids(&view.waiting), ["github:waiting"]);
        assert_eq!(ids(&view.parked), ["github:parked"]);
        assert_eq!(ids(&view.done), ["github:done"]);
    }

    #[test]
    fn parked_items_are_ordered_by_when_they_come_back() {
        let mut queue = queue_of(vec![
            pr("changed", ItemState::NeedsReview),
            pr("later", ItemState::NeedsReview),
            pr("soon", ItemState::NeedsReview),
        ]);
        decide(
            &mut queue,
            Decision::Park(id("github", "changed"), ParkFor::Changed),
        );
        decide(
            &mut queue,
            Decision::Park(
                id("github", "later"),
                ParkFor::Until(NOW + Duration::hours(3)),
            ),
        );
        decide(
            &mut queue,
            Decision::Park(
                id("github", "soon"),
                ParkFor::Until(NOW + Duration::hours(1)),
            ),
        );

        assert_eq!(
            ids(&queue.view(NOW).parked),
            ["github:soon", "github:later", "github:changed"]
        );
    }

    #[test]
    fn done_items_are_most_recent_first() {
        let mut queue = queue_of(vec![
            pr("first", ItemState::NeedsReview),
            pr("second", ItemState::NeedsReview),
        ]);
        queue
            .decide(NOW, Decision::Finish(id("github", "first")))
            .expect("finish");
        queue
            .decide(
                NOW + Duration::minutes(5),
                Decision::Finish(id("github", "second")),
            )
            .expect("finish");

        assert_eq!(
            ids(&queue.view(NOW).done),
            ["github:second", "github:first"]
        );
    }

    #[test]
    fn focus_time_holds_muted_items_out_of_the_queue() {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(
            NOW,
            &source("slack"),
            vec![report(
                "slack",
                "dm",
                SourceCategory::Messaging,
                ItemState::Unread,
            )],
        );
        decide(
            &mut queue,
            Decision::StartFocus {
                until: NOW + Duration::hours(1),
            },
        );

        let view = queue.view(NOW);
        assert!(view.next.is_empty());
        assert_eq!(ids(&view.held_for_focus), ["slack:dm"]);

        decide(&mut queue, Decision::EndFocus);
        assert_eq!(next_ids(&queue.view(NOW)), ["slack:dm"]);
    }
}
