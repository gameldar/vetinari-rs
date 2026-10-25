//! `RunMirror`: a current graph for every run the daemon owns, from one
//! fetch and the event stream after it (design 9.1, engine spec 7.3).
//!
//! The mirror decides what to fetch and when it is due from `Clock::now()`;
//! the runtime decides when to ask, and owns the stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use time::{Duration, OffsetDateTime};

use super::ports::RunGateway;
use crate::application::ports::{Clock, PortResult};
use crate::domain::devflow::ids::RunId;
use crate::domain::devflow::run_view::{Applied, NodeChange, RunEvent, RunGraph, apply};

/// How long a refetch waits for more reasons to happen, so a burst of
/// missed events costs one fetch.
pub const DEBOUNCE: Duration = Duration::milliseconds(250);

/// How long a failed fetch waits before it is due again.
pub const RETRY: Duration = Duration::seconds(1);

/// What an event meant to the mirror.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// A watched run whose graph the event moved.
    pub changed: Option<RunId>,
    /// A watched run the event says may be over: its leases are worth a
    /// heartbeat now, whose answer is the authority (design 7.3).
    pub ending: Option<RunId>,
}

#[derive(Debug, Default)]
struct Watched {
    graph: Option<RunGraph>,
    buffered: Vec<RunEvent>,
    refetch_at: Option<OffsetDateTime>,
}

/// The graphs of the runs being watched.
pub struct RunMirror {
    runs: Arc<dyn RunGateway>,
    clock: Arc<dyn Clock>,
    watched: BTreeMap<RunId, Watched>,
}

impl RunMirror {
    /// A mirror watching nothing.
    #[must_use]
    pub fn new(runs: Arc<dyn RunGateway>, clock: Arc<dyn Clock>) -> Self {
        Self {
            runs,
            clock,
            watched: BTreeMap::new(),
        }
    }

    /// Start watching a run: its graph is due at once. Subscribe to the
    /// stream first, so nothing falls between.
    pub fn watch(&mut self, run: RunId) {
        let now = self.clock.now();
        self.watched.entry(run).or_default().refetch_at = Some(now);
    }

    /// Stop watching a run.
    pub fn forget(&mut self, run: &RunId) {
        self.watched.remove(run);
    }

    /// A watched run's graph, once fetched.
    #[must_use]
    pub fn graph(&self, run: &RunId) -> Option<&RunGraph> {
        self.watched
            .get(run)
            .and_then(|watched| watched.graph.as_ref())
    }

    /// Take one event from the stream.
    pub fn observe(&mut self, event: &RunEvent) -> Observed {
        let Some((run, _)) = event.position() else {
            self.reconnected();
            return Observed::default();
        };
        let due = self.clock.now() + DEBOUNCE;
        let Some(watched) = self.watched.get_mut(run) else {
            return Observed::default();
        };
        let ending = matches!(
            event,
            RunEvent::Node {
                change: NodeChange::Cancelled,
                ..
            }
        ) || matches!(event, RunEvent::Run { status, .. } if status.is_finished());
        let mut observed = Observed {
            changed: None,
            ending: ending.then(|| run.clone()),
        };
        match watched.graph.as_mut() {
            None => watched.buffered.push(event.clone()),
            Some(graph) => match apply(graph, event) {
                Applied::Updated => observed.changed = Some(run.clone()),
                Applied::Already => {}
                Applied::Refetch => watched.schedule(due),
            },
        }
        observed
    }

    /// The stream was reopened, and may have missed anything: every graph is
    /// due again.
    pub fn reconnected(&mut self) {
        let due = self.clock.now() + DEBOUNCE;
        for watched in self.watched.values_mut() {
            watched.schedule(due);
        }
    }

    /// The runs whose graph is due now.
    #[must_use]
    pub fn refetches_due(&self) -> Vec<RunId> {
        let now = self.clock.now();
        self.watched
            .iter()
            .filter(|(_, watched)| watched.refetch_at.is_some_and(|at| at <= now))
            .map(|(run, _)| run.clone())
            .collect()
    }

    /// When the next refetch falls due, for the runtime to wait until.
    #[must_use]
    pub fn next_due(&self) -> Option<OffsetDateTime> {
        self.watched
            .values()
            .filter_map(|watched| watched.refetch_at)
            .min()
    }

