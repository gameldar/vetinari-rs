//! A run as one row of the queue: the phase strip and what needs the
//! developer (design 9.2).
//!
//! Phases come from step kinds, not node ids, so a second template gets a
//! strip without new code here.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use super::compile::node_step;
use super::ids::{NodeId, NodeInstance, RunId};
use super::run_view::{InstanceState, InstanceStatus, RunGraph, RunNode, RunNodeKind, RunStatus};
use super::step::{GateKind, StepKind};
use super::template::TemplateKind;
use crate::domain::ids::SessionId;

/// A column of the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Preflight and its gate.
    Pre,
    /// Implementing.
    Impl,
    /// Review and its gate.
    Rev,
    /// Opening the pull request.
    Pr,
}

impl Phase {
    /// The phase a step belongs to.
    #[must_use]
    pub const fn of(step: StepKind) -> Self {
        match step {
            StepKind::Preflight | StepKind::Gate(GateKind::InfoNeeded) => Self::Pre,
            StepKind::Implement => Self::Impl,
            StepKind::Review | StepKind::Gate(GateKind::ReviewStuck) => Self::Rev,
            StepKind::OpenPr => Self::Pr,
        }
    }

    /// Every phase, in strip order.
    pub const ALL: [Self; 4] = [Self::Pre, Self::Impl, Self::Rev, Self::Pr];

    /// Its label on the strip.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Impl => "impl",
            Self::Rev => "rev",
            Self::Pr => "pr",
        }
    }
}

/// Where a phase is, as the strip draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Finished.
    Done,
    /// An agent is running it on this machine.
    RunningHere,
    /// In flight, but not running here: offered, claimed elsewhere, or the
    /// engine's.
    Elsewhere,
    /// An open gate or an agent question.
    NeedsYou,
    /// Failed.
    Failed,
    /// Next, not started.
    Next,
    /// Not reached.
    NotReached,
}

impl Mark {
    /// The strip's glyph.
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Done => '✓',
            Self::RunningHere => '●',
            Self::Elsewhere => '◐',
            Self::NeedsYou => '?',
            Self::Failed => '✗',
            Self::Next => '○',
            Self::NotReached => '·',
        }
    }

    /// Whether something is happening in the phase now.
    const fn is_active(self) -> bool {
        matches!(
            self,
            Self::RunningHere | Self::Elsewhere | Self::NeedsYou | Self::Failed
        )
    }
}

/// A loop's iteration and its bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Round {
    /// The iteration.
    pub iteration: NonZeroU32,
    /// The most iterations.
    pub bound: NonZeroU32,
}

/// One phase of the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseCell {
    /// Which phase.
    pub phase: Phase,
    /// Where it is.
    pub mark: Mark,
    /// The loop's round, shown once a loop is past its first.
    pub round: Option<Round>,
}

/// What the developer is waited on for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedsYou {
    /// An open gate.
    Gate {
        /// The gate's instance.
        instance: NodeInstance,
        /// Which gate.
        gate: GateKind,
    },
    /// An agent asked something.
    Question {
        /// The step's instance.
        instance: NodeInstance,
        /// What it asked.
        question: String,
    },
}

/// What only this machine knows about one of a run's steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalStep {
    /// An agent runs it here.
    Running {
        /// The agent's session.
        session: SessionId,
        /// What it is waiting to be told, if anything.
        question: Option<String>,
    },
    /// A gate held here, waiting for an answer.
    AwaitingPerson,
}

/// The steps of one run this machine holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalSteps(BTreeMap<NodeInstance, LocalStep>);

impl LocalSteps {
    /// Record a step.
    #[must_use]
    pub fn with(mut self, instance: NodeInstance, step: LocalStep) -> Self {
        self.0.insert(instance, step);
        self
    }

    /// What is known about an instance.
    #[must_use]
    pub fn get(&self, instance: &NodeInstance) -> Option<&LocalStep> {
        self.0.get(instance)
    }
}

/// A run as the queue shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The run.
    pub run: RunId,
    /// Where it is as a whole.
    pub status: RunStatus,
    /// The strip, in phase order.
    pub phases: Vec<PhaseCell>,
    /// What the developer is waited on for, first in phase order.
    pub needs_you: Option<NeedsYou>,
}

