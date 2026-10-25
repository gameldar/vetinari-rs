//! `--scripted <file>`: replay a recorded run instead of talking to a
//! gateway, so the TUI can be developed without one.
//!
//! A script names its runs, each with the graph it starts from, and a list
//! of steps. A step waits `after_ms`, then may replace its run's graph (what
//! a refetch now returns) and may emit one stream event; a step with a graph
//! and no event emits a `resync`, so the replacement is fetched. Graphs and
//! events are in the gateway's own JSON, read by the same adapter code a
//! live daemon uses.
//!
//! A run may carry its outputs, for the popup's notes. A step may `hold` a
//! gate, which then needs the developer as one a worker holds would, and a
//! later step may `await` it: the script stops there until the gate is
//! answered over the socket.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::Notify;
use vetinari_adapters::autognomus::wire::{RunGraphReply, run_event};
use vetinari_core::application::devflow::api::ApiError;
use vetinari_core::application::devflow::api::RunLabel;
use vetinari_core::application::devflow::ports::{
    CancelReply, RunEventStream, RunEvents, RunGateway,
};
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::devflow::compile::WorkflowGraph;
use vetinari_core::domain::devflow::ids::{DefinitionId, NodeInstance, RunId};
use vetinari_core::domain::devflow::outcome::GateAnswer;
use vetinari_core::domain::devflow::run_view::{RunEvent, RunGraph, RunHeader, RunStatus};
use vetinari_core::domain::devflow::step::StepConfig;
use vetinari_core::domain::devflow::summary::{LocalStep, LocalSteps};
use vetinari_core::domain::text::NonEmptyText;

use crate::gates::Gates;
use crate::runtime::OwnedRun;
use crate::steps::LocalBoard;

/// Why a script could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// It is not a script.
    #[error("not a script: {0}")]
    Json(#[from] serde_json::Error),
    /// A graph or event in it does not read as the gateway's would.
    #[error("script step {step}: {error}")]
    Contents {
        /// Which step, or 0 for the runs.
        step: usize,
        /// What was wrong.
        error: PortError,
    },
    /// A run's ticket or title is blank.
    #[error("a run's ticket and title must not be blank")]
    Label,
}

/// A gate a script names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRef {
    /// The run.
    pub run: RunId,
    /// The gate's instance.
    pub instance: NodeInstance,
}

/// One step of a script, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// How long after the step before.
    pub after: StdDuration,
    /// A gate held from now until it is answered.
    pub hold: Option<GateRef>,
    /// A gate to wait on before the rest of the step.
    pub awaits: Option<GateRef>,
    /// The graph its run has from now on.
    pub graph: Option<RunGraph>,
    /// What the stream says.
    pub event: Option<RunEvent>,
}

/// A script, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The runs and what is known of them.
    pub runs: Vec<OwnedRun>,
    /// Each run's graph at the start.
    pub graphs: Vec<RunGraph>,
    /// Each run's outputs, by instance.
    pub outputs: BTreeMap<RunId, BTreeMap<String, Value>>,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

