//! The step worker: claims vetinari's steps from the engine, runs them, keeps
//! their leases alive, and reports how they ended.
//!
//! It decides what is due from the clock and the lease; the daemon runtime
//! decides when to ask, owns the agent registry and hands it in. Every
//! decision is therefore testable against a fixed clock without sleeping.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;

use time::{Duration, OffsetDateTime};

use super::lease::{next_heartbeat, retry_after};
use super::ports::{
    LeaseJournal, LeaseReply, PullRequestPublisher, RunGateway, StepFiles, TaskGateway, TaskReply,
};
use crate::application::agents::AgentRegistry;
use crate::application::ports::{Clock, PortError, PortResult, SkillStore, WorktreeManager};
use crate::application::workflows::{StepLaunch, WorkflowEngine};
use crate::domain::agent::AgentStatus;
use crate::domain::devflow::compile::{StepPolicies, WorkerQueues, node};
use crate::domain::devflow::held::{HeldLease, HeldState};
use crate::domain::devflow::ids::{NodeId, NodeInstance, RunId, TaskId, TaskQueue, WorkerName};
use crate::domain::devflow::outcome::{GateAnswer, ReviewOutcome, StepOutcome};
use crate::domain::devflow::result::StepResult;
use crate::domain::devflow::step::StepKind;
use crate::domain::devflow::task::{ClaimedTask, RawTask, StepFailure, StepInput, StepOutput};
use crate::domain::ids::{SessionId, SpaceId};
use crate::domain::profile::ProfileSet;
use crate::domain::skill::Bindings;

/// What a binding says when there is nothing to say.
const NONE: &str = "none";

/// How long a claim long-polls: the gateway's cap.
pub const CLAIM_WAIT: Duration = Duration::seconds(30);

/// What the worker is configured with.
#[derive(Debug, Clone)]
pub struct WorkerSettings {
    /// What it calls itself in a claim.
    pub name: WorkerName,
    /// The queues it claims from.
    pub queues: WorkerQueues,
    /// How many agent steps may run at once.
    pub max_agent_steps: NonZeroUsize,
    /// Each step's lease, the same policies `compile` wrote.
    pub policies: StepPolicies,
}

/// The ports the worker calls.
#[derive(Clone)]
pub struct WorkerPorts {
    /// The engine's task endpoints.
    pub tasks: Arc<dyn TaskGateway>,
    /// The engine's run endpoints.
    pub runs: Arc<dyn RunGateway>,
    /// Where held leases are kept.
    pub journal: Arc<dyn LeaseJournal>,
    /// Result and head files.
    pub files: Arc<dyn StepFiles>,
    /// For a worktree's `HEAD`.
    pub worktrees: Arc<dyn WorktreeManager>,
    /// The skill library, reloaded at each claim.
    pub skills: Arc<dyn SkillStore>,
    /// Opens the pull request.
    pub publisher: Arc<dyn PullRequestPublisher>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
}

/// How a completion or failure landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Recorded; the lease is dropped.
    Accepted,
    /// The gateway could not be reached; kept and resent.
    Pending,
    /// Another attempt owns the task; the lease is dropped.
    Stale,
    /// The run is over; the lease is dropped and the step cleaned up.
    Gone,
    /// The task does not exist; as `Gone`.
    NotFound,
}

/// What became of a claimed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The claim was not a vetinari step and was failed permanently.
    Rejected,
    /// An agent is running it.
    Running(SessionId),
    /// A gate is open for a person.
    AwaitingPerson,
    /// It finished, and this is how the report landed.
    Settled(Settlement),
}

/// What became of a person's answer to a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReply {
    /// The gateway accepted it.
    Sent,
    /// The gateway could not be reached; it is sent when it can be.
    Queued,
    /// The gate is no longer open.
    NotOpen,
}

/// A held lease and its heartbeat schedule.
#[derive(Debug, Clone)]
struct Holding {
    held: HeldLease,
    due: OffsetDateTime,
    failures: Option<NonZeroU32>,
    space: Option<SpaceId>,
}

/// Claims, runs and reports vetinari's steps.
pub struct StepWorker {
    ports: WorkerPorts,
    workflows: Arc<WorkflowEngine>,
    profiles: ProfileSet,
    settings: WorkerSettings,
    held: BTreeMap<TaskId, Holding>,
}

impl StepWorker {
    /// Build a worker.
    #[must_use]
    pub fn new(
        ports: WorkerPorts,
        workflows: Arc<WorkflowEngine>,
        profiles: ProfileSet,
        settings: WorkerSettings,
    ) -> Self {
        Self {
            ports,
            workflows,
            profiles,
            settings,
            held: BTreeMap::new(),
        }
    }

    /// Whether there is capacity to claim from a queue. Gates cost nothing
    /// to hold; agent steps are limited, so a busy machine leaves work
    /// offered rather than claimed and idle.
    #[must_use]
    pub fn can_claim(&self, queue: &TaskQueue) -> bool {
        if *queue != self.settings.queues.agent {
            return true;
        }
        let running = self
            .held
            .values()
            .filter(|holding| {
                holding.held.step().is_agent_step()
                    && matches!(
                        holding.held.state,
                        HeldState::Starting | HeldState::Running { .. }
                    )
            })
            .count();
        running < self.settings.max_agent_steps.get()
    }

    /// Claim from a queue, when there is capacity, and start what comes.
    pub async fn claim(
        &mut self,
        queue: &TaskQueue,
        registry: &mut AgentRegistry,
    ) -> PortResult<Option<Progress>> {
        if !self.can_claim(queue) {
            return Ok(None);
        }
        match self
            .ports
            .tasks
            .claim(queue, &self.settings.name, CLAIM_WAIT)
            .await?
        {
            Some(raw) => self.accept(raw, registry).await.map(Some),
            None => Ok(None),
        }
    }

    /// Start a claimed task. One that is not a vetinari step is failed
    /// permanently at once: it is a task some other tool put on a vetinari
    /// queue, or a schema this version no longer reads.
    pub async fn accept(
        &mut self,
        raw: RawTask,
        registry: &mut AgentRegistry,
    ) -> PortResult<Progress> {
        let task = match ClaimedTask::parse(&raw) {
            Ok(task) => task,
            Err(error) => {
                let failure = StepFailure::permanent(format!("not a vetinari step: {error}"));
                self.ports
                    .tasks
                    .fail(raw.id(), raw.lease(), &failure)
                    .await?;
                return Ok(Progress::Rejected);
            }
        };
        let id = task.id.clone();
        self.hold(task, HeldState::Starting);
        self.save().await?;
        self.begin(&id, registry).await
    }

