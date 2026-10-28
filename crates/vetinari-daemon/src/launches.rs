//! Who starts and dismisses runs: the launcher over the gateway, or nobody
//! for a daemon that replays a script.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use vetinari_core::application::devflow::api::ApiError;
use vetinari_core::application::devflow::launcher::{
    DismissError, LaunchError, Launcher, RunOrigin,
};
use vetinari_core::application::devflow::ports::RepoOrigins;
use vetinari_core::application::devflow::start::{self, StartError};
use vetinari_core::application::ports::Clock;
use vetinari_core::domain::devflow::ids::RunId;
use vetinari_core::domain::devflow::input::WorkflowInput;
use vetinari_core::domain::devflow::template::DevWorkflow;
use vetinari_core::domain::profile::ProfileSet;

use crate::hub::Hub;
use crate::runtime::{OwnedRun, RunChange};

/// Starts and dismisses this daemon's runs.
#[async_trait]
pub trait Launches: Send + Sync {
    /// Start a run of the workflow `input` asks for.
    async fn start(&self, input: WorkflowInput) -> Result<RunId, ApiError>;

    /// Take a finished run off the queue, and its worktree with it when
    /// asked.
    async fn dismiss(&self, run: &RunId, remove_worktree: bool) -> Result<(), ApiError>;
}

/// A daemon that starts nothing: one replaying a script.
pub struct NoLaunches;

const SCRIPTED: &str = "this daemon replays a script and starts or dismisses no runs";

#[async_trait]
impl Launches for NoLaunches {
    async fn start(&self, _input: WorkflowInput) -> Result<RunId, ApiError> {
        Err(ApiError::Invalid(SCRIPTED.to_owned()))
    }

    async fn dismiss(&self, _run: &RunId, _remove_worktree: bool) -> Result<(), ApiError> {
        Err(ApiError::Invalid(SCRIPTED.to_owned()))
    }
}

/// What a start is checked against: the profiles vetinari has, and where
/// a clone given by path alone points.
pub struct Starts {
    /// The profiles a start may name.
    pub profiles: ProfileSet,
    /// Reads a clone's `origin`.
    pub origins: Arc<dyn RepoOrigins>,
}

/// The launcher over the gateway, knowing every run this daemon owns by
/// its definition. Starts and dismissals take turns, so two starts for one
/// ticket cannot both pass the check for a run already on it.
pub struct GatewayLaunches {
    launcher: Launcher,
    starts: Starts,
    hub: Arc<Hub>,
    clock: Arc<dyn Clock>,
    origins: Mutex<BTreeMap<RunId, RunOrigin>>,
    feed: mpsc::UnboundedSender<RunChange>,
}

impl GatewayLaunches {
    /// Launches through `launcher`, telling the feed of each run it starts
    /// or dismisses.
    #[must_use]
    pub fn new(
        launcher: Launcher,
        starts: Starts,
        hub: Arc<Hub>,
        clock: Arc<dyn Clock>,
        feed: mpsc::UnboundedSender<RunChange>,
    ) -> Self {
        Self {
            launcher,
            starts,
            hub,
            clock,
            origins: Mutex::default(),
            feed,
        }
    }

    /// Know runs found on the gateway, so they can be checked against and
    /// dismissed.
    pub async fn know(&self, found: impl IntoIterator<Item = RunOrigin>) {
        let mut origins = self.origins.lock().await;
        for origin in found {
            origins.insert(origin.run.clone(), origin);
        }
    }
}

#[async_trait]
impl Launches for GatewayLaunches {
    async fn start(&self, input: WorkflowInput) -> Result<RunId, ApiError> {
        let workflow: DevWorkflow =
            start::workflow(input, &self.starts.profiles, self.starts.origins.as_ref())
                .await
                .map_err(|error| match error {
                    StartError::Port(error) => error.into(),
                    refused => ApiError::Invalid(refused.to_string()),
                })?;
        let mut origins = self.origins.lock().await;
        let owned: Vec<RunOrigin> = origins.values().cloned().collect();
        let origin =
            self.launcher
                .launch(&workflow, &owned)
                .await
                .map_err(|error| match error {
                    LaunchError::Conflict { .. } => ApiError::Conflict(error.to_string()),
                    LaunchError::Gateway(error) => error.into(),
                })?;
        let run = origin.run.clone();
        let _ = self.feed.send(RunChange::Owned(OwnedRun {
            run: run.clone(),
            label: Some(origin.label()),
            started_at: Some(self.clock.now()),
        }));
        origins.insert(run.clone(), origin);
        Ok(run)
    }

