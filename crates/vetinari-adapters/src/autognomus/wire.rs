//! The gateway's public JSON, mirrored rather than imported.
//!
//! autognomus publishes no crates, and its `PublicNode` exists so callers do
//! not depend on engine types: the HTTP JSON is the contract, so vetinari owns
//! a copy of the part it uses. Fixtures under
//! `tests/fixtures/autognomus/` keep the copy honest.

use serde::Serialize;
use serde_json::Value;
use vetinari_core::domain::devflow::compile::{GraphEdge, GraphNode, WorkflowGraph};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicLoopExit {
    /// A direct member of the body.
    pub node: String,
    /// The label that ends the loop.
    pub when: String,
}

/// The gateway's `PublicEdge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues, compile};
    use vetinari_core::domain::devflow::ids::{DefinitionId, Rounds, WorkerName};
    use vetinari_core::domain::devflow::template::{
        DevWorkflow, FeatureFromTicket, RepoTarget, TicketRef,
    };
    use vetinari_core::domain::git::BranchName;
    use vetinari_core::domain::ids::{ProfileId, SourceId};
    use vetinari_core::domain::text::NonEmptyText;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "feature" => {
                include_str!("../../tests/fixtures/autognomus/feature-from-ticket.workflow.json")
            }
            "external" => include_str!("../../tests/fixtures/autognomus/public-node-external.json"),
            "loop" => include_str!("../../tests/fixtures/autognomus/public-node-loop.json"),
            other => panic!("no fixture {other}"),
        };
        serde_json::from_str(text).expect("fixture is JSON")
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
}
