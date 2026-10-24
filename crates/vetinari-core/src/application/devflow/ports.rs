//! Ports the dev workflow services call.

use async_trait::async_trait;

use crate::application::ports::PortResult;
use crate::domain::devflow::held::HeldLease;

/// The one thing the daemon persists: the leases it holds, so a restart
/// inside a lease costs nothing but the agent's restart.
#[async_trait]
pub trait LeaseJournal: Send + Sync {
    /// Every lease recorded, or none when nothing has been saved.
    async fn load(&self) -> PortResult<Vec<HeldLease>>;

    /// Replace what is recorded with `held`, atomically.
    async fn save(&self, held: &[HeldLease]) -> PortResult<()>;
}
