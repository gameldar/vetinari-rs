//! The rows of a run's popup (design 9.3): each scope's nodes in order of
//! depth from the scope's entries, a loop followed by its body at the
//! selected iteration, and nodes at the same depth drawn side by side.
//!
//! The order is a property of the drawn graph, not of timing, so the layout
//! does not jump as instances start and finish.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use super::ids::{NodeId, NodeInstance};
use super::run_view::{InstanceState, InstanceStatus, RunGraph, RunNode, RunNodeKind};

/// Which iteration of each loop the developer is looking at. A loop with
/// none selected shows its latest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IterationSelection(BTreeMap<NodeId, NonZeroU32>);

impl IterationSelection {
    /// Look at one iteration of a loop.
    #[must_use]
    pub fn with(mut self, node: NodeId, iteration: NonZeroU32) -> Self {
        self.0.insert(node, iteration);
        self
    }
}

/// A row's place in a group of nodes at the same depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The only node at its depth.
    Alone,
    /// The first of several.
    First,
    /// Neither first nor last of several.
    Middle,
    /// The last of several.
    Last,
}

/// What a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A step or an engine node.
    Node,
    /// A loop, with its iteration tabs.
    Loop {
        /// How many iterations it has reached.
        reached: u32,
        /// The iteration its body rows show.
        selected: NonZeroU32,
        /// The most iterations.
        bound: NonZeroU32,
    },
}

/// One row of the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRow {
    /// How many loops enclose it.
    pub indent: usize,
    /// The node.
    pub node: NodeId,
    /// Its place among the nodes at its depth.
    pub lane: Lane,
    /// What it is.
    pub kind: RowKind,
    /// Its instance at the selected iterations, if reached.
    pub state: Option<InstanceState>,
}

/// The popup's rows for a run.
#[must_use]
pub fn lanes(graph: &RunGraph, selection: &IterationSelection) -> Vec<LaneRow> {
    let mut rows = Vec::new();
    scope_rows(graph, selection, None, &[], &mut rows);
    rows
}

fn scope_rows(
    graph: &RunGraph,
    selection: &IterationSelection,
    scope: Option<&NodeId>,
    iterations: &[NonZeroU32],
    rows: &mut Vec<LaneRow>,
) {
    let members: Vec<&RunNode> = graph
        .nodes
        .iter()
        .filter(|node| node.scope.as_ref() == scope)
        .collect();
    let depths = depths(graph, &members);
    let mut ordered: Vec<(usize, usize, &RunNode)> = members
        .iter()
        .enumerate()
        .map(|(index, node)| (depths[index], index, *node))
        .collect();
    ordered.sort_by_key(|(depth, index, _)| (*depth, *index));

    for (position, (depth, _, node)) in ordered.iter().enumerate() {
        let group = ordered
            .iter()
            .filter(|(other, _, _)| other == depth)
            .count();
        let first = ordered.iter().position(|(other, _, _)| other == depth) == Some(position);
        let last = ordered.iter().rposition(|(other, _, _)| other == depth) == Some(position);
        let lane = match (group, first, last) {
            (1, _, _) => Lane::Alone,
            (_, true, _) => Lane::First,
            (_, _, true) => Lane::Last,
            _ => Lane::Middle,
        };
        let instance = NodeInstance::within(node.id.clone(), iterations.to_vec());
        let state = graph
            .instances
            .iter()
            .find(|state| state.instance == instance)
            .cloned();
        match &node.kind {
            RunNodeKind::Loop { max_iterations, .. } => {
                let reached = reached(graph, &node.id, iterations);
                let latest = NonZeroU32::new(reached).unwrap_or(NonZeroU32::MIN);
                let selected = selection
                    .0
                    .get(&node.id)
                    .copied()
                    .filter(|selected| *selected <= latest)
                    .unwrap_or(latest);
                rows.push(LaneRow {
                    indent: iterations.len(),
                    node: node.id.clone(),
                    lane,
                    kind: RowKind::Loop {
                        reached,
                        selected,
                        bound: *max_iterations,
                    },
                    state,
                });
                let mut inner = iterations.to_vec();
                inner.push(selected);
                scope_rows(graph, selection, Some(&node.id), &inner, rows);
            }
            RunNodeKind::External { .. } | RunNodeKind::Engine => rows.push(LaneRow {
                indent: iterations.len(),
                node: node.id.clone(),
                lane,
                kind: RowKind::Node,
                state,
            }),
        }
    }
}

/// Each member's longest distance from an entry of its scope, over the
/// edges inside the scope. The graph outside loops is acyclic, so one pass
/// per member settles it.
fn depths(graph: &RunGraph, members: &[&RunNode]) -> Vec<usize> {
    let index = |id: &NodeId| members.iter().position(|node| node.id == *id);
    let edges: Vec<(usize, usize)> = graph
        .edges
        .iter()
        .filter_map(|edge| Some((index(&edge.from)?, index(&edge.to)?)))
        .collect();
    let mut depths = vec![0; members.len()];
    for _ in 0..members.len() {
        for (from, to) in &edges {
            depths[*to] = depths[*to].max(depths[*from] + 1);
        }
    }
    depths
}

