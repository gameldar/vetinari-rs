//! Where a run's graph is drawn: a box per node, loops as boxes around their
//! body, and a route per edge.
//!
//! Each scope lays its members out in columns by depth from the scope's
//! entries, the order the popup's rows use. The geometry depends only on the
//! definition, so a view does not move as instances start and finish.

use super::compile::GraphEdge;
use super::ids::{BranchLabel, NodeId};
use super::lanes::depths;
use super::run_view::{RunNode, RunNodeKind};

/// A step's width.
pub const NODE_WIDTH: u32 = 150;
/// A step's height.
pub const NODE_HEIGHT: u32 = 40;
/// The space between columns.
pub const COLUMN_GAP: u32 = 60;
/// The space between nodes in one column.
pub const ROW_GAP: u32 = 24;
/// The space between a loop's border and its body.
pub const LOOP_PADDING: u32 = 16;
/// The band at the top of a loop for its name and rounds.
pub const LOOP_HEADER: u32 = 28;

/// A point, in the layout's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// From the left.
    pub x: u32,
    /// From the top.
    pub y: u32,
}

/// A rectangle, in the layout's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Its left edge.
    pub x: u32,
    /// Its top edge.
    pub y: u32,
    /// Its width.
    pub width: u32,
    /// Its height.
    pub height: u32,
}

/// Where one node is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedNode {
    /// The node.
    pub node: NodeId,
    /// Its box; a loop's encloses its body's.
    pub rect: Rect,
    /// The loop that directly contains it.
    pub scope: Option<NodeId>,
}

/// Where one edge is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedEdge {
    /// The source node.
    pub from: NodeId,
    /// The target node.
    pub to: NodeId,
    /// The label that selects it.
    pub when: Option<BranchLabel>,
    /// The route, from the source's right side to the target's left.
    pub points: Vec<Point>,
}

/// A whole graph's drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The width everything fits in.
    pub width: u32,
    /// The height everything fits in.
    pub height: u32,
    /// Every node, enclosing loops before their body.
    pub nodes: Vec<PlacedNode>,
    /// Every edge whose ends are both drawn.
    pub edges: Vec<PlacedEdge>,
}

/// Lay out a graph's nodes and edges.
#[must_use]
pub fn layout(nodes: &[RunNode], edges: &[GraphEdge]) -> Layout {
    let (width, height, placed) = arrange(nodes, edges, None);
    let routes = edges
        .iter()
        .filter_map(|edge| route(&placed, edge))
        .collect();
    Layout {
        width,
        height,
        nodes: placed,
        edges: routes,
    }
}

