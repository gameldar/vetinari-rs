//! The daemon's step tasks: one task owns the worker and the agent
//! registry, which both want `&mut self` across awaits, and one claim loop
//! per queue long-polls without holding either (design 7.2, 7.4).
//!
//! A claim loop asks the owner whether there is capacity, long-polls the
//! gateway itself, and hands what it gets to the owner, waiting until it is
//! started before it asks again; so two claims never race for one permit.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use vetinari_core::application::agents::AgentRegistry;
use vetinari_core::application::devflow::api::ApiError;
use vetinari_core::application::devflow::ports::TaskGateway;
use vetinari_core::application::devflow::worker::{CLAIM_WAIT, GateReply, StepWorker};
use vetinari_core::application::ports::{Clock, PortError, PortResult};
use vetinari_core::domain::agent::AgentStatus;
use vetinari_core::domain::devflow::held::HeldState;
use vetinari_core::domain::devflow::ids::{NodeInstance, RunId, TaskQueue, WorkerName};
use vetinari_core::domain::devflow::outcome::GateAnswer;
use vetinari_core::domain::devflow::summary::{LocalStep, LocalSteps};
use vetinari_core::domain::devflow::task::RawTask;
use vetinari_core::domain::ids::SessionId;

use crate::gates::Gates;
use crate::signin::SignIn;

/// What the owner of the worker is asked to do.
pub(crate) enum Command {
    CanClaim {
        queue: TaskQueue,
        reply: oneshot::Sender<bool>,
    },
    Accept {
        raw: RawTask,
        reply: oneshot::Sender<()>,
    },
    Ending(RunId),
    AnswerGate {
        run: RunId,
        instance: NodeInstance,
        answer: GateAnswer,
        reply: oneshot::Sender<PortResult<GateReply>>,
    },
}

/// A handle on the task that owns the worker.
#[derive(Clone)]
pub struct Steps(mpsc::Sender<Command>);

impl Steps {
    /// Whether the worker has room for a claim from `queue`; `None` once
    /// the owner has stopped.
    async fn can_claim(&self, queue: &TaskQueue) -> Option<bool> {
        let (reply, answer) = oneshot::channel();
        self.0
            .send(Command::CanClaim {
                queue: queue.clone(),
                reply,
            })
            .await
            .ok()?;
        answer.await.ok()
    }

    /// Hand a claimed task to the worker and wait until it is started.
    async fn accept(&self, raw: RawTask) -> bool {
        let (reply, answer) = oneshot::channel();
        if self.0.send(Command::Accept { raw, reply }).await.is_err() {
            return false;
        }
        answer.await.is_ok()
    }

    /// A handle no owner answers, and the commands sent to it, for tests.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, mpsc::Receiver<Command>) {
        let (sender, receiver) = mpsc::channel(COMMANDS);
        (Self(sender), receiver)
    }

    /// The run may be over: heartbeat its leases now (design 7.3).
    pub fn ending(&self, run: RunId) {
        let _ = self.0.try_send(Command::Ending(run));
    }
}

#[async_trait]
impl Gates for Steps {
    async fn answer(
        &self,
        run: &RunId,
        instance: &NodeInstance,
        answer: GateAnswer,
    ) -> Result<GateReply, ApiError> {
        let (reply, answered) = oneshot::channel();
        let gone = || ApiError::Unavailable("the worker has stopped".to_owned());
        self.0
            .send(Command::AnswerGate {
                run: run.clone(),
                instance: instance.clone(),
                answer,
                reply,
            })
            .await
            .map_err(|_| gone())?;
        Ok(answered.await.map_err(|_| gone())??)
    }
}

/// What this machine knows about each run's steps, for the summary, and
/// which runs it just changed.
pub struct LocalBoard {
    steps: Mutex<BTreeMap<RunId, LocalSteps>>,
    touched: mpsc::UnboundedSender<RunId>,
}

