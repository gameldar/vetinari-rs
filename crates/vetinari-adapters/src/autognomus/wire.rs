//! The gateway's public JSON, mirrored rather than imported.
//!
//! autognomus publishes no crates, and its `PublicNode` exists so callers do
//! not depend on engine types: the HTTP JSON is the contract, so vetinari owns
//! a copy of the part it uses. Fixtures under
//! `tests/fixtures/autognomus/` keep the copy honest.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use vetinari_core::application::ports::PortError;
use vetinari_core::domain::devflow::compile::{GraphEdge, GraphNode, WorkflowGraph};
use vetinari_core::domain::devflow::ids::{
    BranchLabel, DefinitionId, LeaseToken, NodeId, NodeInstance, RunId, TaskId, TaskQueue,
};
use vetinari_core::domain::devflow::run_view::{
    InstanceState, InstanceStatus, RunGraph, RunHeader, RunNode, RunNodeKind, RunStatus,
};
use vetinari_core::domain::devflow::task::RawTask;

/// How errors from this backend name it.
const WHAT: &str = "autognomus";

/// `POST /v1/workflows`: a definition to register.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowBody {
    /// The definition id.
    pub id: String,
    /// What may start it. Vetinari's runs are started by the launcher.
    pub triggers: Vec<PublicTrigger>,
    /// Its nodes.
    pub nodes: Vec<PublicNode>,
    /// Its edges.
    pub edges: Vec<PublicEdge>,
}

/// What starts a workflow. Vetinari only starts runs by hand.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PublicTrigger {
    /// `POST /v1/workflows/{id}/runs`.
    Manual,
}

/// A node, internally tagged by `kind` as the gateway's `PublicNode` is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PublicNode {
    /// Claimed and executed by a worker outside the platform.
    External {
        /// The node id.
        id: String,
        /// The queue it is offered on.
        queue: String,
        /// What the worker reads.
        config: Value,
        /// How long a claim lasts without a heartbeat.
        lease_seconds: u32,
        /// How long one attempt may take from its offer.
        #[serde(skip_serializing_if = "Option::is_none")]
        deadline_seconds: Option<u32>,
        /// How many attempts.
        retry: RetrySpec,
    },
    /// Runs its body until its exit is met or its iterations run out.
    Loop {
        /// The node id.
        id: String,
        /// The nodes each iteration runs.
        body: Vec<String>,
        /// What ends it early.
        exit: PublicLoopExit,
        /// The most iterations.
        max_iterations: u32,
    },
}

/// The gateway's `RetrySpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetrySpec {
    /// How many attempts before the run fails.
    pub attempts: u32,
}

/// A loop's exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicLoopExit {
    /// A direct member of the body.
    pub node: String,
    /// The label that ends the loop.
    pub when: String,
}

/// The gateway's `PublicEdge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicEdge {
    /// The source node.
    pub from: String,
    /// The target node.
    pub to: String,
    /// The label that selects it, or none for always.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

impl From<&WorkflowGraph> for WorkflowBody {
    fn from(graph: &WorkflowGraph) -> Self {
        Self {
            id: graph.id.to_string(),
            triggers: vec![PublicTrigger::Manual],
            nodes: graph.nodes.iter().map(PublicNode::from).collect(),
            edges: graph.edges.iter().map(PublicEdge::from).collect(),
        }
    }
}

impl From<&GraphNode> for PublicNode {
    fn from(node: &GraphNode) -> Self {
        match node {
            GraphNode::External {
                id,
                queue,
                config,
                policy,
            } => PublicNode::External {
                id: id.to_string(),
                queue: queue.to_string(),
                config: config.to_json(),
                lease_seconds: policy.lease.get(),
                deadline_seconds: policy.deadline.map(|deadline| deadline.get().get()),
                retry: RetrySpec {
                    attempts: policy.attempts.get(),
                },
            },
            GraphNode::Loop {
                id,
                body,
                exit,
                max_iterations,
            } => PublicNode::Loop {
                id: id.to_string(),
                body: body.iter().map(ToString::to_string).collect(),
                exit: PublicLoopExit {
                    node: exit.node.to_string(),
                    when: exit.when.to_string(),
                },
                max_iterations: max_iterations.get().get(),
            },
        }
    }
}