impl Script {
    /// Read a script.
    pub fn parse(text: &str) -> Result<Self, ScriptError> {
        let raw: RawScript = serde_json::from_str(text)?;
        let mut runs = Vec::new();
        let mut graphs = Vec::new();
        let mut outputs = BTreeMap::new();
        for run in raw.runs {
            let graph = run
                .graph
                .into_graph()
                .map_err(|error| ScriptError::Contents { step: 0, error })?;
            let label = RunLabel {
                ticket: NonEmptyText::parse(&run.ticket).map_err(|_| ScriptError::Label)?,
                title: NonEmptyText::parse(&run.title).map_err(|_| ScriptError::Label)?,
            };
            outputs.insert(graph.run.clone(), run.outputs);
            runs.push(OwnedRun {
                run: graph.run.clone(),
                label: Some(label),
                started_at: run.started_at,
            });
            graphs.push(graph);
        }
        let steps = raw
            .steps
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                let contents = |error| ScriptError::Contents {
                    step: index + 1,
                    error,
                };
                let graph = step
                    .graph
                    .map(RunGraphReply::into_graph)
                    .transpose()
                    .map_err(contents)?;
                let event = match step.event {
                    Some(name) => Some(run_event(&name, &step.data.to_string()).map_err(contents)?),
                    None => graph.as_ref().map(|_| RunEvent::Resync),
                };
                let gate = |raw: Option<RawGate>| {
                    raw.map(|raw| {
                        Ok::<_, PortError>(GateRef {
                            run: RunId::parse(&raw.run)
                                .map_err(|error| PortError::protocol(WHAT, error.to_string()))?,
                            instance: NodeInstance::parse(&raw.instance)
                                .map_err(|error| PortError::protocol(WHAT, error.to_string()))?,
                        })
                    })
                    .transpose()
                    .map_err(contents)
                };
                Ok(Step {
                    after: StdDuration::from_millis(step.after_ms),
                    hold: gate(step.hold)?,
                    awaits: gate(step.awaits)?,
                    graph,
                    event,
                })
            })
            .collect::<Result<_, ScriptError>>()?;
        Ok(Self {
            runs,
            graphs,
            outputs,
            steps,
        })
    }

    /// The gateway, stream and gates that replay it; held gates are
    /// published to `local`.
    #[must_use]
    pub fn replay(
        self,
        local: Arc<LocalBoard>,
    ) -> (
        Arc<ScriptedGateway>,
        Arc<ScriptedEvents>,
        Arc<ScriptedGates>,
    ) {
        let gates = Arc::new(ScriptedGates {
            held: Mutex::default(),
            answered: Mutex::default(),
            notify: Notify::new(),
            local,
        });
        let gateway = Arc::new(ScriptedGateway {
            graphs: Mutex::new(
                self.graphs
                    .into_iter()
                    .map(|graph| (graph.run.clone(), graph))
                    .collect(),
            ),
            outputs: self.outputs,
            gates: gates.clone(),
            steps: Mutex::new(self.steps.into()),
            kicked: Notify::new(),
        });
        let events = Arc::new(ScriptedEvents {
            gateway: gateway.clone(),
        });
        (gateway, events, gates)
    }
}

/// The gates a script holds, and the answers they were given.
pub struct ScriptedGates {
    held: Mutex<Vec<GateRef>>,
    answered: Mutex<Vec<(RunId, NodeInstance, GateAnswer)>>,
    notify: Notify,
    local: Arc<LocalBoard>,
}

impl ScriptedGates {
    /// Every answer given, in order.
    #[must_use]
    pub fn answered(&self) -> Vec<(RunId, NodeInstance, GateAnswer)> {
        self.answered.lock().expect("answered lock").clone()
    }

    fn hold(&self, gate: GateRef) {
        self.held.lock().expect("held lock").push(gate);
        self.publish();
    }

    fn release_run(&self, run: &RunId) {
        self.held
            .lock()
            .expect("held lock")
            .retain(|gate| gate.run != *run);
        self.publish();
        self.notify.notify_waiters();
    }

    fn is_held(&self, gate: &GateRef) -> bool {
        self.held.lock().expect("held lock").contains(gate)
    }

    fn publish(&self) {
        let mut runs: BTreeMap<RunId, LocalSteps> = BTreeMap::new();
        for gate in self.held.lock().expect("held lock").iter() {
            let steps = runs.remove(&gate.run).unwrap_or_default();
            runs.insert(
                gate.run.clone(),
                steps.with(gate.instance.clone(), LocalStep::AwaitingPerson),
            );
        }
        self.local.replace(runs);
    }

    async fn wait_for(&self, gate: &GateRef) {
        loop {
            let notified = self.notify.notified();
            if !self.is_held(gate) {
                return;
            }
            notified.await;
        }
    }
}

