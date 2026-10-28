//! Compiling a template to the graph vetinari registers for one run.
//!
//! `compile` is total: every input it accepts produces a graph the engine
//! will register, because `Rounds`, `LeaseSecs` and `TaskQueue` cannot hold a
//! value the engine rejects. The graph names only the two node kinds vetinari
//! uses.

use std::num::NonZeroU32;

use super::ids::{BranchLabel, DefinitionId, NodeId, Rounds, TaskQueue, WorkerName};
use super::outcome::label;
use super::run_view::{RunNode, RunNodeKind};
use super::step::{GateKind, StepConfig, StepKind};
use super::template::{DevWorkflow, FeatureFromTicket, TemplateKind};
use crate::domain::ids::SkillId;

/// The node ids of [`FeatureFromTicket`]'s graph.
pub mod node {
    /// The loop around preflight and its gate.
    pub const INTAKE: &str = "intake";
    /// Check the ticket.
    pub const PREFLIGHT: &str = "preflight";
    /// Ask a person to update the ticket.
    pub const ASK_INFO: &str = "ask-info";
    /// The loop around implement and review.
    pub const REWORK: &str = "rework";
    /// Implement the ticket.
    pub const IMPLEMENT: &str = "implement";
    /// Review the implementation.
    pub const REVIEW: &str = "review";
    /// Ask a person what to do with work review never approved.
    pub const REVIEW_STUCK: &str = "review-stuck";
    /// Open the pull request.
    pub const OPEN_PR: &str = "open-pr";
}

/// The built-in skills agent steps run.
pub mod skill {
    /// Preflight.
    pub const TICKET_PREFLIGHT: &str = "ticket-preflight";
    /// Implement.
    pub const IMPLEMENT_TDD: &str = "implement-tdd";
    /// Review.
    pub const SELF_REVIEW: &str = "self-review";
}

/// A lease was shorter than the engine allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a lease of {found} s is under the engine's minimum of {min} s", min = LeaseSecs::MIN)]
pub struct LeaseSecsError {
    /// The offending length.
    pub found: u32,
}

/// How long a claim lasts without a heartbeat, in seconds: at least 30, the
/// engine's floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseSecs(u32);

impl LeaseSecs {
    /// The engine's minimum.
    pub const MIN: u32 = 30;

    /// Accept a lease the engine will register.
    pub fn new(secs: u32) -> Result<Self, LeaseSecsError> {
        if secs < Self::MIN {
            return Err(LeaseSecsError { found: secs });
        }
        Ok(Self(secs))
    }

    /// The lease in seconds.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// How long one attempt may take from the moment it is offered, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeadlineSecs(NonZeroU32);

impl DeadlineSecs {
    /// A deadline of this many seconds.
    #[must_use]
    pub const fn new(secs: NonZeroU32) -> Self {
        Self(secs)
    }

    /// The deadline in seconds.
    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

/// How the engine treats one step: its lease, its deadline, its attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepPolicy {
    /// How long a claim lasts without a heartbeat.
    pub lease: LeaseSecs,
    /// How long one attempt may take; unbounded when absent.
    pub deadline: Option<DeadlineSecs>,
    /// How many attempts before the run fails. Lease expiry and deadlines
    /// each consume one.
    pub attempts: NonZeroU32,
}

/// The policy for each step, from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepPolicies {
    /// Preflight.
    pub preflight: StepPolicy,
    /// Implement.
    pub implement: StepPolicy,
    /// Review.
    pub review: StepPolicy,
    /// Open PR.
    pub open_pr: StepPolicy,
    /// Every gate.
    pub gate: StepPolicy,
}

impl StepPolicies {
    /// The policy for one step kind.
    #[must_use]
    pub const fn for_step(&self, kind: StepKind) -> StepPolicy {
        match kind {
            StepKind::Preflight => self.preflight,
            StepKind::Implement => self.implement,
            StepKind::Review => self.review,
            StepKind::OpenPr => self.open_pr,
            StepKind::Gate(_) => self.gate,
        }
    }
}