impl From<&GraphEdge> for PublicEdge {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            from: edge.from.to_string(),
            to: edge.to.to_string(),
            when: edge.when.as_ref().map(ToString::to_string),
        }
    }
}

/// `200` from `POST /v1/tasks/claim`. The reply also names the run,
/// workflow, instance and attempt; all but the workflow are inside `id`,
/// which is authoritative, so they are not read.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClaimReply {
    /// `<run>~<instance>~<attempt>`.
    pub id: String,
    /// The lease token.
    pub lease: String,
    /// When the lease runs out.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// The queue it was claimed from.
    pub queue: String,
    /// The node's config.
    #[serde(default)]
    pub config: Value,
    /// The node's input.
    #[serde(default)]
    pub input: Value,
}

impl ClaimReply {
    /// Parse the identifiers; `config` and `input` are the domain's to read.
    pub fn into_raw(self) -> Result<RawTask, PortError> {
        let invalid = |what: &str, error: &dyn std::fmt::Display| {
            PortError::protocol(WHAT, format!("claim reply {what}: {error}"))
        };
        Ok(RawTask::new(
            TaskId::parse(&self.id).map_err(|error| invalid("id", &error))?,
            LeaseToken::parse(&self.lease).map_err(|error| invalid("lease", &error))?,
            self.expires_at,
            TaskQueue::parse(&self.queue).map_err(|error| invalid("queue", &error))?,
            self.config,
            self.input,
        ))
    }
}

/// `GET /v1/runs/{id}`, and each entry of `GET /v1/workflows/{id}/runs`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunViewReply {
    /// The run id.
    pub id: String,
    /// The definition it is a run of.
    pub workflow: String,
    /// Where the run is, tagged by `state`.
    #[serde(flatten)]
    pub state: RunStateReply,
    /// Every output recorded, keyed by instance.
    #[serde(default)]
    pub outputs: BTreeMap<String, Value>,
    /// When it was started.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The engine's `RunStateView`, amended and legacy shapes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum RunStateReply {
    /// Started, not yet dispatched.
    Pending,
    /// Something is in flight.
    Active,
    /// The pre-amendment shape of `Active`.
    Running,
    /// The pre-amendment shape of `Active`, waiting on a timer.
    Suspended,
    /// Finished.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// A state this copy does not know.
    #[serde(other)]
    Unknown,
}

/// Each entry of `GET /v1/workflows`. Only the id is read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkflowSummaryReply {
    /// The definition id.
    pub id: String,
}

/// `201` from `POST /v1/workflows/{id}/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StartedReply {
    /// The new run's id.
    pub run: String,
}

/// `GET /v1/runs/{id}/graph`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunGraphReply {
    /// The run, without its outputs' relevance: they are not in a graph.
    pub run: RunViewReply,
    /// The last event the graph reflects.
    pub seq: u64,
    /// Every node.
    pub nodes: Vec<GraphNodeReply>,
    /// Every edge.
    pub edges: Vec<PublicEdge>,
    /// Every instance reached.
    pub instances: Vec<InstanceReply>,
}

/// One node of a graph view.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GraphNodeReply {
    /// The node id.
    pub id: String,
    /// What it is, tagged by `kind`.
    #[serde(flatten)]
    pub kind: NodeKindReply,
    /// The loop that directly contains it.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A graph node's kind.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NodeKindReply {
    /// Claimed from a queue.
    External {
        /// The queue.
        queue: String,
    },
    /// A loop.
    Loop {
        /// Its body.
        body: Vec<String>,
        /// Its exit.
        exit: PublicLoopExit,
        /// Its bound.
        max_iterations: u32,
    },
    /// A component, delay or join, or a kind this copy does not know.
    #[serde(other)]
    Other,
}

