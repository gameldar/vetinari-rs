//! Starting and dismissing runs (design 4.4, 10.3): compile the run's own
//! definition, register it, start it, and refuse a second run for one
//! ticket until the first is dismissed.

use std::sync::Arc;

use serde_json::json;

use super::api::RunLabel;
use super::ports::{RunGateway, RunIds, RunWorktrees};
use crate::application::ports::PortError;
use crate::domain::devflow::compile::{GraphNode, StepPolicies, WorkerQueues, compile};
use crate::domain::devflow::ids::{DefinitionId, RunId};
use crate::domain::devflow::run_view::RunStatus;
use crate::domain::devflow::step::StepConfig;
use crate::domain::devflow::template::DevWorkflow;

/// A run of one of vetinari's definitions, and what its steps are
/// configured with: what the daemon knows of a run beyond its graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOrigin {
    /// The run.
    pub run: RunId,
    /// Its definition, which lives until the run is dismissed.
    pub definition: DefinitionId,
    /// The ticket, repository, profile and branch every step carries.
    pub config: StepConfig,
}

impl RunOrigin {
    /// What the run is about, for its queue row.
    #[must_use]
    pub fn label(&self) -> RunLabel {
        RunLabel {
            ticket: self.config.ticket.key.clone(),
            title: self.config.ticket.title.clone(),
        }
    }
}

/// Why a run was not started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The ticket already has a run that is not dismissed.
    #[error("{ticket} already has a run, {run}; dismiss it first")]
    Conflict {
        /// The ticket.
        ticket: String,
        /// Its run.
        run: RunId,
    },
    /// The gateway refused or could not be reached.
    #[error(transparent)]
    Gateway(#[from] PortError),
}

/// Why a run was not dismissed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DismissError {
    /// The run is still going; it has to finish or be cancelled first.
    #[error("run {run} is still going; cancel it first")]
    Active {
        /// The run.
        run: RunId,
    },
    /// Its worktree or its definition could not be removed.
    #[error(transparent)]
    Port(#[from] PortError),
}

/// Starts and dismisses runs on the gateway.
pub struct Launcher {
    runs: Arc<dyn RunGateway>,
    ids: Arc<dyn RunIds>,
    worktrees: Arc<dyn RunWorktrees>,
    queues: WorkerQueues,
    policies: StepPolicies,
}

impl Launcher {
    /// A launcher whose runs' steps are claimed from `queues`.
    #[must_use]
    pub fn new(
        runs: Arc<dyn RunGateway>,
        ids: Arc<dyn RunIds>,
        worktrees: Arc<dyn RunWorktrees>,
        queues: WorkerQueues,
        policies: StepPolicies,
    ) -> Self {
        Self {
            runs,
            ids,
            worktrees,
            queues,
            policies,
        }
    }