impl Default for StepPolicies {
    /// Agent steps get two spare attempts to survive a lost lease or a crash;
    /// gates hold for days, so only daemon downtime burns their attempts.
    fn default() -> Self {
        let policy = |lease, deadline, attempts| StepPolicy {
            lease: LeaseSecs::new(lease).expect("default lease is above the floor"),
            deadline: NonZeroU32::new(deadline).map(DeadlineSecs::new),
            attempts: NonZeroU32::new(attempts).expect("default attempts are positive"),
        };
        Self {
            preflight: policy(300, 30 * 60, 3),
            implement: policy(300, 4 * 3600, 3),
            review: policy(300, 3600, 3),
            open_pr: policy(120, 10 * 60, 5),
            gate: policy(900, 0, 10),
        }
    }
}

/// The queues a worker claims from. A step's worktree is on the disk of the
/// machine that cut it, so every queue is named for its worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerQueues {
    /// Where agent steps and open-pr are offered.
    pub agent: TaskQueue,
    /// Where gates are offered.
    pub human: TaskQueue,
}

impl WorkerQueues {
    /// `vetinari-<worker>-agent` and `vetinari-<worker>-human`.
    #[must_use]
    pub fn for_worker(worker: &WorkerName) -> Self {
        let queue = |role| {
            TaskQueue::parse(&format!("vetinari-{worker}-{role}"))
                .expect("a worker name always leaves room for its queues")
        };
        Self {
            agent: queue("agent"),
            human: queue("human"),
        }
    }

    /// The queue a step is offered on: gates to a person, the rest to an
    /// agent.
    #[must_use]
    pub const fn for_step(&self, kind: StepKind) -> &TaskQueue {
        match kind {
            StepKind::Gate(_) => &self.human,
            _ => &self.agent,
        }
    }
}

/// A list with at least one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    /// A list of `head` followed by `tail`.
    #[must_use]
    pub const fn new(head: T, tail: Vec<T>) -> Self {
        Self { head, tail }
    }

    /// Every entry, in order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.head).chain(&self.tail)
    }
}

/// What ends a loop: `node` producing `when`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopExit {
    /// A direct member of the loop's body.
    pub node: NodeId,
    /// The label that ends the loop.
    pub when: BranchLabel,
}

/// One node of a compiled graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphNode {
    /// A step a vetinari worker claims and executes.
    External {
        /// The node id.
        id: NodeId,
        /// Where it is offered.
        queue: TaskQueue,
        /// What the worker needs to run it.
        config: Box<StepConfig>,
        /// Lease, deadline and attempts.
        policy: StepPolicy,
    },
    /// Runs its body again until `exit` is met or `max_iterations` is reached.
    Loop {
        /// The node id.
        id: NodeId,
        /// The nodes run each iteration.
        body: NonEmpty<NodeId>,
        /// What ends the loop early.
        exit: LoopExit,
        /// The most iterations it runs.
        max_iterations: Rounds,
    },
}

impl GraphNode {
    /// The node id.
    #[must_use]
    pub const fn id(&self) -> &NodeId {
        match self {
            GraphNode::External { id, .. } | GraphNode::Loop { id, .. } => id,
        }
    }
}

/// An edge, taken when `from`'s output carries `when`, or always without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    /// The source node.
    pub from: NodeId,
    /// The target node.
    pub to: NodeId,
    /// The label that selects this edge.
    pub when: Option<BranchLabel>,
}

/// A compiled definition, ready to be mapped to the engine's public JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGraph {
    /// The definition id it is registered under.
    pub id: DefinitionId,
    /// Its nodes.
    pub nodes: Vec<GraphNode>,
    /// Its edges.
    pub edges: Vec<GraphEdge>,
}

/// Compile one run's definition of a template.
#[must_use]
pub fn compile(
    workflow: &DevWorkflow,
    id: DefinitionId,
    queues: &WorkerQueues,
    policy: &StepPolicies,
) -> WorkflowGraph {
    match workflow {
        DevWorkflow::FeatureFromTicket(feature) => feature_graph(feature, id, queues, policy),
    }
}

