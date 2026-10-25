//! The gateway's public JSON, mirrored rather than imported.
//!
//! autognomus publishes no crates, and its `PublicNode` exists so callers do
//! not depend on engine types: the HTTP JSON is the contract, so vetinari owns
//! a copy of the part it uses. Fixtures under
//! `tests/fixtures/autognomus/` keep the copy honest.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use vetinari_core::application::ports::PortError;
use vetinari_core::domain::devflow::compile::{GraphEdge, GraphNode, WorkflowGraph};
use vetinari_core::domain::devflow::ids::{LeaseToken, TaskId, TaskQueue};
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues, compile};
    use vetinari_core::domain::devflow::ids::{DefinitionId, Rounds, WorkerName};
    use vetinari_core::domain::devflow::outcome::{ReviewOutcome, StepOutcome};
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
}
