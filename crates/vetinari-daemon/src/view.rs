//! The web page's view of the daemon's runs: domain types mapped to
//! `vetinari_web_model`, with each graph laid out by the domain.

use vetinari_core::application::devflow::api::{BoardChange, DetailRow, RunDetail, RunItem};
use vetinari_core::domain::devflow::compile::GraphEdge;
use vetinari_core::domain::devflow::lanes::RowKind;
use vetinari_core::domain::devflow::layout::{Layout, PlacedNode, layout};
use vetinari_core::domain::devflow::run_view::{
    InstanceState, InstanceStatus, RunGraph, RunNode, RunNodeKind, RunStatus,
};
use vetinari_core::domain::devflow::step::StepKind;
use vetinari_core::domain::devflow::summary::{self, LocalSteps, Mark, NeedsYou};
use vetinari_web_model as model;
use vetinari_web_model::{Drawing, ListEvent, RunEntry, RunView};

/// A run as the list shows it.
#[must_use]
pub fn entry(item: &RunItem) -> RunEntry {
    let summary = &item.summary;
    RunEntry {
        run: summary.run.as_str().to_owned(),
        ticket: item
            .label
            .as_ref()
            .map(|label| label.ticket.as_str().to_owned()),
        title: item
            .label
            .as_ref()
            .map(|label| label.title.as_str().to_owned()),
        status: status(summary.status),
        phases: summary
            .phases
            .iter()
            .map(|cell| model::Phase {
                name: cell.phase.name().to_owned(),
                mark: mark(cell.mark),
                glyph: cell.mark.glyph().to_string(),
                round: cell.round.map(|round| model::Round {
                    iteration: round.iteration.get(),
                    bound: round.bound.get(),
                }),
            })
            .collect(),
        needs_you: summary.needs_you.as_ref().map(|needs| match needs {
            NeedsYou::Gate { instance, gate } => model::NeedsYou::Gate {
                instance: instance.to_string(),
                gate: StepKind::Gate(*gate).name().to_owned(),
            },
            NeedsYou::Question { instance, question } => model::NeedsYou::Question {
                instance: instance.to_string(),
                question: question.clone(),
            },
        }),
        started_at: item.started_at.and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        }),
    }
}

/// A change to the board as the list hears it.
#[must_use]
pub fn list_event(change: &BoardChange) -> ListEvent {
    match change {
        BoardChange::Run(item) => ListEvent::Run { entry: entry(item) },
        BoardChange::Removed(run) => ListEvent::Removed {
            run: run.as_str().to_owned(),
        },
    }
}

/// A run drawn at the rounds its detail was taken at.
#[must_use]
pub fn run_view(detail: &RunDetail, graph: &RunGraph) -> RunView {
    let row = |placed: &PlacedNode| {
        detail
            .rows
            .iter()
            .find(|row: &&DetailRow| row.row.node == placed.node)
    };
    let drawing = drawing(layout(&graph.nodes, &graph.edges), |placed| {
        match row(placed) {
            Some(row) => (
                match row.row.kind {
                    RowKind::Node => model::NodeKind::Step,
                    RowKind::Loop {
                        reached,
                        selected,
                        bound,
                    } => model::NodeKind::Loop {
                        reached,
                        selected: selected.get(),
                        bound: bound.get(),
                    },
                },
                row.mark,
                row.row.state.as_ref().map(node_state),
            ),
            None => (model::NodeKind::Step, Mark::NotReached, None),
        }
    });
    RunView {
        entry: entry(&detail.item),
        drawing,
        questions: detail
            .notes
            .questions
            .iter()
            .map(|line| line.as_str().to_owned())
            .collect(),
        findings: detail
            .notes
            .findings
            .as_ref()
            .map(|findings| model::Findings {
                round: findings.round.get(),
                items: findings
                    .findings
                    .iter()
                    .map(|finding| model::Finding {
                        text: finding.text.as_str().to_owned(),
                        file: finding.file.as_ref().map(|file| file.as_str().to_owned()),
                        line: finding.line.map(std::num::NonZeroU32::get),
                    })
                    .collect(),
            }),
    }
}