/// The step a template's node runs, or `None` for a loop or a node the
/// template does not have. A run graph names its nodes but not their
/// config, so this is how a view knows what a node is for.
#[must_use]
pub fn node_step(template: TemplateKind, node: &NodeId) -> Option<StepKind> {
    match template {
        TemplateKind::FeatureFromTicket => match node.as_str() {
            node::PREFLIGHT => Some(StepKind::Preflight),
            node::ASK_INFO => Some(StepKind::Gate(GateKind::InfoNeeded)),
            node::IMPLEMENT => Some(StepKind::Implement),
            node::REVIEW => Some(StepKind::Review),
            node::REVIEW_STUCK => Some(StepKind::Gate(GateKind::ReviewStuck)),
            node::OPEN_PR => Some(StepKind::OpenPr),
            _ => None,
        },
    }
}

/// The nodes a compiled graph is drawn as, each scoped to the loop whose
/// body names it.
#[must_use]
pub fn drawn(graph: &WorkflowGraph) -> Vec<RunNode> {
    scoped(
        graph
            .nodes
            .iter()
            .map(|node| match node {
                GraphNode::External { id, queue, .. } => (
                    id.clone(),
                    RunNodeKind::External {
                        queue: queue.clone(),
                    },
                ),
                GraphNode::Loop {
                    id,
                    body,
                    exit,
                    max_iterations,
                } => (
                    id.clone(),
                    RunNodeKind::Loop {
                        body: body.iter().cloned().collect(),
                        exit: exit.node.clone(),
                        exit_when: exit.when.clone(),
                        max_iterations: max_iterations.get(),
                    },
                ),
            })
            .collect(),
    )
}

/// A template as a run of it would be drawn before it starts: its nodes and
/// edges, with the given round limits, offered on `queues`. Nothing is
/// registered and no ticket is needed.
#[must_use]
pub fn preview(
    template: TemplateKind,
    intake: Rounds,
    review: Rounds,
    queues: &WorkerQueues,
) -> (Vec<RunNode>, Vec<GraphEdge>) {
    let (parts, edges) = match template {
        TemplateKind::FeatureFromTicket => feature_shape(intake, review),
    };
    let nodes = parts
        .into_iter()
        .map(|part| match part {
            Part::Step { id, kind, .. } => (
                NodeId::known(id),
                RunNodeKind::External {
                    queue: queues.for_step(kind).clone(),
                },
            ),
            Part::Loop {
                id,
                body,
                exit,
                when,
                max,
            } => (
                NodeId::known(id),
                RunNodeKind::Loop {
                    body: body.iter().map(|node| NodeId::known(node)).collect(),
                    exit: NodeId::known(exit),
                    exit_when: BranchLabel::known(when),
                    max_iterations: max.get(),
                },
            ),
        })
        .collect();
    (scoped(nodes), edges)
}

/// Nodes, each scoped to the loop whose body names it.
fn scoped(nodes: Vec<(NodeId, RunNodeKind)>) -> Vec<RunNode> {
    let scope_of = |id: &NodeId| {
        nodes.iter().find_map(|(loop_id, kind)| match kind {
            RunNodeKind::Loop { body, .. } if body.contains(id) => Some(loop_id.clone()),
            _ => None,
        })
    };
    let scopes: Vec<Option<NodeId>> = nodes.iter().map(|(id, _)| scope_of(id)).collect();
    nodes
        .into_iter()
        .zip(scopes)
        .map(|((id, kind), scope)| RunNode { id, kind, scope })
        .collect()
}