/// How many iterations a loop at these enclosing iterations has reached:
/// its current one while it runs, else the highest any body instance has.
fn reached(graph: &RunGraph, node: &NodeId, iterations: &[NonZeroU32]) -> u32 {
    let looping = graph.instances.iter().find_map(|state| match state.status {
        InstanceStatus::Looping { iteration }
            if state.instance.node() == node && state.instance.iterations() == iterations =>
        {
            Some(iteration.get())
        }
        _ => None,
    });
    looping.unwrap_or_else(|| {
        let body: Vec<&NodeId> = graph
            .nodes
            .iter()
            .filter(|member| member.scope.as_ref() == Some(node))
            .map(|member| &member.id)
            .collect();
        graph
            .instances
            .iter()
            .filter(|state| body.contains(&state.instance.node()))
            .filter_map(|state| {
                let (last, outer) = state.instance.iterations().split_last()?;
                (outer == iterations).then_some(last.get())
            })
            .max()
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::compile::GraphEdge;
    use crate::domain::devflow::ids::BranchLabel;
    use crate::domain::devflow::run_view::tests::{feature_graph, id, instance, nz, queue, state};

    fn done(label: Option<&str>) -> InstanceStatus {
        InstanceStatus::Done {
            branch: label.map(|label| BranchLabel::parse(label).expect("label")),
        }
    }

    fn second_round() -> RunGraph {
        feature_graph(
            9,
            vec![
                state("intake", done(Some("done"))),
                state("preflight@1", done(Some("ready"))),
                state("rework", InstanceStatus::Looping { iteration: nz(2) }),
                state("implement@1", done(None)),
                state("review@1", done(Some("changes-requested"))),
                state("implement@2", InstanceStatus::Running),
            ],
        )
    }

    fn outline(rows: &[LaneRow]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let tabs = match row.kind {
                    RowKind::Loop {
                        reached,
                        selected,
                        bound,
                    } => format!(" [{selected} of {reached}, max {bound}]"),
                    RowKind::Node => String::new(),
                };
                let at = row
                    .state
                    .as_ref()
                    .map(|state| format!(" {}", state.instance))
                    .unwrap_or_default();
                format!("{}{}{tabs}{at}", "  ".repeat(row.indent), row.node)
            })
            .collect()
    }

    #[test]
    fn a_runs_rows_follow_depth_with_each_loop_at_its_latest_round() {
        let rows = lanes(&second_round(), &IterationSelection::default());
        assert_eq!(
            outline(&rows),
            [
                "intake [1 of 1, max 3] intake",
                "  preflight preflight@1",
                "  ask-info",
                "rework [2 of 2, max 3] rework",
                "  implement implement@2",
                "  review",
                "review-stuck",
                "open-pr",
            ]
        );
        assert!(rows.iter().all(|row| row.lane == Lane::Alone));
        assert_eq!(
            rows[4].state.as_ref().map(|state| &state.status),
            Some(&InstanceStatus::Running)
        );
    }

    #[test]
    fn selecting_an_earlier_round_shows_its_instances() {
        let rows = lanes(
            &second_round(),
            &IterationSelection::default().with(id("rework"), nz(1)),
        );
        assert_eq!(
            outline(&rows)[3..6],
            [
                "rework [1 of 2, max 3] rework",
                "  implement implement@1",
                "  review review@1",
            ]
        );
    }

    #[test]
    fn a_selection_past_the_reached_rounds_shows_the_latest() {
        let rows = lanes(
            &second_round(),
            &IterationSelection::default().with(id("rework"), nz(3)),
        );
        assert_eq!(outline(&rows)[3], "rework [2 of 2, max 3] rework");
    }

    #[test]
    fn a_loop_not_reached_shows_its_first_round_empty() {
        let rows = lanes(
            &feature_graph(0, Vec::new()),
            &IterationSelection::default(),
        );
        assert_eq!(outline(&rows)[0], "intake [1 of 0, max 3]");
        assert!(rows.iter().all(|row| row.state.is_none()));
    }

    #[test]
    fn nodes_at_the_same_depth_are_drawn_as_lanes() {
        let mut graph = feature_graph(1, vec![state("implement@1", done(None))]);
        graph.nodes.retain(|node| node.scope != Some(id("rework")));
        graph.edges.retain(|edge| edge.from != id("implement"));
        for name in ["implement", "review-tests", "review-design", "join"] {
            graph.nodes.push(RunNode {
                id: id(name),
                kind: if name == "join" {
                    RunNodeKind::Engine
                } else {
                    RunNodeKind::External {
                        queue: queue("vetinari-hex-agent"),
                    }
                },
                scope: Some(id("rework")),
            });
        }
        for (from, to) in [
            ("implement", "review-tests"),
            ("implement", "review-design"),
            ("review-tests", "join"),
            ("review-design", "join"),
        ] {
            graph.edges.push(GraphEdge {
                from: id(from),
                to: id(to),
                when: None,
            });
        }
        let rows = lanes(&graph, &IterationSelection::default());
        let body: Vec<(String, Lane)> = rows
            .iter()
            .filter(|row| row.indent == 1 && rows[3].node == id("rework"))
            .skip(2)
            .map(|row| (row.node.to_string(), row.lane))
            .collect();
        assert_eq!(
            body,
            [
                ("implement".to_owned(), Lane::Alone),
                ("review-tests".to_owned(), Lane::First),
                ("review-design".to_owned(), Lane::Last),
                ("join".to_owned(), Lane::Alone),
            ]
        );
        assert_eq!(
            rows[4].state.as_ref().map(|state| state.instance.clone()),
            Some(instance("implement@1"))
        );
    }
}
