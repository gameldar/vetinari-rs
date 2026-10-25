//! Starting a run (design 4.3, 10.3): compile the run's own definition,
//! register it, start it, and refuse a second live run for one ticket.

use std::sync::Arc;

use serde_json::json;

use super::api::{RunItem, RunLabel};
use super::ports::{RunGateway, RunIds};
use crate::application::ports::PortError;
use crate::domain::devflow::compile::{StepPolicies, WorkerQueues, compile};
use crate::domain::devflow::ids::{DefinitionId, RunId};
use crate::domain::devflow::template::DevWorkflow;

/// A run that was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    /// The run.
    pub run: RunId,
    /// Its definition, which lives until the run is dismissed.
    pub definition: DefinitionId,
    /// What it is about.
    pub label: RunLabel,
}

/// Why a run was not started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The ticket already has a live run.
    #[error("{ticket} already has a live run, {run}")]
    Conflict {
        /// The ticket.
        ticket: String,
        /// Its live run.
        run: RunId,
    },
    /// The gateway refused or could not be reached.
    #[error(transparent)]
    Gateway(#[from] PortError),
}

fn label(workflow: &DevWorkflow) -> RunLabel {
    match workflow {
        DevWorkflow::FeatureFromTicket(feature) => RunLabel {
            ticket: feature.ticket.key.clone(),
            title: feature.ticket.title.clone(),
        },
    }
}

/// Starts runs on the gateway.
pub struct Launcher {
    runs: Arc<dyn RunGateway>,
    ids: Arc<dyn RunIds>,
    queues: WorkerQueues,
    policies: StepPolicies,
}

impl Launcher {
    /// A launcher whose runs' steps are claimed from `queues`.
    #[must_use]
    pub fn new(
        runs: Arc<dyn RunGateway>,
        ids: Arc<dyn RunIds>,
        queues: WorkerQueues,
        policies: StepPolicies,
    ) -> Self {
        Self {
            runs,
            ids,
            queues,
            policies,
        }
    }

    /// Start a run of `workflow`, unless a run of `live`, the runs not yet
    /// dismissed, is on its ticket: the two would share one branch and
    /// worktree, so a second run waits for the first to be dismissed,
    /// finished or not (design 15).
    pub async fn launch(
        &self,
        workflow: &DevWorkflow,
        live: &[RunItem],
    ) -> Result<Launched, LaunchError> {
        let label = label(workflow);
        if let Some(running) = live
            .iter()
            .find(|item| item.label.as_ref().map(|known| &known.ticket) == Some(&label.ticket))
        {
            return Err(LaunchError::Conflict {
                ticket: label.ticket.to_string(),
                run: running.summary.run.clone(),
            });
        }
        let definition = DefinitionId::for_run(workflow.kind(), &self.ids.next());
        let graph = compile(workflow, definition.clone(), &self.queues, &self.policies);
        self.runs.register(&graph).await?;
        match self.runs.start(&definition, &json!({})).await {
            Ok(run) => Ok(Launched {
                run,
                definition,
                label,
            }),
            Err(error) => {
                // A definition outlives its run only until it is dismissed;
                // one with no run would never be. Failing to delete it
                // leaves a definition with no runs, which nothing lists.
                let _ = self.runs.remove(&definition).await;
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::domain::devflow::ids::{Ulid, WorkerName};
    use crate::domain::devflow::run_view::RunStatus;
    use crate::domain::devflow::summary::RunSummary;
    use crate::domain::devflow::template::sample_feature;
    use crate::domain::text::NonEmptyText;
    use crate::testing::{CountingRunIds, FakeRunGateway};

    fn queues() -> WorkerQueues {
        WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker"))
    }

    fn launcher(runs: &Arc<FakeRunGateway>) -> Launcher {
        Launcher::new(
            runs.clone(),
            Arc::new(CountingRunIds::default()),
            queues(),
            StepPolicies::default(),
        )
    }

    fn workflow() -> DevWorkflow {
        DevWorkflow::FeatureFromTicket(sample_feature())
    }

    fn live(ticket: &str, status: RunStatus) -> RunItem {
        RunItem {
            summary: RunSummary {
                run: RunId::parse("01JB80").expect("run"),
                status,
                phases: Vec::new(),
                needs_you: None,
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse(ticket).expect("ticket"),
                title: NonEmptyText::parse("Something").expect("title"),
            }),
            started_at: Some(datetime!(2026-10-25 09:00 UTC)),
        }
    }

    #[tokio::test]
    async fn a_run_gets_a_definition_of_its_own_registered_and_started() {
        let runs = Arc::new(FakeRunGateway::default());
        let launched = launcher(&runs)
            .launch(&workflow(), &[live("ENG-9", RunStatus::Active)])
            .await
            .expect("launched");

        let definition = DefinitionId::for_run(
            crate::domain::devflow::template::TemplateKind::FeatureFromTicket,
            &Ulid::from_parts(1, 0),
        );
        assert_eq!(
            runs.registered(),
            [compile(
                &workflow(),
                definition.clone(),
                &queues(),
                &StepPolicies::default()
            )]
        );
        assert_eq!(runs.started(), [(definition.to_string(), json!({}))]);
        assert_eq!(launched.run.as_str(), "run-1");
        assert_eq!(launched.definition, definition);
        assert_eq!(launched.label.ticket.as_str(), "ENG-412");
        assert_eq!(
            launched.label.title.as_str(),
            "Rate-limit retries / honour 503"
        );
    }

    #[tokio::test]
    async fn a_ticket_with_a_live_run_is_not_given_a_second() {
        let runs = Arc::new(FakeRunGateway::default());
        let error = launcher(&runs)
            .launch(&workflow(), &[live("ENG-412", RunStatus::Active)])
            .await
            .expect_err("conflict");
        assert_eq!(
            error,
            LaunchError::Conflict {
                ticket: "ENG-412".to_owned(),
                run: RunId::parse("01JB80").expect("run"),
            }
        );
        assert!(runs.registered().is_empty());
    }

    #[tokio::test]
    async fn a_ticket_whose_finished_run_is_not_dismissed_is_not_given_a_second() {
        let runs = Arc::new(FakeRunGateway::default());
        let error = launcher(&runs)
            .launch(&workflow(), &[live("ENG-412", RunStatus::Completed)])
            .await
            .expect_err("the finished run still holds the branch");
        assert!(matches!(error, LaunchError::Conflict { .. }));
        assert!(runs.registered().is_empty());
    }

    #[tokio::test]
    async fn a_definition_whose_run_would_not_start_is_deleted() {
        let runs = Arc::new(FakeRunGateway::default());
        runs.fail_start(PortError::unavailable("autognomus", "down"));
        let error = launcher(&runs)
            .launch(&workflow(), &[])
            .await
            .expect_err("not started");
        assert!(matches!(
            error,
            LaunchError::Gateway(PortError::Unavailable { .. })
        ));
        assert_eq!(runs.registered().len(), 1);
        assert_eq!(runs.removed(), [runs.registered()[0].id.to_string()]);
    }

    #[tokio::test]
    async fn a_definition_that_would_not_register_is_not_started() {
        let runs = Arc::new(FakeRunGateway::default());
        runs.fail_register(PortError::Refused {
            what: "autognomus".to_owned(),
            detail: "exit not in body".to_owned(),
        });
        launcher(&runs)
            .launch(&workflow(), &[])
            .await
            .expect_err("not registered");
        assert!(runs.started().is_empty());
        assert!(runs.removed().is_empty());
    }
}
