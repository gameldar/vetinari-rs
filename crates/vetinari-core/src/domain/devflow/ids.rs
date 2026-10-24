//! Identifiers the engine and vetinari have to agree on.
//!
//! Each follows the engine's own rule, so a value that constructs here is one
//! the engine accepts, and a value the engine sends is checked exactly once.

use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::domain::ids::{EmptyId, opaque_id};

opaque_id!(
    /// One execution of a dev workflow, as the engine names it.
    RunId
);
opaque_id!(
    /// A lease the engine minted at claim time. Opaque to vetinari.
    LeaseToken
);

/// Why a task queue name could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskQueueError {
    /// The name was empty.
    #[error("task queue is empty")]
    Empty,
    /// The name was longer than the engine accepts.
    #[error("task queue is {len} characters, maximum is {max}")]
    TooLong {
        /// Length of the offending name.
        len: usize,
        /// The engine's limit.
        max: usize,
    },
    /// The name held a character outside `[a-z0-9-]`, or began with `-`.
    #[error("task queue contains invalid character {found:?}")]
    InvalidChar {
        /// The first offending character.
        found: char,
    },
}

/// A queue a worker claims tasks from: `[a-z0-9][a-z0-9-]{0,63}`, the
/// engine's rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskQueue(String);

impl TaskQueue {
    /// The longest name the engine accepts.
    pub const MAX_LEN: usize = 64;