/// One instance of a graph view.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InstanceReply {
    /// The instance.
    pub instance: String,
    /// Where it is, tagged by `state`.
    #[serde(flatten)]
    pub status: InstanceStatusReply,
    /// The current attempt.
    #[serde(default)]
    pub attempt: Option<u32>,
    /// When it first became ready.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub first_ready_at: Option<OffsetDateTime>,
    /// When it finished.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

/// The engine's `InstanceStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum InstanceStatusReply {
    /// Ready.
    Ready,
    /// Running in the engine.
    Running,
    /// On a timer.
    Waiting {
        /// Until when.
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
    },
    /// Offered on a queue.
    Offered {
        /// The queue.
        queue: String,
    },
    /// Leased to a worker.
    Claimed {
        /// The worker's label.
        worker: String,
        /// When the lease runs out.
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
    },
    /// A loop on an iteration.
    Looping {
        /// Which.
        iteration: u32,
    },
    /// Finished.
    Done {
        /// The label it took.
        #[serde(default)]
        branch: Option<String>,
    },
    /// Failed.
    Failed {
        /// Why.
        error: String,
    },
    /// Will never run.
    Skipped,
    /// Abandoned.
    Cancelled,
    /// A status this copy does not know.
    #[serde(other)]
    Unknown,
}

impl RunStateReply {
    /// As the domain sees it. An unknown state is read as still active, so
    /// the run stays watched until a state this copy knows says otherwise.
    #[must_use]
    pub const fn status(self) -> RunStatus {
        match self {
            Self::Completed => RunStatus::Completed,
            Self::Failed => RunStatus::Failed,
            Self::Cancelled => RunStatus::Cancelled,
            Self::Pending | Self::Active | Self::Running | Self::Suspended | Self::Unknown => {
                RunStatus::Active
            }
        }
    }
}

impl RunViewReply {
    /// As a listing entry.
    pub fn into_header(self) -> Result<RunHeader, PortError> {
        Ok(RunHeader {
            id: parsed("run id", RunId::parse(&self.id))?,
            definition: parsed("workflow", DefinitionId::parse(&self.workflow))?,
            status: self.state.status(),
            created_at: self.created_at,
        })
    }
}

impl RunGraphReply {
    /// As the domain's graph. An instance whose status this copy does not
    /// know is left out, as if not yet reached, and logged; the next event
    /// or refetch brings it back.
    pub fn into_graph(self) -> Result<RunGraph, PortError> {
        Ok(RunGraph {
            run: parsed("run id", RunId::parse(&self.run.id))?,
            status: self.run.state.status(),
            seq: self.seq,
            nodes: self
                .nodes
                .into_iter()
                .map(GraphNodeReply::into_node)
                .collect::<Result<_, _>>()?,
            edges: self
                .edges
                .into_iter()
                .map(PublicEdge::into_edge)
                .collect::<Result<_, _>>()?,
            instances: self
                .instances
                .into_iter()
                .filter_map(|instance| instance.into_state().transpose())
                .collect::<Result<_, _>>()?,
        })
    }
}

impl GraphNodeReply {
    fn into_node(self) -> Result<RunNode, PortError> {
        let kind = match self.kind {
            NodeKindReply::External { queue } => RunNodeKind::External {
                queue: parsed("queue", TaskQueue::parse(&queue))?,
            },
            NodeKindReply::Loop {
                body,
                exit,
                max_iterations,
            } => RunNodeKind::Loop {
                body: body
                    .iter()
                    .map(|node| parsed("loop body", NodeId::parse(node)))
                    .collect::<Result<_, _>>()?,
                exit: parsed("loop exit", NodeId::parse(&exit.node))?,
                max_iterations: NonZeroU32::new(max_iterations)
                    .ok_or_else(|| PortError::protocol(WHAT, "a loop with no iterations"))?,
            },
            NodeKindReply::Other => RunNodeKind::Engine,
        };
        Ok(RunNode {
            id: parsed("node id", NodeId::parse(&self.id))?,
            kind,
            scope: self
                .scope
                .map(|scope| parsed("scope", NodeId::parse(&scope)))
                .transpose()?,
        })
    }
}

