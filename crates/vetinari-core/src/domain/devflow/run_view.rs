//! What a run looks like from outside the engine: its graph, where each node
//! instance is, and the events that move it.
//!
//! The engine computes the graph on request (engine spec 6) and publishes
//! events as hints (7.2); `seq` ties the two together, so a watcher can tell
//! a missed event from one it already has.

use std::num::NonZeroU32;

use time::OffsetDateTime;

use super::compile::GraphEdge;
use super::ids::{BranchLabel, DefinitionId, NodeId, NodeInstance, RunId, TaskQueue};

/// Where a run is as a whole. A pending run is active: it has been started
/// and nothing has finished it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Started and not finished.
    Active,
    /// Every path reached its end.
    Completed,
    /// A node failed with no retries left.
    Failed,
    /// Somebody cancelled it.
    Cancelled,
}

impl RunStatus {
    /// Whether nothing more will happen to the run.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        !matches!(self, Self::Active)
    }
}

/// A run as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHeader {
    /// The run.
    pub id: RunId,
    /// The definition it is a run of.
    pub definition: DefinitionId,
    /// Where it is.
    pub status: RunStatus,
    /// When it was started.
    pub created_at: OffsetDateTime,
}

/// What a node of the drawn graph is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunNodeKind {
    /// A step a worker claims from a queue.
    External {
        /// The queue it is offered on.
        queue: TaskQueue,
    },
    /// A loop over a body of nodes.
    Loop {
        /// The nodes each iteration runs.
        body: Vec<NodeId>,
        /// The body node whose label can end the loop.
        exit: NodeId,
        /// The most iterations.
        max_iterations: NonZeroU32,
    },
    /// A node the engine runs itself: a component, a delay or a join.
    /// Vetinari's templates have none, but a graph may show them.
    Engine,
}

/// One node of the drawn graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunNode {
    /// The node id.
    pub id: NodeId,
    /// What it is.
    pub kind: RunNodeKind,
    /// The loop that directly contains it.
    pub scope: Option<NodeId>,
}

/// Where one node instance is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceStatus {
    /// Ready to run.
    Ready,
    /// The engine is running it.
    Running,
    /// Waiting on a delay or a retry backoff.
    Waiting {
        /// Until when.
        until: OffsetDateTime,
    },
    /// Offered to workers on a queue.
    Offered {
        /// The queue.
        queue: TaskQueue,
    },
    /// A worker holds a lease on it.
    Claimed {
        /// The worker's label: any worker's, so not held to vetinari's rules.
        worker: String,
        /// When the lease runs out.
        expires_at: OffsetDateTime,
    },
    /// A loop on one of its iterations.
    Looping {
        /// Which.
        iteration: NonZeroU32,
    },
    /// Finished, with the label it took.
    Done {
        /// The branch label, if the node produced one.
        branch: Option<BranchLabel>,
    },
    /// Failed with no retries left.
    Failed {
        /// Why.
        error: String,
    },
    /// Will never run: derived by the engine, never by vetinari.
    Skipped,
    /// Abandoned by a cancel.
    Cancelled,
}

/// One node instance and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceState {
    /// The instance.
    pub instance: NodeInstance,
    /// Where it is.
    pub status: InstanceStatus,
    /// The current attempt, once it has one.
    pub attempt: Option<NonZeroU32>,
    /// When it first became ready.
    pub first_ready_at: Option<OffsetDateTime>,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
}

/// A run's graph as the engine computed it at `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGraph {
    /// The run.
    pub run: RunId,
    /// Where it is as a whole.
    pub status: RunStatus,
    /// The number of the last event the graph reflects.
    pub seq: u64,
    /// Every node of the definition.
    pub nodes: Vec<RunNode>,
    /// Every edge of the definition.
    pub edges: Vec<GraphEdge>,
    /// Every instance reached so far. A node with none has not been reached.
    pub instances: Vec<InstanceState>,
}

/// What happened to a node instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeChange {
    /// It became ready.
    Ready,
    /// The engine started it.
    Dispatched,
    /// It is waiting on a timer.
    Suspended {
        /// Until when.
        resume_at: OffsetDateTime,
    },
    /// It was offered on a queue.
    Offered {
        /// The queue.
        queue: TaskQueue,
    },
    /// A worker claimed it.
    Claimed {
        /// The worker's label.
        worker: String,
        /// When the lease runs out.
        expires_at: OffsetDateTime,
    },
    /// It finished.
    Completed {
        /// The label it took.
        branch: Option<BranchLabel>,
    },
    /// An attempt failed.
    Failed {
        /// Why.
        error: String,
        /// Whether another attempt follows.
        will_retry: bool,
    },
    /// A cancel abandoned it.
    Cancelled,
}