    /// Parse a queue name.
    pub fn parse(raw: &str) -> Result<Self, TaskQueueError> {
        if raw.is_empty() {
            return Err(TaskQueueError::Empty);
        }
        if let Some(found) = first_invalid(raw) {
            return Err(TaskQueueError::InvalidChar { found });
        }
        if raw.len() > Self::MAX_LEN {
            return Err(TaskQueueError::TooLong {
                len: raw.len(),
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a node id could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeIdError {
    /// The id was empty.
    #[error("node id is empty")]
    Empty,
    /// The id held `@`, which the engine reserves for loop iterations.
    #[error("node id {id:?} contains `@`, which marks a loop iteration")]
    ContainsAt {
        /// The offending id.
        id: String,
    },
}

/// A node of a workflow graph: non-empty, no `@`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Parse a node id.
    pub fn parse(raw: &str) -> Result<Self, NodeIdError> {
        if raw.is_empty() {
            return Err(NodeIdError::Empty);
        }
        if raw.contains('@') {
            return Err(NodeIdError::ContainsAt { id: raw.to_owned() });
        }
        Ok(Self(raw.to_owned()))
    }

    /// A node id a template names, which is valid by construction.
    pub(crate) fn known(id: &'static str) -> Self {
        debug_assert!(Self::parse(id).is_ok(), "{id} is not a node id");
        Self(id.to_owned())
    }

    /// Borrow the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a node instance could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeInstanceError {
    /// The node part was not a node id vetinari can hold.
    #[error(transparent)]
    Node(#[from] NodeIdError),
    /// The iteration suffix held an empty or zero iteration.
    #[error("instance {raw:?} has an iteration that is not a positive number")]
    Iteration {
        /// The offending instance.
        raw: String,
    },
}

/// A node, and the iteration of every loop enclosing it, outermost first:
/// `review`, `review@2` or `check@2.1`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeInstance {
    node: NodeId,
    iterations: Vec<NonZeroU32>,
}

impl NodeInstance {
    /// A node outside every loop.
    #[must_use]
    pub const fn top_level(node: NodeId) -> Self {
        Self {
            node,
            iterations: Vec::new(),
        }
    }

    /// A node inside loops, at these iterations, outermost first.
    #[must_use]
    pub const fn within(node: NodeId, iterations: Vec<NonZeroU32>) -> Self {
        Self { node, iterations }
    }

    /// Parse the engine's string form. A suffix after the last `@` counts as
    /// iterations only when it is digits and dots, as the engine rules, so a
    /// legacy id such as `a@b` reads as a top-level node and is then refused
    /// because a vetinari node id never holds `@`.
    pub fn parse(raw: &str) -> Result<Self, NodeInstanceError> {
        let Some((node, suffix)) = raw
            .rsplit_once('@')
            .filter(|(_, suffix)| suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
        else {
            return Ok(Self::top_level(NodeId::parse(raw)?));
        };
        let node = NodeId::parse(node)?;
        let iterations = suffix
            .split('.')
            .map(|part| part.parse::<NonZeroU32>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| NodeInstanceError::Iteration {
                raw: raw.to_owned(),
            })?;
        Ok(Self::within(node, iterations))
    }

    /// The node.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.node
    }

    /// The iteration of each enclosing loop, outermost first.
    #[must_use]
    pub fn iterations(&self) -> &[NonZeroU32] {
        &self.iterations
    }
}

impl fmt::Display for NodeInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.node.as_str())?;
        for (index, iteration) in self.iterations.iter().enumerate() {
            let separator = if index == 0 { '@' } else { '.' };
            write!(f, "{separator}{iteration}")?;
        }
        Ok(())
    }
}

/// Why a task id could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskIdError {
    /// The id was not `<run>~<instance>~<attempt>`.
    #[error("task id {raw:?} is not <run>~<instance>~<attempt>")]
    Shape {
        /// The offending id.
        raw: String,
    },
    /// The run part was blank.
    #[error("task id has an empty run")]
    Run(#[from] EmptyId),
    /// The instance part did not parse.
    #[error(transparent)]
    Instance(#[from] NodeInstanceError),
    /// The attempt part was not a positive number.
    #[error("task id {raw:?} has an attempt that is not a positive number")]
    Attempt {
        /// The offending id.
        raw: String,
    },
}

/// The engine's `<run>~<instance>~<attempt>`, parsed from a claim reply and
/// never assembled by vetinari, so a change in its format is one parser.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId {
    run: RunId,
    instance: NodeInstance,
    attempt: NonZeroU32,
}

impl TaskId {
    /// Parse the engine's string form. The run is everything before the
    /// first `~` and the attempt everything after the last one.
    pub fn parse(raw: &str) -> Result<Self, TaskIdError> {
        let shape = || TaskIdError::Shape {
            raw: raw.to_owned(),
        };
        let (run, rest) = raw.split_once('~').ok_or_else(shape)?;
        let (instance, attempt) = rest.rsplit_once('~').ok_or_else(shape)?;
        Ok(Self {
            run: RunId::parse(run)?,
            instance: NodeInstance::parse(instance)?,
            attempt: attempt.parse().map_err(|_| TaskIdError::Attempt {
                raw: raw.to_owned(),
            })?,
        })
    }

    /// The run the task belongs to.
    #[must_use]
    pub const fn run(&self) -> &RunId {
        &self.run
    }

    /// The node instance the task executes.
    #[must_use]
    pub const fn instance(&self) -> &NodeInstance {
        &self.instance
    }

    /// Which attempt of the instance this is.
    #[must_use]
    pub const fn attempt(&self) -> NonZeroU32 {
        self.attempt
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}~{}~{}", self.run, self.instance, self.attempt)
    }
}

/// Why a branch label could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchLabelError {
    /// The label was empty.
    #[error("branch label is empty")]
    Empty,
    /// The label held a character outside `[a-z0-9-]`.
    #[error("branch label contains invalid character {found:?}")]
    InvalidChar {
        /// The first offending character.
        found: char,
    },
}

/// The label a node's output carries in `branch`, matched against an edge's
/// `when`: non-empty, `[a-z0-9-]+`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchLabel(String);

impl BranchLabel {
    /// Parse a label.
    pub fn parse(raw: &str) -> Result<Self, BranchLabelError> {
        if raw.is_empty() {
            return Err(BranchLabelError::Empty);
        }
        if let Some(found) = raw.chars().find(|ch| !is_word_char(*ch)) {
            return Err(BranchLabelError::InvalidChar { found });
        }
        Ok(Self(raw.to_owned()))
    }