impl PublicEdge {
    fn into_edge(self) -> Result<GraphEdge, PortError> {
        Ok(GraphEdge {
            from: parsed("edge", NodeId::parse(&self.from))?,
            to: parsed("edge", NodeId::parse(&self.to))?,
            when: self
                .when
                .map(|label| parsed("edge label", BranchLabel::parse(&label)))
                .transpose()?,
        })
    }
}

impl InstanceReply {
    fn into_state(self) -> Result<Option<InstanceState>, PortError> {
        let instance = parsed("instance", NodeInstance::parse(&self.instance))?;
        let status = match self.status {
            InstanceStatusReply::Ready => InstanceStatus::Ready,
            InstanceStatusReply::Running => InstanceStatus::Running,
            InstanceStatusReply::Waiting { until } => InstanceStatus::Waiting { until },
            InstanceStatusReply::Offered { queue } => InstanceStatus::Offered {
                queue: parsed("queue", TaskQueue::parse(&queue))?,
            },
            InstanceStatusReply::Claimed { worker, expires_at } => {
                InstanceStatus::Claimed { worker, expires_at }
            }
            InstanceStatusReply::Looping { iteration } => InstanceStatus::Looping {
                iteration: NonZeroU32::new(iteration)
                    .ok_or_else(|| PortError::protocol(WHAT, "iteration 0"))?,
            },
            InstanceStatusReply::Done { branch } => InstanceStatus::Done {
                branch: branch
                    .map(|label| parsed("branch", BranchLabel::parse(&label)))
                    .transpose()?,
            },
            InstanceStatusReply::Failed { error } => InstanceStatus::Failed { error },
            InstanceStatusReply::Skipped => InstanceStatus::Skipped,
            InstanceStatusReply::Cancelled => InstanceStatus::Cancelled,
            InstanceStatusReply::Unknown => {
                tracing::warn!(%instance, "autognomus reported a status this version cannot read");
                return Ok(None);
            }
        };
        Ok(Some(InstanceState {
            instance,
            status,
            attempt: self.attempt.and_then(NonZeroU32::new),
            first_ready_at: self.first_ready_at,
            finished_at: self.finished_at,
        }))
    }
}

