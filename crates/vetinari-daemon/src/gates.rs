//! Who answers a gate: the worker that holds it, a script, or nobody.

use async_trait::async_trait;
use vetinari_core::application::devflow::api::ApiError;
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::domain::devflow::ids::{NodeInstance, RunId};
use vetinari_core::domain::devflow::outcome::GateAnswer;

/// Answers the gates this daemon holds.
#[async_trait]
pub trait Gates: Send + Sync {
    /// Answer the gate `instance` of `run`.
    async fn answer(
        &self,
        run: &RunId,
        instance: &NodeInstance,
        answer: GateAnswer,
    ) -> Result<GateReply, ApiError>;
}

/// A daemon that holds no gates.
pub struct NoGates;

#[async_trait]
impl Gates for NoGates {
    async fn answer(
        &self,
        _run: &RunId,
        _instance: &NodeInstance,
        _answer: GateAnswer,
    ) -> Result<GateReply, ApiError> {
        Ok(GateReply::NotOpen)
    }
}