#[async_trait]
impl Gates for ScriptedGates {
    async fn answer(
        &self,
        run: &RunId,
        instance: &NodeInstance,
        answer: GateAnswer,
    ) -> Result<GateReply, ApiError> {
        let gate = GateRef {
            run: run.clone(),
            instance: instance.clone(),
        };
        let held = {
            let mut held = self.held.lock().expect("held lock");
            let before = held.len();
            held.retain(|other| *other != gate);
            held.len() != before
        };
        if !held {
            return Ok(GateReply::NotOpen);
        }
        self.answered
            .lock()
            .expect("answered lock")
            .push((run.clone(), instance.clone(), answer));
        self.publish();
        self.notify.notify_waiters();
        Ok(GateReply::Sent)
    }
}

#[derive(Deserialize)]
struct RawScript {
    runs: Vec<RawRun>,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Deserialize)]
struct RawRun {
    ticket: String,
    title: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    graph: RunGraphReply,
    #[serde(default)]
    outputs: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct RawGate {
    run: String,
    instance: String,
}

#[derive(Deserialize)]
struct RawStep {
    #[serde(default)]
    after_ms: u64,
    #[serde(default)]
    graph: Option<RunGraphReply>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    hold: Option<RawGate>,
    #[serde(default, rename = "await")]
    awaits: Option<RawGate>,
}

/// The run endpoints of a script: each run's current graph and outputs,
/// and no definitions.
pub struct ScriptedGateway {
    graphs: Mutex<BTreeMap<RunId, RunGraph>>,
    outputs: BTreeMap<RunId, BTreeMap<String, Value>>,
    gates: Arc<ScriptedGates>,
    steps: Mutex<VecDeque<Step>>,
    kicked: Notify,
}

const WHAT: &str = "scripted gateway";

#[async_trait]
impl RunGateway for ScriptedGateway {
    async fn register(&self, _graph: &WorkflowGraph) -> PortResult<()> {
        Err(PortError::unsupported(WHAT, "register"))
    }

    async fn start(&self, _definition: &DefinitionId, _input: &Value) -> PortResult<RunId> {
        Err(PortError::unsupported(WHAT, "start"))
    }

    async fn remove(&self, _definition: &DefinitionId) -> PortResult<()> {
        Err(PortError::unsupported(WHAT, "remove"))
    }

    async fn definitions(&self) -> PortResult<Vec<DefinitionId>> {
        Ok(Vec::new())
    }

    async fn step_config(&self, _definition: &DefinitionId) -> PortResult<StepConfig> {
        Err(PortError::unsupported(WHAT, "step_config"))
    }

    async fn runs(&self, _definition: &DefinitionId) -> PortResult<Vec<RunHeader>> {
        Ok(Vec::new())
    }

    async fn graph(&self, run: &RunId) -> PortResult<RunGraph> {
        self.graphs
            .lock()
            .expect("graphs lock")
            .get(run)
            .cloned()
            .ok_or_else(|| PortError::not_found("run", run.as_str()))
    }

    /// The run is marked cancelled, its gates let go and the rest of its
    /// script dropped, and the stream says to refetch.
    async fn cancel(&self, run: &RunId) -> PortResult<CancelReply> {
        let mut graphs = self.graphs.lock().expect("graphs lock");
        let graph = graphs
            .get_mut(run)
            .ok_or_else(|| PortError::not_found("run", run.as_str()))?;
        if graph.status.is_finished() {
            return Ok(CancelReply::AlreadyFinished);
        }
        graph.status = RunStatus::Cancelled;
        graph.seq += 1;
        drop(graphs);
        self.steps.lock().expect("steps lock").retain(|step| {
            step.graph.as_ref().is_none_or(|graph| graph.run != *run)
                && step.hold.as_ref().is_none_or(|gate| gate.run != *run)
                && step.awaits.as_ref().is_none_or(|gate| gate.run != *run)
        });
        self.gates.release_run(run);
        self.kicked.notify_waiters();
        Ok(CancelReply::Cancelled)
    }