    async fn dismiss(&self, run: &RunId, remove_worktree: bool) -> Result<(), ApiError> {
        let mut origins = self.origins.lock().await;
        let not_found = || ApiError::NotFound(format!("run {run} is not one this daemon owns"));
        let origin = origins.get(run).ok_or_else(not_found)?;
        let item = self.hub.item(run).ok_or_else(not_found)?;
        self.launcher
            .dismiss(origin, item.summary.status, remove_worktree)
            .await
            .map_err(|error| match error {
                DismissError::Active { .. } => ApiError::Conflict(error.to_string()),
                DismissError::Port(error) => error.into(),
            })?;
        origins.remove(run);
        let _ = self.feed.send(RunChange::Dismissed(run.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use time::macros::datetime;
    use vetinari_core::application::devflow::api::{RunItem, RunLabel};
    use vetinari_core::application::ports::PortResult;
    use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues};
    use vetinari_core::domain::devflow::ids::{Rounds, WorkerName};
    use vetinari_core::domain::devflow::run_view::RunStatus;
    use vetinari_core::domain::devflow::summary::RunSummary;
    use vetinari_core::domain::devflow::template::{
        FeatureFromTicket, RepoTarget, TemplateKind, TicketRef,
    };
    use vetinari_core::domain::ids::{ProfileId, SourceId};
    use vetinari_core::domain::profile::Profile;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::domain::workspace::TargetKind;
    use vetinari_core::testing::{
        CountingRunIds, FakeRunGateway, FixedClock, RecordingRunWorktrees,
    };

    use super::*;

    const NOW: time::OffsetDateTime = datetime!(2026-10-25 10:00 UTC);

    struct Harness {
        runs: Arc<FakeRunGateway>,
        hub: Arc<Hub>,
        launches: GatewayLaunches,
        fed: mpsc::UnboundedReceiver<RunChange>,
    }

    struct Origins;

    #[async_trait]
    impl RepoOrigins for Origins {
        async fn origin(&self, _clone: &std::path::Path) -> PortResult<String> {
            Ok("git@github.com:acme/web.git".to_owned())
        }
    }

    fn profiles() -> ProfileSet {
        let work = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Tmux,
        );
        ProfileSet::new(work, Vec::new()).expect("profiles")
    }

    fn harness() -> Harness {
        let runs = Arc::new(FakeRunGateway::default());
        let hub = Arc::new(Hub::default());
        let (feed, fed) = mpsc::unbounded_channel();
        let launcher = Launcher::new(
            runs.clone(),
            Arc::new(CountingRunIds::default()),
            Arc::new(RecordingRunWorktrees::default()),
            WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
            StepPolicies::default(),
        );
        Harness {
            runs,
            hub: hub.clone(),
            launches: GatewayLaunches::new(
                launcher,
                Starts {
                    profiles: profiles(),
                    origins: Arc::new(Origins),
                },
                hub,
                Arc::new(FixedClock::new(NOW)),
                feed,
            ),
            fed,
        }
    }

    fn workflow(ticket: &str) -> WorkflowInput {
        WorkflowInput::from(DevWorkflow::FeatureFromTicket(FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse("jira").expect("source"),
                key: NonEmptyText::parse(ticket).expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
                url: None,
            },
            repo: RepoTarget {
                path: PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: ProfileId::parse("work").expect("profile"),
            base: None,
            max_review_rounds: Rounds::DEFAULT,
            max_intake_rounds: Rounds::DEFAULT,
            agent: None,
        }))
    }

    fn clone_of(profile: &str) -> WorkflowInput {
        TemplateKind::FeatureFromTicket
            .parse_input(&json!({
                "ticket": { "source": "jira", "key": "ENG-7", "title": "Rate-limit retries" },
                "repo": { "path": "/src/web" },
                "profile": profile,
            }))
            .expect("input")
    }