/// One node of a template before a run gives it a ticket.
enum Part {
    Step {
        id: &'static str,
        kind: StepKind,
        skill: Option<&'static str>,
    },
    Loop {
        id: &'static str,
        body: [&'static str; 2],
        exit: &'static str,
        when: &'static str,
        max: Rounds,
    },
}

/// [`FeatureFromTicket`]'s nodes and edges, the same for every run.
fn feature_shape(intake: Rounds, review: Rounds) -> (Vec<Part>, Vec<GraphEdge>) {
    let step = |id, kind, skill| Part::Step { id, kind, skill };
    let edge = |from: &'static str, to: &'static str, when: &'static str| GraphEdge {
        from: NodeId::known(from),
        to: NodeId::known(to),
        when: Some(BranchLabel::known(when)),
    };
    (
        vec![
            Part::Loop {
                id: node::INTAKE,
                body: [node::PREFLIGHT, node::ASK_INFO],
                exit: node::PREFLIGHT,
                when: label::READY,
                max: intake,
            },
            step(
                node::PREFLIGHT,
                StepKind::Preflight,
                Some(skill::TICKET_PREFLIGHT),
            ),
            step(node::ASK_INFO, StepKind::Gate(GateKind::InfoNeeded), None),
            Part::Loop {
                id: node::REWORK,
                body: [node::IMPLEMENT, node::REVIEW],
                exit: node::REVIEW,
                when: label::APPROVED,
                max: review,
            },
            step(
                node::IMPLEMENT,
                StepKind::Implement,
                Some(skill::IMPLEMENT_TDD),
            ),
            step(node::REVIEW, StepKind::Review, Some(skill::SELF_REVIEW)),
            step(
                node::REVIEW_STUCK,
                StepKind::Gate(GateKind::ReviewStuck),
                None,
            ),
            step(node::OPEN_PR, StepKind::OpenPr, None),
        ],
        vec![
            edge(node::PREFLIGHT, node::ASK_INFO, label::NEEDS_INFO),
            edge(node::INTAKE, node::REWORK, label::DONE),
            edge(node::IMPLEMENT, node::REVIEW, label::IMPLEMENTED),
            edge(node::REWORK, node::OPEN_PR, label::DONE),
            edge(node::REWORK, node::REVIEW_STUCK, label::EXHAUSTED),
            edge(node::REVIEW_STUCK, node::OPEN_PR, label::OPEN_ANYWAY),
        ],
    )
}