    /// One of the constants in [`super::outcome::label`], which are valid by
    /// construction.
    pub(crate) fn known(label: &'static str) -> Self {
        debug_assert!(Self::parse(label).is_ok(), "{label} is not a label");
        Self(label.to_owned())
    }

    /// Borrow the label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a worker name could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerNameError {
    /// The name was empty.
    #[error("worker name is empty")]
    Empty,
    /// The name would make a queue name longer than the engine accepts.
    #[error("worker name is {len} characters, maximum is {max}")]
    TooLong {
        /// Length of the offending name.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The name held a character outside `[a-z0-9-]`, or began with `-`.
    #[error("worker name contains invalid character {found:?}")]
    InvalidChar {
        /// The first offending character.
        found: char,
    },
}

/// What this daemon calls itself, and the middle of its queue names.
///
/// Held to the queue rule and short enough that `vetinari-<worker>-human`
/// is still a valid queue, so deriving the queues cannot fail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerName(String);

impl WorkerName {
    /// The longest name that leaves room for `vetinari-` and `-human`.
    pub const MAX_LEN: usize = TaskQueue::MAX_LEN - "vetinari--human".len();

    /// Parse a worker name, lowercasing it.
    pub fn parse(raw: &str) -> Result<Self, WorkerNameError> {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(WorkerNameError::Empty);
        }
        if let Some(found) = first_invalid(&name) {
            return Err(WorkerNameError::InvalidChar { found });
        }
        if name.len() > Self::MAX_LEN {
            return Err(WorkerNameError::TooLong {
                len: name.len(),
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(name))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a definition id could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionIdError {
    /// The id does not start with `vetinari-`, so it is not one of ours.
    #[error("definition id {raw:?} is not a vetinari definition")]
    NotVetinari {
        /// The offending id.
        raw: String,
    },
    /// The id held a character outside `[A-Za-z0-9-]`.
    #[error("definition id contains invalid character {found:?}")]
    InvalidChar {
        /// The first offending character.
        found: char,
    },
}

/// A workflow definition vetinari registered: `vetinari-<template>-<ulid>`.
/// Its prefix is how the daemon recognises its own runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionId(String);

impl DefinitionId {
    /// What every vetinari definition id starts with.
    pub const PREFIX: &'static str = "vetinari-";

    /// Parse a definition id.
    pub fn parse(raw: &str) -> Result<Self, DefinitionIdError> {
        let rest = raw
            .strip_prefix(Self::PREFIX)
            .filter(|rest| !rest.is_empty())
            .ok_or_else(|| DefinitionIdError::NotVetinari {
                raw: raw.to_owned(),
            })?;
        if let Some(found) = rest
            .chars()
            .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == '-'))
        {
            return Err(DefinitionIdError::InvalidChar { found });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DefinitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A loop's bound was out of the engine's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{found} rounds is outside 1..={max}", max = Rounds::MAX)]
pub struct RoundsError {
    /// The offending count.
    pub found: u32,
}

/// How many iterations a loop may run: `1..=20`, the engine's
/// `MAX_LOOP_ITERATIONS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rounds(NonZeroU32);

impl Rounds {
    /// The engine's `MAX_LOOP_ITERATIONS`.
    pub const MAX: u32 = 20;

    /// Accept a count within the engine's range.
    pub fn new(count: u32) -> Result<Self, RoundsError> {
        NonZeroU32::new(count)
            .filter(|count| count.get() <= Self::MAX)
            .map(Self)
            .ok_or(RoundsError { found: count })
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

const fn is_word_char(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'
}

/// The first character that breaks `[a-z0-9][a-z0-9-]*`.
fn first_invalid(raw: &str) -> Option<char> {
    if raw.starts_with('-') {
        return Some('-');
    }
    raw.chars().find(|ch| !is_word_char(*ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    #[test]
    fn a_task_queue_follows_the_engine_rule() {
        assert_eq!(
            TaskQueue::parse("vetinari-hex-agent")
                .expect("valid")
                .as_str(),
            "vetinari-hex-agent"
        );
        assert!(TaskQueue::parse("9").is_ok());
        assert_eq!(TaskQueue::parse(""), Err(TaskQueueError::Empty));
        assert_eq!(
            TaskQueue::parse("-agent"),
            Err(TaskQueueError::InvalidChar { found: '-' })
        );
        assert_eq!(
            TaskQueue::parse("Agent"),
            Err(TaskQueueError::InvalidChar { found: 'A' })
        );
        assert_eq!(
            TaskQueue::parse("a_b"),
            Err(TaskQueueError::InvalidChar { found: '_' })
        );
    }

    #[test]
    fn a_task_queue_is_at_most_sixty_four_characters() {
        assert!(TaskQueue::parse(&"a".repeat(64)).is_ok());
        assert_eq!(
            TaskQueue::parse(&"a".repeat(65)),
            Err(TaskQueueError::TooLong { len: 65, max: 64 })
        );
    }

    #[test]
    fn a_node_id_is_non_empty_and_has_no_at() {
        assert_eq!(NodeId::parse("open-pr").expect("valid").as_str(), "open-pr");
        assert_eq!(NodeId::parse(""), Err(NodeIdError::Empty));
        assert_eq!(
            NodeId::parse("review@2"),
            Err(NodeIdError::ContainsAt {
                id: "review@2".to_owned()
            })
        );
    }

    #[test]
    fn a_top_level_instance_is_its_node_id() {
        let instance = NodeInstance::parse("review").expect("valid");
        assert_eq!(instance.node().as_str(), "review");
        assert!(instance.iterations().is_empty());
        assert_eq!(instance.to_string(), "review");
    }

    #[test]
    fn an_instance_in_loops_carries_each_iteration_outermost_first() {
        let instance = NodeInstance::parse("check@2.1").expect("valid");
        assert_eq!(instance.node().as_str(), "check");
        assert_eq!(instance.iterations(), &[nz(2), nz(1)]);
        assert_eq!(instance.to_string(), "check@2.1");
        assert_eq!(
            NodeInstance::within(NodeId::parse("review").expect("id"), vec![nz(3)]).to_string(),
            "review@3"
        );
    }

    #[test]
    fn a_legacy_id_with_a_non_numeric_suffix_is_a_top_level_node_vetinari_refuses() {
        assert_eq!(
            NodeInstance::parse("a@b"),
            Err(NodeInstanceError::Node(NodeIdError::ContainsAt {
                id: "a@b".to_owned()
            }))
        );
        assert_eq!(
            NodeInstance::parse("a@2.x"),
            Err(NodeInstanceError::Node(NodeIdError::ContainsAt {
                id: "a@2.x".to_owned()
            }))
        );
    }

    #[test]
    fn an_iteration_must_be_positive() {
        for raw in ["review@0", "review@1..2", "review@2."] {
            assert_eq!(
                NodeInstance::parse(raw),
                Err(NodeInstanceError::Iteration {
                    raw: raw.to_owned()
                }),
                "{raw}"
            );
        }
        assert_eq!(
            NodeInstance::parse("@2"),
            Err(NodeInstanceError::Node(NodeIdError::Empty))
        );
    }

    #[test]
    fn a_task_id_splits_into_run_instance_and_attempt() {
        let id = TaskId::parse("01JB7Z~review@2~3").expect("valid");
        assert_eq!(id.run().as_str(), "01JB7Z");
        assert_eq!(id.instance().to_string(), "review@2");
        assert_eq!(id.attempt(), nz(3));
        assert_eq!(id.to_string(), "01JB7Z~review@2~3");
    }

    #[test]
    fn a_task_id_of_the_wrong_shape_is_refused() {
        assert_eq!(
            TaskId::parse("01JB7Z~review"),
            Err(TaskIdError::Shape {
                raw: "01JB7Z~review".to_owned()
            })
        );
        assert_eq!(TaskId::parse(" ~review~1"), Err(TaskIdError::Run(EmptyId)));
        assert_eq!(
            TaskId::parse("r~review~0"),
            Err(TaskIdError::Attempt {
                raw: "r~review~0".to_owned()
            })
        );
        assert_eq!(
            TaskId::parse("r~review~x"),
            Err(TaskIdError::Attempt {
                raw: "r~review~x".to_owned()
            })
        );
        assert!(matches!(
            TaskId::parse("r~review@0~1"),
            Err(TaskIdError::Instance(_))
        ));
    }

    #[test]
    fn a_branch_label_is_lowercase_words_and_dashes() {
        assert_eq!(
            BranchLabel::parse("changes-requested")
                .expect("valid")
                .as_str(),
            "changes-requested"
        );
        assert_eq!(BranchLabel::parse(""), Err(BranchLabelError::Empty));
        assert_eq!(
            BranchLabel::parse("Ready"),
            Err(BranchLabelError::InvalidChar { found: 'R' })
        );
        assert_eq!(
            BranchLabel::parse("needs info"),
            Err(BranchLabelError::InvalidChar { found: ' ' })
        );
    }

    #[test]
    fn a_worker_name_is_lowercased_and_leaves_room_for_its_queues() {
        assert_eq!(WorkerName::parse("Hex").expect("valid").as_str(), "hex");
        assert_eq!(WorkerName::parse(" "), Err(WorkerNameError::Empty));
        assert_eq!(
            WorkerName::parse("hex.local"),
            Err(WorkerNameError::InvalidChar { found: '.' })
        );
        assert_eq!(
            WorkerName::parse("-hex"),
            Err(WorkerNameError::InvalidChar { found: '-' })
        );
        assert!(WorkerName::parse(&"a".repeat(WorkerName::MAX_LEN)).is_ok());
        assert_eq!(
            WorkerName::parse(&"a".repeat(WorkerName::MAX_LEN + 1)),
            Err(WorkerNameError::TooLong {
                len: WorkerName::MAX_LEN + 1,
                max: WorkerName::MAX_LEN
            })
        );
    }

    #[test]
    fn a_definition_id_must_be_one_of_ours() {
        let id = DefinitionId::parse("vetinari-feature-01JB7ZQ3").expect("valid");
        assert_eq!(id.as_str(), "vetinari-feature-01JB7ZQ3");
        assert_eq!(
            DefinitionId::parse("deploy-nightly"),
            Err(DefinitionIdError::NotVetinari {
                raw: "deploy-nightly".to_owned()
            })
        );
        assert_eq!(
            DefinitionId::parse("vetinari-"),
            Err(DefinitionIdError::NotVetinari {
                raw: "vetinari-".to_owned()
            })
        );
        assert_eq!(
            DefinitionId::parse("vetinari-a/b"),
            Err(DefinitionIdError::InvalidChar { found: '/' })
        );
    }

    #[test]
    fn rounds_are_bounded_by_the_engine() {
        assert_eq!(Rounds::new(1).expect("valid").get(), nz(1));
        assert_eq!(Rounds::new(20).expect("valid").get(), nz(20));
        assert_eq!(Rounds::new(0), Err(RoundsError { found: 0 }));
        assert_eq!(Rounds::new(21), Err(RoundsError { found: 21 }));
    }

    #[test]
    fn run_ids_and_lease_tokens_are_kept_verbatim() {
        assert_eq!(
            LeaseToken::parse("01JB7ZQ3XK").expect("valid").as_str(),
            "01JB7ZQ3XK"
        );
        assert!(RunId::parse(" ").is_err());
    }
}