    fn label(ticket: &str) -> RunLabel {
        RunLabel {
            ticket: NonEmptyText::parse(ticket).expect("key"),
            title: NonEmptyText::parse("Rate-limit retries").expect("title"),
        }
    }

    fn on_board(hub: &Hub, run: &RunId, status: RunStatus) {
        hub.put(RunItem {
            summary: RunSummary {
                run: run.clone(),
                status,
                phases: Vec::new(),
                needs_you: None,
            },
            label: Some(label("ENG-412")),
            started_at: Some(NOW),
        });
    }

    #[tokio::test]
    async fn a_started_run_is_owned_by_the_feed_with_its_label() {
        let mut h = harness();
        let run = h
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect("started");
        assert_eq!(run.as_str(), "run-1");
        assert_eq!(
            h.fed.try_recv(),
            Ok(RunChange::Owned(OwnedRun {
                run,
                label: Some(label("ENG-412")),
                started_at: Some(NOW),
            }))
        );
    }

    #[tokio::test]
    async fn a_clone_is_started_as_the_repository_its_origin_names() {
        let h = harness();
        h.launches.start(clone_of("work")).await.expect("started");
        assert_eq!(h.runs.registered().len(), 1);
        let origins = h.launches.origins.lock().await;
        let origin = origins.values().next().expect("owned");
        assert_eq!(origin.config.repo.remote.as_str(), "acme/web");
        assert_eq!(origin.config.repo.path, PathBuf::from("/src/web"));
    }

    #[tokio::test]
    async fn a_start_under_a_profile_vetinari_lacks_is_invalid() {
        let h = harness();
        let error = h
            .launches
            .start(clone_of("home"))
            .await
            .expect_err("invalid");
        assert_eq!(error.code(), "invalid");
        assert!(error.to_string().contains("home"), "{error}");
        assert!(h.runs.registered().is_empty());
    }

    #[tokio::test]
    async fn a_second_start_for_a_ticket_is_a_conflict() {
        let h = harness();
        h.launches
            .start(workflow("ENG-412"))
            .await
            .expect("started");
        let error = h
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect_err("conflict");
        assert_eq!(error.code(), "conflict");
        assert_eq!(h.runs.registered().len(), 1);
        h.launches
            .start(workflow("ENG-9"))
            .await
            .expect("another ticket starts");
    }

    #[tokio::test]
    async fn a_run_found_on_the_gateway_holds_its_ticket_too() {
        let h = harness();
        let first = harness();
        first
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect("started");
        let found = first
            .launches
            .origins
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        h.launches.know(found).await;
        let error = h
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect_err("conflict");
        assert_eq!(error.code(), "conflict");
    }

    #[tokio::test]
    async fn dismissing_a_finished_run_deletes_its_definition_and_frees_its_ticket() {
        let mut h = harness();
        let run = h
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect("started");
        h.fed.try_recv().expect("owned");
        on_board(&h.hub, &run, RunStatus::Completed);

        h.launches.dismiss(&run, false).await.expect("dismissed");
        assert_eq!(h.runs.removed(), [h.runs.registered()[0].id.to_string()]);
        assert_eq!(h.fed.try_recv(), Ok(RunChange::Dismissed(run)));
        h.launches
            .start(workflow("ENG-412"))
            .await
            .expect("the ticket may run again");
    }

    #[tokio::test]
    async fn dismissing_a_run_still_going_is_a_conflict() {
        let h = harness();
        let run = h
            .launches
            .start(workflow("ENG-412"))
            .await
            .expect("started");
        on_board(&h.hub, &run, RunStatus::Active);
        let error = h.launches.dismiss(&run, true).await.expect_err("going");
        assert_eq!(error.code(), "conflict");
        assert!(h.runs.removed().is_empty());
    }

    #[tokio::test]
    async fn dismissing_a_run_this_daemon_does_not_own_is_not_found() {
        let h = harness();
        let error = h
            .launches
            .dismiss(&RunId::parse("r9").expect("run"), false)
            .await
            .expect_err("unknown");
        assert_eq!(error.code(), "not-found");
    }

    #[tokio::test]
    async fn a_scripted_daemon_starts_and_dismisses_nothing() {
        assert_eq!(
            NoLaunches
                .start(workflow("ENG-412"))
                .await
                .expect_err("scripted")
                .code(),
            "invalid"
        );
    }
}
