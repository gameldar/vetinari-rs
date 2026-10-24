//! Leases this worker holds, as the lease journal records them.

use std::path::PathBuf;

use time::OffsetDateTime;

use super::ids::{LeaseToken, TaskId, TaskQueue};
use super::step::StepKind;
use super::task::StepOutput;
use crate::domain::ids::{SessionId, SpaceId};

/// A claimed task this worker has not yet completed or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLease {
    /// The task.
    pub task: TaskId,
    /// The lease the claim holds.
    pub lease: LeaseToken,
    /// When the lease runs out without a heartbeat.
    pub expires_at: OffsetDateTime,
    /// The queue it was claimed from.
    pub queue: TaskQueue,
    /// What the step does.
    pub step: StepKind,
    /// How far the worker got.
    pub state: HeldState,
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
}
