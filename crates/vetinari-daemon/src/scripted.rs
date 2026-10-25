//! `--scripted <file>`: replay a recorded run instead of talking to a
//! gateway, so the TUI can be developed without one.
//!
//! A script names its runs, each with the graph it starts from, and a list
//! of steps. A step waits `after_ms`, then may replace its run's graph (what
//! a refetch now returns) and may emit one stream event; a step with a graph
//! and no event emits a `resync`, so the replacement is fetched. Graphs and
//! events are in the gateway's own JSON, read by the same adapter code a
//! live daemon uses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use vetinari_adapters::autognomus::wire::{RunGraphReply, run_event};
use vetinari_core::application::devflow::api::RunLabel;
use vetinari_core::application::devflow::ports::{
    CancelReply, RunEventStream, RunEvents, RunGateway,
};
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::devflow::compile::WorkflowGraph;
use vetinari_core::domain::devflow::ids::{DefinitionId, RunId};
use vetinari_core::domain::devflow::run_view::{RunEvent, RunGraph, RunHeader};
use vetinari_core::domain::text::NonEmptyText;

use crate::runtime::OwnedRun;

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

/// One step of a script, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// How long after the step before.
    pub after: StdDuration,
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
    /// The steps, in order.
    pub steps: Vec<Step>,
}

impl Script {
    /// Read a script.
    pub fn parse(text: &str) -> Result<Self, ScriptError> {
        let raw: RawScript = serde_json::from_str(text)?;
        let mut runs = Vec::new();
        let mut graphs = Vec::new();
        for run in raw.runs {
            let graph = run
                .graph
                .into_graph()
                .map_err(|error| ScriptError::Contents { step: 0, error })?;
            let label = RunLabel {
                ticket: NonEmptyText::parse(&run.ticket).map_err(|_| ScriptError::Label)?,
                title: NonEmptyText::parse(&run.title).map_err(|_| ScriptError::Label)?,
            };
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
                Ok(Step {
                    after: StdDuration::from_millis(step.after_ms),
                    graph,
                    event,
                })
            })
            .collect::<Result<_, ScriptError>>()?;
        Ok(Self {
            runs,
            graphs,
            steps,
        })
    }

    /// The gateway and stream that replay it.
    #[must_use]
    pub fn replay(self) -> (Arc<ScriptedGateway>, Arc<ScriptedEvents>) {
        let gateway = Arc::new(ScriptedGateway {
            graphs: Mutex::new(
                self.graphs
                    .into_iter()
                    .map(|graph| (graph.run.clone(), graph))
                    .collect(),
            ),
        });
        let events = Arc::new(ScriptedEvents {
            gateway: gateway.clone(),
            steps: Arc::new(Mutex::new(self.steps.into())),
        });
        (gateway, events)
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
}

/// The run endpoints of a script: each run's current graph, and no
/// definitions.
#[derive(Debug, Default)]
pub struct ScriptedGateway {
    graphs: Mutex<BTreeMap<RunId, RunGraph>>,
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

    async fn cancel(&self, _run: &RunId) -> PortResult<CancelReply> {
        Err(PortError::unsupported(WHAT, "cancel"))
    }

    async fn outputs(&self, _run: &RunId) -> PortResult<BTreeMap<String, Value>> {
        Ok(BTreeMap::new())
    }
}

/// The run stream of a script. Its steps play once, shared by every
/// subscription, and the stream then stays open and quiet.
pub struct ScriptedEvents {
    gateway: Arc<ScriptedGateway>,
    steps: Arc<Mutex<VecDeque<Step>>>,
}

#[async_trait]
impl RunEvents for ScriptedEvents {
    async fn subscribe(&self) -> PortResult<Box<dyn RunEventStream>> {
        Ok(Box::new(ScriptedStream {
            gateway: self.gateway.clone(),
            steps: self.steps.clone(),
        }))
    }
}

struct ScriptedStream {
    gateway: Arc<ScriptedGateway>,
    steps: Arc<Mutex<VecDeque<Step>>>,
}

#[async_trait]
impl RunEventStream for ScriptedStream {
    async fn next(&mut self) -> PortResult<Option<RunEvent>> {
        loop {
            let after = self
                .steps
                .lock()
                .expect("steps lock")
                .front()
                .map(|step| step.after);
            let Some(after) = after else {
                std::future::pending::<()>().await;
                return Ok(None);
            };
            tokio::time::sleep(after).await;
            let Some(step) = self.steps.lock().expect("steps lock").pop_front() else {
                continue;
            };
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
        let (runs, events) = script.replay();
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
}
