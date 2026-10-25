//! Leases this worker holds, as the lease journal records them.

use std::path::PathBuf;

use super::step::StepKind;
use super::task::{ClaimedTask, StepFailure, StepOutput};
use crate::domain::ids::{SessionId, SpaceId};

/// A claimed task this worker has not yet completed or failed. The whole
/// claim is kept, because a restarted daemon starts the step again from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLease {
    /// The claim, with its current expiry.
    pub task: ClaimedTask,
    /// How far the worker got.
    pub state: HeldState,
}

impl HeldLease {
    /// What the step does.
    #[must_use]
    pub const fn step(&self) -> StepKind {
        self.task.config.step
    }
}

/// How far the worker got with a held task, which decides what a restarted
/// daemon does with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldState {
    /// Claimed; nothing created yet.
    Starting,
    /// An agent is running the step.
    Running {
        /// The agent's session.
        session: SessionId,
        /// The run's worktree.
        worktree: PathBuf,
        /// The space it was opened in, when one was.
        space: Option<SpaceId>,
    },
    /// A gate is waiting for a person.
    AwaitingPerson,
    /// The step finished; its completion has not reached the gateway yet.
    Completing {
        /// What to complete it with.
        output: StepOutput,
    },
    /// The step failed; the failure has not reached the gateway yet.
    Failing {
        /// What to fail it with.
        failure: StepFailure,
    },
}
