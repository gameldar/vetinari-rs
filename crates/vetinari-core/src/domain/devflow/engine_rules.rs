//! A test-only mirror of the engine's graph validation (engine spec 4.4), so
//! a template that would be refused at registration fails a unit test first.

use std::collections::{BTreeMap, BTreeSet};

use super::compile::{GraphNode, LeaseSecs, WorkflowGraph};
use super::ids::{NodeId, Rounds};

/// Check every rule the engine applies when a definition is registered.
pub(crate) fn check(graph: &WorkflowGraph) -> Result<(), String> {
    if graph.nodes.is_empty() {
        return Err("a workflow needs at least one node".to_owned());
    }
    let mut ids = BTreeSet::new();
    for node in &graph.nodes {
        if !ids.insert(node.id().clone()) {
            return Err(format!("node {} is defined twice", node.id()));
        }
        if node.id().as_str().contains('@') {
            return Err(format!("node {} contains `@`", node.id()));
        }
    }

    // A node's scope is the loop that lists it; top-level nodes have none.
    let mut scope: BTreeMap<&NodeId, &NodeId> = BTreeMap::new();
    for node in &graph.nodes {
        match node {
            GraphNode::Loop {
                id,
                body,
                exit,
                max_iterations,
            } => {
                for member in body.iter() {
                    if !ids.contains(member) {
                        return Err(format!("{id} lists unknown node {member}"));
                    }
                    if scope.insert(member, id).is_some() {
                        return Err(format!("{member} is in more than one body"));
                    }
                }
                if !body.iter().any(|member| member == &exit.node) {
                    return Err(format!("{id} exits on {}, not in its body", exit.node));
                }
                if max_iterations.get().get() > Rounds::MAX {
                    return Err(format!("{id} allows too many iterations"));
                }
            }
            GraphNode::External { id, policy, .. } => {
                if policy.lease.get() < LeaseSecs::MIN {
                    return Err(format!("{id} has a lease under the floor"));
                }
            }
        }
    }
    for node in &graph.nodes {
        let mut seen = BTreeSet::new();
        let mut current = node.id();
        while let Some(parent) = scope.get(current) {
            if parent == &node.id() || !seen.insert(*parent) {
                return Err(format!("{} contains itself", node.id()));
            }
            current = parent;
        }
    }

    for edge in &graph.edges {
        for end in [&edge.from, &edge.to] {
            if !ids.contains(end) {
                return Err(format!("edge names unknown node {end}"));
            }
        }
        if scope.get(&edge.from) != scope.get(&edge.to) {
            return Err(format!("{} -> {} crosses a scope", edge.from, edge.to));
        }
    }

    let mut remaining = ids;
    loop {
        let removable: Vec<NodeId> = remaining
            .iter()
            .filter(|node| {
                !graph
                    .edges
                    .iter()
                    .any(|edge| &&edge.to == node && remaining.contains(&edge.from))
            })
            .cloned()
            .collect();
        if removable.is_empty() {
            break;
        }
        for node in removable {
            remaining.remove(&node);
        }
    }
    if remaining.is_empty() {
        Ok(())
    } else {
        Err("the graph has a cycle".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::compile::{
        GraphEdge, LoopExit, NonEmpty, StepPolicies, WorkerQueues, compile,
    };
    use crate::domain::devflow::ids::{BranchLabel, DefinitionId, WorkerName};
    use crate::domain::devflow::template::{DevWorkflow, sample_feature};

    fn id(raw: &str) -> NodeId {
        NodeId::parse(raw).expect("node id")
    }

    fn sample() -> WorkflowGraph {
        compile(
            &DevWorkflow::FeatureFromTicket(sample_feature()),
            DefinitionId::parse("vetinari-feature-1").expect("id"),
            &WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
            &StepPolicies::default(),
        )
    }

    fn external(name: &str) -> GraphNode {
        let Some(GraphNode::External {
            queue,
            config,
            policy,
            ..
        }) = sample()
            .nodes
            .into_iter()
            .find(|node| node.id().as_str() == "preflight")
        else {
            panic!("sample has preflight");
        };
        GraphNode::External {
            id: id(name),
            queue,
            config,
            policy,
        }
    }

    fn looping(name: &str, body: &[&str], exit: &str) -> GraphNode {
        GraphNode::Loop {
            id: id(name),
            body: NonEmpty::new(id(body[0]), body[1..].iter().map(|raw| id(raw)).collect()),
            exit: LoopExit {
                node: id(exit),
                when: BranchLabel::parse("ready").expect("label"),
            },
            max_iterations: Rounds::new(3).expect("rounds"),
        }
    }

    fn edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: id(from),
            to: id(to),
            when: None,
        }
    }

    fn graph(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> WorkflowGraph {
        WorkflowGraph {
            id: DefinitionId::parse("vetinari-test-1").expect("id"),
            nodes,
            edges,
        }
    }

    #[test]
    fn a_plain_chain_passes() {
        let ok = graph(vec![external("a"), external("b")], vec![edge("a", "b")]);
        assert_eq!(check(&ok), Ok(()));
    }

    #[test]
    fn an_empty_graph_or_duplicate_ids_fail() {
        assert!(check(&graph(vec![], vec![])).is_err());
        assert!(check(&graph(vec![external("a"), external("a")], vec![])).is_err());
    }

    #[test]
    fn an_edge_to_a_missing_node_fails() {
        assert!(check(&graph(vec![external("a")], vec![edge("a", "b")])).is_err());
    }

    #[test]
    fn a_cycle_outside_loops_fails() {
        let cyclic = graph(
            vec![external("a"), external("b")],
            vec![edge("a", "b"), edge("b", "a")],
        );
        assert!(check(&cyclic).is_err());
    }

    #[test]
    fn a_body_member_must_exist_and_belong_to_one_body() {
        assert!(check(&graph(vec![looping("l", &["x"], "x")], vec![])).is_err());
        let shared = graph(
            vec![
                looping("l", &["a"], "a"),
                looping("m", &["a"], "a"),
                external("a"),
            ],
            vec![],
        );
        assert!(check(&shared).is_err());
    }

    #[test]
    fn a_loop_cannot_contain_itself() {
        assert!(check(&graph(vec![looping("l", &["l"], "l")], vec![])).is_err());
        let nested = graph(
            vec![looping("l", &["m"], "m"), looping("m", &["l"], "l")],
            vec![],
        );
        assert!(check(&nested).is_err());
    }

    #[test]
    fn the_exit_must_be_a_direct_member_of_the_body() {
        let outside = graph(
            vec![looping("l", &["a"], "b"), external("a"), external("b")],
            vec![],
        );
        assert!(check(&outside).is_err());
    }

    #[test]
    fn an_edge_may_not_cross_a_scope() {
        let crossing = graph(
            vec![looping("l", &["a"], "a"), external("a"), external("b")],
            vec![edge("a", "b")],
        );
        assert!(check(&crossing).is_err());
        let into_body = graph(
            vec![looping("l", &["a"], "a"), external("a")],
            vec![edge("l", "a")],
        );
        assert!(check(&into_body).is_err());
    }
}