/// Summarise a run's graph for its queue row.
#[must_use]
pub fn summarise(graph: &RunGraph, template: TemplateKind, local: &LocalSteps) -> RunSummary {
    let mut phases = Vec::new();
    let mut needs_you = None;
    let mut before_done = true;
    for phase in Phase::ALL {
        let nodes: Vec<(&RunNode, StepKind)> = graph
            .nodes
            .iter()
            .filter_map(|node| {
                node_step(template, &node.id)
                    .filter(|step| Phase::of(*step) == phase)
                    .map(|step| (node, step))
            })
            .collect();
        if nodes.is_empty() {
            continue;
        }
        let mut mark = Seen::Pending;
        let mut pending = false;
        for (node, step) in &nodes {
            let seen = match current(graph, node) {
                Current::Instance(state) => {
                    let (seen, asks) = seen(state, *step, local);
                    if needs_you.is_none() {
                        needs_you = asks;
                    }
                    seen
                }
                Current::Pending => Seen::Pending,
                Current::Unreached => Seen::Unreached,
            };
            pending |= seen == Seen::Pending;
            mark = mark.max(seen);
        }
        let mark = match mark {
            Seen::NeedsYou => Mark::NeedsYou,
            Seen::Failed => Mark::Failed,
            Seen::RunningHere => Mark::RunningHere,
            Seen::Elsewhere => Mark::Elsewhere,
            Seen::Pending => Mark::Next,
            Seen::Done => Mark::Done,
            Seen::Unreached if pending => Mark::Next,
            Seen::Unreached if before_done && !graph.status.is_finished() => Mark::Next,
            Seen::Unreached => Mark::NotReached,
        };
        before_done = before_done && mark == Mark::Done;
        let round = round(graph, &nodes)
            .filter(|round| round.iteration.get() > 1 && (mark.is_active() || mark == Mark::Done));
        phases.push(PhaseCell { phase, mark, round });
    }
    RunSummary {
        run: graph.run.clone(),
        status: graph.status,
        phases,
        needs_you,
    }
}

/// What one node contributes to its phase, weakest first, so a phase shows
/// the strongest thing any of its nodes is doing. `Pending`, a node its
/// loop's round has not reached, is weakest: the round may never reach it
/// (a gate on a branch not taken), so it marks the phase next only when no
/// node of the phase has anything else to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Seen {
    Pending,
    Unreached,
    Done,
    Elsewhere,
    RunningHere,
    Failed,
    NeedsYou,
}

/// A node's instance in the round that matters now.
enum Current<'a> {
    Instance(&'a InstanceState),
    /// Its loop is on a round it has not reached yet.
    Pending,
    Unreached,
}

/// The instance to show for a node: the one in its loop's current round
/// while the loop runs, else its latest.
fn current<'a>(graph: &'a RunGraph, node: &RunNode) -> Current<'a> {
    let looping = node.scope.as_ref().and_then(|scope| {
        graph.instances.iter().find_map(|state| {
            match (&state.status, state.instance.iterations().is_empty()) {
                (InstanceStatus::Looping { iteration }, true) if state.instance.node() == scope => {
                    Some(*iteration)
                }
                _ => None,
            }
        })
    });
    if let Some(iteration) = looping {
        let wanted = NodeInstance::within(node.id.clone(), vec![iteration]);
        return graph
            .instances
            .iter()
            .find(|state| state.instance == wanted)
            .map_or(Current::Pending, Current::Instance);
    }
    graph
        .instances
        .iter()
        .filter(|state| state.instance.node() == &node.id)
        .max_by(|left, right| left.instance.iterations().cmp(right.instance.iterations()))
        .map_or(Current::Unreached, Current::Instance)
}