    /// The tasks whose heartbeat is due now.
    #[must_use]
    pub fn heartbeats_due(&self) -> Vec<TaskId> {
        let now = self.ports.clock.now();
        self.held
            .iter()
            .filter(|(_, holding)| holding.due <= now)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// When the next heartbeat falls due, for the runtime to wait until.
    #[must_use]
    pub fn next_due(&self) -> Option<OffsetDateTime> {
        self.held.values().map(|holding| holding.due).min()
    }

    /// Heartbeat one held task and act on the answer.
    pub async fn heartbeat(
        &mut self,
        task: &TaskId,
        registry: &mut AgentRegistry,
    ) -> PortResult<()> {
        self.beat(task, registry, false).await
    }

    /// An agent session changed status. Only an ending matters here; a
    /// question is surfaced on the run item, not handled by the worker.
    pub async fn agent_changed(
        &mut self,
        session: &SessionId,
        status: &AgentStatus,
        registry: &mut AgentRegistry,
    ) -> PortResult<Option<Progress>> {
        let _ = registry;
        let Some((id, worktree)) =
            self.held
                .iter()
                .find_map(|(id, holding)| match &holding.held.state {
                    HeldState::Running {
                        session: running,
                        worktree,
                        ..
                    } if running == session => Some((id.clone(), worktree.clone())),
                    _ => None,
                })
        else {
            return Ok(None);
        };
        let progress = match status {
            AgentStatus::Succeeded => self.report_result(&id, &worktree).await?,
            AgentStatus::Failed { message } => {
                self.fail(&id, StepFailure::transient(message.clone()))
                    .await?
            }
            AgentStatus::Cancelled => {
                self.fail(&id, StepFailure::permanent("the agent was cancelled"))
                    .await?
            }
            AgentStatus::Pending | AgentStatus::Running | AgentStatus::WaitingForInput { .. } => {
                return Ok(None);
            }
        };
        Ok(Some(progress))
    }

    /// A person answered an open gate.
    pub async fn answer_gate(
        &mut self,
        run: &RunId,
        instance: &NodeInstance,
        answer: GateAnswer,
    ) -> PortResult<GateReply> {
        let Some((id, step, branch)) = self.held.iter().find_map(|(id, holding)| {
            (id.run() == run
                && id.instance() == instance
                && holding.held.state == HeldState::AwaitingPerson)
                .then(|| {
                    (
                        id.clone(),
                        holding.held.step(),
                        holding.held.task.config.branch.clone(),
                    )
                })
        }) else {
            return Ok(GateReply::NotOpen);
        };
        if step != StepKind::Gate(answer.gate()) {
            return Err(PortError::Refused {
                what: "gate".to_owned(),
                detail: format!("{step} cannot be answered that way"),
            });
        }
        let output = StepOutput {
            outcome: StepOutcome::Gate(answer),
            git_branch: branch,
        };
        Ok(match self.complete(&id, output).await? {
            Progress::Settled(Settlement::Accepted) => GateReply::Sent,
            Progress::Settled(Settlement::Pending) => GateReply::Queued,
            _ => GateReply::NotOpen,
        })
    }

    /// After a restart, before any claim: heartbeat every journalled lease
    /// and pick each one up where it was left.
    pub async fn recover(&mut self, registry: &mut AgentRegistry) -> PortResult<()> {
        let journalled = self.ports.journal.load().await?;
        let now = self.ports.clock.now();
        let ids: Vec<TaskId> = journalled.iter().map(|held| held.task.id.clone()).collect();
        for held in journalled {
            let space = match &held.state {
                HeldState::Running { space, .. } => space.clone(),
                _ => None,
            };
            self.held.insert(
                held.task.id.clone(),
                Holding {
                    held,
                    due: now,
                    failures: None,
                    space,
                },
            );
        }
        for id in ids {
            self.beat(&id, registry, true).await?;
        }
        Ok(())
    }

    /// The run may have been cancelled: heartbeat its leases now, so the
    /// engine's `Gone` stops them at event latency rather than a heartbeat
    /// interval later.
    pub fn hint_cancelled(&mut self, run: &RunId) {
        let now = self.ports.clock.now();
        for (id, holding) in &mut self.held {
            if id.run() == run {
                holding.due = now;
            }
        }
    }

    fn hold(&mut self, task: ClaimedTask, state: HeldState) {
        let due = next_heartbeat(self.lease_of(task.config.step), self.ports.clock.now());
        self.held.insert(
            task.id.clone(),
            Holding {
                held: HeldLease { task, state },
                due,
                failures: None,
                space: None,
            },
        );
    }

    fn lease_of(&self, step: StepKind) -> crate::domain::devflow::compile::LeaseSecs {
        self.settings.policies.for_step(step).lease
    }

    fn set_state(&mut self, id: &TaskId, state: HeldState) {
        if let Some(holding) = self.held.get_mut(id) {
            if let HeldState::Running { space, .. } = &state {
                holding.space.clone_from(space);
            }
            holding.held.state = state;
        }
    }

    async fn save(&self) -> PortResult<()> {
        self.ports.journal.save(&self.held()).await
    }

    fn task(&self, id: &TaskId) -> Option<ClaimedTask> {
        self.held.get(id).map(|holding| holding.held.task.clone())
    }

    /// Run a held task from the beginning, as a fresh claim or after a restart.
    async fn begin(&mut self, id: &TaskId, registry: &mut AgentRegistry) -> PortResult<Progress> {
        let Some(task) = self.task(id) else {
            return Ok(Progress::Settled(Settlement::Stale));
        };
        match task.config.step {
            StepKind::Gate(_) => {
                self.set_state(id, HeldState::AwaitingPerson);
                self.save().await?;
                Ok(Progress::AwaitingPerson)
            }
            StepKind::OpenPr => {
                let Some(profile) = self.profiles.get(&task.config.profile).cloned() else {
                    return self.fail(id, missing_profile(&task)).await;
                };
                match self.ports.publisher.publish(&task.config, &profile).await {
                    Ok(opened) => {
                        let output = StepOutput {
                            outcome: StepOutcome::OpenPr(opened),
                            git_branch: task.config.branch.clone(),
                        };
                        self.complete(id, output).await
                    }
                    Err(error) => self.fail(id, failure_from(&error)).await,
                }
            }
            StepKind::Preflight | StepKind::Implement | StepKind::Review => {
                self.run_agent(&task, registry).await
            }
        }
    }

    async fn run_agent(
        &mut self,
        task: &ClaimedTask,
        registry: &mut AgentRegistry,
    ) -> PortResult<Progress> {
        let Some(profile) = self.profiles.get(&task.config.profile).cloned() else {
            return self.fail(&task.id, missing_profile(task)).await;
        };
        let tree = self
            .workflows
            .run_worktree(&task.config, &profile)
            .path()
            .to_path_buf();
        if let Some(outcome) = self.earlier_result(task, &tree).await {
            let output = StepOutput {
                outcome,
                git_branch: task.config.branch.clone(),
            };
            return self.complete(&task.id, output).await;
        }
        let library = match self.ports.skills.load().await {
            Ok(library) => library.or_builtin(),
            Err(error) => return self.fail(&task.id, failure_from(&error)).await,
        };
        let bindings = self.bindings(task).await;
        let started = self
            .workflows
            .start_step(
                StepLaunch {
                    task: &task.id,
                    config: &task.config,
                    bindings,
                },
                self.ports.files.as_ref(),
                &library,
                &profile,
                registry,
            )
            .await;
        match started {
            Ok(started) => {
                let session = started.outcome.session.clone();
                let worktree = started
                    .outcome
                    .worktree
                    .as_ref()
                    .map_or(tree, |worktree| worktree.path().to_path_buf());
                self.set_state(
                    &task.id,
                    HeldState::Running {
                        session: session.clone(),
                        worktree,
                        space: started.outcome.space.clone(),
                    },
                );
                self.save().await?;
                Ok(Progress::Running(session))
            }
            Err(error) => self.fail(&task.id, failure_from(&error)).await,
        }
    }

    /// A valid result already in the worktree for this instance: this
    /// attempt's own, left by an agent that finished before a restart, or an
    /// earlier attempt's whose recorded `HEAD` is still the worktree's, so
    /// only the completion was lost.
    async fn earlier_result(&self, task: &ClaimedTask, tree: &Path) -> Option<StepOutcome> {
        let instance = task.id.instance();
        let current = task.id.attempt();
        for attempt in (1..=current.get()).rev() {
            let attempt = NonZeroU32::new(attempt)?;
            let Ok(Some(bytes)) = self
                .ports
                .files
                .read(&tree.join(StepResult::path_for(instance, attempt)))
                .await
            else {
                continue;
            };
            let Ok(outcome) = StepResult::parse(task.config.step, &bytes) else {
                continue;
            };
            if attempt == current {
                self.record_head(tree, task).await;
                return Some(outcome);
            }
            let recorded = self
                .ports
                .files
                .read(&tree.join(StepResult::head_path_for(instance, attempt)))
                .await;
            let head = self.ports.worktrees.head(tree).await;
            if let (Ok(Some(recorded)), Ok(head)) = (recorded, head)
                && String::from_utf8_lossy(&recorded).trim() == head.as_str()
            {
                return Some(outcome);
            }
        }
        None
    }

    async fn record_head(&self, tree: &Path, task: &ClaimedTask) {
        if let Ok(head) = self.ports.worktrees.head(tree).await {
            let path = tree.join(StepResult::head_path_for(
                task.id.instance(),
                task.id.attempt(),
            ));
            let _ = self
                .ports
                .files
                .write(&path, head.as_str().as_bytes())
                .await;
        }
    }

    /// The template values only the worker knows. All three are always
    /// bound, because a template refuses to render with one missing.
    async fn bindings(&self, task: &ClaimedTask) -> Bindings {
        let (iteration, previous) = match &task.input {
            StepInput::Iteration {
                iteration,
                previous,
            } => (*iteration, previous.as_ref()),
            StepInput::Plain => (
                task.id
                    .instance()
                    .iterations()
                    .last()
                    .copied()
                    .unwrap_or(NonZeroU32::MIN),
                None,
            ),
        };
        let findings = match previous.map(|previous| &previous.outcome) {
            Some(StepOutcome::Review(ReviewOutcome::ChangesRequested { findings })) => findings
                .as_slice()
                .iter()
                .map(|finding| match (&finding.file, finding.line) {
                    (Some(file), Some(line)) => {
                        format!("{}:{line} {}", file.as_str(), finding.text.as_str())
                    }
                    (Some(file), None) => format!("{} {}", file.as_str(), finding.text.as_str()),
                    _ => finding.text.as_str().to_owned(),
                })
                .collect::<Vec<_>>()
                .join("; "),
            _ => NONE.to_owned(),
        };
        let note = if task.config.step == StepKind::Preflight && iteration.get() > 1 {
            self.recheck_note(task).await
        } else {
            None
        };
        Bindings::new()
            .with("iteration", iteration.get().to_string())
            .with("findings", findings)
            .with("note", note.unwrap_or_else(|| NONE.to_owned()))
    }

    /// The note on the info gate of the iteration before this preflight's.
    /// It is read from the run's outputs, because a loop's `previous` is its
    /// exit node's output, which is preflight's, not the gate's.
    async fn recheck_note(&self, task: &ClaimedTask) -> Option<String> {
        let mut iterations = task.id.instance().iterations().to_vec();
        let last = iterations.pop()?;
        iterations.push(NonZeroU32::new(last.get() - 1)?);
        let gate = NodeInstance::within(NodeId::known(node::ASK_INFO), iterations);
        let outputs = self.ports.runs.outputs(task.id.run()).await.ok()?;
        let output = StepOutput::parse(outputs.get(&gate.to_string())?).ok()?;
        match output.outcome {
            StepOutcome::Gate(GateAnswer::Recheck { note: Some(note) }) => {
                Some(note.as_str().to_owned())
            }
            _ => None,
        }
    }

    async fn report_result(&mut self, id: &TaskId, worktree: &Path) -> PortResult<Progress> {
        let Some(task) = self.task(id) else {
            return Ok(Progress::Settled(Settlement::Stale));
        };
        let path = worktree.join(StepResult::path_for(id.instance(), id.attempt()));
        let bytes = match self.ports.files.read(&path).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let failure = StepFailure::permanent(format!(
                    "the skill wrote no result at {}",
                    path.display()
                ));
                return self.fail(id, failure).await;
            }
            Err(error) => return self.fail(id, failure_from(&error)).await,
        };
        match StepResult::parse(task.config.step, &bytes) {
            Ok(outcome) => {
                self.record_head(worktree, &task).await;
                let output = StepOutput {
                    outcome,
                    git_branch: task.config.branch.clone(),
                };
                self.complete(id, output).await
            }
            Err(error) => {
                let failure =
                    StepFailure::permanent(format!("the skill's result is not valid: {error}"));
                self.fail(id, failure).await
            }
        }
    }

    async fn complete(&mut self, id: &TaskId, output: StepOutput) -> PortResult<Progress> {
        let Some(task) = self.task(id) else {
            return Ok(Progress::Settled(Settlement::Stale));
        };
        match self.ports.tasks.complete(id, &task.lease, &output).await {
            Ok(reply) => Ok(Progress::Settled(self.settle(id, reply).await?)),
            Err(error) if error.is_transient() => {
                self.set_state(id, HeldState::Completing { output });
                self.save().await?;
                Ok(Progress::Settled(Settlement::Pending))
            }
            Err(error @ PortError::Protocol { .. }) => {
                let failure =
                    StepFailure::permanent(format!("the engine refused the output: {error}"));
                self.fail(id, failure).await
            }
            Err(error) => Err(error),
        }
    }

    async fn fail(&mut self, id: &TaskId, failure: StepFailure) -> PortResult<Progress> {
        let Some(task) = self.task(id) else {
            return Ok(Progress::Settled(Settlement::Stale));
        };
        match self.ports.tasks.fail(id, &task.lease, &failure).await {
            Ok(reply) => Ok(Progress::Settled(self.settle(id, reply).await?)),
            Err(error) if error.is_transient() => {
                self.set_state(id, HeldState::Failing { failure });
                self.save().await?;
                Ok(Progress::Settled(Settlement::Pending))
            }
            Err(error) => Err(error),
        }
    }

    /// Drop a lease the engine answered for. A run that is over also has its
    /// space closed; the worktree is never touched (7.6).
    async fn settle(&mut self, id: &TaskId, reply: TaskReply) -> PortResult<Settlement> {
        let settlement = match reply {
            TaskReply::Accepted => Settlement::Accepted,
            TaskReply::Stale => Settlement::Stale,
            TaskReply::Gone => Settlement::Gone,
            TaskReply::NotFound => Settlement::NotFound,
        };
        if let Some(holding) = self.held.remove(id)
            && matches!(settlement, Settlement::Gone | Settlement::NotFound)
            && let Some(space) = &holding.space
        {
            let _ = self.workflows.workspaces().close_space(space).await;
        }
        self.save().await?;
        Ok(settlement)
    }

    async fn beat(
        &mut self,
        id: &TaskId,
        registry: &mut AgentRegistry,
        restarting: bool,
    ) -> PortResult<()> {
        let Some(holding) = self.held.get(id) else {
            return Ok(());
        };
        let lease = self.lease_of(holding.held.step());
        let token = holding.held.task.lease.clone();
        let now = self.ports.clock.now();
        match self.ports.tasks.heartbeat(id, &token).await {
            Ok(LeaseReply::Extended { expires_at }) => {
                let state = {
                    let holding = self.held.get_mut(id).expect("held a moment ago");
                    holding.held.task.expires_at = expires_at;
                    holding.due = next_heartbeat(lease, now);
                    holding.failures = None;
                    holding.held.state.clone()
                };
                self.save().await?;
                match state {
                    HeldState::Completing { output } => {
                        self.complete(id, output).await?;
                    }
                    HeldState::Failing { failure } => {
                        self.fail(id, failure).await?;
                    }
                    HeldState::Starting | HeldState::Running { .. } if restarting => {
                        self.begin(id, registry).await?;
                    }
                    HeldState::Starting | HeldState::Running { .. } | HeldState::AwaitingPerson => {
                    }
                }
            }
            Ok(LeaseReply::Stale) => self.release(id, registry, false).await?,
            Ok(LeaseReply::Gone | LeaseReply::NotFound) => {
                self.release(id, registry, true).await?;
            }
            Err(error) if error.is_transient() => {
                if let Some(holding) = self.held.get_mut(id) {
                    let failures = holding
                        .failures
                        .map_or(NonZeroU32::MIN, |failures| failures.saturating_add(1));
                    holding.failures = Some(failures);
                    holding.due = now + retry_after(lease, failures);
                }
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// Stop a step whose lease is no longer ours: cancel its agent, close its
    /// space when the run is over, keep its worktree, drop the lease.
    async fn release(
        &mut self,
        id: &TaskId,
        registry: &mut AgentRegistry,
        run_over: bool,
    ) -> PortResult<()> {
        let Some(holding) = self.held.remove(id) else {
            return Ok(());
        };
        if let HeldState::Running { session, .. } = &holding.held.state {
            let _ = registry.cancel(session).await;
        }
        if run_over && let Some(space) = &holding.space {
            let _ = self.workflows.workspaces().close_space(space).await;
        }
        self.save().await
    }

    /// Every lease held.
    #[must_use]
    pub fn held(&self) -> Vec<HeldLease> {
        self.held
            .values()
            .map(|holding| holding.held.clone())
            .collect()
    }

    /// The gates waiting for a person.
    #[must_use]
    pub fn open_gates(&self) -> Vec<&ClaimedTask> {
        self.held
            .values()
            .filter(|holding| holding.held.state == HeldState::AwaitingPerson)
            .map(|holding| &holding.held.task)
            .collect()
    }
}

fn missing_profile(task: &ClaimedTask) -> StepFailure {
    StepFailure::permanent(format!(
        "profile {} is not configured on this machine",
        task.config.profile
    ))
}

/// A port error as a step failure: worth retrying only when the port says so.
fn failure_from(error: &PortError) -> StepFailure {
    if error.is_transient() {
        StepFailure::transient(error.to_string())
    } else {
        StepFailure::permanent(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use time::macros::datetime;

    use super::*;
    use crate::application::workspaces::WorkspaceDirectory;
    use crate::domain::agent::TaskClass;
    use crate::domain::devflow::compile::LeaseSecs;
    use crate::domain::devflow::ids::LeaseToken;
    use crate::domain::devflow::outcome::{Line, OpenPrOutcome};
    use crate::domain::devflow::step::{GateKind, StepConfig};
    use crate::domain::devflow::task::FailureKind;
    use crate::domain::devflow::template::{RepoTarget, TemplateKind, TicketRef};
    use crate::domain::git::BranchName;
    use crate::domain::ids::{AgentId, ProfileId, SkillId, Slug};
    use crate::domain::profile::Profile;
    use crate::domain::skill::{PromptTemplate, Skill, SkillLibrary};
    use crate::domain::text::NonEmptyText;
    use crate::domain::workspace::TargetKind;
    use crate::testing::{
        FakeAgentEngine, FakePullRequestPublisher, FakeRunGateway, FakeStepFiles, FakeTaskGateway,
        FakeWorkspaceTarget, FakeWorktreeManager, FixedClock, MemoryLeaseJournal, MemorySkillStore,
        TaskCall,
    };

    const NOW: OffsetDateTime = datetime!(2026-10-25 10:00 UTC);
    const LATER: OffsetDateTime = datetime!(2026-10-25 10:10 UTC);
    const RUN: &str = "01JB7Z";
    const TREE: &str = "/work/trees/web-eng-412-rate-limit-retries";
    const AGENT_QUEUE: &str = "vetinari-hex-agent";
    const HUMAN_QUEUE: &str = "vetinari-hex-human";

    struct Harness {
        worker: StepWorker,
        registry: AgentRegistry,
        clock: Arc<FixedClock>,
        tasks: Arc<FakeTaskGateway>,
        runs: Arc<FakeRunGateway>,
        journal: Arc<MemoryLeaseJournal>,
        files: Arc<FakeStepFiles>,
        worktrees: Arc<FakeWorktreeManager>,
        publisher: Arc<FakePullRequestPublisher>,
        target: Arc<FakeWorkspaceTarget>,
        claude: Arc<FakeAgentEngine>,
    }

    fn harness_with(max_agent_steps: usize) -> Harness {
        harness_with_skills(max_agent_steps, SkillLibrary::builtin())
    }

    fn harness_with_skills(max_agent_steps: usize, saved: SkillLibrary) -> Harness {
        let clock = Arc::new(FixedClock::new(NOW));
        let tasks = Arc::new(FakeTaskGateway::new(LATER));
        let runs = Arc::new(FakeRunGateway::default());
        let journal = Arc::new(MemoryLeaseJournal::default());
        let files = Arc::new(FakeStepFiles::default());
        let worktrees = Arc::new(FakeWorktreeManager::default());
        let publisher = Arc::new(FakePullRequestPublisher::default());
        let target = Arc::new(FakeWorkspaceTarget::new(TargetKind::Herdr));
        let mut workspaces = WorkspaceDirectory::default();
        workspaces.register(target.clone());
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
                runs: runs.clone(),
                journal: journal.clone(),
                files: files.clone(),
                worktrees: worktrees.clone(),
                skills: Arc::new(MemorySkillStore::with_library(saved)),
                publisher: publisher.clone(),
                clock: clock.clone(),
            },
            Arc::new(WorkflowEngine::new(worktrees.clone(), workspaces)),
            ProfileSet::new(profile, Vec::new()).expect("profiles"),
            WorkerSettings {
                name: WorkerName::parse("hex").expect("worker"),
                queues: WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker")),
                max_agent_steps: NonZeroUsize::new(max_agent_steps).expect("non-zero"),
                policies: StepPolicies::default(),
            },
        );
        Harness {
            worker,
            registry,
            clock,
            tasks,
            runs,
            journal,
            files,
            worktrees,
            publisher,
            target,
            claude,
        }
    }

    fn harness() -> Harness {
        harness_with(2)
    }

    fn config(step: StepKind) -> StepConfig {
        let skill = match step {
            StepKind::Preflight => Some("ticket-preflight"),
            StepKind::Implement => Some("implement-tdd"),
            StepKind::Review => Some("self-review"),
            StepKind::OpenPr | StepKind::Gate(_) => None,
        };
        StepConfig {
            step,
            template: TemplateKind::FeatureFromTicket,
            ticket: TicketRef {
                source: Slug::parse("jira").expect("id"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
                url: None,
            },
            repo: RepoTarget {
                path: PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: ProfileId::parse("work").expect("id"),
            branch: BranchName::sanitized("vetinari/eng-412-rate-limit-retries"),
            base: None,
            skill: skill.map(|skill| SkillId::parse(skill).expect("id")),
            agent: None,
        }
    }

    fn queue_for(step: StepKind) -> &'static str {
        match step {
            StepKind::Gate(_) => HUMAN_QUEUE,
            _ => AGENT_QUEUE,
        }
    }

    fn raw_with(instance: &str, attempt: u32, step: StepKind, input: serde_json::Value) -> RawTask {
        RawTask::new(
            TaskId::parse(&format!("{RUN}~{instance}~{attempt}")).expect("id"),
            LeaseToken::parse("01JB80").expect("lease"),
            datetime!(2026-10-25 10:05 UTC),
            TaskQueue::parse(queue_for(step)).expect("queue"),
            config(step).to_json(),
            input,
        )
    }

    fn raw(instance: &str, step: StepKind) -> RawTask {
        raw_with(instance, 1, step, json!({ "input": {}, "iteration": 1 }))
    }

    fn task_id(instance: &str, attempt: u32) -> TaskId {
        TaskId::parse(&format!("{RUN}~{instance}~{attempt}")).expect("id")
    }

    fn result_path(instance: &str, attempt: u32) -> PathBuf {
        PathBuf::from(TREE).join(format!(".vetinari/steps/{instance}-{attempt}.json"))
    }

    fn queue(name: &str) -> TaskQueue {
        TaskQueue::parse(name).expect("queue")
    }

    fn branch() -> BranchName {
        BranchName::sanitized("vetinari/eng-412-rate-limit-retries")
    }

    fn completes(h: &Harness) -> Vec<(String, StepOutput)> {
        h.tasks
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                TaskCall::Complete(task, output) => Some((task, output)),
                _ => None,
            })
            .collect()
    }

    fn failures(h: &Harness) -> Vec<(String, StepFailure)> {
        h.tasks
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                TaskCall::Fail(task, failure) => Some((task, failure)),
                _ => None,
            })
            .collect()
    }

    fn heartbeats(h: &Harness) -> usize {
        h.tasks
            .calls()
            .iter()
            .filter(|call| matches!(call, TaskCall::Heartbeat(_)))
            .count()
    }

    async fn start_implement(h: &mut Harness) -> SessionId {
        match h
            .worker
            .accept(raw("implement@1", StepKind::Implement), &mut h.registry)
            .await
            .expect("accept")
        {
            Progress::Running(session) => session,
            other => panic!("expected an agent, got {other:?}"),
        }
    }

    async fn finish(h: &mut Harness, session: &SessionId, status: AgentStatus) -> Option<Progress> {
        h.worker
            .agent_changed(session, &status, &mut h.registry)
            .await
            .expect("agent changed")
    }

    // 7.2 Claiming

    #[tokio::test]
    async fn a_claimed_agent_step_starts_and_completes_with_its_result() {
        let mut h = harness();
        h.tasks
            .on_claim(Ok(Some(raw("implement@1", StepKind::Implement))));

        let progress = h
            .worker
            .claim(&queue(AGENT_QUEUE), &mut h.registry)
            .await
            .expect("claim");
        let Some(Progress::Running(session)) = progress else {
            panic!("expected a running agent, got {progress:?}");
        };
        assert_eq!(h.claude.spawned().len(), 1);
        assert!(matches!(
            h.journal.held()[..],
            [HeldLease {
                state: HeldState::Running { .. },
                ..
            }]
        ));

        h.worktrees.set_head(Path::new(TREE), "abc123");
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "implemented", "summary": "Honour Retry-After" }"#,
        );
        let progress = finish(&mut h, &session, AgentStatus::Succeeded).await;

        assert_eq!(progress, Some(Progress::Settled(Settlement::Accepted)));
        let completes = completes(&h);
        assert_eq!(completes.len(), 1);
        assert_eq!(completes[0].0, "01JB7Z~implement@1~1");
        assert_eq!(completes[0].1.outcome.label(), "implemented");
        assert_eq!(completes[0].1.git_branch, branch());
        assert!(h.journal.held().is_empty(), "the lease is dropped");
        assert_eq!(
            h.files
                .contents(&PathBuf::from(TREE).join(".vetinari/steps/implement@1-1.head")),
            Some("abc123".to_owned())
        );
    }

    #[tokio::test]
    async fn a_step_runs_on_the_built_in_skills_when_none_are_saved() {
        // The skills file does not exist until someone edits a skill, and a
        // file saved before these skills shipped does not hold them.
        let mut h = harness_with_skills(2, SkillLibrary::default());
        h.tasks
            .on_claim(Ok(Some(raw("preflight@1", StepKind::Preflight))));

        let progress = h
            .worker
            .claim(&queue(AGENT_QUEUE), &mut h.registry)
            .await
            .expect("claim");

        assert!(
            matches!(progress, Some(Progress::Running(_))),
            "{progress:?}"
        );
        assert!(failures(&h).is_empty(), "{:?}", failures(&h));
    }

    #[tokio::test]
    async fn a_saved_skill_wins_over_the_built_in_it_replaces() {
        let mut saved = SkillLibrary::default();
        saved.upsert(Skill::new(
            SkillId::parse("ticket-preflight").expect("id"),
            NonEmptyText::parse("Our preflight").expect("name"),
            PromptTemplate::parse("Our own check of {{ticket}}").expect("template"),
            TaskClass::IssueSummary,
        ));
        let mut h = harness_with_skills(2, saved);
        h.tasks
            .on_claim(Ok(Some(raw("preflight@1", StepKind::Preflight))));

        h.worker
            .claim(&queue(AGENT_QUEUE), &mut h.registry)
            .await
            .expect("claim");

        let spawned = h.claude.spawned();
        assert_eq!(spawned.len(), 1);
        assert!(
            spawned[0].prompt().as_str().starts_with("Our own check of"),
            "{}",
            spawned[0].prompt().as_str()
        );
    }

    #[tokio::test]
    async fn an_empty_queue_starts_nothing() {
        let mut h = harness();
        let progress = h
            .worker
            .claim(&queue(AGENT_QUEUE), &mut h.registry)
            .await
            .expect("claim");
        assert_eq!(progress, None);
        assert!(h.journal.held().is_empty());
    }

    #[tokio::test]
    async fn capacity_limits_agent_claims_but_not_gates() {
        let mut h = harness_with(1);
        let session = start_implement(&mut h).await;

        assert!(!h.worker.can_claim(&queue(AGENT_QUEUE)));
        assert!(h.worker.can_claim(&queue(HUMAN_QUEUE)));
        let progress = h
            .worker
            .claim(&queue(AGENT_QUEUE), &mut h.registry)
            .await
            .expect("claim");
        assert_eq!(progress, None);
        assert!(
            !h.tasks
                .calls()
                .iter()
                .any(|call| matches!(call, TaskCall::Claim(_))),
            "a full worker does not even ask"
        );

        finish(
            &mut h,
            &session,
            AgentStatus::Failed {
                message: "x".into(),
            },
        )
        .await;
        assert!(h.worker.can_claim(&queue(AGENT_QUEUE)));
    }

    #[tokio::test]
    async fn a_claim_that_is_not_a_vetinari_step_is_failed_permanently() {
        let mut h = harness();
        let mut foreign = raw("deploy", StepKind::Implement);
        foreign = RawTask::new(
            foreign.id().clone(),
            foreign.lease().clone(),
            NOW,
            queue(AGENT_QUEUE),
            json!({ "job": "deploy" }),
            json!({}),
        );

        let progress = h
            .worker
            .accept(foreign, &mut h.registry)
            .await
            .expect("accept");

        assert_eq!(progress, Progress::Rejected);
        let failures = failures(&h);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].1.kind, FailureKind::Permanent);
        assert!(h.claude.spawned().is_empty());
        assert!(h.journal.held().is_empty());
    }

    // 7.3 Heartbeats

    #[tokio::test]
    async fn heartbeats_fall_due_a_third_of_the_lease_after_each_extension() {
        let mut h = harness();
        start_implement(&mut h).await;
        let lease = StepPolicies::default().implement.lease;

        assert!(h.worker.heartbeats_due().is_empty());
        assert_eq!(h.worker.next_due(), Some(next_heartbeat(lease, NOW)));

        h.clock.advance(Duration::seconds(100));
        assert_eq!(h.worker.heartbeats_due(), [task_id("implement@1", 1)]);
        h.worker
            .heartbeat(&task_id("implement@1", 1), &mut h.registry)
            .await
            .expect("heartbeat");

        assert!(h.worker.heartbeats_due().is_empty());
        let extended = NOW + Duration::seconds(100);
        assert_eq!(h.worker.next_due(), Some(next_heartbeat(lease, extended)));
        assert_eq!(h.journal.held()[0].task.expires_at, LATER);
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_retried_and_never_stops_the_agent() {
        let mut h = harness();
        start_implement(&mut h).await;
        let lease = LeaseSecs::new(300).expect("lease");
        h.clock.advance(Duration::seconds(100));
        let at = h.clock.now();
        for failures in 1..=2 {
            h.tasks.on_heartbeat(Err(PortError::unavailable(
                "autognomus",
                "connection refused",
            )));
            h.worker
                .heartbeat(&task_id("implement@1", 1), &mut h.registry)
                .await
                .expect("not fatal");
            assert_eq!(
                h.worker.next_due(),
                Some(h.clock.now() + retry_after(lease, NonZeroU32::new(failures).expect("n")))
            );
        }
        assert!(h.claude.cancelled().is_empty());
        assert_eq!(h.journal.held().len(), 1);
        assert_eq!(h.clock.now(), at);
    }

    #[tokio::test]
    async fn a_heartbeat_that_cannot_authenticate_is_an_error() {
        let mut h = harness();
        start_implement(&mut h).await;
        h.tasks.on_heartbeat(Err(PortError::Unauthenticated {
            provider: crate::domain::credentials::Provider::GitHub,
        }));
        assert!(
            h.worker
                .heartbeat(&task_id("implement@1", 1), &mut h.registry)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_cancel_hint_makes_the_runs_heartbeats_due_now() {
        let mut h = harness();
        start_implement(&mut h).await;
        h.worker
            .hint_cancelled(&RunId::parse("someone-else").expect("run"));
        assert!(h.worker.heartbeats_due().is_empty());
        h.worker.hint_cancelled(&RunId::parse(RUN).expect("run"));
        assert_eq!(h.worker.heartbeats_due(), [task_id("implement@1", 1)]);
    }

    // 7.6 Stale, Gone, cancellation

    #[tokio::test]
    async fn stale_cancels_the_agent_and_keeps_the_space() {
        let mut h = harness();
        start_implement(&mut h).await;
        h.tasks.on_heartbeat(Ok(LeaseReply::Stale));
        h.worker
            .heartbeat(&task_id("implement@1", 1), &mut h.registry)
            .await
            .expect("heartbeat");
        assert_eq!(h.claude.cancelled().len(), 1);
        assert_eq!(h.target.spaces().len(), 1);
        assert!(h.journal.held().is_empty());
        assert!(h.worker.held().is_empty());
    }

    #[tokio::test]
    async fn gone_and_not_found_cancel_the_agent_and_close_the_space() {
        for reply in [LeaseReply::Gone, LeaseReply::NotFound] {
            let mut h = harness();
            start_implement(&mut h).await;
            h.tasks.on_heartbeat(Ok(reply));
            h.worker
                .heartbeat(&task_id("implement@1", 1), &mut h.registry)
                .await
                .expect("heartbeat");
            assert_eq!(h.claude.cancelled().len(), 1, "{reply:?}");
            assert!(h.target.spaces().is_empty(), "{reply:?}");
            assert!(h.journal.held().is_empty(), "{reply:?}");
            assert!(h.worktrees.removed().is_empty(), "the worktree is kept");
        }
    }

    // 6 and 7.5 Results, completing and failing

    #[tokio::test]
    async fn a_skill_that_wrote_no_result_fails_permanently() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        finish(&mut h, &session, AgentStatus::Succeeded).await;
        let failures = failures(&h);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].1.kind, FailureKind::Permanent);
        assert!(
            failures[0].1.message.contains("no result"),
            "{:?}",
            failures[0]
        );
    }

    #[tokio::test]
    async fn an_invalid_result_fails_permanently_with_the_parse_error() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "approved", "summary": "s" }"#,
        );
        finish(&mut h, &session, AgentStatus::Succeeded).await;
        let failures = failures(&h);
        assert_eq!(failures[0].1.kind, FailureKind::Permanent);
        assert!(
            failures[0].1.message.contains("approved"),
            "{:?}",
            failures[0]
        );
    }

    #[tokio::test]
    async fn a_failed_agent_fails_transiently_and_a_cancelled_one_permanently() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        finish(
            &mut h,
            &session,
            AgentStatus::Failed {
                message: "claude exited 1".into(),
            },
        )
        .await;
        assert_eq!(failures(&h)[0].1, StepFailure::transient("claude exited 1"));

        let mut h = harness();
        let session = start_implement(&mut h).await;
        finish(&mut h, &session, AgentStatus::Cancelled).await;
        assert_eq!(failures(&h)[0].1.kind, FailureKind::Permanent);
    }

    #[tokio::test]
    async fn an_agent_still_working_or_asking_changes_nothing() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        let progress = finish(
            &mut h,
            &session,
            AgentStatus::WaitingForInput {
                question: "which page size?".into(),
            },
        )
        .await;
        assert_eq!(progress, None);
        assert!(completes(&h).is_empty() && failures(&h).is_empty());
        assert_eq!(h.journal.held().len(), 1);
    }

    #[tokio::test]
    async fn a_completion_the_gateway_cannot_take_is_kept_and_resent_with_the_heartbeat() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "implemented", "summary": "s" }"#,
        );
        h.tasks
            .on_complete(Err(PortError::unavailable("autognomus", "503")));
        let progress = finish(&mut h, &session, AgentStatus::Succeeded).await;

        assert_eq!(progress, Some(Progress::Settled(Settlement::Pending)));
        assert!(matches!(
            h.journal.held()[..],
            [HeldLease {
                state: HeldState::Completing { .. },
                ..
            }]
        ));

        h.worker
            .heartbeat(&task_id("implement@1", 1), &mut h.registry)
            .await
            .expect("heartbeat");
        assert_eq!(completes(&h).len(), 2);
        assert!(h.journal.held().is_empty());
    }

    #[tokio::test]
    async fn an_output_the_engine_refuses_is_failed_permanently() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "implemented", "summary": "s" }"#,
        );
        h.tasks.on_complete(Err(PortError::protocol(
            "autognomus",
            "422 output is not an object",
        )));
        finish(&mut h, &session, AgentStatus::Succeeded).await;
        assert_eq!(failures(&h)[0].1.kind, FailureKind::Permanent);
        assert!(h.journal.held().is_empty());
    }

    #[tokio::test]
    async fn a_completion_after_the_run_ended_closes_the_space() {
        let mut h = harness();
        let session = start_implement(&mut h).await;
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "implemented", "summary": "s" }"#,
        );
        h.tasks.on_complete(Ok(TaskReply::Gone));
        let progress = finish(&mut h, &session, AgentStatus::Succeeded).await;
        assert_eq!(progress, Some(Progress::Settled(Settlement::Gone)));
        assert!(h.target.spaces().is_empty());
        assert!(h.journal.held().is_empty());
    }

    #[tokio::test]
    async fn an_earlier_attempts_result_is_adopted_only_while_head_is_unchanged() {
        let earlier = r#"{ "schema": 1, "outcome": "implemented", "summary": "done before" }"#;
        let head = PathBuf::from(TREE).join(".vetinari/steps/implement@1-1.head");

        let mut h = harness();
        h.files.put(&result_path("implement@1", 1), earlier);
        h.files.put(&head, "abc123");
        h.worktrees.set_head(Path::new(TREE), "abc123");
        let progress = h
            .worker
            .accept(
                raw_with(
                    "implement@1",
                    2,
                    StepKind::Implement,
                    json!({ "input": {}, "iteration": 1 }),
                ),
                &mut h.registry,
            )
            .await
            .expect("accept");
        assert_eq!(progress, Progress::Settled(Settlement::Accepted));
        assert!(h.claude.spawned().is_empty());
        assert_eq!(completes(&h)[0].0, "01JB7Z~implement@1~2");

        let mut h = harness();
        h.files.put(&result_path("implement@1", 1), earlier);
        h.files.put(&head, "abc123");
        h.worktrees.set_head(Path::new(TREE), "def456");
        let progress = h
            .worker
            .accept(
                raw_with(
                    "implement@1",
                    2,
                    StepKind::Implement,
                    json!({ "input": {}, "iteration": 1 }),
                ),
                &mut h.registry,
            )
            .await
            .expect("accept");
        assert!(matches!(progress, Progress::Running(_)));
        assert!(completes(&h).is_empty());
    }

    // Bindings from the loop envelope

    #[tokio::test]
    async fn a_rework_round_is_told_the_previous_reviews_findings() {
        let mut h = harness();
        let previous = json!({
            "branch": "changes-requested", "step": "review",
            "findings": [ { "file": "src/retry.rs", "line": 88, "text": "Retry-After ignored on 503" } ],
            "git_branch": "vetinari/eng-412-rate-limit-retries"
        });
        h.worker
            .accept(
                raw_with(
                    "implement@2",
                    1,
                    StepKind::Implement,
                    json!({ "input": {}, "iteration": 2, "previous": previous }),
                ),
                &mut h.registry,
            )
            .await
            .expect("accept");
        let prompt = h.claude.spawned()[0].prompt().as_str().to_owned();
        assert!(prompt.contains("round 2"), "{prompt}");
        assert!(
            prompt.contains("src/retry.rs:88 Retry-After ignored on 503"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn a_recheck_is_told_the_persons_note_from_the_gate() {
        let mut h = harness();
        h.runs.set_output(
            RUN,
            "ask-info@1",
            json!({
                "branch": "recheck", "step": "info-needed", "note": "Page size is 50",
                "git_branch": "vetinari/eng-412-rate-limit-retries"
            }),
        );
        let previous = json!({
            "branch": "needs-info", "step": "preflight", "questions": ["Page size?"],
            "git_branch": "vetinari/eng-412-rate-limit-retries"
        });
        h.worker
            .accept(
                raw_with(
                    "preflight@2",
                    1,
                    StepKind::Preflight,
                    json!({ "input": {}, "iteration": 2, "previous": previous }),
                ),
                &mut h.registry,
            )
            .await
            .expect("accept");
        let prompt = h.claude.spawned()[0].prompt().as_str().to_owned();
        assert!(prompt.contains("Page size is 50"), "{prompt}");
    }

    // Open PR

    #[tokio::test]
    async fn open_pr_publishes_and_completes_without_an_agent() {
        let mut h = harness();
        h.publisher.on_publish(Ok(OpenPrOutcome::Opened {
            number: 1204,
            url: Line::parse("https://github.com/acme/web/pull/1204").expect("url"),
        }));
        let progress = h
            .worker
            .accept(
                raw_with("open-pr", 1, StepKind::OpenPr, json!({ "branch": "done" })),
                &mut h.registry,
            )
            .await
            .expect("accept");
        assert_eq!(progress, Progress::Settled(Settlement::Accepted));
        assert_eq!(h.publisher.published(), [branch().to_string()]);
        assert_eq!(completes(&h)[0].1.outcome.label(), "opened");
        assert!(h.claude.spawned().is_empty());
    }

    #[tokio::test]
    async fn open_pr_that_cannot_reach_the_host_fails_transiently() {
        let mut h = harness();
        h.publisher
            .on_publish(Err(PortError::unavailable("github", "timeout")));
        h.worker
            .accept(
                raw_with("open-pr", 1, StepKind::OpenPr, json!({})),
                &mut h.registry,
            )
            .await
            .expect("accept");
        assert_eq!(failures(&h)[0].1.kind, FailureKind::Transient);
    }

    // 8 Gates

    async fn open_info_gate(h: &mut Harness) {
        let progress = h
            .worker
            .accept(
                raw("ask-info@1", StepKind::Gate(GateKind::InfoNeeded)),
                &mut h.registry,
            )
            .await
            .expect("accept");
        assert_eq!(progress, Progress::AwaitingPerson);
    }

    fn ask_info() -> NodeInstance {
        NodeInstance::parse("ask-info@1").expect("instance")
    }

    #[tokio::test]
    async fn an_answered_gate_completes_with_the_answer() {
        let mut h = harness();
        open_info_gate(&mut h).await;
        assert_eq!(h.worker.open_gates().len(), 1);

        let note = Some(Line::parse("Page size is 50").expect("note"));
        let reply = h
            .worker
            .answer_gate(
                &RunId::parse(RUN).expect("run"),
                &ask_info(),
                GateAnswer::Recheck { note: note.clone() },
            )
            .await
            .expect("answer");

        assert_eq!(reply, GateReply::Sent);
        assert_eq!(
            completes(&h)[0].1.outcome,
            StepOutcome::Gate(GateAnswer::Recheck { note })
        );
        assert!(h.worker.open_gates().is_empty());
        assert!(h.journal.held().is_empty());
    }

    #[tokio::test]
    async fn an_answer_after_the_run_ended_says_the_gate_is_closed() {
        let mut h = harness();
        open_info_gate(&mut h).await;
        h.tasks.on_complete(Ok(TaskReply::Gone));
        let reply = h
            .worker
            .answer_gate(
                &RunId::parse(RUN).expect("run"),
                &ask_info(),
                GateAnswer::Recheck { note: None },
            )
            .await
            .expect("answer");
        assert_eq!(reply, GateReply::NotOpen);
        assert!(h.worker.open_gates().is_empty());
    }

    #[tokio::test]
    async fn an_answer_while_the_gateway_is_away_is_queued() {
        let mut h = harness();
        open_info_gate(&mut h).await;
        h.tasks
            .on_complete(Err(PortError::unavailable("autognomus", "503")));
        let reply = h
            .worker
            .answer_gate(
                &RunId::parse(RUN).expect("run"),
                &ask_info(),
                GateAnswer::Recheck { note: None },
            )
            .await
            .expect("answer");
        assert_eq!(reply, GateReply::Queued);
        assert!(matches!(
            h.journal.held()[..],
            [HeldLease {
                state: HeldState::Completing { .. },
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn an_answer_to_a_gate_that_is_not_held_or_does_not_fit_is_refused() {
        let mut h = harness();
        let reply = h
            .worker
            .answer_gate(
                &RunId::parse(RUN).expect("run"),
                &ask_info(),
                GateAnswer::Recheck { note: None },
            )
            .await
            .expect("answer");
        assert_eq!(reply, GateReply::NotOpen);

        open_info_gate(&mut h).await;
        assert!(
            h.worker
                .answer_gate(
                    &RunId::parse(RUN).expect("run"),
                    &ask_info(),
                    GateAnswer::OpenAnyway
                )
                .await
                .is_err()
        );
        assert!(completes(&h).is_empty());
    }

    // 7.7 Restart

    fn journalled(instance: &str, step: StepKind, state: HeldState) -> HeldLease {
        HeldLease {
            task: ClaimedTask::parse(&raw(instance, step)).expect("claim"),
            state,
        }
    }

    #[tokio::test]
    async fn a_restart_starts_journalled_agent_steps_again_in_the_same_worktree() {
        for state in [
            HeldState::Starting,
            HeldState::Running {
                session: SessionId::parse("claude-code-old").expect("session"),
                worktree: PathBuf::from(TREE),
                space: None,
            },
        ] {
            let mut h = harness();
            h.journal.seed(vec![journalled(
                "implement@1",
                StepKind::Implement,
                state.clone(),
            )]);
            h.worker.recover(&mut h.registry).await.expect("recover");
            assert_eq!(heartbeats(&h), 1, "{state:?}");
            assert_eq!(h.claude.spawned().len(), 1, "{state:?}");
            assert_eq!(h.claude.spawned()[0].working_dir(), Path::new(TREE));
            assert!(matches!(
                h.journal.held()[..],
                [HeldLease {
                    state: HeldState::Running { .. },
                    ..
                }]
            ));
        }
    }

    #[tokio::test]
    async fn a_restart_adopts_a_result_the_old_agent_finished_writing() {
        let mut h = harness();
        h.journal.seed(vec![journalled(
            "implement@1",
            StepKind::Implement,
            HeldState::Starting,
        )]);
        h.files.put(
            &result_path("implement@1", 1),
            r#"{ "schema": 1, "outcome": "implemented", "summary": "s" }"#,
        );
        h.worker.recover(&mut h.registry).await.expect("recover");
        assert!(h.claude.spawned().is_empty());
        assert_eq!(completes(&h).len(), 1);
    }

    #[tokio::test]
    async fn a_restart_reopens_gates_and_resends_what_was_waiting() {
        let mut h = harness();
        let output = StepOutput {
            outcome: StepOutcome::Gate(GateAnswer::OpenAnyway),
            git_branch: branch(),
        };
        h.journal.seed(vec![
            journalled(
                "ask-info@1",
                StepKind::Gate(GateKind::InfoNeeded),
                HeldState::AwaitingPerson,
            ),
            journalled(
                "review-stuck",
                StepKind::Gate(GateKind::ReviewStuck),
                HeldState::Completing { output },
            ),
            journalled(
                "review@1",
                StepKind::Review,
                HeldState::Failing {
                    failure: StepFailure::transient("crashed"),
                },
            ),
        ]);
        h.worker.recover(&mut h.registry).await.expect("recover");

        assert_eq!(h.worker.open_gates().len(), 1);
        assert_eq!(completes(&h).len(), 1);
        assert_eq!(
            failures(&h),
            [(
                "01JB7Z~review@1~1".to_owned(),
                StepFailure::transient("crashed")
            )]
        );
        assert_eq!(h.journal.held().len(), 1);
    }

    #[tokio::test]
    async fn a_restart_drops_leases_the_engine_no_longer_honours_and_keeps_unanswered_ones() {
        let mut h = harness();
        h.journal.seed(vec![
            journalled("implement@1", StepKind::Implement, HeldState::Starting),
            journalled("review@1", StepKind::Review, HeldState::Starting),
            journalled("preflight@1", StepKind::Preflight, HeldState::Starting),
        ]);
        h.tasks.on_heartbeat(Ok(LeaseReply::Stale));
        h.tasks.on_heartbeat(Ok(LeaseReply::Gone));
        h.tasks
            .on_heartbeat(Err(PortError::unavailable("autognomus", "503")));
        h.worker.recover(&mut h.registry).await.expect("recover");

        assert!(h.claude.spawned().is_empty());
        let held = h.journal.held();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].task.id, task_id("preflight@1", 1));
    }
}
