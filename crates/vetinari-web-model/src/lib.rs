//! The JSON vetinarid's web adapter sends and the vetinari web page reads.
//! Both compile these types, so neither can drift from the other.

use serde::{Deserialize, Serialize};

/// How a phase or a node is marked, as the TUI's strip marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mark {
    /// Finished.
    Done,
    /// Running on this machine.
    RunningHere,
    /// Offered or claimed elsewhere.
    Elsewhere,
    /// Waiting on the developer.
    NeedsYou,
    /// Failed.
    Failed,
    /// What runs next.
    Next,
    /// Not reached.
    NotReached,
}

/// Where a run is as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

/// A loop's round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    /// The round it is on.
    pub iteration: u32,
    /// The most rounds.
    pub bound: u32,
}

/// One cell of a run's phase strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    /// The phase's short name.
    pub name: String,
    /// Its mark.
    pub mark: Mark,
    /// The mark as the TUI draws it.
    pub glyph: String,
    /// Its loop's round, for a phase that loops.
    pub round: Option<Round>,
}

/// What a run is waiting on the developer for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NeedsYou {
    /// A gate to answer.
    Gate {
        /// The gate's instance.
        instance: String,
        /// Which gate.
        gate: String,
    },
    /// An agent's question.
    Question {
        /// The step's instance.
        instance: String,
        /// The question.
        question: String,
    },
}

/// A run as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    /// The run.
    pub run: String,
    /// The ticket's key, when known.
    pub ticket: Option<String>,
    /// The ticket's title, when known.
    pub title: Option<String>,
    /// Where it is.
    pub status: RunStatus,
    /// The phase strip.
    pub phases: Vec<Phase>,
    /// What it is waiting on the developer for.
    pub needs_you: Option<NeedsYou>,
    /// When it started, RFC 3339.
    pub started_at: Option<String>,
}

/// A change to the run list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ListEvent {
    /// Every run, replacing what the page had.
    Snapshot {
        /// The runs.
        runs: Vec<RunEntry>,
    },
    /// A run that is new or changed.
    Run {
        /// The run.
        entry: RunEntry,
    },
    /// A run that left the list.
    Removed {
        /// The run.
        run: String,
    },
    /// Whether the daemon needs signing in to its gateway.
    SignIn {
        /// Whether it does.
        needed: bool,
    },
}

/// A point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// From the left.
    pub x: u32,
    /// From the top.
    pub y: u32,
}

/// A rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// What a drawn node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeKind {
    /// A step.
    Step,
    /// A loop, with its rounds.
    Loop {
        /// How many rounds it has reached.
        reached: u32,
        /// The round its body shows.
        selected: u32,
        /// The most rounds.
        bound: u32,
    },
}

/// Where a node's instance is, at the selected rounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// The instance.
    pub instance: String,
    /// Its status, as the engine names it.
    pub status: String,
    /// What the status carries: the label taken, the worker, the error.
    pub detail: Option<String>,
    /// The current attempt.
    pub attempt: Option<u32>,
}

/// A drawn node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewNode {
    /// The node.
    pub node: String,
    /// Where it is drawn; a loop's encloses its body.
    pub rect: Rect,
    /// The loop that directly contains it.
    pub scope: Option<String>,
    /// What it is.
    pub kind: NodeKind,
    /// Its mark.
    pub mark: Mark,
    /// The mark as the TUI draws it.
    pub glyph: String,
    /// Its instance, once reached.
    pub state: Option<NodeState>,
}

/// A drawn edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewEdge {
    /// The source node.
    pub from: String,
    /// The target node.
    pub to: String,
    /// The label that selects it.
    pub when: Option<String>,
    /// Its route.
    pub points: Vec<Point>,
}

/// A graph, laid out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drawing {
    /// The width it fits in.
    pub width: u32,
    /// The height it fits in.
    pub height: u32,
    /// Its nodes, loops before their body.
    pub nodes: Vec<ViewNode>,
    /// Its edges.
    pub edges: Vec<ViewEdge>,
}

/// A review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// What review said.
    pub text: String,
    /// The file it is about.
    pub file: Option<String>,
    /// The line it is about.
    pub line: Option<u32>,
}

/// The latest review's findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    /// The round that found them.
    pub round: u32,
    /// The findings.
    pub items: Vec<Finding>,
}