fn seen(state: &InstanceState, step: StepKind, local: &LocalSteps) -> (Seen, Option<NeedsYou>) {
    match local.get(&state.instance) {
        Some(LocalStep::AwaitingPerson) => {
            let asks = match step {
                StepKind::Gate(gate) => Some(NeedsYou::Gate {
                    instance: state.instance.clone(),
                    gate,
                }),
                _ => None,
            };
            return (Seen::NeedsYou, asks);
        }
        Some(LocalStep::Running {
            question: Some(question),
            ..
        }) => {
            return (
                Seen::NeedsYou,
                Some(NeedsYou::Question {
                    instance: state.instance.clone(),
                    question: question.clone(),
                }),
            );
        }
        Some(LocalStep::Running { question: None, .. })
            if !matches!(state.status, InstanceStatus::Failed { .. }) =>
        {
            return (Seen::RunningHere, None);
        }
        Some(LocalStep::Running { .. }) | None => {}
    }
    let seen = match state.status {
        InstanceStatus::Failed { .. } => Seen::Failed,
        InstanceStatus::Ready
        | InstanceStatus::Running
        | InstanceStatus::Waiting { .. }
        | InstanceStatus::Offered { .. }
        | InstanceStatus::Claimed { .. }
        | InstanceStatus::Looping { .. } => Seen::Elsewhere,
        InstanceStatus::Done { .. } => Seen::Done,
        InstanceStatus::Skipped | InstanceStatus::Cancelled => Seen::Unreached,
    };
    (seen, None)
}