    async fn outputs(&self, run: &RunId) -> PortResult<BTreeMap<String, Value>> {
        Ok(self.outputs.get(run).cloned().unwrap_or_default())
    }
}

/// The run stream of a script. Its steps play once, shared by every
/// subscription, and the stream then stays open and quiet.
pub struct ScriptedEvents {
    gateway: Arc<ScriptedGateway>,
}

#[async_trait]
impl RunEvents for ScriptedEvents {
    async fn subscribe(&self) -> PortResult<Box<dyn RunEventStream>> {
        Ok(Box::new(ScriptedStream {
            gateway: self.gateway.clone(),
        }))
    }
}

struct ScriptedStream {
    gateway: Arc<ScriptedGateway>,
}

impl ScriptedStream {
    fn front<T>(&self, read: impl FnOnce(&Step) -> T) -> Option<T> {
        self.gateway
            .steps
            .lock()
            .expect("steps lock")
            .front()
            .map(read)
    }
}

#[async_trait]
impl RunEventStream for ScriptedStream {
    async fn next(&mut self) -> PortResult<Option<RunEvent>> {
        loop {
            let kicked = self.gateway.kicked.notified();
            let Some(after) = self.front(|step| step.after) else {
                kicked.await;
                return Ok(Some(RunEvent::Resync));
            };
            tokio::select! {
                () = tokio::time::sleep(after) => {}
                () = kicked => return Ok(Some(RunEvent::Resync)),
            }
            if let Some(gate) = self.front(|step| step.awaits.clone()).flatten() {
                let kicked = self.gateway.kicked.notified();
                tokio::select! {
                    () = self.gateway.gates.wait_for(&gate) => {}
                    () = kicked => return Ok(Some(RunEvent::Resync)),
                }
            }
            let Some(step) = self.gateway.steps.lock().expect("steps lock").pop_front() else {
                continue;
            };
            if let Some(gate) = step.hold {
                self.gateway.gates.hold(gate);
            }
            if let Some(graph) = step.graph {
                self.gateway
                    .graphs
                    .lock()
                    .expect("graphs lock")
                    .insert(graph.run.clone(), graph);
            }
            if let Some(event) = step.event {
                return Ok(Some(event));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use time::macros::datetime;
    use vetinari_core::domain::devflow::summary::Mark;
    use vetinari_core::domain::devflow::template::TemplateKind;

    use vetinari_core::domain::devflow::summary::NeedsYou;

    use super::*;
    use crate::hub::Hub;
    use crate::runtime::{Feed, TokioClock, feed_runs};

    const SCRIPT: &str = include_str!("../tests/fixtures/feature-run.script.json");

    fn strip(hub: &Hub) -> String {
        hub.snapshot()
            .first()
            .map(|item| {
                item.summary
                    .phases
                    .iter()
                    .map(|cell| format!("{}{}", cell.phase.name(), cell.mark.glyph()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_script_reads_its_runs_and_steps() {
        let script = Script::parse(SCRIPT).expect("script");
        assert_eq!(script.runs.len(), 1);
        let run = &script.runs[0];
        assert_eq!(run.run.as_str(), "01JB7ZR2M4Q9V8C6T3N5HKD0WX");
        assert_eq!(
            run.label.as_ref().map(|label| label.ticket.as_str()),
            Some("ENG-412")
        );
        assert_eq!(run.started_at, Some(datetime!(2026-10-25 09:00 UTC)));
        assert_eq!(script.graphs[0].seq, 3);
        assert_eq!(script.steps.len(), 4);
        assert_eq!(script.steps[0].after, StdDuration::from_millis(1500));
        assert!(matches!(
            script.steps[0].event,
            Some(RunEvent::Node { seq: 4, .. })
        ));
        assert_eq!(script.steps[1].event, Some(RunEvent::Resync));
        assert_eq!(
            script.steps[3].graph.as_ref().map(|graph| graph.seq),
            Some(22)
        );
    }

    #[test]
    fn a_script_with_a_graph_the_gateway_would_not_send_is_refused() {
        let broken = SCRIPT.replacen("\"vetinari-hex-agent\"", "\"Not A Queue\"", 1);
        assert!(matches!(
            Script::parse(&broken),
            Err(ScriptError::Contents { .. })
        ));
        assert!(matches!(Script::parse("{}"), Err(ScriptError::Json(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn a_replay_moves_the_run_from_preflight_to_an_opened_pull_request() {
        let script = Script::parse(SCRIPT).expect("script");
        let owned = script.runs.clone();
        let (runs, events, _gates) = script.replay(crate::steps::LocalBoard::new().0);
        let hub = Arc::new(Hub::default());
        tokio::spawn(feed_runs(
            Feed {
                runs,
                events,
                clock: Arc::new(TokioClock::starting_at(datetime!(2026-10-25 10:00 UTC))),
                template: TemplateKind::FeatureFromTicket,
                hub: hub.clone(),
                local: crate::steps::LocalBoard::new().0,
                steps: None,
            },
            owned,
            tokio::sync::mpsc::unbounded_channel().1,
            tokio::sync::mpsc::unbounded_channel().1,
        ));

        tokio::time::sleep(StdDuration::from_millis(500)).await;
        assert_eq!(strip(&hub), "pre◐ impl· rev· pr·");
        tokio::time::sleep(StdDuration::from_millis(1500)).await;
        assert_eq!(strip(&hub), "pre✓ impl○ rev· pr·");
        tokio::time::sleep(StdDuration::from_millis(1500)).await;
        assert_eq!(strip(&hub), "pre✓ impl◐ rev○ pr·");
        tokio::time::sleep(StdDuration::from_millis(3000)).await;
        let item = hub.snapshot()[0].clone();
        assert_eq!(item.summary.phases[1].mark, Mark::Elsewhere);
        assert_eq!(
            item.summary.phases[1]
                .round
                .map(|round| round.iteration.get()),
            Some(2)
        );
        tokio::time::sleep(StdDuration::from_millis(3000)).await;
        assert_eq!(strip(&hub), "pre✓ impl✓ rev✓ pr✓");
    }

    const GATES: &str = include_str!("../tests/fixtures/gate-run.script.json");

    fn gate_run() -> RunId {
        RunId::parse("01JB80A1B2C3D4E5F6G7H8J9K0").expect("run")
    }

    fn ask_info() -> NodeInstance {
        NodeInstance::parse("ask-info@1").expect("instance")
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_gate_needs_you_until_it_is_answered() {
        let script = Script::parse(GATES).expect("script");
        let owned = script.runs.clone();
        let (local, touched) = crate::steps::LocalBoard::new();
        let (runs, events, gates) = script.replay(local.clone());
        let hub = Arc::new(Hub::default());
        tokio::spawn(feed_runs(
            Feed {
                runs: runs.clone(),
                events,
                clock: Arc::new(TokioClock::starting_at(datetime!(2026-10-25 10:00 UTC))),
                template: TemplateKind::FeatureFromTicket,
                hub: hub.clone(),
                local: local.clone(),
                steps: None,
            },
            owned,
            touched,
            tokio::sync::mpsc::unbounded_channel().1,
        ));
        tokio::time::sleep(StdDuration::from_secs(60)).await;
        let gated = |hub: &Hub| {
            hub.snapshot()
                .into_iter()
                .find(|item| item.summary.run == gate_run())
                .expect("the gated run")
        };
        assert!(matches!(
            gated(&hub).summary.needs_you,
            Some(NeedsYou::Gate { ref instance, .. }) if *instance == ask_info()
        ));
        assert_eq!(
            runs.outputs(&gate_run()).await.expect("outputs")["preflight@1"]["questions"][0],
            serde_json::json!("Page size, or whether the client chooses it")
        );

        let wrong = NodeInstance::parse("review-stuck").expect("instance");
        assert_eq!(
            gates
                .answer(&gate_run(), &wrong, GateAnswer::OpenAnyway)
                .await,
            Ok(GateReply::NotOpen)
        );
        assert_eq!(
            gates
                .answer(&gate_run(), &ask_info(), GateAnswer::Recheck { note: None })
                .await,
            Ok(GateReply::Sent)
        );
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        let item = gated(&hub);
        assert_eq!(item.summary.needs_you, None);
        assert_eq!(item.summary.phases[0].mark, Mark::Elsewhere);
        assert_eq!(
            gates.answered(),
            [(gate_run(), ask_info(), GateAnswer::Recheck { note: None })]
        );
    }
}
