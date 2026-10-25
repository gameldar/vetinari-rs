//! The daemon's tokio tasks: here, keeping the run board current from the
//! engine's run stream and graph fetches.
//!
//! The application decides what is due from `Clock::now()`; this module
//! decides when to ask, which is why the clock it hands the application
//! follows tokio's time and its tests run with that time paused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::Instant;
use vetinari_core::application::devflow::api::{RunItem, RunLabel};
use vetinari_core::application::devflow::mirror::RunMirror;
use vetinari_core::application::devflow::ports::{RunEventStream, RunEvents, RunGateway};
use vetinari_core::application::ports::{Clock, PortError};
use vetinari_core::domain::devflow::ids::RunId;
use vetinari_core::domain::devflow::run_view::RunEvent;
use vetinari_core::domain::devflow::summary::summarise;
use vetinari_core::domain::devflow::template::TemplateKind;

use crate::hub::Hub;
use crate::steps::{LocalBoard, Steps};

/// A clock that reads tokio's time from a fixed start, so a test with
/// paused time moves the application's clock with its own.
#[derive(Debug, Clone)]
pub struct TokioClock {
    base: OffsetDateTime,
    start: Instant,
}

impl TokioClock {
    /// A clock reading `base` now.
    #[must_use]
    pub fn starting_at(base: OffsetDateTime) -> Self {
        Self {
            base,
            start: Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> OffsetDateTime {
        let elapsed = Instant::now().duration_since(self.start);
        self.base + elapsed
    }
}

/// A run the daemon owns, and what it knows about it beyond its graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRun {
    /// The run.
    pub run: RunId,
    /// What it is about, when known.
    pub label: Option<RunLabel>,
    /// When it started, when known.
    pub started_at: Option<OffsetDateTime>,
}

/// What `feed_runs` needs.
pub struct Feed {
    /// The run endpoints.
    pub runs: Arc<dyn RunGateway>,
    /// The run stream.
    pub events: Arc<dyn RunEvents>,
    /// The application's clock.
    pub clock: Arc<dyn Clock>,
    /// The template every owned run is of.
    pub template: TemplateKind,
    /// Where the items go.
    pub hub: Arc<Hub>,
    /// What this machine knows of each run's steps.
    pub local: Arc<LocalBoard>,
    /// The worker, told when a run may be over; none when nothing claims.
    pub steps: Option<Steps>,
}

/// The longest wait between attempts to open the stream.
const MAX_BACKOFF: StdDuration = StdDuration::from_secs(60);

/// Keep the hub's items for `owned` current, forever: open the stream, then
/// fetch each graph, then apply events and refetch as the mirror asks. A
/// closed stream is reopened with backoff, and every graph refetched.
pub async fn feed_runs(
    feed: Feed,
    owned: Vec<OwnedRun>,
    mut touched: mpsc::UnboundedReceiver<RunId>,
) {
    let mut owned: BTreeMap<RunId, OwnedRun> = owned
        .into_iter()
        .map(|run| (run.run.clone(), run))
        .collect();
    let mut mirror = RunMirror::new(feed.runs.clone(), feed.clock.clone());
    let mut backoff = MIN_BACKOFF;
    loop {
        let stream = match feed.events.subscribe().await {
            Ok(stream) => {
                backoff = MIN_BACKOFF;
                stream
            }
            Err(error) => {
                tracing::warn!(%error, "cannot open the run stream");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let (sender, mut received) = mpsc::channel(STREAM_BUFFER);
        let reading = tokio::spawn(read(stream, sender));
        mirror.reconnected();
        for run in owned.keys() {
            mirror.watch(run.clone());
        }
        loop {
            let wait = mirror.next_due().map(|due| {
                StdDuration::try_from(due - feed.clock.now()).unwrap_or(StdDuration::ZERO)
            });
            tokio::select! {
                event = received.recv() => {
                    let Some(event) = event else { break };
                    let observed = mirror.observe(&event);
                    if let Some(run) = observed.changed {
                        publish(&feed, &mirror, &owned, &run);
                    }
                    if let (Some(run), Some(steps)) = (observed.ending, &feed.steps) {
                        steps.ending(run);
                    }
                }
                Some(run) = touched.recv() => {
                    if !owned.contains_key(&run) {
                        owned.insert(run.clone(), OwnedRun {
                            run: run.clone(),
                            label: None,
                            started_at: None,
                        });
                        mirror.watch(run.clone());
                    }
                    publish(&feed, &mirror, &owned, &run);
                }
                () = sleep_or_forever(wait) => {
                    for run in mirror.refetches_due() {
                        match mirror.refetch(&run).await {
                            Ok(true) => publish(&feed, &mirror, &owned, &run),
                            Ok(false) => {}
                            Err(PortError::NotFound { .. }) => {
                                mirror.forget(&run);
                                owned.remove(&run);
                                feed.hub.remove(&run);
                            }
                            Err(error) => tracing::warn!(%error, %run, "cannot fetch a run's graph"),
                        }
                    }
                }
            }
        }
        reading.abort();
        tokio::time::sleep(MIN_BACKOFF).await;
    }
}

/// The shortest wait before the stream is opened again.
const MIN_BACKOFF: StdDuration = StdDuration::from_secs(1);

/// How many events may wait for the mirror.
const STREAM_BUFFER: usize = 256;

/// Move a stream's events onto a channel, which `select!` can wait on
/// without losing a half-read event. Ends, closing the channel, when the
/// stream does.
async fn read(mut stream: Box<dyn RunEventStream>, sender: mpsc::Sender<RunEvent>) {
    while let Ok(Some(event)) = stream.next().await {
        if sender.send(event).await.is_err() {
            return;
        }
    }
}

async fn sleep_or_forever(wait: Option<StdDuration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}

fn publish(feed: &Feed, mirror: &RunMirror, owned: &BTreeMap<RunId, OwnedRun>, run: &RunId) {
    let (Some(graph), Some(known)) = (mirror.graph(run), owned.get(run)) else {
        return;
    };
    feed.hub.put_graph(graph.clone());
    feed.hub.put(RunItem {
        summary: summarise(graph, feed.template, &feed.local.get(run)),
        label: known.label.clone(),
        started_at: known.started_at,
    });
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use time::macros::datetime;
    use vetinari_core::application::ports::PortResult;
    use vetinari_core::domain::devflow::ids::{NodeId, NodeInstance, TaskQueue};
    use vetinari_core::domain::devflow::run_view::{
        InstanceState, InstanceStatus, NodeChange, RunGraph, RunNode, RunNodeKind, RunStatus,
    };
    use vetinari_core::domain::devflow::summary::Mark;
    use vetinari_core::domain::devflow::summary::{LocalStep, LocalSteps};
    use vetinari_core::domain::ids::SessionId;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::testing::FakeRunGateway;

    use super::*;
    use crate::steps::Command;

    const START: OffsetDateTime = datetime!(2026-10-25 10:00 UTC);

    /// A run stream fed by the test: each `subscribe` takes the next
    /// receiver, and dropping a sender closes that stream.
    #[derive(Default)]
    struct FakeRunEvents {
        streams: Mutex<Vec<mpsc::UnboundedReceiver<RunEvent>>>,
        opened: Mutex<usize>,
    }

    impl FakeRunEvents {
        fn stream(&self) -> mpsc::UnboundedSender<RunEvent> {
            let (sender, receiver) = mpsc::unbounded_channel();
            self.streams.lock().expect("lock").push(receiver);
            sender
        }

        fn opened(&self) -> usize {
            *self.opened.lock().expect("lock")
        }
    }

    struct FakeStream(mpsc::UnboundedReceiver<RunEvent>);

    #[async_trait]
    impl RunEventStream for FakeStream {
        async fn next(&mut self) -> PortResult<Option<RunEvent>> {
            Ok(self.0.recv().await)
        }
    }

    #[async_trait]
    impl RunEvents for FakeRunEvents {
        async fn subscribe(&self) -> PortResult<Box<dyn RunEventStream>> {
            *self.opened.lock().expect("lock") += 1;
            let mut streams = self.streams.lock().expect("lock");
            if streams.is_empty() {
                return Err(PortError::unavailable("fake", "no stream scripted"));
            }
            Ok(Box::new(FakeStream(streams.remove(0))))
        }
    }

    fn run() -> RunId {
        RunId::parse("01JB7ZR2M4Q9V8C6T3N5HKD0WX").expect("run")
    }

    fn graph(seq: u64, status: InstanceStatus) -> RunGraph {
        RunGraph {
            run: run(),
            status: RunStatus::Active,
            seq,
            nodes: vec![RunNode {
                id: NodeId::parse("open-pr").expect("id"),
                kind: RunNodeKind::External {
                    queue: TaskQueue::parse("vetinari-hex-agent").expect("queue"),
                },
                scope: None,
            }],
            edges: Vec::new(),
            instances: vec![InstanceState {
                instance: NodeInstance::parse("open-pr").expect("instance"),
                status,
                attempt: Some(NonZeroU32::MIN),
                first_ready_at: None,
                finished_at: None,
            }],
        }
    }

    fn owned() -> OwnedRun {
        OwnedRun {
            run: run(),
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: Some(datetime!(2026-10-25 09:00 UTC)),
        }
    }

    struct Harness {
        runs: Arc<FakeRunGateway>,
        events: Arc<FakeRunEvents>,
        hub: Arc<Hub>,
        local: Arc<LocalBoard>,
        touched: Mutex<Option<mpsc::UnboundedReceiver<RunId>>>,
        steps: Steps,
        commands: Mutex<Option<mpsc::Receiver<Command>>>,
    }

    fn start_owning(h: &Harness, owned: Vec<OwnedRun>) {
        let feed = Feed {
            runs: h.runs.clone(),
            events: h.events.clone(),
            clock: Arc::new(TokioClock::starting_at(START)),
            template: TemplateKind::FeatureFromTicket,
            hub: h.hub.clone(),
            local: h.local.clone(),
            steps: Some(h.steps.clone()),
        };
        let touched = h
            .touched
            .lock()
            .expect("lock")
            .take()
            .expect("started once");
        tokio::spawn(feed_runs(feed, owned, touched));
    }

    fn start(h: &Harness) {
        start_owning(h, vec![owned()]);
    }

    fn harness() -> Harness {
        let (local, touched) = LocalBoard::new();
        let (steps, commands) = Steps::detached();
        Harness {
            runs: Arc::new(FakeRunGateway::default()),
            events: Arc::new(FakeRunEvents::default()),
            hub: Arc::new(Hub::default()),
            local,
            touched: Mutex::new(Some(touched)),
            steps,
            commands: Mutex::new(Some(commands)),
        }
    }

    fn running_here() -> BTreeMap<RunId, LocalSteps> {
        BTreeMap::from([(
            run(),
            LocalSteps::default().with(
                NodeInstance::parse("open-pr").expect("instance"),
                LocalStep::Running {
                    session: SessionId::parse("claude-code-1").expect("id"),
                    question: None,
                },
            ),
        )])
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_running_here_is_marked_so_as_soon_as_the_worker_says() {
        let h = harness();
        let _stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::Elsewhere));

        h.local.replace(running_here());
        settle().await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::RunningHere));
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_this_machine_claimed_from_is_watched_though_it_was_not_owned() {
        let h = harness();
        let _stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start_owning(&h, Vec::new());
        settle().await;
        assert!(h.hub.snapshot().is_empty());

        h.local.replace(running_here());
        settle().await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::RunningHere));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_step_hints_the_worker_to_heartbeat_its_run() {
        let h = harness();
        let stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;

        stream.send(event(4, NodeChange::Cancelled)).expect("send");
        settle().await;
        let mut commands = h.commands.lock().expect("lock").take().expect("commands");
        assert!(matches!(commands.try_recv(), Ok(Command::Ending(ending)) if ending == run()));
    }

