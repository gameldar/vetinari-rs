//! A record of every time a rule brought an item back, and what the
//! developer did next, so the rules can be tuned from real use.

use std::collections::BTreeMap;

use time::{Duration, OffsetDateTime};

use super::lifecycle::Rule;
use super::{Happened, Note, Queue};
use crate::domain::ids::ItemId;

/// What the developer did with an item that came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Response {
    /// Worked on it.
    Took,
    /// Finished it.
    Finished,
    /// Silenced it.
    Silenced,
    /// Parked it again.
    Parked,
    /// Waited on a reply to it.
    WaitedOnReply,
    /// Handed it to an agent.
    HandedToAgent,
    /// Waited on its agent.
    WaitedOnAgent,
    /// Nothing, before another rule brought it back again.
    CameBackAgain,
}

impl Response {
    const fn from_decision(happened: Happened) -> Option<Self> {
        match happened {
            Happened::Taken => Some(Self::Took),
            Happened::Finished => Some(Self::Finished),
            Happened::Silenced => Some(Self::Silenced),
            Happened::Parked => Some(Self::Parked),
            Happened::WaitingOnReply => Some(Self::WaitedOnReply),
            Happened::HandedToAgent => Some(Self::HandedToAgent),
            Happened::WaitingOnAgent => Some(Self::WaitedOnAgent),
            _ => None,
        }
    }
}

/// How a return was answered, and how long that took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// What the developer did.
    pub response: Response,
    /// How long after the return they did it.
    pub after: Duration,
}

/// One return to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordEntry {
    /// The item that came back.
    pub item: ItemId,
    /// The rule that brought it back.
    pub rule: Rule,
    /// When.
    pub at: OffsetDateTime,
    /// What happened next, once something has.
    pub outcome: Option<Outcome>,
}

/// One rule's returns, summarised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleTally {
    /// The rule.
    pub rule: Rule,
    /// How many times it brought something back.
    pub fired: usize,
    /// How many of those are still unanswered.
    pub open: usize,
    /// How the answered ones were answered.
    pub responses: BTreeMap<Response, usize>,
    /// The mean time to answer, once any have been.
    pub mean_time_to_act: Option<Duration>,
}

impl Queue {
    /// Every return so far, oldest first.
    #[must_use]
    pub fn record(&self) -> &[RecordEntry] {
        &self.record
    }

    /// The record summarised per rule.
    #[must_use]
    pub fn tuning(&self) -> Vec<RuleTally> {
        let mut tallies: BTreeMap<Rule, (RuleTally, Vec<Duration>)> = BTreeMap::new();
        for entry in &self.record {
            let (tally, waits) = tallies.entry(entry.rule).or_insert_with(|| {
                (
                    RuleTally {
                        rule: entry.rule,
                        fired: 0,
                        open: 0,
                        responses: BTreeMap::new(),
                        mean_time_to_act: None,
                    },
                    Vec::new(),
                )
            });
            tally.fired += 1;
            match entry.outcome {
                None => tally.open += 1,
                Some(outcome) => {
                    *tally.responses.entry(outcome.response).or_insert(0) += 1;
                    waits.push(outcome.after);
                }
            }
        }
        tallies
            .into_values()
            .map(|(mut tally, waits)| {
                tally.mean_time_to_act = u32::try_from(waits.len())
                    .ok()
                    .filter(|count| *count > 0)
                    .map(|count| waits.iter().copied().sum::<Duration>() / count);
                tally
            })
            .collect()
    }