/// One event from the engine's run stream. Events are hints (engine spec
/// 7.2): `seq` says whether one was missed, and the graph is the truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    /// A node instance moved.
    Node {
        /// The run.
        run: RunId,
        /// The instance.
        instance: NodeInstance,
        /// Its attempt, once it has one.
        attempt: Option<NonZeroU32>,
        /// When.
        at: OffsetDateTime,
        /// The run's sequence number for this event.
        seq: u64,
        /// What happened.
        change: NodeChange,
    },
    /// A loop began an iteration.
    Iteration {
        /// The run.
        run: RunId,
        /// The loop's instance.
        instance: NodeInstance,
        /// Which iteration.
        iteration: NonZeroU32,
        /// The run's sequence number for this event.
        seq: u64,
    },
    /// The run as a whole started or finished.
    Run {
        /// The run.
        run: RunId,
        /// The run's sequence number for this event.
        seq: u64,
        /// Where it is now.
        status: RunStatus,
    },
    /// The run changed in a way only its graph shows: an event this version
    /// cannot read, which still counts in `seq`.
    Changed {
        /// The run.
        run: RunId,
        /// The run's sequence number for this event.
        seq: u64,
    },
    /// The stream dropped events: every run's graph is to be fetched again.
    Resync,
}

/// What applying an event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The graph moved to the event's `seq`.
    Updated,
    /// The graph already reflects the event.
    Already,
    /// The event cannot be applied here: fetch the graph again.
    Refetch,
}

/// Apply one event to a run's graph (engine spec 7.3). Only the next event
/// in `seq` is applied; a gap, an event for another run, a loop iteration, a
/// run finishing, a node failing with a retry to come, and anything the
/// adapter could not read are refetches, because what follows from them
/// (`Skipped`, a backoff) is the engine's to derive.
pub fn apply(graph: &mut RunGraph, event: &RunEvent) -> Applied {
    let Some((run, seq)) = event.position() else {
        return Applied::Refetch;
    };
    if *run != graph.run {
        return Applied::Refetch;
    }
    if seq <= graph.seq {
        return Applied::Already;
    }
    if seq != graph.seq + 1 {
        return Applied::Refetch;
    }
    match event {
        RunEvent::Run {
            status: RunStatus::Active,
            ..
        } => {}
        RunEvent::Node {
            instance,
            attempt,
            at,
            change,
            ..
        } => {
            let Some(status) = change.status() else {
                return Applied::Refetch;
            };
            let finished = matches!(
                status,
                InstanceStatus::Done { .. }
                    | InstanceStatus::Failed { .. }
                    | InstanceStatus::Cancelled
            );
            match graph
                .instances
                .iter_mut()
                .find(|state| state.instance == *instance)
            {
                Some(state) => {
                    state.status = status;
                    state.attempt = attempt.or(state.attempt);
                    if finished {
                        state.finished_at = Some(*at);
                    }
                }
                None => graph.instances.push(InstanceState {
                    instance: instance.clone(),
                    status,
                    attempt: *attempt,
                    first_ready_at: Some(*at),
                    finished_at: finished.then_some(*at),
                }),
            }
        }
        RunEvent::Run { .. }
        | RunEvent::Iteration { .. }
        | RunEvent::Changed { .. }
        | RunEvent::Resync => return Applied::Refetch,
    }
    graph.seq = seq;
    Applied::Updated
}

impl RunEvent {
    /// The run and `seq` the event belongs to; a resync has neither.
    #[must_use]
    pub const fn position(&self) -> Option<(&RunId, u64)> {
        match self {
            Self::Node { run, seq, .. }
            | Self::Iteration { run, seq, .. }
            | Self::Run { run, seq, .. }
            | Self::Changed { run, seq } => Some((run, *seq)),
            Self::Resync => None,
        }
    }
}

