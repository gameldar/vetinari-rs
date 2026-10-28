//! Test doubles for the daemon's own ports.

use std::sync::Mutex;

use async_trait::async_trait;
use vetinari_core::application::devflow::api::ApiError;
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::domain::devflow::ids::{NodeInstance, RunId};
use vetinari_core::domain::devflow::input::WorkflowInput;
use vetinari_core::domain::devflow::outcome::GateAnswer;

use crate::gates::Gates;
use crate::launches::Launches;

/// Starts each ticket once, as `run-<n>`, and records dismissals.
#[derive(Default)]
pub struct RecordingLaunches {
    pub started: Mutex<Vec<String>>,
    pub dismissed: Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl Launches for RecordingLaunches {
    async fn start(&self, input: WorkflowInput) -> Result<RunId, ApiError> {
        let WorkflowInput::FeatureFromTicket(feature) = input;
        let mut started = self.started.lock().expect("lock");
        let ticket = feature.ticket.key.to_string();
        if started.contains(&ticket) {
            return Err(ApiError::Conflict(format!("{ticket} already has a run")));
        }
        started.push(ticket);
        Ok(RunId::parse(&format!("run-{}", started.len())).expect("run"))
    }

    async fn dismiss(&self, run: &RunId, remove_worktree: bool) -> Result<(), ApiError> {
        self.dismissed
            .lock()
            .expect("lock")
            .push((run.to_string(), remove_worktree));
        Ok(())
    }
}

#[derive(Default)]
pub struct RecordingGates(pub Mutex<Vec<(String, String, GateAnswer)>>);

#[async_trait]
impl Gates for RecordingGates {
    async fn answer(
        &self,
        run: &RunId,
        instance: &NodeInstance,
        answer: GateAnswer,
    ) -> Result<GateReply, ApiError> {
        self.0
            .lock()
            .expect("lock")
            .push((run.to_string(), instance.to_string(), answer));
        Ok(GateReply::Sent)
    }
}