/// A scope's members placed from the origin, with the size they take.
fn arrange(
    nodes: &[RunNode],
    edges: &[GraphEdge],
    scope: Option<&NodeId>,
) -> (u32, u32, Vec<PlacedNode>) {
    let members: Vec<&RunNode> = nodes
        .iter()
        .filter(|node| node.scope.as_ref() == scope)
        .collect();
    let depths = depths(edges, &members);
    let sized: Vec<(u32, u32, Vec<PlacedNode>)> = members
        .iter()
        .map(|node| match node.kind {
            RunNodeKind::Loop { .. } => {
                let (width, height, body) = arrange(nodes, edges, Some(&node.id));
                (
                    width + 2 * LOOP_PADDING,
                    height + LOOP_HEADER + LOOP_PADDING,
                    body,
                )
            }
            RunNodeKind::External { .. } | RunNodeKind::Engine => {
                (NODE_WIDTH, NODE_HEIGHT, Vec::new())
            }
        })
        .collect();

    let columns: Vec<Vec<usize>> = (0..depths.iter().max().map_or(0, |deepest| deepest + 1))
        .map(|depth| {
            (0..members.len())
                .filter(|index| depths[*index] == depth)
                .collect()
        })
        .collect();
    let widths: Vec<u32> = columns
        .iter()
        .map(|column| {
            column
                .iter()
                .map(|index| sized[*index].0)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let heights: Vec<u32> = columns
        .iter()
        .map(|column| {
            let total: u32 = column.iter().map(|index| sized[*index].1).sum();
            total + ROW_GAP * (column.len() as u32).saturating_sub(1)
        })
        .collect();
    let height = heights.iter().copied().max().unwrap_or(0);
    let width = widths.iter().sum::<u32>() + COLUMN_GAP * widths.len().saturating_sub(1) as u32;

    let mut placed = Vec::new();
    let mut x = 0;
    for (depth, column) in columns.iter().enumerate() {
        let mut y = (height - heights[depth]) / 2;
        for &index in column {
            let (node_width, node_height, body) = &sized[index];
            placed.push(PlacedNode {
                node: members[index].id.clone(),
                rect: Rect {
                    x,
                    y,
                    width: *node_width,
                    height: *node_height,
                },
                scope: scope.cloned(),
            });
            placed.extend(body.iter().map(|inner| PlacedNode {
                rect: Rect {
                    x: inner.rect.x + x + LOOP_PADDING,
                    y: inner.rect.y + y + LOOP_HEADER,
                    ..inner.rect
                },
                ..inner.clone()
            }));
            y += node_height + ROW_GAP;
        }
        x += widths[depth] + COLUMN_GAP;
    }
    (width, height, placed)
}

/// An edge's route between the nodes it joins, if both are drawn.
fn route(placed: &[PlacedNode], edge: &GraphEdge) -> Option<PlacedEdge> {
    let rect = |node: &NodeId| {
        placed
            .iter()
            .find(|placed| placed.node == *node)
            .map(|placed| placed.rect)
    };
    let (from, to) = (rect(&edge.from)?, rect(&edge.to)?);
    let start = Point {
        x: from.x + from.width,
        y: from.y + from.height / 2,
    };
    let end = Point {
        x: to.x,
        y: to.y + to.height / 2,
    };
    let points = if start.y == end.y {
        vec![start, end]
    } else {
        let middle = start.x.midpoint(end.x);
        vec![
            start,
            Point {
                x: middle,
                y: start.y,
            },
            Point {
                x: middle,
                y: end.y,
            },
            end,
        ]
    };
    Some(PlacedEdge {
        from: edge.from.clone(),
        to: edge.to.clone(),
        when: edge.when.clone(),
        points,
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::domain::devflow::ids::TaskQueue;

    fn id(raw: &str) -> NodeId {
        NodeId::parse(raw).expect("node id")
    }

    fn step(raw: &str, scope: Option<&str>) -> RunNode {
        RunNode {
            id: id(raw),
            kind: RunNodeKind::External {
                queue: TaskQueue::parse("vetinari-hex-agent").expect("queue"),
            },
            scope: scope.map(id),
        }
    }

    fn looping(raw: &str, body: &[&str], exit: &str) -> RunNode {
        RunNode {
            id: id(raw),
            kind: RunNodeKind::Loop {
                body: body.iter().map(|node| id(node)).collect(),
                exit: id(exit),
                exit_when: BranchLabel::parse("approved").expect("label"),
                max_iterations: NonZeroU32::new(3).expect("non-zero"),
            },
            scope: None,
        }
    }

    fn edge(from: &str, to: &str, when: Option<&str>) -> GraphEdge {
        GraphEdge {
            from: id(from),
            to: id(to),
            when: when.map(|when| BranchLabel::parse(when).expect("label")),
        }
    }

    fn rect_of<'a>(layout: &'a Layout, node: &str) -> &'a Rect {
        &layout
            .nodes
            .iter()
            .find(|placed| placed.node.as_str() == node)
            .unwrap_or_else(|| panic!("{node} is not placed"))
            .rect
    }

    fn contains(outer: &Rect, inner: &Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    fn overlap(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn a_lone_step_is_one_node_box_at_the_origin() {
        let drawn = layout(&[step("preflight", None)], &[]);

        assert_eq!(
            *rect_of(&drawn, "preflight"),
            Rect {
                x: 0,
                y: 0,
                width: NODE_WIDTH,
                height: NODE_HEIGHT
            }
        );
        assert_eq!((drawn.width, drawn.height), (NODE_WIDTH, NODE_HEIGHT));
    }

    #[test]
    fn a_step_after_another_is_drawn_in_the_next_column() {
        let drawn = layout(
            &[step("review", None), step("implement", None)],
            &[edge("implement", "review", Some("implemented"))],
        );

        assert_eq!(rect_of(&drawn, "implement").x, 0);
        assert_eq!(rect_of(&drawn, "review").x, NODE_WIDTH + COLUMN_GAP);
        assert_eq!(rect_of(&drawn, "review").y, rect_of(&drawn, "implement").y);
    }

    #[test]
    fn steps_at_the_same_depth_share_a_column_without_overlapping() {
        let drawn = layout(
            &[
                step("rework", None),
                step("open-pr", None),
                step("review-stuck", None),
            ],
            &[
                edge("rework", "open-pr", Some("done")),
                edge("rework", "review-stuck", Some("exhausted")),
            ],
        );

        let open = rect_of(&drawn, "open-pr");
        let stuck = rect_of(&drawn, "review-stuck");
        assert_eq!(open.x, stuck.x);
        assert!(!overlap(open, stuck));
        assert_eq!(drawn.height, 2 * NODE_HEIGHT + ROW_GAP);
    }

    #[test]
    fn a_loop_encloses_its_body_below_its_header_and_pushes_what_follows_right() {
        let drawn = layout(
            &[
                looping("rework", &["implement", "review"], "review"),
                step("implement", Some("rework")),
                step("review", Some("rework")),
                step("open-pr", None),
            ],
            &[
                edge("implement", "review", Some("implemented")),
                edge("rework", "open-pr", Some("done")),
            ],
        );

        let rework = rect_of(&drawn, "rework");
        for body in ["implement", "review"] {
            assert!(contains(rework, rect_of(&drawn, body)), "{body} is outside");
            assert!(rect_of(&drawn, body).y >= rework.y + LOOP_HEADER);
        }
        assert_eq!(rework.width, 2 * NODE_WIDTH + COLUMN_GAP + 2 * LOOP_PADDING);
        assert_eq!(rect_of(&drawn, "open-pr").x, rework.width + COLUMN_GAP);
    }

    #[test]
    fn a_loop_is_listed_before_its_body() {
        let drawn = layout(
            &[
                step("implement", Some("rework")),
                looping("rework", &["implement"], "implement"),
            ],
            &[],
        );

        let order: Vec<&str> = drawn
            .nodes
            .iter()
            .map(|placed| placed.node.as_str())
            .collect();
        assert_eq!(order, ["rework", "implement"]);
        assert_eq!(drawn.nodes[1].scope, Some(id("rework")));
    }

    #[test]
    fn an_edge_runs_from_the_sources_right_side_to_the_targets_left_with_its_label() {
        let drawn = layout(
            &[
                step("rework", None),
                step("open-pr", None),
                step("review-stuck", None),
            ],
            &[edge("rework", "review-stuck", Some("exhausted"))],
        );

        let route = &drawn.edges[0];
        let from = rect_of(&drawn, "rework");
        let to = rect_of(&drawn, "review-stuck");
        assert_eq!(
            route.when,
            Some(BranchLabel::parse("exhausted").expect("label"))
        );
        assert_eq!(
            route.points.first(),
            Some(&Point {
                x: from.x + from.width,
                y: from.y + from.height / 2
            })
        );
        assert_eq!(
            route.points.last(),
            Some(&Point {
                x: to.x,
                y: to.y + to.height / 2
            })
        );
    }

    #[test]
    fn an_edge_to_a_node_that_is_not_drawn_is_left_out() {
        let drawn = layout(&[step("review", None)], &[edge("review", "gone", None)]);

        assert!(drawn.edges.is_empty());
    }

    #[test]
    fn everything_fits_inside_the_layouts_size() {
        let drawn = layout(
            &[
                looping("intake", &["preflight", "ask-info"], "preflight"),
                step("preflight", Some("intake")),
                step("ask-info", Some("intake")),
                looping("rework", &["implement", "review"], "review"),
                step("implement", Some("rework")),
                step("review", Some("rework")),
                step("review-stuck", None),
                step("open-pr", None),
            ],
            &[
                edge("preflight", "ask-info", Some("needs-info")),
                edge("intake", "rework", Some("done")),
                edge("implement", "review", Some("implemented")),
                edge("rework", "open-pr", Some("done")),
                edge("rework", "review-stuck", Some("exhausted")),
                edge("review-stuck", "open-pr", Some("open-anyway")),
            ],
        );

        let whole = Rect {
            x: 0,
            y: 0,
            width: drawn.width,
            height: drawn.height,
        };
        for placed in &drawn.nodes {
            assert!(
                contains(&whole, &placed.rect),
                "{} is outside",
                placed.node.as_str()
            );
        }
        assert_eq!(drawn.edges.len(), 6);
        let top: Vec<&PlacedNode> = drawn
            .nodes
            .iter()
            .filter(|placed| placed.scope.is_none())
            .collect();
        for (index, a) in top.iter().enumerate() {
            for b in &top[index + 1..] {
                assert!(
                    !overlap(&a.rect, &b.rect),
                    "{} overlaps {}",
                    a.node.as_str(),
                    b.node.as_str()
                );
            }
        }
        assert!(rect_of(&drawn, "review-stuck").x < rect_of(&drawn, "open-pr").x);
    }
}