    /// Fetch a run's graph and apply what was buffered above it. Returns
    /// whether the held graph changed: one older than the graph held is
    /// discarded, so racing fetches cannot move the view backwards.
    pub async fn refetch(&mut self, run: &RunId) -> PortResult<bool> {
        if let Some(watched) = self.watched.get_mut(run) {
            watched.refetch_at = None;
        }
        let fetched = self.runs.graph(run).await;
        let now = self.clock.now();
        let Some(watched) = self.watched.get_mut(run) else {
            return fetched.map(|_| false);
        };
        let mut graph = match fetched {
            Ok(graph) => graph,
            Err(error) => {
                watched.schedule(now + RETRY);
                return Err(error);
            }
        };
        if watched
            .graph
            .as_ref()
            .is_some_and(|held| held.seq > graph.seq)
        {
            return Ok(false);
        }
        let mut buffered = std::mem::take(&mut watched.buffered);
        buffered.sort_by_key(|event| event.position().map(|(_, seq)| seq));
        for event in &buffered {
            if apply(&mut graph, event) == Applied::Refetch {
                watched.schedule(now + DEBOUNCE);
                break;
            }
        }
        watched.graph = Some(graph);
        Ok(true)
    }
}

impl Watched {
    /// Make a refetch due at `at`, unless one is due sooner.
    fn schedule(&mut self, at: OffsetDateTime) {
        self.refetch_at = Some(self.refetch_at.map_or(at, |due| due.min(at)));
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use time::macros::datetime;

    use super::*;
    use crate::domain::devflow::ids::NodeInstance;
    use crate::domain::devflow::run_view::{InstanceState, InstanceStatus, RunStatus};
    use crate::testing::{FakeRunGateway, FixedClock};

    const NOW: OffsetDateTime = datetime!(2026-10-25 10:00 UTC);

    struct Harness {
        mirror: RunMirror,
        runs: Arc<FakeRunGateway>,
        clock: Arc<FixedClock>,
    }

    fn harness() -> Harness {
        let runs = Arc::new(FakeRunGateway::default());
        let clock = Arc::new(FixedClock::new(NOW));
        Harness {
            mirror: RunMirror::new(runs.clone(), clock.clone()),
            runs,
            clock,
        }
    }

    fn run() -> RunId {
        RunId::parse("01JB7ZR2M4Q9V8C6T3N5HKD0WX").expect("run")
    }

    fn other() -> RunId {
        RunId::parse("01JB80A1B2").expect("run")
    }

    fn instance(raw: &str) -> NodeInstance {
        NodeInstance::parse(raw).expect("instance")
    }

    fn graph_of(run: RunId, seq: u64) -> RunGraph {
        RunGraph {
            run,
            status: RunStatus::Active,
            seq,
            nodes: Vec::new(),
            edges: Vec::new(),
            instances: vec![InstanceState {
                instance: instance("implement@1"),
                status: InstanceStatus::Ready,
                attempt: None,
                first_ready_at: None,
                finished_at: None,
            }],
        }
    }

    fn graph(seq: u64) -> RunGraph {
        graph_of(run(), seq)
    }

    fn event_for(run: RunId, seq: u64, change: NodeChange) -> RunEvent {
        RunEvent::Node {
            run,
            instance: instance("implement@1"),
            attempt: Some(NonZeroU32::MIN),
            at: NOW,
            seq,
            change,
        }
    }

    fn event(seq: u64, change: NodeChange) -> RunEvent {
        event_for(run(), seq, change)
    }

    fn status(h: &Harness) -> InstanceStatus {
        h.mirror.graph(&run()).expect("graph").instances[0]
            .status
            .clone()
    }

    async fn watched_at(seq: u64) -> Harness {
        let mut h = harness();
        h.runs.set_graph(graph(seq));
        h.mirror.watch(run());
        h.mirror.refetch(&run()).await.expect("fetch");
        h
    }

    #[tokio::test]
    async fn a_watched_run_is_fetched_at_once() {
        let mut h = harness();
        h.runs.set_graph(graph(3));
        h.mirror.watch(run());
        assert_eq!(h.mirror.refetches_due(), [run()]);
        assert_eq!(h.mirror.graph(&run()), None);

        assert!(h.mirror.refetch(&run()).await.expect("fetch"));
        assert_eq!(h.mirror.graph(&run()).map(|graph| graph.seq), Some(3));
        assert!(h.mirror.refetches_due().is_empty());
        assert_eq!(h.runs.fetched(), [run().to_string()]);
    }

    #[tokio::test]
    async fn events_before_the_graph_are_applied_above_its_seq() {
        let mut h = harness();
        h.runs.set_graph(graph(5));
        h.mirror.watch(run());
        for (seq, change) in [
            (4, NodeChange::Dispatched),
            (6, NodeChange::Dispatched),
            (5, NodeChange::Ready),
        ] {
            assert_eq!(h.mirror.observe(&event(seq, change)), Observed::default());
        }
        h.mirror.refetch(&run()).await.expect("fetch");

        assert_eq!(h.mirror.graph(&run()).map(|graph| graph.seq), Some(6));
        assert_eq!(status(&h), InstanceStatus::Running);
    }

    #[tokio::test]
    async fn the_next_event_moves_a_held_graph() {
        let mut h = watched_at(5).await;
        let observed = h.mirror.observe(&event(6, NodeChange::Dispatched));
        assert_eq!(observed.changed, Some(run()));
        assert_eq!(status(&h), InstanceStatus::Running);
        assert!(h.mirror.refetches_due().is_empty());
    }

    #[tokio::test]
    async fn a_gap_schedules_one_debounced_refetch() {
        let mut h = watched_at(5).await;
        h.mirror.observe(&event(7, NodeChange::Dispatched));
        h.mirror.observe(&event(8, NodeChange::Cancelled));

        assert!(h.mirror.refetches_due().is_empty());
        assert_eq!(h.mirror.next_due(), Some(NOW + DEBOUNCE));
        h.clock.advance(DEBOUNCE);
        assert_eq!(h.mirror.refetches_due(), [run()]);
    }

    #[tokio::test]
    async fn a_resync_or_a_reconnect_refetches_every_run() {
        let mut h = watched_at(5).await;
        h.runs.set_graph(graph_of(other(), 2));
        h.mirror.watch(other());
        h.mirror.refetch(&other()).await.expect("fetch");

        h.mirror.observe(&RunEvent::Resync);
        h.clock.advance(DEBOUNCE);
        assert_eq!(h.mirror.refetches_due(), [run(), other()]);

        for run in [run(), other()] {
            h.mirror.refetch(&run).await.expect("fetch");
        }
        h.mirror.reconnected();
        h.clock.advance(DEBOUNCE);
        assert_eq!(h.mirror.refetches_due().len(), 2);
    }

    #[tokio::test]
    async fn a_fetched_graph_older_than_the_one_held_is_discarded() {
        let mut h = watched_at(5).await;
        h.mirror.observe(&event(6, NodeChange::Dispatched));
        assert!(!h.mirror.refetch(&run()).await.expect("fetch"));
        assert_eq!(h.mirror.graph(&run()).map(|graph| graph.seq), Some(6));
    }

    #[tokio::test]
    async fn a_cancel_or_a_run_ending_is_a_hint_to_heartbeat() {
        let mut h = watched_at(5).await;
        assert_eq!(
            h.mirror.observe(&event(6, NodeChange::Cancelled)).ending,
            Some(run())
        );
        let ended = h.mirror.observe(&RunEvent::Run {
            run: run(),
            seq: 7,
            status: RunStatus::Cancelled,
        });
        assert_eq!(ended.ending, Some(run()));
        assert_eq!(
            h.mirror.observe(&event(8, NodeChange::Dispatched)).ending,
            None
        );
    }

    #[tokio::test]
    async fn events_for_runs_not_watched_are_ignored() {
        let mut h = watched_at(5).await;
        let observed = h
            .mirror
            .observe(&event_for(other(), 1, NodeChange::Cancelled));
        assert_eq!(observed, Observed::default());
        assert!(h.mirror.refetches_due().is_empty());
    }

    #[tokio::test]
    async fn a_fetch_that_fails_is_due_again_after_a_pause() {
        let mut h = harness();
        h.mirror.watch(run());
        assert!(h.mirror.refetch(&run()).await.is_err());
        assert!(h.mirror.refetches_due().is_empty());
        assert_eq!(h.mirror.next_due(), Some(NOW + RETRY));
    }

    #[tokio::test]
    async fn a_forgotten_run_is_neither_held_nor_due() {
        let mut h = watched_at(5).await;
        h.mirror.reconnected();
        h.mirror.forget(&run());
        h.clock.advance(DEBOUNCE);
        assert_eq!(h.mirror.graph(&run()), None);
        assert!(h.mirror.refetches_due().is_empty());
        assert_eq!(h.mirror.next_due(), None);
    }
}