fn feature_graph(
    feature: &FeatureFromTicket,
    id: DefinitionId,
    queues: &WorkerQueues,
    policies: &StepPolicies,
) -> WorkflowGraph {
    let branch = feature.branch();
    let (parts, edges) = feature_shape(feature.max_intake_rounds, feature.max_review_rounds);
    let nodes = parts
        .into_iter()
        .map(|part| match part {
            Part::Step { id, kind, skill } => GraphNode::External {
                id: NodeId::known(id),
                queue: queues.for_step(kind).clone(),
                config: Box::new(StepConfig {
                    step: kind,
                    template: TemplateKind::FeatureFromTicket,
                    ticket: feature.ticket.clone(),
                    repo: feature.repo.clone(),
                    profile: feature.profile.clone(),
                    branch: branch.clone(),
                    base: feature.base.clone(),
                    skill: skill
                        .map(|skill| SkillId::parse(skill).expect("built-in skill id is valid")),
                    agent: feature.agent.clone().filter(|_| kind.is_agent_step()),
                }),
                policy: policies.for_step(kind),
            },
            Part::Loop {
                id,
                body,
                exit,
                when,
                max,
            } => GraphNode::Loop {
                id: NodeId::known(id),
                body: NonEmpty::new(NodeId::known(body[0]), vec![NodeId::known(body[1])]),
                exit: LoopExit {
                    node: NodeId::known(exit),
                    when: BranchLabel::known(when),
                },
                max_iterations: max,
            },
        })
        .collect();
    WorkflowGraph { id, nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::engine_rules;
    use crate::domain::devflow::outcome::{
        Finding, GateAnswer, ImplementOutcome, Line, OpenPrOutcome, PreflightOutcome,
        ReviewOutcome, ShortList, StepOutcome, Summary,
    };
    use crate::domain::devflow::template::sample_feature;
    use crate::domain::ids::AgentId;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn queues() -> WorkerQueues {
        WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker"))
    }

    fn compiled(feature: FeatureFromTicket) -> WorkflowGraph {
        compile(
            &DevWorkflow::FeatureFromTicket(feature),
            DefinitionId::parse("vetinari-feature-01JB7ZQ3").expect("id"),
            &queues(),
            &StepPolicies::default(),
        )
    }

    #[test]
    fn a_compiled_graph_is_drawn_with_each_loop_scoping_its_body() {
        let graph = compiled(sample_feature());
        let nodes = drawn(&graph);

        let scope = |id: &str| {
            nodes
                .iter()
                .find(|node| node.id.as_str() == id)
                .and_then(|node| node.scope.as_ref().map(NodeId::as_str))
        };
        assert_eq!(scope("preflight"), Some("intake"));
        assert_eq!(scope("review"), Some("rework"));
        assert_eq!(scope("open-pr"), None);
        let rework = nodes
            .iter()
            .find(|node| node.id.as_str() == "rework")
            .expect("rework");
        assert_eq!(
            rework.kind,
            RunNodeKind::Loop {
                body: vec![
                    NodeId::parse("implement").expect("id"),
                    NodeId::parse("review").expect("id")
                ],
                exit: NodeId::parse("review").expect("id"),
                exit_when: BranchLabel::parse("approved").expect("label"),
                max_iterations: nz(3),
            }
        );
        let ask = nodes
            .iter()
            .find(|node| node.id.as_str() == "ask-info")
            .expect("ask-info");
        assert_eq!(
            ask.kind,
            RunNodeKind::External {
                queue: queues().human
            }
        );
    }

    #[test]
    fn a_preview_draws_what_a_run_with_the_same_rounds_registers() {
        let mut feature = sample_feature();
        feature.max_intake_rounds = Rounds::new(2).expect("rounds");
        feature.max_review_rounds = Rounds::new(5).expect("rounds");
        let graph = compiled(feature.clone());

        let (nodes, edges) = preview(
            TemplateKind::FeatureFromTicket,
            feature.max_intake_rounds,
            feature.max_review_rounds,
            &queues(),
        );

        assert_eq!(nodes, drawn(&graph));
        assert_eq!(edges, graph.edges);
    }

    #[test]
    fn every_external_node_runs_the_step_node_step_names() {
        for graph in every_template() {
            for node in &graph.nodes {
                let expected = match node {
                    GraphNode::External { config, .. } => Some(config.step),
                    GraphNode::Loop { .. } => None,
                };
                assert_eq!(
                    node_step(TemplateKind::FeatureFromTicket, node.id()),
                    expected,
                    "{}",
                    node.id()
                );
            }
        }
        assert_eq!(
            node_step(
                TemplateKind::FeatureFromTicket,
                &NodeId::parse("deploy").expect("id")
            ),
            None
        );
    }

    /// Every template this crate can compile, for the checks that must hold
    /// of all of them.
    fn every_template() -> Vec<WorkflowGraph> {
        let mut pinned = sample_feature();
        pinned.agent = Some(AgentId::parse("claude-code").expect("id"));
        vec![compiled(sample_feature()), compiled(pinned)]
    }

    fn node<'a>(graph: &'a WorkflowGraph, id: &str) -> &'a GraphNode {
        graph
            .nodes
            .iter()
            .find(|node| node.id().as_str() == id)
            .unwrap_or_else(|| panic!("no node {id}"))
    }

    fn config<'a>(graph: &'a WorkflowGraph, id: &str) -> &'a StepConfig {
        match node(graph, id) {
            GraphNode::External { config, .. } => config,
            GraphNode::Loop { .. } => panic!("{id} is a loop"),
        }
    }

    #[test]
    fn a_lease_is_at_least_thirty_seconds() {
        assert_eq!(LeaseSecs::new(30).expect("valid").get(), 30);
        assert_eq!(LeaseSecs::new(29), Err(LeaseSecsError { found: 29 }));
    }

    #[test]
    fn a_workers_queues_are_named_for_it() {
        let queues = queues();
        assert_eq!(queues.agent.as_str(), "vetinari-hex-agent");
        assert_eq!(queues.human.as_str(), "vetinari-hex-human");
        let longest = WorkerName::parse(&"a".repeat(WorkerName::MAX_LEN)).expect("worker");
        assert_eq!(
            WorkerQueues::for_worker(&longest).human.as_str().len(),
            TaskQueue::MAX_LEN
        );
    }

    #[test]
    fn the_default_policies_follow_the_design_table() {
        let policies = StepPolicies::default();
        let row = |kind| {
            let policy = policies.for_step(kind);
            (
                policy.lease.get(),
                policy.deadline.map(|deadline| deadline.get().get()),
                policy.attempts.get(),
            )
        };
        assert_eq!(row(StepKind::Preflight), (300, Some(1800), 3));
        assert_eq!(row(StepKind::Implement), (300, Some(4 * 3600), 3));
        assert_eq!(row(StepKind::Review), (300, Some(3600), 3));
        assert_eq!(row(StepKind::OpenPr), (120, Some(600), 5));
        assert_eq!(row(StepKind::Gate(GateKind::InfoNeeded)), (900, None, 10));
        assert_eq!(row(StepKind::Gate(GateKind::ReviewStuck)), (900, None, 10));
    }

    #[test]
    fn the_feature_graph_has_an_intake_loop_a_rework_loop_and_open_pr() {
        let graph = compiled(sample_feature());
        let ids: Vec<_> = graph.nodes.iter().map(|node| node.id().as_str()).collect();
        assert_eq!(
            ids,
            [
                "intake",
                "preflight",
                "ask-info",
                "rework",
                "implement",
                "review",
                "review-stuck",
                "open-pr"
            ]
        );
        let edges: Vec<_> = graph
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.from.as_str(),
                    edge.to.as_str(),
                    edge.when.as_ref().map(BranchLabel::as_str),
                )
            })
            .collect();
        assert_eq!(
            edges,
            [
                ("preflight", "ask-info", Some("needs-info")),
                ("intake", "rework", Some("done")),
                ("implement", "review", Some("implemented")),
                ("rework", "open-pr", Some("done")),
                ("rework", "review-stuck", Some("exhausted")),
                ("review-stuck", "open-pr", Some("open-anyway")),
            ]
        );
    }

    #[test]
    fn the_loops_exit_on_ready_and_approved_within_their_rounds() {
        let mut feature = sample_feature();
        feature.max_intake_rounds = Rounds::new(2).expect("rounds");
        feature.max_review_rounds = Rounds::new(5).expect("rounds");
        let graph = compiled(feature);
        let GraphNode::Loop {
            body,
            exit,
            max_iterations,
            ..
        } = node(&graph, "intake")
        else {
            panic!("intake is a loop")
        };
        let body: Vec<_> = body.iter().map(NodeId::as_str).collect();
        assert_eq!(body, ["preflight", "ask-info"]);
        assert_eq!(
            (exit.node.as_str(), exit.when.as_str()),
            ("preflight", "ready")
        );
        assert_eq!(max_iterations.get(), nz(2));

        let GraphNode::Loop {
            body,
            exit,
            max_iterations,
            ..
        } = node(&graph, "rework")
        else {
            panic!("rework is a loop")
        };
        let body: Vec<_> = body.iter().map(NodeId::as_str).collect();
        assert_eq!(body, ["implement", "review"]);
        assert_eq!(
            (exit.node.as_str(), exit.when.as_str()),
            ("review", "approved")
        );
        assert_eq!(max_iterations.get(), nz(5));
    }

    #[test]
    fn agent_steps_go_to_the_agent_queue_and_gates_to_the_human_queue() {
        let graph = compiled(sample_feature());
        for node in &graph.nodes {
            if let GraphNode::External {
                id,
                queue,
                config,
                policy,
            } = node
            {
                let expected = match config.step {
                    StepKind::Gate(_) => &queues().human,
                    _ => &queues().agent,
                };
                assert_eq!(queue, expected, "{id}");
                assert_eq!(policy, &StepPolicies::default().for_step(config.step));
            }
        }
    }

    #[test]
    fn every_step_carries_the_run_and_agent_steps_their_skill() {
        let feature = sample_feature();
        let graph = compiled(feature.clone());
        let steps = [
            ("preflight", StepKind::Preflight, Some("ticket-preflight")),
            ("ask-info", StepKind::Gate(GateKind::InfoNeeded), None),
            ("implement", StepKind::Implement, Some("implement-tdd")),
            ("review", StepKind::Review, Some("self-review")),
            ("review-stuck", StepKind::Gate(GateKind::ReviewStuck), None),
            ("open-pr", StepKind::OpenPr, None),
        ];
        for (id, step, skill) in steps {
            let config = config(&graph, id);
            assert_eq!(config.step, step, "{id}");
            assert_eq!(config.skill.as_ref().map(SkillId::as_str), skill, "{id}");
            assert_eq!(config.template, TemplateKind::FeatureFromTicket);
            assert_eq!(config.ticket, feature.ticket);
            assert_eq!(config.repo, feature.repo);
            assert_eq!(config.profile, feature.profile);
            assert_eq!(config.branch, feature.branch());
            assert_eq!(config.base, feature.base);
            assert_eq!(config.agent, None);
        }
    }

    #[test]
    fn a_pinned_agent_is_written_into_every_agent_step_only() {
        let mut feature = sample_feature();
        let agent = AgentId::parse("claude-code").expect("id");
        feature.agent = Some(agent.clone());
        let graph = compiled(feature);
        for id in ["preflight", "implement", "review"] {
            assert_eq!(config(&graph, id).agent.as_ref(), Some(&agent), "{id}");
        }
        for id in ["ask-info", "review-stuck", "open-pr"] {
            assert_eq!(config(&graph, id).agent, None, "{id}");
        }
    }

    /// One outcome of every variant a step kind can report.
    fn every_outcome(kind: StepKind) -> Vec<StepOutcome> {
        let summary = || Summary::parse("s").expect("summary");
        let line = || Line::parse("l").expect("line");
        match kind {
            StepKind::Preflight => vec![
                StepOutcome::Preflight(PreflightOutcome::Ready { summary: summary() }),
                StepOutcome::Preflight(PreflightOutcome::NeedsInfo {
                    questions: ShortList::parse(vec![line()]).expect("list"),
                }),
            ],
            StepKind::Implement => vec![StepOutcome::Implement(ImplementOutcome::Implemented {
                summary: summary(),
            })],
            StepKind::Review => vec![
                StepOutcome::Review(ReviewOutcome::Approved { summary: summary() }),
                StepOutcome::Review(ReviewOutcome::ChangesRequested {
                    findings: ShortList::parse(vec![Finding {
                        file: None,
                        line: None,
                        text: line(),
                    }])
                    .expect("list"),
                }),
            ],
            StepKind::OpenPr => vec![StepOutcome::OpenPr(OpenPrOutcome::Opened {
                number: 1,
                url: line(),
            })],
            StepKind::Gate(GateKind::InfoNeeded) => {
                vec![StepOutcome::Gate(GateAnswer::Recheck { note: None })]
            }
            StepKind::Gate(GateKind::ReviewStuck) => {
                vec![StepOutcome::Gate(GateAnswer::OpenAnyway)]
            }
        }
    }

    #[test]
    fn every_compiled_edge_label_is_produced_by_its_source() {
        for graph in every_template() {
            for edge in &graph.edges {
                let Some(when) = &edge.when else { continue };
                let produced: Vec<BranchLabel> = match node(&graph, edge.from.as_str()) {
                    GraphNode::External { config, .. } => every_outcome(config.step)
                        .iter()
                        .map(StepOutcome::branch)
                        .collect(),
                    GraphNode::Loop { .. } => [label::DONE, label::EXHAUSTED]
                        .into_iter()
                        .map(BranchLabel::known)
                        .collect(),
                };
                assert!(
                    produced.contains(when),
                    "{} -> {} on {when}: {} never produces it",
                    edge.from,
                    edge.to,
                    edge.from
                );
            }
        }
    }

    #[test]
    fn every_loop_exit_label_is_produced_by_its_exit_node() {
        for graph in every_template() {
            for node in &graph.nodes {
                let GraphNode::Loop { id, exit, .. } = node else {
                    continue;
                };
                let GraphNode::External { config, .. } = self::node(&graph, exit.node.as_str())
                else {
                    panic!("{id} exits on a loop");
                };
                assert!(
                    every_outcome(config.step)
                        .iter()
                        .any(|outcome| outcome.branch() == exit.when),
                    "{id} exits on {} which {} never produces",
                    exit.when,
                    exit.node
                );
            }
        }
    }

    #[test]
    fn every_template_satisfies_the_engines_graph_rules() {
        for graph in every_template() {
            assert_eq!(engine_rules::check(&graph), Ok(()));
        }
    }
}
