//! Ports the dev workflow services call.

use std::path::Path;

use async_trait::async_trait;

use crate::application::ports::PortResult;
use crate::domain::devflow::held::HeldLease;

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