impl LocalBoard {
    /// A board, and the receiver that hears which runs it changed.
    #[must_use]
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<RunId>) {
        let (touched, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                steps: Mutex::default(),
                touched,
            }),
            receiver,
        )
    }

    /// A run's local steps; none when it has none here.
    #[must_use]
    pub fn get(&self, run: &RunId) -> LocalSteps {
        self.steps
            .lock()
            .expect("local lock")
            .get(run)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace every run's steps, telling the receiver of each run that
    /// changed.
    pub(crate) fn replace(&self, next: BTreeMap<RunId, LocalSteps>) {
        let mut steps = self.steps.lock().expect("local lock");
        let mut touched: Vec<RunId> = steps
            .iter()
            .filter(|(run, held)| next.get(*run) != Some(*held))
            .map(|(run, _)| run.clone())
            .collect();
        touched.extend(next.keys().filter(|run| !steps.contains_key(*run)).cloned());
        *steps = next;
        drop(steps);
        for run in touched {
            let _ = self.touched.send(run);
        }
    }
}

/// What the owner of the worker is built from.
pub struct StepParts {
    /// The worker.
    pub worker: StepWorker,
    /// The agent engines and their sessions.
    pub registry: AgentRegistry,
    /// The clock the worker reads.
    pub clock: Arc<dyn Clock>,
    /// How often agent sessions are polled.
    pub tick: StdDuration,
}

/// How long the owner waits after a heartbeat it could not send for a
/// reason a retry will not fix, so one bad lease cannot spin it.
const HEARTBEAT_PAUSE: StdDuration = StdDuration::from_secs(5);

/// How many commands may wait for the owner.
const COMMANDS: usize = 64;

/// Start the owner of the worker. It recovers the journal before it
/// answers anything, so no claim is made before every held lease has been
/// heartbeated (design 7.7).
#[must_use]
pub fn spawn_steps(parts: StepParts, local: Arc<LocalBoard>) -> (Steps, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(COMMANDS);
    (Steps(sender), tokio::spawn(own(parts, receiver, local)))
}