/// An identifier from the gateway that did not parse.
fn parsed<T, E: std::fmt::Display>(what: &str, result: Result<T, E>) -> Result<T, PortError> {
    result.map_err(|error| PortError::protocol(WHAT, format!("{what}: {error}")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use std::num::NonZeroU32;
    use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues, compile};

    use time::macros::datetime;
    use vetinari_core::domain::devflow::ids::{
        BranchLabel, DefinitionId, NodeId, Rounds, WorkerName,
    };
    use vetinari_core::domain::devflow::outcome::{ReviewOutcome, StepOutcome};
    use vetinari_core::domain::devflow::run_view::{InstanceStatus, RunNode, RunNodeKind};
    use vetinari_core::domain::devflow::step::StepKind;
    use vetinari_core::domain::devflow::task::{ClaimedTask, StepInput, StepOutput};
    use vetinari_core::domain::devflow::template::{
        DevWorkflow, FeatureFromTicket, RepoTarget, TicketRef,
    };
    use vetinari_core::domain::git::BranchName;
    use vetinari_core::domain::ids::{ProfileId, SourceId};
    use vetinari_core::domain::text::NonEmptyText;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(fixture_text(name)).expect("fixture is JSON")
    }

    fn fixture_text(name: &str) -> &'static str {
        match name {
            "feature" => {
                include_str!("../../tests/fixtures/autognomus/feature-from-ticket.workflow.json")
            }
            "external" => include_str!("../../tests/fixtures/autognomus/public-node-external.json"),
            "loop" => include_str!("../../tests/fixtures/autognomus/public-node-loop.json"),
            "claim-preflight" => {
                include_str!("../../tests/fixtures/autognomus/claim-preflight-first.json")
            }
            "claim-implement" => {
                include_str!("../../tests/fixtures/autognomus/claim-implement-rework.json")
            }
            "claim-open-pr" => include_str!("../../tests/fixtures/autognomus/claim-open-pr.json"),
            "run-graph" => include_str!("../../tests/fixtures/autognomus/run-graph.json"),
            "instance-statuses" => {
                include_str!("../../tests/fixtures/autognomus/instance-statuses.json")
            }
            "run-view" => include_str!("../../tests/fixtures/autognomus/run-view.json"),
            "workflow-runs" => include_str!("../../tests/fixtures/autognomus/workflow-runs.json"),
            other => panic!("no fixture {other}"),
        }
    }

    fn claimed(name: &str) -> ClaimedTask {
        let reply: ClaimReply = serde_json::from_str(fixture_text(name)).expect("claim reply");
        ClaimedTask::parse(&reply.into_raw().expect("identifiers")).expect("a vetinari step")
    }

    fn json(value: &impl Serialize) -> Value {
        serde_json::to_value(value).expect("serialises")
    }

    fn feature() -> FeatureFromTicket {
        FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse("jira").expect("id"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit webhook retries").expect("title"),
                url: Some("https://acme.atlassian.net/browse/ENG-412".to_owned()),
            },
            repo: RepoTarget {
                path: PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: ProfileId::parse("work").expect("id"),
            base: Some(BranchName::sanitized("main")),
            max_review_rounds: Rounds::new(3).expect("rounds"),
            max_intake_rounds: Rounds::new(3).expect("rounds"),
            agent: None,
        }
    }

    #[test]
    fn an_external_node_has_the_resolved_public_shape() {
        let node = PublicNode::External {
            id: "implement".to_owned(),
            queue: "vetinari".to_owned(),
            config: serde_json::json!({}),
            lease_seconds: 300,
            deadline_seconds: Some(14400),
            retry: RetrySpec { attempts: 1 },
        };
        assert_eq!(json(&node), fixture("external"));
    }

    #[test]
    fn a_loop_node_has_the_resolved_public_shape() {
        let node = PublicNode::Loop {
            id: "rework".to_owned(),
            body: vec!["implement".to_owned(), "review".to_owned()],
            exit: PublicLoopExit {
                node: "review".to_owned(),
                when: "approved".to_owned(),
            },
            max_iterations: 3,
        };
        assert_eq!(json(&node), fixture("loop"));
    }

    #[test]
    fn a_compiled_feature_maps_to_its_golden_body() {
        let graph = compile(
            &DevWorkflow::FeatureFromTicket(feature()),
            DefinitionId::parse("vetinari-feature-01JB7ZQ3XK").expect("id"),
            &WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
            &StepPolicies::default(),
        );
        assert_eq!(json(&WorkflowBody::from(&graph)), fixture("feature"));
    }

    #[test]
    fn a_first_iteration_claim_reads_as_preflight_with_no_previous() {
        let task = claimed("claim-preflight");
        assert_eq!(task.id.run().as_str(), "01JB7ZR2M4Q9V8C6T3N5HKD0WX");
        assert_eq!(task.id.instance().to_string(), "preflight@1");
        assert_eq!(task.id.attempt().get(), 1);
        assert_eq!(task.lease.as_str(), "01JB7ZS9D1F4G7H2J5K8M3N6P0");
        assert_eq!(
            task.expires_at,
            time::macros::datetime!(2026-10-25 10:05:00 UTC)
        );
        assert_eq!(task.queue.as_str(), "vetinari-hex-agent");
        assert_eq!(task.config.step, StepKind::Preflight);
        assert_eq!(task.config.ticket, feature().ticket);
        assert_eq!(task.config.branch, feature().branch());
        assert_eq!(
            task.input,
            StepInput::Iteration {
                iteration: std::num::NonZeroU32::MIN,
                previous: None
            }
        );
    }

    #[test]
    fn a_rework_claim_carries_the_previous_reviews_findings() {
        let task = claimed("claim-implement");
        assert_eq!(task.config.step, StepKind::Implement);
        let StepInput::Iteration {
            iteration,
            previous:
                Some(StepOutput {
                    outcome: StepOutcome::Review(ReviewOutcome::ChangesRequested { findings }),
                    ..
                }),
        } = task.input
        else {
            panic!("expected review findings, got {:?}", task.input);
        };
        assert_eq!(iteration.get(), 2);
        assert_eq!(
            findings.as_slice()[0].text.as_str(),
            "Retry-After ignored on 503"
        );
    }

    #[test]
    fn a_claim_after_a_loop_has_plain_input() {
        let task = claimed("claim-open-pr");
        assert_eq!(task.config.step, StepKind::OpenPr);
        assert_eq!(task.id.attempt().get(), 2);
        assert_eq!(task.input, StepInput::Plain);
    }

    #[test]
    fn a_claim_with_unreadable_identifiers_is_a_protocol_error() {
        let mut reply: ClaimReply =
            serde_json::from_str(fixture_text("claim-open-pr")).expect("claim reply");
        reply.id = "01JB7ZR2M4Q9V8C6T3N5HKD0WX/open-pr/2".to_owned();
        assert!(matches!(
            reply.clone().into_raw(),
            Err(PortError::Protocol { .. })
        ));
        reply.id = "r~open-pr~2".to_owned();
        reply.queue = "Not A Queue".to_owned();
        assert!(matches!(reply.into_raw(), Err(PortError::Protocol { .. })));
    }

    // Run views

    fn graph_reply() -> RunGraphReply {
        serde_json::from_str(fixture_text("run-graph")).expect("graph reply")
    }

    fn graph_with_instances(instances: Value) -> RunGraph {
        let mut reply = fixture("run-graph");
        reply["instances"] = instances;
        serde_json::from_value::<RunGraphReply>(reply)
            .expect("graph reply")
            .into_graph()
            .expect("graph")
    }

    #[test]
    fn a_graph_view_reads_as_the_runs_nodes_edges_and_instances() {
        let graph = graph_reply().into_graph().expect("graph");
        assert_eq!(graph.run.as_str(), "01JB7ZR2M4Q9V8C6T3N5HKD0WX");
        assert_eq!(graph.status, RunStatus::Active);
        assert_eq!(graph.seq, 17);
        assert_eq!(graph.nodes.len(), 8);
        assert_eq!(
            graph.nodes[3],
            RunNode {
                id: NodeId::parse("rework").expect("id"),
                kind: RunNodeKind::Loop {
                    body: vec![
                        NodeId::parse("implement").expect("id"),
                        NodeId::parse("review").expect("id"),
                    ],
                    exit: NodeId::parse("review").expect("id"),
                    max_iterations: NonZeroU32::new(3).expect("bound"),
                },
                scope: None,
            }
        );
        assert_eq!(
            graph.nodes[4].scope,
            Some(NodeId::parse("rework").expect("id"))
        );
        assert_eq!(graph.edges.len(), 5);
        assert_eq!(
            graph.edges[1].when,
            Some(BranchLabel::parse("needs-info").expect("label"))
        );
        let claimed = graph.instances.last().expect("an instance");
        assert_eq!(claimed.instance.to_string(), "review@2");
        assert_eq!(
            claimed.status,
            InstanceStatus::Claimed {
                worker: "hex".to_owned(),
                expires_at: datetime!(2026-10-25 10:30 UTC),
            }
        );
        assert_eq!(claimed.attempt, NonZeroU32::new(1));
        assert_eq!(
            claimed.first_ready_at,
            Some(datetime!(2026-10-25 10:20 UTC))
        );
        assert_eq!(claimed.finished_at, None);
    }

    #[test]
    fn every_instance_status_reads() {
        let graph = graph_with_instances(fixture("instance-statuses"));
        let statuses: Vec<InstanceStatus> = graph
            .instances
            .into_iter()
            .map(|state| state.status)
            .collect();
        assert_eq!(
            statuses,
            [
                InstanceStatus::Ready,
                InstanceStatus::Running,
                InstanceStatus::Waiting {
                    until: datetime!(2026-10-25 10:01 UTC)
                },
                InstanceStatus::Offered {
                    queue: TaskQueue::parse("vetinari-hex-agent").expect("queue")
                },
                InstanceStatus::Claimed {
                    worker: "ci-runner.07".to_owned(),
                    expires_at: datetime!(2026-10-25 10:05 UTC),
                },
                InstanceStatus::Looping {
                    iteration: NonZeroU32::new(2).expect("iteration")
                },
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("approved").expect("label"))
                },
                InstanceStatus::Done { branch: None },
                InstanceStatus::Failed {
                    error: "agent crashed".to_owned()
                },
                InstanceStatus::Skipped,
                InstanceStatus::Cancelled,
            ]
        );
    }

    #[test]
    fn an_instance_with_a_status_this_copy_does_not_know_is_left_out() {
        let graph = graph_with_instances(serde_json::json!([
            { "instance": "implement@1", "state": "hibernating", "attempt": 1 },
            { "instance": "review@1", "state": "ready" }
        ]));
        assert_eq!(graph.instances.len(), 1);
        assert_eq!(graph.instances[0].instance.to_string(), "review@1");
    }

    #[test]
    fn a_node_the_engine_runs_itself_reads_as_an_engine_node() {
        let mut reply = fixture("run-graph");
        reply["nodes"] = serde_json::json!([
            { "id": "settle", "kind": "join", "scope": null },
            { "id": "pause", "kind": "delay", "duration_seconds": 60 }
        ]);
        let graph = serde_json::from_value::<RunGraphReply>(reply)
            .expect("graph reply")
            .into_graph()
            .expect("graph");
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.kind == RunNodeKind::Engine)
        );
    }

    #[test]
    fn a_graph_with_unreadable_identifiers_is_a_protocol_error() {
        let mut reply = fixture("run-graph");
        reply["instances"][0]["instance"] = serde_json::json!("review@0");
        let reply: RunGraphReply = serde_json::from_value(reply).expect("graph reply");
        assert!(matches!(
            reply.into_graph(),
            Err(PortError::Protocol { .. })
        ));
    }

    #[test]
    fn run_states_fold_into_the_four_the_domain_has() {
        for (state, status) in [
            ("pending", RunStatus::Active),
            ("active", RunStatus::Active),
            ("running", RunStatus::Active),
            ("suspended", RunStatus::Active),
            ("completed", RunStatus::Completed),
            ("failed", RunStatus::Failed),
            ("cancelled", RunStatus::Cancelled),
            ("thawing", RunStatus::Active),
        ] {
            let mut reply = fixture("run-view");
            reply["state"] = serde_json::json!(state);
            let reply: RunViewReply = serde_json::from_value(reply).expect("run view");
            assert_eq!(reply.state.status(), status, "{state}");
        }
    }

    #[test]
    fn a_run_view_keeps_its_outputs_by_instance() {
        let reply: RunViewReply = serde_json::from_str(fixture_text("run-view")).expect("run view");
        assert_eq!(
            reply.outputs["ask-info@1"]["note"],
            serde_json::json!("Page size is 50")
        );
    }

    #[test]
    fn a_listed_run_reads_as_a_header() {
        let replies: Vec<RunViewReply> =
            serde_json::from_str(fixture_text("workflow-runs")).expect("runs");
        let headers: Vec<RunHeader> = replies
            .into_iter()
            .map(|reply| reply.into_header().expect("header"))
            .collect();
        assert_eq!(headers[0].id.as_str(), "01JB7ZR2M4Q9V8C6T3N5HKD0WX");
        assert_eq!(
            headers[0].definition,
            DefinitionId::parse("vetinari-feature-01JB7ZQ3XK").expect("id")
        );
        assert_eq!(headers[0].status, RunStatus::Completed);
        assert_eq!(headers[1].status, RunStatus::Active);
        assert_eq!(headers[1].created_at, datetime!(2026-10-25 11:00 UTC));
    }

    #[test]
    fn a_run_of_a_definition_vetinari_did_not_register_is_a_protocol_error() {
        let mut reply = fixture("run-view");
        reply["workflow"] = serde_json::json!("deploy-nightly");
        let reply: RunViewReply = serde_json::from_value(reply).expect("run view");
        assert!(matches!(
            reply.into_header(),
            Err(PortError::Protocol { .. })
        ));
    }
}
