//! Ports the dev workflow services call.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use time::{Duration, OffsetDateTime};

use crate::application::ports::PortResult;
use crate::domain::devflow::compile::WorkflowGraph;
use crate::domain::devflow::held::HeldLease;
use crate::domain::devflow::ids::{
    DefinitionId, LeaseToken, RunId, TaskId, TaskQueue, Ulid, WorkerName,
};
use crate::domain::devflow::outcome::OpenPrOutcome;
use crate::domain::devflow::run_view::{RunEvent, RunGraph, RunHeader};
use crate::domain::devflow::step::StepConfig;
use crate::domain::devflow::task::{RawTask, StepFailure, StepOutput};
use crate::domain::profile::Profile;

/// What the engine said to a heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseReply {
    /// The lease runs until this instant now.
    Extended {
        /// The new expiry.
        expires_at: OffsetDateTime,
    },
    /// The lease or attempt is not the current one: another attempt owns it.
    Stale,
    /// The run is cancelled, failed or completed.
    Gone,
    /// No such run or instance.
    NotFound,
}

/// What the engine said to a completion or a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskReply {
    /// Recorded.
    Accepted,
    /// The lease or attempt is not the current one.
    Stale,
    /// The run is cancelled, failed or completed.
    Gone,
    /// No such run or instance.
    NotFound,
}

/// The engine's task endpoints. The adapter maps HTTP statuses onto these
/// replies; anything else is a `PortError`, and only `Unavailable` is
/// worth retrying.
#[async_trait]
pub trait TaskGateway: Send + Sync {
    /// Take the oldest task on a queue, waiting up to `wait` for one.
    async fn claim(
        &self,
        queue: &TaskQueue,
        worker: &WorkerName,
        wait: Duration,
    ) -> PortResult<Option<RawTask>>;

    /// Keep a lease alive.
    async fn heartbeat(&self, task: &TaskId, lease: &LeaseToken) -> PortResult<LeaseReply>;

    /// Finish a task with its output.
    async fn complete(
        &self,
        task: &TaskId,
        lease: &LeaseToken,
        output: &StepOutput,
    ) -> PortResult<TaskReply>;

    /// Fail a task.
    async fn fail(
        &self,
        task: &TaskId,
        lease: &LeaseToken,
        failure: &StepFailure,
    ) -> PortResult<TaskReply>;
}

/// What the engine said to a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReply {
    /// The run is cancelled; its in-flight instances are abandoned.
    Cancelled,
    /// The run had already finished.
    AlreadyFinished,
}

/// The engine's definition and run endpoints.
#[async_trait]
pub trait RunGateway: Send + Sync {
    /// Register a compiled definition under its id.
    async fn register(&self, graph: &WorkflowGraph) -> PortResult<()>;

    /// Start a run of a definition.
    async fn start(
        &self,
        definition: &DefinitionId,
        input: &serde_json::Value,
    ) -> PortResult<RunId>;

    /// Delete a definition. Deleting one that is not there succeeds.
    async fn remove(&self, definition: &DefinitionId) -> PortResult<()>;

    /// Every live definition vetinari registered: those whose id says so.
    async fn definitions(&self) -> PortResult<Vec<DefinitionId>>;

    /// What a definition's steps are configured with: the ticket, the
    /// repository, the profile and the branch, which `compile` writes into
    /// every step alike. A definition with no step vetinari can read is a
    /// protocol error.
    async fn step_config(&self, definition: &DefinitionId) -> PortResult<StepConfig>;

    /// The runs of a definition.
    async fn runs(&self, definition: &DefinitionId) -> PortResult<Vec<RunHeader>>;

    /// A run's graph as it is now.
    async fn graph(&self, run: &RunId) -> PortResult<RunGraph>;

    /// Cancel a run.
    async fn cancel(&self, run: &RunId) -> PortResult<CancelReply>;

    /// Every output a run has recorded, keyed by instance string form.
    async fn outputs(&self, run: &RunId) -> PortResult<BTreeMap<String, serde_json::Value>>;
}

/// The worktree a run's steps share, as far as dismissing the run needs.
#[async_trait]
pub trait RunWorktrees: Send + Sync {
    /// Remove the worktree of the run whose steps carry `config`.
    async fn remove(&self, config: &StepConfig) -> PortResult<()>;
}

/// Where a run's definition gets the ULID in its id.
pub trait RunIds: Send + Sync {
    /// A fresh ULID.
    fn next(&self) -> Ulid;
}

/// The engine's stream of every run's events.
#[async_trait]
pub trait RunEvents: Send + Sync {
    /// Open the stream. Subscribe before fetching a graph, so no event
    /// falls between the two (engine spec 7.3).
    async fn subscribe(&self) -> PortResult<Box<dyn RunEventStream>>;
}

/// An open run event stream.
#[async_trait]
pub trait RunEventStream: Send {
    /// The next event, or `None` once the stream has closed. A closed stream
    /// may have dropped events, so whoever reopens it refetches every graph.
    async fn next(&mut self) -> PortResult<Option<RunEvent>>;
}

/// Opens the pull request at the end of a run.
#[async_trait]
pub trait PullRequestPublisher: Send + Sync {
    /// Push the run's branch and open its pull request, or find the one an
    /// earlier attempt opened.
    async fn publish(&self, config: &StepConfig, profile: &Profile) -> PortResult<OpenPrOutcome>;
}

/// The files a step leaves in its worktree: the result its skill writes and
/// the `HEAD` the worker records beside it.
#[async_trait]
pub trait StepFiles: Send + Sync {
    /// Make a worktree ready for step files: keep `.vetinari/` out of git
    /// through the worktree's `info/exclude`, so a result is never committed
    /// and the developer's `.gitignore` is untouched.
    async fn prepare(&self, worktree: &Path) -> PortResult<()>;

    /// Read a file, or `None` when there is none.
    async fn read(&self, path: &Path) -> PortResult<Option<Vec<u8>>>;

    /// Write a file, creating its directory.
    async fn write(&self, path: &Path, contents: &[u8]) -> PortResult<()>;
}

/// The one thing the daemon persists: the leases it holds, so a restart
/// inside a lease costs nothing but the agent's restart.
#[async_trait]
pub trait LeaseJournal: Send + Sync {
    /// Every lease recorded, or none when nothing has been saved.
    async fn load(&self) -> PortResult<Vec<HeldLease>>;

    /// Replace what is recorded with `held`, atomically.
    async fn save(&self, held: &[HeldLease]) -> PortResult<()>;
}