async fn own(parts: StepParts, mut commands: mpsc::Receiver<Command>, local: Arc<LocalBoard>) {
    let StepParts {
        mut worker,
        mut registry,
        clock,
        tick,
    } = parts;
    if let Err(error) = worker.recover(&mut registry).await {
        tracing::warn!(%error, "cannot recover the lease journal");
    }
    publish(&worker, &registry, &local);
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut paused_until: Option<tokio::time::Instant> = None;
    loop {
        let due = worker.next_due().map(|due| {
            let wait = StdDuration::try_from(due - clock.now()).unwrap_or(StdDuration::ZERO);
            let at = tokio::time::Instant::now() + wait;
            paused_until.map_or(at, |paused| at.max(paused))
        });
        tokio::select! {
            command = commands.recv() => match command {
                None => return,
                Some(Command::CanClaim { queue, reply }) => {
                    let _ = reply.send(worker.can_claim(&queue));
                }
                Some(Command::Accept { raw, reply }) => {
                    if let Err(error) = worker.accept(raw, &mut registry).await {
                        tracing::warn!(%error, "cannot start a claimed step");
                    }
                    let _ = reply.send(());
                }
                Some(Command::Ending(run)) => worker.hint_cancelled(&run),
                Some(Command::AnswerGate { run, instance, answer, reply }) => {
                    let _ = reply.send(worker.answer_gate(&run, &instance, answer).await);
                }
            },
            () = sleep_until(due) => {
                paused_until = None;
                for task in worker.heartbeats_due() {
                    if let Err(error) = worker.heartbeat(&task, &mut registry).await {
                        tracing::warn!(%error, %task, "cannot heartbeat a step");
                        paused_until = Some(tokio::time::Instant::now() + HEARTBEAT_PAUSE);
                    }
                }
            }
            _ = ticker.tick() => poll(&mut worker, &mut registry).await,
        }
        publish(&worker, &registry, &local);
    }
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Ask the engines how their sessions are, and tell the worker of each that
/// changed.
async fn poll(worker: &mut StepWorker, registry: &mut AgentRegistry) {
    let before = statuses(registry);
    for (session, error) in registry.poll().await {
        tracing::warn!(%error, %session, "cannot check an agent");
    }
    for (session, status) in statuses(registry) {
        if before.get(&session) == Some(&status) {
            continue;
        }
        if let Err(error) = worker.agent_changed(&session, &status, registry).await {
            tracing::warn!(%error, %session, "cannot report a finished agent");
        }
    }
}

fn statuses(registry: &AgentRegistry) -> BTreeMap<SessionId, AgentStatus> {
    registry
        .sessions()
        .into_iter()
        .map(|session| (session.id().clone(), session.status().clone()))
        .collect()
}

/// What the worker holds, as each run's local steps.
fn publish(worker: &StepWorker, registry: &AgentRegistry, local: &LocalBoard) {
    let mut runs: BTreeMap<RunId, LocalSteps> = BTreeMap::new();
    for held in worker.held() {
        let step = match &held.state {
            HeldState::Running { session, .. } => LocalStep::Running {
                session: session.clone(),
                question: registry
                    .session(session)
                    .and_then(|session| match session.status() {
                        AgentStatus::WaitingForInput { question } => Some(question.clone()),
                        _ => None,
                    }),
            },
            HeldState::AwaitingPerson => LocalStep::AwaitingPerson,
            HeldState::Starting | HeldState::Completing { .. } | HeldState::Failing { .. } => {
                continue;
            }
        };
        let run = held.task.id.run().clone();
        let steps = runs.remove(&run).unwrap_or_default();
        runs.insert(run, steps.with(held.task.id.instance().clone(), step));
    }
    local.replace(runs);
}

/// The shortest time between two claims on one queue, so a gateway that
/// answers an empty claim at once is not asked again in a tight loop.
const CLAIM_PAUSE: StdDuration = StdDuration::from_secs(1);

/// The longest backoff after the gateway could not be reached.
const MAX_BACKOFF: StdDuration = StdDuration::from_secs(60);

/// Claim from one queue for as long as the owner runs: ask for capacity,
/// long-poll, hand over what comes. An unreachable gateway backs off 1, 2,
/// 4 ... 60 seconds. A token the gateway refuses even renewed asks for a
/// sign-in and waits for it when the daemon can sign in, and backs off like
/// an unreachable gateway when the token comes from a file, which may yet be
/// rotated; any other refusal stops the loop, since asking again will not
/// change the answer.
pub async fn claim_loop(
    tasks: Arc<dyn TaskGateway>,
    queue: TaskQueue,
    worker: WorkerName,
    steps: Steps,
    sign_in: Option<Arc<SignIn>>,
) {
    let mut backoff = CLAIM_PAUSE;
    loop {
        match steps.can_claim(&queue).await {
            None => return,
            Some(false) => {
                tokio::time::sleep(CLAIM_PAUSE).await;
                continue;
            }
            Some(true) => {}
        }
        let asked = tokio::time::Instant::now();
        match tasks.claim(&queue, &worker, CLAIM_WAIT).await {
            Ok(Some(raw)) => {
                backoff = CLAIM_PAUSE;
                if !steps.accept(raw).await {
                    return;
                }
            }
            Ok(None) => {
                backoff = CLAIM_PAUSE;
                tokio::time::sleep_until(asked + CLAIM_PAUSE).await;
            }
            Err(error) if error.is_transient() => {
                tracing::warn!(%error, %queue, "cannot claim; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(error) => match (&error, &sign_in) {
                (PortError::Unauthenticated { .. }, Some(sign_in)) => {
                    tracing::warn!(%queue, "autognomus needs a sign-in; claiming waits for it");
                    sign_in.lost();
                    sign_in.signed_in().await;
                    backoff = CLAIM_PAUSE;
                }
                (PortError::Unauthenticated { .. }, None) => {
                    tracing::warn!(%queue, "autognomus refused the token file's token; backing off");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                _ => {
                    tracing::error!(%error, %queue, "claiming stopped");
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use time::OffsetDateTime;
    use time::macros::datetime;
    use vetinari_adapters::autognomus::wire::ClaimReply;
    use vetinari_core::application::devflow::worker::{WorkerPorts, WorkerSettings};
    use vetinari_core::application::workflows::WorkflowEngine;
    use vetinari_core::application::workspaces::WorkspaceDirectory;
    use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues};
    use vetinari_core::domain::devflow::held::HeldLease;
    use vetinari_core::domain::devflow::outcome::{PreflightOutcome, StepOutcome};
    use vetinari_core::domain::devflow::task::ClaimedTask;
    use vetinari_core::domain::ids::{AgentId, ProfileId};
    use vetinari_core::domain::profile::{Profile, ProfileSet};
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::domain::workspace::TargetKind;
    use vetinari_core::testing::{
        FakeAgentEngine, FakePullRequestPublisher, FakeRunGateway, FakeStepFiles, FakeTaskGateway,
        FakeWorktreeManager, MemoryLeaseJournal, MemorySkillStore, TaskCall,
    };

    use vetinari_core::application::ports::PortError;

    use super::*;
    use crate::runtime::TokioClock;

    const START: OffsetDateTime = datetime!(2026-10-25 10:00 UTC);
    const TREE: &str = "/work/trees/web-eng-412-rate-limit-webhook-retries";
    const RUN: &str = "01JB7ZR2M4Q9V8C6T3N5HKD0WX";
    const TASK: &str = "01JB7ZR2M4Q9V8C6T3N5HKD0WX~preflight@1~1";

    struct Harness {
        steps: Steps,
        tasks: Arc<FakeTaskGateway>,
        journal: Arc<MemoryLeaseJournal>,
        files: Arc<FakeStepFiles>,
        claude: Arc<FakeAgentEngine>,
        local: Arc<LocalBoard>,
        touched: mpsc::UnboundedReceiver<RunId>,
    }

    fn worker() -> WorkerName {
        WorkerName::parse("hex").expect("worker")
    }

    fn queues() -> WorkerQueues {
        WorkerQueues::for_worker(&worker())
    }

    fn raw() -> RawTask {
        serde_json::from_str::<ClaimReply>(include_str!(
            "../../vetinari-adapters/tests/fixtures/autognomus/claim-preflight-first.json"
        ))
        .expect("claim")
        .into_raw()
        .expect("raw")
    }

    fn start(tasks: Arc<FakeTaskGateway>, journal: Arc<MemoryLeaseJournal>) -> Harness {
        let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(START));
        let files = Arc::new(FakeStepFiles::default());
        let worktrees = Arc::new(FakeWorktreeManager::default());
        let claude = Arc::new(FakeAgentEngine::new(
            AgentId::parse("claude-code").expect("id"),
        ));
        let mut registry = AgentRegistry::new(clock.clone());
        registry.register(claude.clone());
        let profile = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Herdr,
        )
        .with_worktree_root(PathBuf::from("/work/trees"));
        let worker = StepWorker::new(
            WorkerPorts {
                tasks: tasks.clone(),
                runs: Arc::new(FakeRunGateway::default()),
                journal: journal.clone(),
                files: files.clone(),
                worktrees: worktrees.clone(),
                skills: Arc::new(MemorySkillStore::default()),
                publisher: Arc::new(FakePullRequestPublisher::default()),
                clock: clock.clone(),
            },
            Arc::new(WorkflowEngine::new(
                worktrees,
                WorkspaceDirectory::default(),
            )),
            ProfileSet::new(profile, Vec::new()).expect("profiles"),
            WorkerSettings {
                name: worker(),
                queues: queues(),
                max_agent_steps: NonZeroUsize::new(1).expect("non-zero"),
                policies: StepPolicies::default(),
            },
        );
        let (local, touched) = LocalBoard::new();
        let (steps, _owner) = spawn_steps(
            StepParts {
                worker,
                registry,
                clock,
                tick: StdDuration::from_millis(500),
            },
            local.clone(),
        );
        Harness {
            steps,
            tasks,
            journal,
            files,
            claude,
            local,
            touched,
        }
    }

    fn harness() -> Harness {
        start(
            Arc::new(FakeTaskGateway::new(datetime!(2026-10-25 10:10 UTC))),
            Arc::new(MemoryLeaseJournal::default()),
        )
    }

    fn claim_on(h: &Harness, queue: TaskQueue) {
        tokio::spawn(claim_loop(
            h.tasks.clone(),
            queue,
            worker(),
            h.steps.clone(),
            None,
        ));
    }

    async fn settle(for_: StdDuration) {
        tokio::time::sleep(for_).await;
    }

    fn claims(h: &Harness) -> usize {
        h.tasks
            .calls()
            .iter()
            .filter(|call| matches!(call, TaskCall::Claim(_)))
            .count()
    }

    fn heartbeats(h: &Harness) -> usize {
        h.tasks
            .calls()
            .iter()
            .filter(|call| matches!(call, TaskCall::Heartbeat(_)))
            .count()
    }

    fn run() -> RunId {
        RunId::parse(RUN).expect("run")
    }

    #[tokio::test(start_paused = true)]
    async fn a_claimed_step_runs_and_its_finished_agent_completes_the_task() {
        let mut h = harness();
        h.tasks.on_claim(Ok(Some(raw())));
        claim_on(&h, queues().agent);
        settle(StdDuration::from_millis(100)).await;

        let spawned = h.claude.spawned();
        assert_eq!(spawned.len(), 1, "the preflight's agent is started");
        assert!(matches!(
            h.local
                .get(&run())
                .get(&NodeInstance::parse("preflight@1").expect("instance")),
            Some(LocalStep::Running { question: None, .. })
        ));
        assert_eq!(h.touched.recv().await, Some(run()));

        let result = PathBuf::from(TREE).join(".vetinari/steps/preflight@1-1.json");
        h.files.put(
            &result,
            r#"{"schema":1,"outcome":"ready","summary":"Retry with backoff"}"#,
        );
        h.claude.set_status(
            &SessionId::parse("claude-code-1").expect("id"),
            AgentStatus::Succeeded,
        );
        settle(StdDuration::from_secs(1)).await;

        let completes: Vec<_> = h
            .tasks
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                TaskCall::Complete(task, output) => Some((task, output)),
                _ => None,
            })
            .collect();
        assert_eq!(completes.len(), 1);
        assert_eq!(completes[0].0, TASK);
        assert!(matches!(
            completes[0].1.outcome,
            StepOutcome::Preflight(PreflightOutcome::Ready { .. })
        ));
        assert_eq!(h.local.get(&run()), LocalSteps::default());
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_machine_leaves_the_agent_queue_alone() {
        let h = harness();
        h.tasks.on_claim(Ok(Some(raw())));
        claim_on(&h, queues().agent);
        settle(StdDuration::from_secs(10)).await;
        assert_eq!(claims(&h), 1, "one agent step fills a one-step machine");
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_queue_is_asked_again_but_not_in_a_tight_loop() {
        let h = harness();
        claim_on(&h, queues().human);
        settle(StdDuration::from_millis(3500)).await;
        assert_eq!(claims(&h), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_gateway_is_claimed_from_with_backoff() {
        let h = harness();
        for _ in 0..3 {
            h.tasks
                .on_claim(Err(PortError::unavailable("autognomus", "down")));
        }
        claim_on(&h, queues().human);
        settle(StdDuration::from_millis(7500)).await;
        assert_eq!(claims(&h), 4, "at 0, 1, 3 and 7 seconds");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_claim_stops_the_loop() {
        let h = harness();
        h.tasks.on_claim(Err(PortError::Refused {
            what: "autognomus".to_owned(),
            detail: "sign in".to_owned(),
        }));
        claim_on(&h, queues().human);
        settle(StdDuration::from_secs(120)).await;
        assert_eq!(claims(&h), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_token_the_daemon_cannot_sign_in_for_is_tried_again_later() {
        let h = harness();
        h.tasks.on_claim(Err(PortError::Unauthenticated {
            provider: vetinari_core::domain::credentials::Provider::Autognomus,
        }));
        claim_on(&h, queues().human);
        settle(StdDuration::from_secs(120)).await;
        assert!(
            claims(&h) >= 2,
            "a token file may be rotated: {}",
            claims(&h)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_refused_for_its_token_waits_for_a_sign_in_then_claims_again() {
        use vetinari_core::application::auth::AuthService;
        use vetinari_core::domain::credentials::{
            CredentialKey, Provider, Secret, StoredCredential,
        };
        use vetinari_core::testing::{FakeDeviceCodeFlow, FixedClock, MemoryCredentialStore};

        let h = harness();
        let store = Arc::new(MemoryCredentialStore::default());
        let sign_in = crate::signin::SignIn::new(Arc::new(AuthService::new(
            store.clone(),
            Arc::new(FakeDeviceCodeFlow::new(1, datetime!(2026-10-25 11:00 UTC))),
            Arc::new(FixedClock::new(datetime!(2026-10-25 10:00 UTC))),
        )));
        h.tasks.on_claim(Err(PortError::Unauthenticated {
            provider: Provider::Autognomus,
        }));
        tokio::spawn(claim_loop(
            h.tasks.clone(),
            queues().human,
            worker(),
            h.steps.clone(),
            Some(sign_in.clone()),
        ));
        settle(StdDuration::from_secs(120)).await;
        assert_eq!(claims(&h), 1);
        assert!(sign_in.needed(), "the daemon asks for a sign-in");

        store.seed(
            &CredentialKey::new(Provider::Autognomus, None),
            StoredCredential::oauth(Secret::new("jwt"), Some(Secret::new("r")), None),
        );
        sign_in.check().await;
        settle(StdDuration::from_millis(100)).await;
        assert_eq!(claims(&h), 2, "claiming resumes once signed in");
    }

    #[tokio::test(start_paused = true)]
    async fn an_ending_run_has_its_leases_heartbeated_at_once() {
        let h = harness();
        h.tasks.on_claim(Ok(Some(raw())));
        claim_on(&h, queues().agent);
        settle(StdDuration::from_secs(1)).await;
        assert_eq!(heartbeats(&h), 0);

        h.steps.ending(run());
        settle(StdDuration::from_millis(10)).await;
        assert_eq!(heartbeats(&h), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn journalled_leases_are_heartbeated_before_any_claim() {
        let journal = Arc::new(MemoryLeaseJournal::default());
        journal.seed(vec![HeldLease {
            task: ClaimedTask::parse(&raw()).expect("task"),
            state: HeldState::Running {
                session: SessionId::parse("old-1").expect("id"),
                worktree: PathBuf::from(TREE),
                space: None,
            },
        }]);
        let h = start(
            Arc::new(FakeTaskGateway::new(datetime!(2026-10-25 10:10 UTC))),
            journal,
        );
        claim_on(&h, queues().agent);
        settle(StdDuration::from_millis(100)).await;

        let calls = h.tasks.calls();
        assert_eq!(calls.first(), Some(&TaskCall::Heartbeat(TASK.to_owned())));
        assert_eq!(h.claude.spawned().len(), 1, "the step is started again");
        assert_eq!(h.journal.held().len(), 1);
        assert_eq!(claims(&h), 0, "the restarted step fills the machine");
    }

    fn gate_raw() -> RawTask {
        let mut claim: serde_json::Value = serde_json::from_str(include_str!(
            "../../vetinari-adapters/tests/fixtures/autognomus/claim-preflight-first.json"
        ))
        .expect("claim");
        claim["id"] = serde_json::json!(format!("{RUN}~ask-info@1~1"));
        claim["instance"] = serde_json::json!("ask-info@1");
        claim["queue"] = serde_json::json!("vetinari-hex-human");
        claim["config"]["step"] = serde_json::json!("info-needed");
        claim["config"]
            .as_object_mut()
            .expect("config")
            .remove("skill");
        serde_json::from_value::<ClaimReply>(claim)
            .expect("claim")
            .into_raw()
            .expect("raw")
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_gate_needs_you_and_is_answered_through_the_steps() {
        let h = harness();
        h.tasks.on_claim(Ok(Some(gate_raw())));
        claim_on(&h, queues().human);
        settle(StdDuration::from_millis(100)).await;
        let ask_info = NodeInstance::parse("ask-info@1").expect("instance");
        assert_eq!(
            h.local.get(&run()).get(&ask_info),
            Some(&LocalStep::AwaitingPerson)
        );

        let reply = h
            .steps
            .answer(&run(), &ask_info, GateAnswer::Recheck { note: None })
            .await;
        assert_eq!(reply, Ok(GateReply::Sent));
        assert!(h.tasks.calls().iter().any(|call| matches!(
            call,
            TaskCall::Complete(task, _) if task == &format!("{RUN}~ask-info@1~1")
        )));
        settle(StdDuration::from_millis(10)).await;
        assert_eq!(h.local.get(&run()), LocalSteps::default());
    }
}