    fn pr_mark(hub: &Hub) -> Option<Mark> {
        hub.snapshot()
            .first()
            .and_then(|item| item.summary.phases.first().map(|cell| cell.mark))
    }

    async fn settle() {
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }

    fn event(seq: u64, change: NodeChange) -> RunEvent {
        RunEvent::Node {
            run: run(),
            instance: NodeInstance::parse("open-pr").expect("instance"),
            attempt: Some(NonZeroU32::MIN),
            at: START,
            seq,
            change,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_owned_run_appears_with_its_label_and_strip() {
        let h = harness();
        let _stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;

        let items = h.hub.snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, owned().label);
        assert_eq!(items[0].started_at, owned().started_at);
        assert_eq!(pr_mark(&h.hub), Some(Mark::Elsewhere));
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_event_moves_the_item() {
        let h = harness();
        let stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;

        stream
            .send(event(4, NodeChange::Completed { branch: None }))
            .expect("send");
        settle().await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::Done));
    }

    #[tokio::test(start_paused = true)]
    async fn a_gap_refetches_the_graph_after_the_debounce() {
        let h = harness();
        let stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;

        h.runs
            .set_graph(graph(9, InstanceStatus::Done { branch: None }));
        stream
            .send(event(9, NodeChange::Completed { branch: None }))
            .expect("send");
        settle().await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::Done));
        assert_eq!(h.runs.fetched().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_stream_is_reopened_and_every_graph_refetched() {
        let h = harness();
        let first = h.events.stream();
        let _second = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;

        drop(first);
        h.runs
            .set_graph(graph(5, InstanceStatus::Done { branch: None }));
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        assert_eq!(h.events.opened(), 2);
        assert_eq!(pr_mark(&h.hub), Some(Mark::Done));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_cannot_open_is_retried_with_backoff() {
        let h = harness();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        tokio::time::sleep(StdDuration::from_secs(4)).await;
        let early = h.events.opened();
        assert!((2..=3).contains(&early), "{early}");

        let _stream = h.events.stream();
        tokio::time::sleep(MAX_BACKOFF * 2).await;
        assert_eq!(pr_mark(&h.hub), Some(Mark::Elsewhere));
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_the_gateway_no_longer_has_leaves_the_board() {
        let h = harness();
        let stream = h.events.stream();
        h.runs.set_graph(graph(3, InstanceStatus::Running));
        start(&h);
        settle().await;
        assert_eq!(h.hub.snapshot().len(), 1);

        h.runs.remove_graph(&run());
        stream.send(RunEvent::Resync).expect("send");
        settle().await;
        assert!(h.hub.snapshot().is_empty());
    }
}