impl NodeChange {
    /// The status the change leaves an instance in, or `None` when that is
    /// the engine's to derive: a failure with a retry to come waits on a
    /// backoff the event does not carry.
    fn status(&self) -> Option<InstanceStatus> {
        Some(match self {
            Self::Ready => InstanceStatus::Ready,
            Self::Dispatched => InstanceStatus::Running,
            Self::Suspended { resume_at } => InstanceStatus::Waiting { until: *resume_at },
            Self::Offered { queue } => InstanceStatus::Offered {
                queue: queue.clone(),
            },
            Self::Claimed { worker, expires_at } => InstanceStatus::Claimed {
                worker: worker.clone(),
                expires_at: *expires_at,
            },
            Self::Completed { branch } => InstanceStatus::Done {
                branch: branch.clone(),
            },
            Self::Failed {
                will_retry: true, ..
            } => return None,
            Self::Failed { error, .. } => InstanceStatus::Failed {
                error: error.clone(),
            },
            Self::Cancelled => InstanceStatus::Cancelled,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use time::macros::datetime;

    use super::*;

    pub(crate) fn id(raw: &str) -> NodeId {
        NodeId::parse(raw).expect("node id")
    }

    pub(crate) fn instance(raw: &str) -> NodeInstance {
        NodeInstance::parse(raw).expect("instance")
    }

    pub(crate) fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    pub(crate) fn run() -> RunId {
        RunId::parse("01JB7ZR2M4Q9V8C6T3N5HKD0WX").expect("run")
    }

    pub(crate) fn queue(name: &str) -> TaskQueue {
        TaskQueue::parse(name).expect("queue")
    }

    pub(crate) fn state(raw: &str, status: InstanceStatus) -> InstanceState {
        InstanceState {
            instance: instance(raw),
            status,
            attempt: Some(NonZeroU32::MIN),
            first_ready_at: Some(datetime!(2026-10-25 09:00 UTC)),
            finished_at: None,
        }
    }

    fn external(node: &str, scope: Option<&str>, human: bool) -> RunNode {
        RunNode {
            id: id(node),
            kind: RunNodeKind::External {
                queue: queue(if human {
                    "vetinari-hex-human"
                } else {
                    "vetinari-hex-agent"
                }),
            },
            scope: scope.map(id),
        }
    }

    fn looping(node: &str, body: [&str; 2], exit: &str) -> RunNode {
        RunNode {
            id: id(node),
            kind: RunNodeKind::Loop {
                body: body.into_iter().map(id).collect(),
                exit: id(exit),
                max_iterations: nz(3),
            },
            scope: None,
        }
    }

    fn edge(from: &str, to: &str, when: Option<&str>) -> GraphEdge {
        GraphEdge {
            from: id(from),
            to: id(to),
            when: when.map(|label| BranchLabel::parse(label).expect("label")),
        }
    }

    /// The feature graph as the engine would draw it, with these instances.
    pub(crate) fn feature_graph(seq: u64, instances: Vec<InstanceState>) -> RunGraph {
        RunGraph {
            run: run(),
            status: RunStatus::Active,
            seq,
            nodes: vec![
                looping("intake", ["preflight", "ask-info"], "preflight"),
                external("preflight", Some("intake"), false),
                external("ask-info", Some("intake"), true),
                looping("rework", ["implement", "review"], "review"),
                external("implement", Some("rework"), false),
                external("review", Some("rework"), false),
                external("review-stuck", None, true),
                external("open-pr", None, false),
            ],
            edges: vec![
                edge("intake", "rework", Some("ready")),
                edge("preflight", "ask-info", Some("needs-info")),
                edge("implement", "review", None),
                edge("rework", "open-pr", Some("approved")),
                edge("rework", "review-stuck", Some("exhausted")),
                edge("review-stuck", "open-pr", Some("open-anyway")),
            ],
            instances,
        }
    }

    fn node_event(seq: u64, raw: &str, change: NodeChange) -> RunEvent {
        RunEvent::Node {
            run: run(),
            instance: instance(raw),
            attempt: Some(NonZeroU32::MIN),
            at: datetime!(2026-10-25 10:00 UTC),
            seq,
            change,
        }
    }

    fn status_of<'a>(graph: &'a RunGraph, raw: &str) -> Option<&'a InstanceStatus> {
        graph
            .instances
            .iter()
            .find(|state| state.instance == instance(raw))
            .map(|state| &state.status)
    }

    #[test]
    fn the_next_event_moves_an_instance_and_the_graph_to_its_seq() {
        let mut graph = feature_graph(4, vec![state("implement@1", InstanceStatus::Ready)]);
        let applied = apply(
            &mut graph,
            &node_event(
                5,
                "implement@1",
                NodeChange::Offered {
                    queue: queue("vetinari-hex-agent"),
                },
            ),
        );
        assert_eq!(applied, Applied::Updated);
        assert_eq!(graph.seq, 5);
        assert_eq!(
            status_of(&graph, "implement@1"),
            Some(&InstanceStatus::Offered {
                queue: queue("vetinari-hex-agent")
            })
        );
    }

    #[test]
    fn each_node_change_sets_the_status_it_means() {
        let until = datetime!(2026-10-25 10:30 UTC);
        let label = BranchLabel::parse("approved").expect("label");
        for (change, status) in [
            (NodeChange::Ready, InstanceStatus::Ready),
            (NodeChange::Dispatched, InstanceStatus::Running),
            (
                NodeChange::Suspended { resume_at: until },
                InstanceStatus::Waiting { until },
            ),
            (
                NodeChange::Claimed {
                    worker: "hex".to_owned(),
                    expires_at: until,
                },
                InstanceStatus::Claimed {
                    worker: "hex".to_owned(),
                    expires_at: until,
                },
            ),
            (
                NodeChange::Completed {
                    branch: Some(label.clone()),
                },
                InstanceStatus::Done {
                    branch: Some(label.clone()),
                },
            ),
            (
                NodeChange::Failed {
                    error: "boom".to_owned(),
                    will_retry: false,
                },
                InstanceStatus::Failed {
                    error: "boom".to_owned(),
                },
            ),
            (NodeChange::Cancelled, InstanceStatus::Cancelled),
        ] {
            let mut graph = feature_graph(1, vec![state("review@1", InstanceStatus::Running)]);
            assert_eq!(
                apply(&mut graph, &node_event(2, "review@1", change.clone())),
                Applied::Updated,
                "{change:?}"
            );
            assert_eq!(status_of(&graph, "review@1"), Some(&status), "{change:?}");
        }
    }

    #[test]
    fn a_finishing_change_records_when_and_a_new_instance_is_added() {
        let mut graph = feature_graph(1, vec![state("implement@1", InstanceStatus::Running)]);
        apply(
            &mut graph,
            &node_event(2, "implement@1", NodeChange::Completed { branch: None }),
        );
        apply(&mut graph, &node_event(3, "review@1", NodeChange::Ready));

        assert_eq!(
            graph.instances[0].finished_at,
            Some(datetime!(2026-10-25 10:00 UTC))
        );
        assert_eq!(
            graph.instances[1],
            InstanceState {
                instance: instance("review@1"),
                status: InstanceStatus::Ready,
                attempt: Some(NonZeroU32::MIN),
                first_ready_at: Some(datetime!(2026-10-25 10:00 UTC)),
                finished_at: None,
            }
        );
    }

    #[test]
    fn an_event_the_graph_already_reflects_changes_nothing() {
        let mut graph = feature_graph(5, vec![state("implement@1", InstanceStatus::Running)]);
        let before = graph.clone();
        assert_eq!(
            apply(
                &mut graph,
                &node_event(5, "implement@1", NodeChange::Cancelled)
            ),
            Applied::Already
        );
        assert_eq!(graph, before);
    }

    #[test]
    fn a_gap_in_seq_is_a_refetch_and_changes_nothing() {
        let mut graph = feature_graph(5, vec![state("implement@1", InstanceStatus::Running)]);
        let before = graph.clone();
        assert_eq!(
            apply(
                &mut graph,
                &node_event(7, "implement@1", NodeChange::Cancelled)
            ),
            Applied::Refetch
        );
        assert_eq!(graph, before);
    }

    #[test]
    fn what_the_engine_derives_from_an_event_is_a_refetch() {
        let other = RunId::parse("01JB80").expect("run");
        for event in [
            RunEvent::Iteration {
                run: run(),
                instance: instance("rework"),
                iteration: nz(2),
                seq: 2,
            },
            RunEvent::Run {
                run: run(),
                seq: 2,
                status: RunStatus::Completed,
            },
            RunEvent::Changed { run: run(), seq: 2 },
            RunEvent::Resync,
            node_event(
                2,
                "implement@1",
                NodeChange::Failed {
                    error: "agent crashed".to_owned(),
                    will_retry: true,
                },
            ),
            RunEvent::Changed { run: other, seq: 2 },
        ] {
            let mut graph = feature_graph(1, vec![state("implement@1", InstanceStatus::Running)]);
            assert_eq!(apply(&mut graph, &event), Applied::Refetch, "{event:?}");
            assert_eq!(graph.seq, 1, "{event:?}");
        }
    }

    #[test]
    fn a_run_starting_moves_only_its_seq() {
        let mut graph = feature_graph(0, Vec::new());
        assert_eq!(
            apply(
                &mut graph,
                &RunEvent::Run {
                    run: run(),
                    seq: 1,
                    status: RunStatus::Active
                }
            ),
            Applied::Updated
        );
        assert_eq!((graph.seq, graph.status), (1, RunStatus::Active));
    }
}