/// One run, drawn at the selected rounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunView {
    /// The run as the list shows it.
    pub entry: RunEntry,
    /// Its graph.
    pub drawing: Drawing,
    /// What preflight asked.
    pub questions: Vec<String>,
    /// What the latest review found.
    pub findings: Option<Findings>,
}

/// An answer to a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GateAnswer {
    /// Check again, with what changed.
    Recheck {
        /// A note for the step.
        #[serde(default)]
        note: Option<String>,
    },
    /// Open the pull request although review never approved.
    OpenAnyway,
}

/// What became of a gate answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReply {
    /// The gateway accepted it.
    pub sent: bool,
    /// The gateway could not be reached, and it is sent when it can be.
    pub queued: bool,
}

/// Dismissing a finished run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismiss {
    /// Whether its worktree goes too.
    pub remove_worktree: bool,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// What went wrong.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry() -> RunEntry {
        RunEntry {
            run: "01JB7ZQ3".to_owned(),
            ticket: Some("ENG-398".to_owned()),
            title: Some("Paginate audit export".to_owned()),
            status: RunStatus::Active,
            phases: vec![Phase {
                name: "rev".to_owned(),
                mark: Mark::RunningHere,
                glyph: "●".to_owned(),
                round: Some(Round {
                    iteration: 2,
                    bound: 3,
                }),
            }],
            needs_you: Some(NeedsYou::Gate {
                instance: "ask-info@1".to_owned(),
                gate: "info-needed".to_owned(),
            }),
            started_at: None,
        }
    }

    #[test]
    fn a_list_event_is_tagged_by_its_type() {
        assert_eq!(
            serde_json::to_value(ListEvent::Removed {
                run: "r1".to_owned()
            })
            .expect("json"),
            json!({ "type": "removed", "run": "r1" })
        );
        assert_eq!(
            serde_json::to_value(ListEvent::SignIn { needed: true }).expect("json"),
            json!({ "type": "sign-in", "needed": true })
        );
    }

    #[test]
    fn an_entry_names_marks_and_what_needs_you_in_kebab_case() {
        let value = serde_json::to_value(entry()).expect("json");

        assert_eq!(value["status"], "active");
        assert_eq!(value["phases"][0]["mark"], "running-here");
        assert_eq!(
            value["phases"][0]["round"],
            json!({ "iteration": 2, "bound": 3 })
        );
        assert_eq!(
            value["needs_you"],
            json!({ "kind": "gate", "instance": "ask-info@1", "gate": "info-needed" })
        );
    }

    #[test]
    fn a_run_view_reads_back_as_it_was_written() {
        let view = RunView {
            entry: entry(),
            drawing: Drawing {
                width: 210,
                height: 40,
                nodes: vec![ViewNode {
                    node: "rework".to_owned(),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 150,
                        height: 40,
                    },
                    scope: None,
                    kind: NodeKind::Loop {
                        reached: 2,
                        selected: 1,
                        bound: 3,
                    },
                    mark: Mark::Done,
                    glyph: "✓".to_owned(),
                    state: Some(NodeState {
                        instance: "rework".to_owned(),
                        status: "looping".to_owned(),
                        detail: Some("2".to_owned()),
                        attempt: None,
                    }),
                }],
                edges: vec![ViewEdge {
                    from: "rework".to_owned(),
                    to: "open-pr".to_owned(),
                    when: Some("done".to_owned()),
                    points: vec![Point { x: 150, y: 20 }, Point { x: 210, y: 20 }],
                }],
            },
            questions: vec!["Which page size?".to_owned()],
            findings: Some(Findings {
                round: 1,
                items: vec![Finding {
                    text: "Needs a test".to_owned(),
                    file: Some("src/lib.rs".to_owned()),
                    line: Some(12),
                }],
            }),
        };

        let json = serde_json::to_string(&view).expect("json");
        let back: RunView = serde_json::from_str(&json).expect("parse");

        assert_eq!(back, view);
    }

    #[test]
    fn a_gate_answer_is_tagged_by_its_kind() {
        let recheck: GateAnswer =
            serde_json::from_value(json!({ "kind": "recheck", "note": "50 a page" }))
                .expect("parse");
        let anyway: GateAnswer =
            serde_json::from_value(json!({ "kind": "open-anyway" })).expect("parse");

        assert_eq!(
            recheck,
            GateAnswer::Recheck {
                note: Some("50 a page".to_owned())
            }
        );
        assert_eq!(anyway, GateAnswer::OpenAnyway);
    }
}