/// A template's nodes and edges drawn before any run, every node unreached.
#[must_use]
pub fn preview_drawing(nodes: &[RunNode], edges: &[GraphEdge]) -> Drawing {
    let unreached = summary::row_mark(None, &LocalSteps::default());
    drawing(layout(nodes, edges), |placed| {
        let kind = match nodes
            .iter()
            .find(|node| node.id == placed.node)
            .map(|node| &node.kind)
        {
            Some(RunNodeKind::Loop { max_iterations, .. }) => model::NodeKind::Loop {
                reached: 0,
                selected: 1,
                bound: max_iterations.get(),
            },
            _ => model::NodeKind::Step,
        };
        (kind, unreached, None)
    })
}

/// A layout, each node dressed by `dress` with its kind, mark and state.
fn drawing(
    layout: Layout,
    dress: impl Fn(&PlacedNode) -> (model::NodeKind, Mark, Option<model::NodeState>),
) -> Drawing {
    Drawing {
        width: layout.width,
        height: layout.height,
        nodes: layout
            .nodes
            .iter()
            .map(|placed| {
                let (kind, node_mark, state) = dress(placed);
                model::ViewNode {
                    node: placed.node.as_str().to_owned(),
                    rect: model::Rect {
                        x: placed.rect.x,
                        y: placed.rect.y,
                        width: placed.rect.width,
                        height: placed.rect.height,
                    },
                    scope: placed.scope.as_ref().map(|scope| scope.as_str().to_owned()),
                    kind,
                    mark: mark(node_mark),
                    glyph: node_mark.glyph().to_string(),
                    state,
                }
            })
            .collect(),
        edges: layout
            .edges
            .iter()
            .map(|edge| model::ViewEdge {
                from: edge.from.as_str().to_owned(),
                to: edge.to.as_str().to_owned(),
                when: edge.when.as_ref().map(|when| when.as_str().to_owned()),
                points: edge
                    .points
                    .iter()
                    .map(|point| model::Point {
                        x: point.x,
                        y: point.y,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn node_state(state: &InstanceState) -> model::NodeState {
    let (status, detail) = match &state.status {
        InstanceStatus::Ready => ("ready", None),
        InstanceStatus::Running => ("running", None),
        InstanceStatus::Waiting { until } => (
            "waiting",
            until
                .format(&time::format_description::well_known::Rfc3339)
                .ok(),
        ),
        InstanceStatus::Offered { queue } => ("offered", Some(queue.as_str().to_owned())),
        InstanceStatus::Claimed { worker, .. } => ("claimed", Some(worker.clone())),
        InstanceStatus::Looping { iteration } => ("looping", Some(iteration.to_string())),
        InstanceStatus::Done { branch } => (
            "done",
            branch.as_ref().map(|branch| branch.as_str().to_owned()),
        ),
        InstanceStatus::Failed { error } => ("failed", Some(error.clone())),
        InstanceStatus::Skipped => ("skipped", None),
        InstanceStatus::Cancelled => ("cancelled", None),
    };
    model::NodeState {
        instance: state.instance.to_string(),
        status: status.to_owned(),
        detail,
        attempt: state.attempt.map(std::num::NonZeroU32::get),
    }
}

const fn mark(mark: Mark) -> model::Mark {
    match mark {
        Mark::Done => model::Mark::Done,
        Mark::RunningHere => model::Mark::RunningHere,
        Mark::Elsewhere => model::Mark::Elsewhere,
        Mark::NeedsYou => model::Mark::NeedsYou,
        Mark::Failed => model::Mark::Failed,
        Mark::Next => model::Mark::Next,
        Mark::NotReached => model::Mark::NotReached,
    }
}

const fn status(status: RunStatus) -> model::RunStatus {
    match status {
        RunStatus::Active => model::RunStatus::Active,
        RunStatus::Completed => model::RunStatus::Completed,
        RunStatus::Failed => model::RunStatus::Failed,
        RunStatus::Cancelled => model::RunStatus::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use super::*;
    use serde_json::json;
    use vetinari_core::application::devflow::api::RunLabel;
    use vetinari_core::domain::devflow::compile::{WorkerQueues, preview};
    use vetinari_core::domain::devflow::ids::{
        BranchLabel, NodeInstance, Rounds, RunId, WorkerName,
    };
    use vetinari_core::domain::devflow::lanes::IterationSelection;
    use vetinari_core::domain::devflow::layout::{LOOP_HEADER, layout};
    use vetinari_core::domain::devflow::run_view::{InstanceState, InstanceStatus, RunStatus};
    use vetinari_core::domain::devflow::step::GateKind;
    use vetinari_core::domain::devflow::summary::{
        LocalSteps, Mark, NeedsYou, Phase, PhaseCell, Round, RunSummary,
    };
    use vetinari_core::domain::devflow::template::TemplateKind;
    use vetinari_core::domain::text::NonEmptyText;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn run() -> RunId {
        RunId::parse("01JB7ZQ3").expect("run")
    }

    fn item() -> RunItem {
        RunItem {
            summary: RunSummary {
                run: run(),
                status: RunStatus::Active,
                phases: vec![PhaseCell {
                    phase: Phase::Rev,
                    mark: Mark::RunningHere,
                    round: Some(Round {
                        iteration: nz(2),
                        bound: nz(3),
                    }),
                }],
                needs_you: Some(NeedsYou::Gate {
                    instance: NodeInstance::parse("review-stuck").expect("instance"),
                    gate: GateKind::ReviewStuck,
                }),
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: None,
        }
    }

    fn state(instance: &str, status: InstanceStatus) -> InstanceState {
        InstanceState {
            instance: NodeInstance::parse(instance).expect("instance"),
            status,
            attempt: None,
            first_ready_at: None,
            finished_at: None,
        }
    }

    fn feature_graph(instances: Vec<InstanceState>) -> RunGraph {
        let (nodes, edges) = preview(
            TemplateKind::FeatureFromTicket,
            Rounds::DEFAULT,
            Rounds::DEFAULT,
            &WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
        );
        RunGraph {
            run: run(),
            status: RunStatus::Active,
            seq: 9,
            nodes,
            edges,
            instances,
        }
    }

    fn drawn<'a>(view: &'a Drawing, node: &str) -> &'a model::ViewNode {
        view.nodes
            .iter()
            .find(|drawn| drawn.node == node)
            .unwrap_or_else(|| panic!("{node} is not drawn"))
    }

    #[test]
    fn an_entry_carries_the_ticket_the_strip_and_what_needs_you() {
        let entry = entry(&item());

        assert_eq!(entry.run, "01JB7ZQ3");
        assert_eq!(entry.ticket.as_deref(), Some("ENG-412"));
        assert_eq!(entry.title.as_deref(), Some("Rate-limit retries"));
        assert_eq!(entry.status, model::RunStatus::Active);
        assert_eq!(
            entry.phases,
            [model::Phase {
                name: "rev".to_owned(),
                mark: model::Mark::RunningHere,
                glyph: Mark::RunningHere.glyph().to_string(),
                round: Some(model::Round {
                    iteration: 2,
                    bound: 3
                }),
            }]
        );
        assert_eq!(
            entry.needs_you,
            Some(model::NeedsYou::Gate {
                instance: "review-stuck".to_owned(),
                gate: "review-stuck".to_owned(),
            })
        );
    }

    #[test]
    fn a_removal_is_heard_as_the_run_leaving_the_list() {
        assert_eq!(
            serde_json::to_value(list_event(&BoardChange::Removed(run()))).expect("json"),
            json!({ "type": "removed", "run": "01JB7ZQ3" })
        );
        assert_eq!(
            list_event(&BoardChange::Run(item())),
            ListEvent::Run {
                entry: entry(&item())
            }
        );
    }

    #[test]
    fn a_run_is_drawn_where_the_layout_puts_each_node_with_its_rows_mark_and_state() {
        let graph = feature_graph(vec![
            state(
                "intake",
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("done").expect("label")),
                },
            ),
            state(
                "preflight@1",
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("ready").expect("label")),
                },
            ),
            state("rework", InstanceStatus::Looping { iteration: nz(2) }),
            state(
                "implement@1",
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("implemented").expect("label")),
                },
            ),
            state(
                "review@1",
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("changes-requested").expect("label")),
                },
            ),
            state(
                "implement@2",
                InstanceStatus::Claimed {
                    worker: "elsewhere".to_owned(),
                    expires_at: time::OffsetDateTime::UNIX_EPOCH,
                },
            ),
        ]);
        let detail = RunDetail::of(
            item(),
            &graph,
            TemplateKind::FeatureFromTicket,
            &IterationSelection::default(),
            &LocalSteps::default(),
            &BTreeMap::new(),
        );

        let view = run_view(&detail, &graph);

        let geometry = layout(&graph.nodes, &graph.edges);
        assert_eq!(view.drawing.width, geometry.width);
        assert_eq!(view.drawing.nodes.len(), geometry.nodes.len());
        let rework = drawn(&view.drawing, "rework");
        assert_eq!(
            rework.kind,
            model::NodeKind::Loop {
                reached: 2,
                selected: 2,
                bound: 3
            }
        );
        let implement = drawn(&view.drawing, "implement");
        assert_eq!(implement.scope.as_deref(), Some("rework"));
        assert!(implement.rect.y >= rework.rect.y + LOOP_HEADER);
        assert_eq!(implement.mark, model::Mark::Elsewhere);
        let state = implement.state.as_ref().expect("implement@2 is reached");
        assert_eq!(state.instance, "implement@2");
        assert_eq!(state.status, "claimed");
        assert_eq!(state.detail.as_deref(), Some("elsewhere"));
        let preflight = drawn(&view.drawing, "preflight");
        assert_eq!(
            preflight
                .state
                .as_ref()
                .and_then(|state| state.detail.as_deref()),
            Some("ready")
        );
        assert_eq!(drawn(&view.drawing, "open-pr").state, None);
        assert_eq!(view.drawing.edges.len(), 6);
        assert_eq!(view.entry, entry(&item()));
    }

    #[test]
    fn an_earlier_round_shows_the_body_as_it_was() {
        let graph = feature_graph(vec![
            state("rework", InstanceStatus::Looping { iteration: nz(2) }),
            state(
                "review@1",
                InstanceStatus::Done {
                    branch: Some(BranchLabel::parse("changes-requested").expect("label")),
                },
            ),
        ]);
        let detail = RunDetail::of(
            item(),
            &graph,
            TemplateKind::FeatureFromTicket,
            &IterationSelection::default().with(
                vetinari_core::domain::devflow::ids::NodeId::parse("rework").expect("id"),
                nz(1),
            ),
            &LocalSteps::default(),
            &BTreeMap::new(),
        );

        let view = run_view(&detail, &graph);

        assert_eq!(
            drawn(&view.drawing, "rework").kind,
            model::NodeKind::Loop {
                reached: 2,
                selected: 1,
                bound: 3
            }
        );
        assert_eq!(drawn(&view.drawing, "review").mark, model::Mark::Done);
    }

    #[test]
    fn a_preview_draws_every_node_unreached_and_every_loop_at_no_rounds() {
        let graph = feature_graph(Vec::new());

        let drawing = preview_drawing(&graph.nodes, &graph.edges);

        assert_eq!(drawing.nodes.len(), 8);
        assert!(
            drawing
                .nodes
                .iter()
                .all(|node| node.state.is_none() && node.mark == model::Mark::NotReached)
        );
        assert_eq!(
            drawn(&drawing, "intake").kind,
            model::NodeKind::Loop {
                reached: 0,
                selected: 1,
                bound: 3
            }
        );
        assert_eq!(drawing.edges.len(), 6);
    }
}
