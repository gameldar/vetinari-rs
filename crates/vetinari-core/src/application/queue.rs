//! The attention queue as a service: polls go in, decisions go in, and a
//! view of what needs the developer next comes out.

use time::OffsetDateTime;

use super::attention::{Polled, SourceProblem};
use crate::domain::queue::rules::QueueRules;
use crate::domain::queue::view::QueueView;
use crate::domain::queue::{Decision, Note, Queue, Refusal};

/// Owns the queue and what the last poll could not reach.
#[derive(Debug, Clone)]
pub struct QueueService {
    queue: Queue,
    problems: Vec<SourceProblem>,
}

impl QueueService {
    /// An empty queue under some rules.
    #[must_use]
    pub const fn new(rules: QueueRules) -> Self {
        Self {
            queue: Queue::new(rules),
            problems: Vec::new(),
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
        notes
    }

    /// Bring back whatever time has caught up with.
    pub fn tick(&mut self, now: OffsetDateTime) -> Vec<Note> {
        self.queue.tick(now)
    }

    /// Apply a decision.
    pub fn decide(
        &mut self,
        now: OffsetDateTime,
        decision: Decision,
    ) -> Result<Vec<Note>, Refusal> {
        self.queue.decide(now, decision)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::PortError;
    use crate::domain::attention::{AttentionItem, ItemState, SourceCategory};
    use crate::domain::ids::{ItemId, SourceId};
    use crate::domain::queue::Position;
    use crate::domain::queue::lifecycle::{Closure, Lifecycle};
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