    /// Open an entry for every return and answer the item's open entry with
    /// the developer's next decision, or with the next return if they made
    /// none.
    pub(super) fn keep_record(&mut self, now: OffsetDateTime, notes: &[Note]) {
        for note in notes {
            let response = match note.happened {
                Happened::Resurfaced(_) => Some(Response::CameBackAgain),
                other => Response::from_decision(other),
            };
            if let Some(response) = response
                && let Some(open) = self
                    .record
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.item == note.item && entry.outcome.is_none())
            {
                open.outcome = Some(Outcome {
                    response,
                    after: now - open.at,
                });
            }
            if let Happened::Resurfaced(rule) = note.happened {
                self.record.push(RecordEntry {
                    item: note.item.clone(),
                    rule,
                    at: now,
                    outcome: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attention::{AttentionItem, ItemState, SourceCategory};
    use crate::domain::ids::SourceId;
    use crate::domain::queue::rules::QueueRules;
    use crate::domain::queue::{Decision, ParkFor};
    use crate::domain::text::NonEmptyText;
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
        at: OffsetDateTime,
    ) -> AttentionItem {
        AttentionItem::builder(
            &source(source_name),
            native,
            category,
            NonEmptyText::parse(&format!("item {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(at)
        .build()
    }

    fn id(source_name: &str, native: &str) -> ItemId {
        ItemId::from_parts(&source(source_name), native)
    }

    fn queue_of(natives: &[&str]) -> Queue {
        let mut queue = Queue::new(QueueRules::default());
        queue.observe(
            NOW,
            &source("github"),
            natives
                .iter()
                .map(|native| {
                    report(
                        "github",
                        native,
                        SourceCategory::CodeReview,
                        ItemState::NeedsReview,
                        NOW,
                    )
                })
                .collect(),
        );
        queue
    }

    fn decide(queue: &mut Queue, at: OffsetDateTime, decision: Decision) {
        queue.decide(at, decision).expect("decision accepted");
    }

    fn snooze(queue: &mut Queue, native: &str, minutes: i64) -> OffsetDateTime {
        let until = NOW + Duration::minutes(minutes);
        decide(
            queue,
            NOW,
            Decision::Park(id("github", native), ParkFor::Until(until)),
        );
        until
    }

    #[test]
    fn a_return_is_recorded_with_the_rule_that_caused_it() {
        let mut queue = queue_of(&["41"]);
        let until = snooze(&mut queue, "41", 60);

        queue.tick(until);

        assert_eq!(
            queue.record(),
            [RecordEntry {
                item: id("github", "41"),
                rule: Rule::SnoozeEnded,
                at: until,
                outcome: None,
            }]
        );
    }

    #[test]
    fn the_next_decision_on_the_item_answers_it() {
        let mut queue = queue_of(&["41"]);
        let until = snooze(&mut queue, "41", 60);
        queue.tick(until);

        decide(
            &mut queue,
            until + Duration::minutes(5),
            Decision::Take(id("github", "41")),
        );

        assert_eq!(
            queue.record()[0].outcome,
            Some(Outcome {
                response: Response::Took,
                after: Duration::minutes(5)
            })
        );
    }

    #[test]
    fn decisions_on_items_that_did_not_come_back_record_nothing() {
        let mut queue = queue_of(&["41"]);

        decide(&mut queue, NOW, Decision::Take(id("github", "41")));

        assert!(queue.record().is_empty());
    }

    #[test]
    fn coming_back_again_unanswered_answers_the_earlier_return() {
        let mut queue = queue_of(&["41"]);
        let agent = id("agents", "w9");
        let agent_says = |queue: &mut Queue, state, at| {
            queue.observe(
                at,
                &source("agents"),
                vec![report(
                    "agents",
                    "w9",
                    SourceCategory::AgentState,
                    state,
                    at,
                )],
            );
        };
        agent_says(&mut queue, ItemState::AgentRunning, NOW);
        decide(
            &mut queue,
            NOW,
            Decision::Delegate {
                item: id("github", "41"),
                agent,
            },
        );
        agent_says(
            &mut queue,
            ItemState::AgentWaiting,
            NOW + Duration::minutes(5),
        );

        agent_says(
            &mut queue,
            ItemState::AgentDone,
            NOW + Duration::minutes(20),
        );

        let record = queue.record();
        assert_eq!(record.len(), 2);
        assert_eq!(
            record[0].outcome,
            Some(Outcome {
                response: Response::CameBackAgain,
                after: Duration::minutes(15)
            })
        );
        assert_eq!(record[1].outcome, None);
    }

    #[test]
    fn the_record_is_summarised_per_rule() {
        let mut queue = queue_of(&["a", "b", "c"]);
        for native in ["a", "b", "c"] {
            snooze(&mut queue, native, 60);
        }
        let back = NOW + Duration::minutes(60);
        queue.tick(back);
        decide(
            &mut queue,
            back + Duration::minutes(2),
            Decision::Take(id("github", "a")),
        );
        decide(
            &mut queue,
            back + Duration::minutes(10),
            Decision::Park(id("github", "b"), ParkFor::Changed),
        );

        assert_eq!(
            queue.tuning(),
            [RuleTally {
                rule: Rule::SnoozeEnded,
                fired: 3,
                open: 1,
                responses: BTreeMap::from([(Response::Took, 1), (Response::Parked, 1)]),
                mean_time_to_act: Some(Duration::minutes(6)),
            }]
        );
    }
}