/// The round of the loop a phase's nodes run in: its current iteration
/// while it runs, else the last one any of its body reached.
fn round(graph: &RunGraph, nodes: &[(&RunNode, StepKind)]) -> Option<Round> {
    let scope = nodes.iter().find_map(|(node, _)| node.scope.as_ref())?;
    let bound = graph.nodes.iter().find_map(|node| match &node.kind {
        RunNodeKind::Loop { max_iterations, .. } if node.id == *scope => Some(*max_iterations),
        _ => None,
    })?;
    let looping = graph.instances.iter().find_map(|state| match state.status {
        InstanceStatus::Looping { iteration } if state.instance.node() == scope => Some(iteration),
        _ => None,
    });
    let iteration = looping.or_else(|| {
        let body: Vec<&NodeId> = graph
            .nodes
            .iter()
            .filter(|node| node.scope.as_ref() == Some(scope))
            .map(|node| &node.id)
            .collect();
        graph
            .instances
            .iter()
            .filter(|state| body.contains(&state.instance.node()))
            .filter_map(|state| state.instance.iterations().first().copied())
            .max()
    })?;
    Some(Round { iteration, bound })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::ids::BranchLabel;
    use crate::domain::devflow::run_view::tests::{feature_graph, instance, nz, queue, state};
    use time::macros::datetime;

    fn done(label: Option<&str>) -> InstanceStatus {
        InstanceStatus::Done {
            branch: label.map(|label| BranchLabel::parse(label).expect("label")),
        }
    }

    fn claimed() -> InstanceStatus {
        InstanceStatus::Claimed {
            worker: "hex".to_owned(),
            expires_at: datetime!(2026-10-25 10:30 UTC),
        }
    }

    fn strip(summary: &RunSummary) -> String {
        summary
            .phases
            .iter()
            .map(|cell| {
                let round = cell
                    .round
                    .map(|round| format!("{}/{}", round.iteration, round.bound))
                    .unwrap_or_default();
                format!("{}{}{round}", cell.phase.name(), cell.mark.glyph())
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn summary(instances: Vec<InstanceState>, local: &LocalSteps) -> RunSummary {
        summarise(
            &feature_graph(9, instances),
            TemplateKind::FeatureFromTicket,
            local,
        )
    }

    fn session() -> SessionId {
        SessionId::parse("4f2a").expect("session")
    }

    fn second_round() -> Vec<InstanceState> {
        vec![
            state("intake", done(Some("done"))),
            state("preflight@1", done(Some("ready"))),
            state("rework", InstanceStatus::Looping { iteration: nz(2) }),
            state("implement@1", done(None)),
            state("review@1", done(Some("changes-requested"))),
            state("implement@2", claimed()),
        ]
    }

    #[test]
    fn a_step_running_here_in_a_later_round_shows_its_round() {
        let local = LocalSteps::default().with(
            instance("implement@2"),
            LocalStep::Running {
                session: session(),
                question: None,
            },
        );
        let summary = summary(second_round(), &local);
        assert_eq!(strip(&summary), "pre✓ impl●2/3 rev○ pr·");
        assert_eq!(summary.needs_you, None);
    }

    #[test]
    fn a_gate_held_here_needs_you() {
        let local = LocalSteps::default().with(instance("ask-info@1"), LocalStep::AwaitingPerson);
        let summary = summary(
            vec![
                state("intake", InstanceStatus::Looping { iteration: nz(1) }),
                state("preflight@1", done(Some("needs-info"))),
                state("ask-info@1", claimed()),
            ],
            &local,
        );
        assert_eq!(strip(&summary), "pre? impl· rev· pr·");
        assert_eq!(
            summary.needs_you,
            Some(NeedsYou::Gate {
                instance: instance("ask-info@1"),
                gate: GateKind::InfoNeeded,
            })
        );
    }

    #[test]
    fn an_agent_question_needs_you() {
        let local = LocalSteps::default().with(
            instance("implement@2"),
            LocalStep::Running {
                session: session(),
                question: Some("Which client?".to_owned()),
            },
        );
        let summary = summary(second_round(), &local);
        assert_eq!(strip(&summary), "pre✓ impl?2/3 rev○ pr·");
        assert_eq!(
            summary.needs_you,
            Some(NeedsYou::Question {
                instance: instance("implement@2"),
                question: "Which client?".to_owned(),
            })
        );
    }

    #[test]
    fn a_step_in_flight_but_not_here_is_elsewhere() {
        let summary = summary(
            vec![
                state("intake", done(Some("done"))),
                state("preflight@1", done(Some("ready"))),
                state("rework", InstanceStatus::Looping { iteration: nz(1) }),
                state(
                    "implement@1",
                    InstanceStatus::Offered {
                        queue: queue("vetinari-hex-agent"),
                    },
                ),
            ],
            &LocalSteps::default(),
        );
        assert_eq!(strip(&summary), "pre✓ impl◐ rev○ pr·");
    }

    #[test]
    fn a_failed_step_shows_failed() {
        let mut instances = second_round();
        instances[5] = state(
            "implement@2",
            InstanceStatus::Failed {
                error: "agent crashed".to_owned(),
            },
        );
        let summary = summary(instances, &LocalSteps::default());
        assert_eq!(strip(&summary), "pre✓ impl✗2/3 rev○ pr·");
    }

    #[test]
    fn a_node_its_round_may_never_reach_does_not_hold_the_phase_open() {
        let summary = summary(
            vec![
                state("intake", InstanceStatus::Looping { iteration: nz(1) }),
                state("preflight@1", done(Some("ready"))),
            ],
            &LocalSteps::default(),
        );
        assert_eq!(strip(&summary), "pre✓ impl○ rev· pr·");
    }

    #[test]
    fn a_fresh_run_is_next_at_its_first_phase() {
        let summary = summary(Vec::new(), &LocalSteps::default());
        assert_eq!(strip(&summary), "pre○ impl· rev· pr·");
    }

    #[test]
    fn a_finished_run_keeps_the_round_a_loop_took_more_than_one_of() {
        let summary = summary(
            vec![
                state("intake", done(Some("done"))),
                state("preflight@1", done(Some("ready"))),
                state("rework", done(Some("done"))),
                state("implement@1", done(None)),
                state("review@1", done(Some("changes-requested"))),
                state("implement@2", done(None)),
                state("review@2", done(Some("approved"))),
                state("review-stuck", InstanceStatus::Skipped),
                state("open-pr", done(Some("opened"))),
            ],
            &LocalSteps::default(),
        );
        assert_eq!(strip(&summary), "pre✓ impl✓2/3 rev✓2/3 pr✓");
    }

    #[test]
    fn every_step_kind_has_a_phase() {
        assert_eq!(
            [
                StepKind::Preflight,
                StepKind::Gate(GateKind::InfoNeeded),
                StepKind::Implement,
                StepKind::Review,
                StepKind::Gate(GateKind::ReviewStuck),
                StepKind::OpenPr,
            ]
            .map(Phase::of),
            [
                Phase::Pre,
                Phase::Pre,
                Phase::Impl,
                Phase::Rev,
                Phase::Rev,
                Phase::Pr
            ]
        );
    }
}
