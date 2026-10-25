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