    /// Start a run of `workflow`, unless one of `owned`, the runs not yet
    /// dismissed, is on its ticket: the two would share one branch and
    /// worktree, so a second run waits for the first to be dismissed,
    /// finished or not (design 15).
    pub async fn launch(
        &self,
        workflow: &DevWorkflow,
        owned: &[RunOrigin],
    ) -> Result<RunOrigin, LaunchError> {
        let definition = DefinitionId::for_run(workflow.kind(), &self.ids.next());
        let graph = compile(workflow, definition.clone(), &self.queues, &self.policies);
        let config = graph
            .nodes
            .iter()
            .find_map(|node| match node {
                GraphNode::External { config, .. } => Some(config.as_ref().clone()),
                GraphNode::Loop { .. } => None,
            })
            .expect("every template has a step");
        if let Some(running) = owned
            .iter()
            .find(|origin| origin.config.ticket.key == config.ticket.key)
        {
            return Err(LaunchError::Conflict {
                ticket: config.ticket.key.to_string(),
                run: running.run.clone(),
            });
        }
        self.runs.register(&graph).await?;
        match self.runs.start(&definition, &json!({})).await {
            Ok(run) => Ok(RunOrigin {
                run,
                definition,
                config,
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

    /// Dismiss a finished run: delete its definition, which takes it off
    /// the queue, and first its worktree when asked. A worktree that will
    /// not go keeps the run, so the developer can dismiss it again without.
    pub async fn dismiss(
        &self,
        origin: &RunOrigin,
        status: RunStatus,
        remove_worktree: bool,
    ) -> Result<(), DismissError> {
        if status == RunStatus::Active {
            return Err(DismissError::Active {
                run: origin.run.clone(),
            });
        }
        if remove_worktree {
            self.worktrees.remove(&origin.config).await?;
        }
        self.runs.remove(&origin.definition).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::ids::{Ulid, WorkerName};
    use crate::domain::devflow::template::sample_feature;
    use crate::domain::text::NonEmptyText;
    use crate::testing::{CountingRunIds, FakeRunGateway, RecordingRunWorktrees};

    fn queues() -> WorkerQueues {
        WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker"))
    }

    fn launcher(runs: &Arc<FakeRunGateway>) -> Launcher {
        launcher_with(runs, &Arc::new(RecordingRunWorktrees::default()))
    }

    fn launcher_with(
        runs: &Arc<FakeRunGateway>,
        worktrees: &Arc<RecordingRunWorktrees>,
    ) -> Launcher {
        Launcher::new(
            runs.clone(),
            Arc::new(CountingRunIds::default()),
            worktrees.clone(),
            queues(),
            StepPolicies::default(),
        )
    }

    fn workflow() -> DevWorkflow {
        DevWorkflow::FeatureFromTicket(sample_feature())
    }

    fn owned(ticket: &str) -> RunOrigin {
        let mut feature = sample_feature();
        feature.ticket.key = NonEmptyText::parse(ticket).expect("ticket");
        let graph = compile(
            &DevWorkflow::FeatureFromTicket(feature),
            DefinitionId::parse("vetinari-feature-01JB80").expect("definition"),
            &queues(),
            &StepPolicies::default(),
        );
        let config = graph
            .nodes
            .into_iter()
            .find_map(|node| match node {
                GraphNode::External { config, .. } => Some(*config),
                GraphNode::Loop { .. } => None,
            })
            .expect("a step");
        RunOrigin {
            run: RunId::parse("01JB80").expect("run"),
            definition: graph.id,
            config,
        }
    }

    #[tokio::test]
    async fn a_run_gets_a_definition_of_its_own_registered_and_started() {
        let runs = Arc::new(FakeRunGateway::default());
        let launched = launcher(&runs)
            .launch(&workflow(), &[owned("ENG-9")])
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
        assert_eq!(launched.label().ticket.as_str(), "ENG-412");
        assert_eq!(
            launched.label().title.as_str(),
            "Rate-limit retries / honour 503"
        );
    }

    #[tokio::test]
    async fn a_ticket_with_a_live_run_is_not_given_a_second() {
        let runs = Arc::new(FakeRunGateway::default());
        let error = launcher(&runs)
            .launch(&workflow(), &[owned("ENG-412")])
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

    #[tokio::test]
    async fn dismissing_a_finished_run_deletes_its_definition_and_keeps_its_worktree() {
        let runs = Arc::new(FakeRunGateway::default());
        let worktrees = Arc::new(RecordingRunWorktrees::default());
        launcher_with(&runs, &worktrees)
            .dismiss(&owned("ENG-412"), RunStatus::Completed, false)
            .await
            .expect("dismissed");
        assert_eq!(runs.removed(), ["vetinari-feature-01JB80"]);
        assert!(worktrees.removed().is_empty());
    }

    #[tokio::test]
    async fn a_dismissal_may_remove_the_runs_worktree_too() {
        let runs = Arc::new(FakeRunGateway::default());
        let worktrees = Arc::new(RecordingRunWorktrees::default());
        launcher_with(&runs, &worktrees)
            .dismiss(&owned("ENG-412"), RunStatus::Cancelled, true)
            .await
            .expect("dismissed");
        assert_eq!(
            worktrees.removed(),
            ["vetinari/eng-412-rate-limit-retries-honour-503"]
        );
        assert_eq!(runs.removed(), ["vetinari-feature-01JB80"]);
    }

    #[tokio::test]
    async fn a_run_still_going_is_not_dismissed() {
        let runs = Arc::new(FakeRunGateway::default());
        let worktrees = Arc::new(RecordingRunWorktrees::default());
        let error = launcher_with(&runs, &worktrees)
            .dismiss(&owned("ENG-412"), RunStatus::Active, true)
            .await
            .expect_err("still going");
        assert_eq!(
            error,
            DismissError::Active {
                run: RunId::parse("01JB80").expect("run")
            }
        );
        assert!(runs.removed().is_empty());
        assert!(worktrees.removed().is_empty());
    }

    #[tokio::test]
    async fn a_worktree_that_will_not_go_keeps_the_run() {
        let runs = Arc::new(FakeRunGateway::default());
        let worktrees = Arc::new(RecordingRunWorktrees::default());
        worktrees.refuse(PortError::Refused {
            what: "git".to_owned(),
            detail: "the worktree has changes".to_owned(),
        });
        let error = launcher_with(&runs, &worktrees)
            .dismiss(&owned("ENG-412"), RunStatus::Failed, true)
            .await
            .expect_err("refused");
        assert!(matches!(
            error,
            DismissError::Port(PortError::Refused { .. })
        ));
        assert!(runs.removed().is_empty());
    }
}
