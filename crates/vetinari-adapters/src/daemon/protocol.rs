//! The daemon's socket frames (design 10.2): newline-delimited JSON, one
//! frame per line.
//!
//! Both ends of the socket are adapters, so the frames live here, shared by
//! the server in `vetinari-daemon` and the TUI's client. Unknown fields are
//! ignored in both directions.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use vetinari_core::application::auth::AuthState;
use vetinari_core::application::devflow::api::{
    ApiError, BoardChange, DetailRow, Hello, RunDetail, RunItem, RunLabel,
};
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::application::ports::DeviceCodeChallenge;
use vetinari_core::domain::devflow::ids::{
    BranchLabel, NodeId, NodeInstance, Rounds, RunId, TaskQueue,
};
use vetinari_core::domain::devflow::lanes::{IterationSelection, Lane, LaneRow, RowKind};
use vetinari_core::domain::devflow::notes::{ReviewFindings, RunNotes};
use vetinari_core::domain::devflow::outcome::{Finding, GateAnswer, Line};
use vetinari_core::domain::devflow::run_view::RunStatus;
use vetinari_core::domain::devflow::run_view::{InstanceState, InstanceStatus};
use vetinari_core::domain::devflow::step::StepKind;
use vetinari_core::domain::devflow::summary::{Mark, NeedsYou};
use vetinari_core::domain::devflow::summary::{Phase, PhaseCell, Round, RunSummary};
use vetinari_core::domain::devflow::template::{
    DevWorkflow, FeatureFromTicket, RepoTarget, TemplateKind, TicketRef,
};
use vetinari_core::domain::ids::{ProfileId, SourceId};
use vetinari_core::domain::text::NonEmptyText;

/// The longest frame either end accepts, newline excluded.
pub const MAX_FRAME: usize = 1 << 20;

/// A request as it arrives.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RequestFrame {
    /// The id the reply carries.
    pub id: u64,
    /// The method.
    pub method: String,
    /// Its params.
    #[serde(default)]
    pub params: Value,
}

/// A request the daemon understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `hello`.
    Hello {
        /// The client's name and version.
        client: String,
        /// The protocol it speaks.
        protocol: u32,
    },
    /// `runs.watch`: a stream of run items.
    RunsWatch,
    /// `stream.close`.
    StreamClose {
        /// The stream to close.
        stream: u64,
    },
    /// `run.watch`: a stream of one run's detail.
    RunWatch {
        /// The run.
        run: RunId,
        /// Which iteration of each loop to show; the latest otherwise.
        selection: IterationSelection,
    },
    /// `gate.answer`.
    GateAnswer {
        /// The run.
        run: RunId,
        /// The gate's instance.
        instance: NodeInstance,
        /// The answer.
        answer: GateAnswer,
    },
    /// `run.cancel`.
    RunCancel {
        /// The run.
        run: RunId,
    },
    /// `run.start`: a run of a template, for one ticket.
    RunStart(Box<DevWorkflow>),
    /// `run.dismiss`: take a finished run off the queue.
    RunDismiss {
        /// The run.
        run: RunId,
        /// Whether its worktree goes too.
        remove_worktree: bool,
    },
    /// `auth.status`: whether the daemon is signed in to the gateway.
    AuthStatus,
    /// `auth.begin`: a stream of one sign-in, its challenge then its end.
    AuthBegin,
}

/// A line that is not a request: its id when one could be read, so the
/// error can say which request it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadFrame {
    /// The request's id, if it had a readable one.
    pub id: Option<u64>,
    /// What was wrong.
    pub error: ApiError,
}

/// Read one line as a request.
pub fn parse_request(line: &str) -> Result<(u64, Request), BadFrame> {
    let frame: RequestFrame = serde_json::from_str(line).map_err(|error| BadFrame {
        id: serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| value.get("id")?.as_u64()),
        error: ApiError::Invalid(format!("not a request: {error}")),
    })?;
    let invalid = |detail: String| BadFrame {
        id: Some(frame.id),
        error: ApiError::Invalid(detail),
    };
    let params = |error: serde_json::Error| invalid(format!("{} params: {error}", frame.method));
    let request = match frame.method.as_str() {
        "hello" => {
            #[derive(Deserialize)]
            struct Params {
                client: String,
                protocol: u32,
            }
            let Params { client, protocol } =
                serde_json::from_value(frame.params.clone()).map_err(params)?;
            Request::Hello { client, protocol }
        }
        "runs.watch" => Request::RunsWatch,
        "stream.close" => {
            #[derive(Deserialize)]
            struct Params {
                stream: u64,
            }
            let Params { stream } = serde_json::from_value(frame.params.clone()).map_err(params)?;
            Request::StreamClose { stream }
        }
        "run.watch" => {
            #[derive(Deserialize)]
            struct Params {
                run: String,
                #[serde(default)]
                iterations: BTreeMap<String, NonZeroU32>,
            }
            let Params { run, iterations } =
                serde_json::from_value(frame.params.clone()).map_err(params)?;
            let mut selection = IterationSelection::default();
            for (node, iteration) in iterations {
                let node = NodeId::parse(&node).map_err(|error| invalid(error.to_string()))?;
                selection = selection.with(node, iteration);
            }
            Request::RunWatch {
                run: run_id(&run).map_err(invalid)?,
                selection,
            }
        }
        "gate.answer" => {
            #[derive(Deserialize)]
            struct Params {
                run: String,
                instance: String,
                answer: Answer,
            }
            #[derive(Deserialize)]
            #[serde(tag = "kind", rename_all = "kebab-case")]
            enum Answer {
                Recheck {
                    #[serde(default)]
                    note: Option<String>,
                },
                OpenAnyway,
            }
            let Params {
                run,
                instance,
                answer,
            } = serde_json::from_value(frame.params.clone()).map_err(params)?;
            let answer = match answer {
                Answer::Recheck { note } => GateAnswer::Recheck {
                    note: note
                        .filter(|note| !note.trim().is_empty())
                        .map(|note| Line::parse(&note))
                        .transpose()
                        .map_err(|error| invalid(format!("note: {error}")))?,
                },
                Answer::OpenAnyway => GateAnswer::OpenAnyway,
            };
            Request::GateAnswer {
                run: run_id(&run).map_err(invalid)?,
                instance: NodeInstance::parse(&instance)
                    .map_err(|error| invalid(error.to_string()))?,
                answer,
            }
        }
        "run.cancel" => {
            #[derive(Deserialize)]
            struct Params {
                run: String,
            }
            let Params { run } = serde_json::from_value(frame.params.clone()).map_err(params)?;
            Request::RunCancel {
                run: run_id(&run).map_err(invalid)?,
            }
        }
        "run.start" => Request::RunStart(Box::new(run_start(&frame.params).map_err(invalid)?)),
        "run.dismiss" => {
            #[derive(Deserialize)]
            struct Params {
                run: String,
                remove_worktree: bool,
            }
            let Params {
                run,
                remove_worktree,
            } = serde_json::from_value(frame.params.clone()).map_err(params)?;
            Request::RunDismiss {
                run: run_id(&run).map_err(invalid)?,
                remove_worktree,
            }
        }
        "auth.status" => Request::AuthStatus,
        "auth.begin" => Request::AuthBegin,
        other => return Err(invalid(format!("no method {other:?}"))),
    };
    Ok((frame.id, request))
}

/// `run.start`'s params for `workflow`, as a client sends them.
#[must_use]
pub fn run_start_params(workflow: &DevWorkflow) -> Value {
    match workflow {
        DevWorkflow::FeatureFromTicket(feature) => json!({
            "template": workflow.kind().name(),
            "ticket": {
                "source": feature.ticket.source.as_str(),
                "key": feature.ticket.key.as_str(),
                "title": feature.ticket.title.as_str(),
                "url": feature.ticket.url,
            },
            "repo": {
                "path": feature.repo.path,
                "remote": feature.repo.remote.as_str(),
            },
            "profile": feature.profile.as_str(),
            "max_review_rounds": feature.max_review_rounds.get().get(),
        }),
    }
}

/// Read `run.start`'s params: a ticket, its repository and profile, for
/// the one template there is. What a run does not say is the default.
fn run_start(params: &Value) -> Result<DevWorkflow, String> {
    #[derive(Deserialize)]
    struct Params {
        template: String,
        ticket: Ticket,
        repo: Repo,
        profile: String,
        #[serde(default)]
        max_review_rounds: Option<u32>,
    }
    #[derive(Deserialize)]
    struct Ticket {
        source: String,
        key: String,
        title: String,
        #[serde(default)]
        url: Option<String>,
    }
    #[derive(Deserialize)]
    struct Repo {
        path: PathBuf,
        remote: String,
    }
    let params: Params = serde_json::from_value(params.clone())
        .map_err(|error| format!("run.start params: {error}"))?;
    let text = |field: &str, raw: &str| {
        NonEmptyText::parse(raw).map_err(|error| format!("{field}: {error}"))
    };
    let template: TemplateKind = params
        .template
        .parse()
        .map_err(|error| format!("template: {error}"))?;
    let max_review_rounds = params
        .max_review_rounds
        .map(Rounds::new)
        .transpose()
        .map_err(|error| format!("max_review_rounds: {error}"))?
        .unwrap_or(Rounds::DEFAULT);
    match template {
        TemplateKind::FeatureFromTicket => Ok(DevWorkflow::FeatureFromTicket(FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse(&params.ticket.source)
                    .map_err(|error| format!("ticket source: {error}"))?,
                key: text("ticket key", &params.ticket.key)?,
                title: text("ticket title", &params.ticket.title)?,
                url: params.ticket.url,
            },
            repo: RepoTarget {
                path: params.repo.path,
                remote: text("repo remote", &params.repo.remote)?,
            },
            profile: ProfileId::parse(&params.profile)
                .map_err(|error| format!("profile: {error}"))?,
            base: None,
            max_review_rounds,
            max_intake_rounds: Rounds::DEFAULT,
            agent: None,
        })),
    }
}

/// `run.start`'s result.
#[must_use]
pub fn run_started_result(run: &RunId) -> Value {
    json!({ "run": run.as_str() })
}

/// Read `run.start`'s result.
pub fn read_run_started(value: &Value) -> Result<RunId, String> {
    run_id(value["run"].as_str().unwrap_or_default())
}

fn run_id(raw: &str) -> Result<RunId, String> {
    RunId::parse(raw).map_err(|error| format!("run: {error}"))
}

/// A frame the daemon sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    /// The reply to a request.
    Reply {
        /// The request's id.
        id: u64,
        /// What it produced.
        result: Value,
    },
    /// A request that failed.
    Error {
        /// The request's id, if it had a readable one.
        id: Option<u64>,
        /// Why.
        error: ApiError,
    },
    /// An event on an open stream.
    Event {
        /// The stream.
        stream: u64,
        /// The event.
        event: Value,
    },
}

impl Outbound {
    /// The frame as one line, newline included.
    #[must_use]
    pub fn to_line(&self) -> String {
        let value = match self {
            Self::Reply { id, result } => json!({ "id": id, "result": result }),
            Self::Error { id, error } => json!({
                "id": id,
                "error": { "code": error.code(), "message": error.to_string() },
            }),
            Self::Event { stream, event } => json!({ "stream": stream, "event": event }),
        };
        let mut line = value.to_string();
        line.push('\n');
        line
    }
}

/// `hello`'s result.
#[must_use]
pub fn hello_result(hello: &Hello) -> Value {
    json!({
        "daemon": hello.daemon,
        "protocol": hello.protocol,
        "worker": hello.worker.as_str(),
    })
}

/// The result of a method that opens a stream.
#[must_use]
pub fn stream_opened(stream: u64) -> Value {
    json!({ "stream": stream })
}

/// The first event of a `runs.watch` stream.
#[must_use]
pub fn snapshot_event(items: &[RunItem]) -> Value {
    json!({ "type": "snapshot", "runs": items.iter().map(run_item).collect::<Vec<_>>() })
}

/// A later event of a `runs.watch` stream.
#[must_use]
pub fn change_event(change: &BoardChange) -> Value {
    match change {
        BoardChange::Run(item) => {
            let mut event = run_item(item);
            event["type"] = json!("run");
            event
        }
        BoardChange::Removed(run) => json!({ "type": "removed", "run": run.as_str() }),
    }
}

/// `gate.answer`'s result: whether the gateway took it, and if not whether
/// it is queued to be sent.
#[must_use]
pub fn gate_result(reply: GateReply) -> Value {
    match reply {
        GateReply::Sent => json!({ "sent": true }),
        GateReply::Queued => json!({ "sent": false, "queued": true }),
        GateReply::NotOpen => json!({ "sent": false, "queued": false }),
    }
}

const fn auth_state_name(state: AuthState) -> &'static str {
    match state {
        AuthState::Missing => "missing",
        AuthState::Valid => "valid",
        AuthState::NearingExpiry => "nearing-expiry",
        AuthState::Expired => "expired",
        AuthState::Corrupt => "corrupt",
    }
}

/// `auth.status`'s result: the state of the daemon's gateway credential.
#[must_use]
pub fn auth_status_result(state: AuthState) -> Value {
    json!({ "state": auth_state_name(state) })
}

/// Read `auth.status`'s result.
pub fn read_auth_status(value: &Value) -> Result<AuthState, String> {
    let name = value["state"].as_str().unwrap_or_default();
    [
        AuthState::Missing,
        AuthState::Valid,
        AuthState::NearingExpiry,
        AuthState::Expired,
        AuthState::Corrupt,
    ]
    .into_iter()
    .find(|state| auth_state_name(*state) == name)
    .ok_or_else(|| format!("no sign-in state {name:?}"))
}

/// The first event of an `auth.begin` stream. The device code stays in the
/// daemon, which polls with it.
#[must_use]
pub fn challenge_event(challenge: &DeviceCodeChallenge) -> Value {
    json!({
        "type": "challenge",
        "user_code": challenge.user_code,
        "verification_uri": challenge.verification_uri,
        "interval": challenge.interval_secs,
        "expires_at": time_value(challenge.expires_at),
    })
}

/// Read a challenge event. Its device code is empty: only the daemon
/// polls.
pub fn read_challenge(value: &Value) -> Result<DeviceCodeChallenge, String> {
    #[derive(Deserialize)]
    struct Wire {
        user_code: String,
        verification_uri: String,
        interval: u64,
        #[serde(with = "time::serde::rfc3339")]
        expires_at: time::OffsetDateTime,
    }
    let wire: Wire = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    Ok(DeviceCodeChallenge {
        user_code: wire.user_code,
        verification_uri: wire.verification_uri,
        device_code: String::new(),
        interval_secs: wire.interval,
        expires_at: wire.expires_at,
    })
}

/// The last event of an `auth.begin` stream: granted, or why not.
#[must_use]
pub fn sign_in_ended_event(rejected: Option<&str>) -> Value {
    match rejected {
        None => json!({ "type": "granted" }),
        Some(reason) => json!({ "type": "rejected", "reason": reason }),
    }
}

/// A `runs.watch` event saying whether the daemon needs a sign-in before it
/// can reach the gateway.
#[must_use]
pub fn auth_event(needs_sign_in: bool) -> Value {
    json!({ "type": "auth", "needs_sign_in": needs_sign_in })
}

/// An event of a `run.watch` stream.
#[must_use]
pub fn detail_event(detail: &RunDetail) -> Value {
    let rows: Vec<Value> = detail.rows.iter().map(detail_row).collect();
    let mut event = json!({
        "type": "graph",
        "run": run_item(&detail.item),
        "rows": rows,
        "questions": detail.notes.questions.iter().map(Line::as_str).collect::<Vec<_>>(),
    });
    if let Some(findings) = &detail.notes.findings {
        event["findings"] = json!({
            "round": findings.round.get(),
            "items": findings.findings.iter().map(|finding| {
                let mut item = json!({ "text": finding.text.as_str() });
                if let Some(file) = &finding.file {
                    item["file"] = json!(file.as_str());
                }
                if let Some(line) = finding.line {
                    item["line"] = json!(line.get());
                }
                item
            }).collect::<Vec<_>>(),
        });
    }
    event
}

fn detail_row(row: &DetailRow) -> Value {
    let mut value = json!({
        "indent": row.row.indent,
        "node": row.row.node.as_str(),
        "lane": match row.row.lane {
            Lane::Alone => "alone",
            Lane::First => "first",
            Lane::Middle => "middle",
            Lane::Last => "last",
        },
        "mark": mark_name(row.mark),
        "glyph": row.mark.glyph().to_string(),
    });
    if let RowKind::Loop {
        reached,
        selected,
        bound,
    } = row.row.kind
    {
        value["loop"] = json!({
            "reached": reached,
            "selected": selected.get(),
            "bound": bound.get(),
        });
    }
    if let Some(state) = &row.row.state {
        value["instance"] = json!(state.instance.to_string());
        let (status, extra) = match &state.status {
            InstanceStatus::Ready => ("ready", None),
            InstanceStatus::Running => ("running", None),
            InstanceStatus::Waiting { until } => ("waiting", Some(("until", time_value(*until)))),
            InstanceStatus::Offered { queue } => {
                ("offered", Some(("queue", json!(queue.as_str()))))
            }
            InstanceStatus::Claimed { worker, expires_at } => {
                value["expires_at"] = time_value(*expires_at);
                ("claimed", Some(("worker", json!(worker))))
            }
            InstanceStatus::Looping { iteration } => {
                ("looping", Some(("iteration", json!(iteration.get()))))
            }
            InstanceStatus::Done { branch } => (
                "done",
                branch
                    .as_ref()
                    .map(|branch| ("branch", json!(branch.as_str()))),
            ),
            InstanceStatus::Failed { error } => ("failed", Some(("error", json!(error)))),
            InstanceStatus::Skipped => ("skipped", None),
            InstanceStatus::Cancelled => ("cancelled", None),
        };
        value["status"] = json!(status);
        if let Some((key, extra)) = extra {
            value[key] = extra;
        }
        if let Some(attempt) = state.attempt {
            value["attempt"] = json!(attempt.get());
        }
        if let Some(at) = state.first_ready_at {
            value["first_ready_at"] = time_value(at);
        }
        if let Some(at) = state.finished_at {
            value["finished_at"] = time_value(at);
        }
    }
    value
}

fn time_value(at: time::OffsetDateTime) -> Value {
    at.format(&time::format_description::well_known::Rfc3339)
        .map_or(Value::Null, Value::String)
}

/// A run item as JSON: the summary the daemon derived, so the client
/// renders without holding any workflow logic.
fn run_item(item: &RunItem) -> Value {
    let summary = &item.summary;
    let phases: Vec<Value> = summary
        .phases
        .iter()
        .map(|cell| {
            let mut phase = json!({
                "phase": cell.phase.name(),
                "mark": mark_name(cell.mark),
                "glyph": cell.mark.glyph().to_string(),
            });
            if let Some(round) = cell.round {
                phase["round"] = json!({
                    "iteration": round.iteration.get(),
                    "bound": round.bound.get(),
                });
            }
            phase
        })
        .collect();
    let mut value = json!({
        "run": summary.run.as_str(),
        "status": status_name(summary.status),
        "phases": phases,
        "needs_you": summary.needs_you.as_ref().map(needs_you),
    });
    if let Some(label) = &item.label {
        value["ticket"] = json!(label.ticket.as_str());
        value["title"] = json!(label.title.as_str());
    }
    if let Some(started_at) = item.started_at.and_then(|at| {
        at.format(&time::format_description::well_known::Rfc3339)
            .ok()
    }) {
        value["started_at"] = json!(started_at);
    }
    value
}

fn needs_you(needs: &NeedsYou) -> Value {
    match needs {
        NeedsYou::Gate { instance, gate } => json!({
            "kind": "gate",
            "instance": instance.to_string(),
            "gate": StepKind::Gate(*gate).name(),
        }),
        NeedsYou::Question { instance, question } => json!({
            "kind": "question",
            "instance": instance.to_string(),
            "question": question,
        }),
    }
}

const fn mark_name(mark: Mark) -> &'static str {
    match mark {
        Mark::Done => "done",
        Mark::RunningHere => "running-here",
        Mark::Elsewhere => "elsewhere",
        Mark::NeedsYou => "needs-you",
        Mark::Failed => "failed",
        Mark::Next => "next",
        Mark::NotReached => "not-reached",
    }
}

const fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Active => "active",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

// The client's side: reading back what the daemon wrote.

#[derive(Deserialize)]
struct WireItem {
    run: String,
    #[serde(default)]
    ticket: Option<String>,
    #[serde(default)]
    title: Option<String>,
    status: String,
    phases: Vec<WirePhase>,
    #[serde(default)]
    needs_you: Option<WireNeeds>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<time::OffsetDateTime>,
}

#[derive(Deserialize)]
struct WirePhase {
    phase: String,
    mark: String,
    #[serde(default)]
    round: Option<WireRound>,
}

#[derive(Deserialize)]
struct WireRound {
    iteration: NonZeroU32,
    bound: NonZeroU32,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum WireNeeds {
    Gate { instance: String, gate: String },
    Question { instance: String, question: String },
}

#[derive(Deserialize)]
struct WireDetail {
    run: Value,
    rows: Vec<WireRow>,
    #[serde(default)]
    questions: Vec<String>,
    #[serde(default)]
    findings: Option<WireFindings>,
}

#[derive(Deserialize)]
struct WireRow {
    indent: usize,
    node: String,
    lane: String,
    mark: String,
    #[serde(default, rename = "loop")]
    looping: Option<WireLoop>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    worker: Option<String>,
    #[serde(default)]
    queue: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    iteration: Option<NonZeroU32>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    until: Option<time::OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<time::OffsetDateTime>,
    #[serde(default)]
    attempt: Option<NonZeroU32>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    first_ready_at: Option<time::OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    finished_at: Option<time::OffsetDateTime>,
}

#[derive(Deserialize)]
struct WireLoop {
    reached: u32,
    selected: NonZeroU32,
    bound: NonZeroU32,
}

#[derive(Deserialize)]
struct WireFindings {
    round: NonZeroU32,
    items: Vec<WireFinding>,
}

#[derive(Deserialize)]
struct WireFinding {
    text: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<NonZeroU32>,
}

/// Read a run item from a `snapshot` entry or a `run` event.
pub fn read_run_item(value: &Value) -> Result<RunItem, String> {
    let wire: WireItem =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    let run = run_id(&wire.run)?;
    let status = match wire.status.as_str() {
        "active" => RunStatus::Active,
        "completed" => RunStatus::Completed,
        "failed" => RunStatus::Failed,
        "cancelled" => RunStatus::Cancelled,
        other => return Err(format!("no run status {other:?}")),
    };
    let phases = wire
        .phases
        .into_iter()
        .map(|phase| {
            Ok(PhaseCell {
                phase: Phase::ALL
                    .into_iter()
                    .find(|known| known.name() == phase.phase)
                    .ok_or_else(|| format!("no phase {:?}", phase.phase))?,
                mark: mark_named(&phase.mark)?,
                round: phase.round.map(|round| Round {
                    iteration: round.iteration,
                    bound: round.bound,
                }),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let instance = |raw: &str| NodeInstance::parse(raw).map_err(|error| error.to_string());
    let needs_you = match wire.needs_you {
        None => None,
        Some(WireNeeds::Gate {
            instance: raw,
            gate,
        }) => Some(NeedsYou::Gate {
            instance: instance(&raw)?,
            gate: match gate.parse::<StepKind>() {
                Ok(StepKind::Gate(gate)) => gate,
                _ => return Err(format!("no gate {gate:?}")),
            },
        }),
        Some(WireNeeds::Question {
            instance: raw,
            question,
        }) => Some(NeedsYou::Question {
            instance: instance(&raw)?,
            question,
        }),
    };
    let label = match (wire.ticket, wire.title) {
        (Some(ticket), Some(title)) => Some(RunLabel {
            ticket: NonEmptyText::parse(&ticket).map_err(|error| error.to_string())?,
            title: NonEmptyText::parse(&title).map_err(|error| error.to_string())?,
        }),
        _ => None,
    };
    Ok(RunItem {
        summary: RunSummary {
            run,
            status,
            phases,
            needs_you,
        },
        label,
        started_at: wire.started_at,
    })
}

/// Read a `run.watch` stream's `graph` event.
pub fn read_detail(value: &Value) -> Result<RunDetail, String> {
    let wire: WireDetail =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    let item = read_run_item(&wire.run)?;
    let rows = wire
        .rows
        .into_iter()
        .map(read_row)
        .collect::<Result<Vec<_>, String>>()?;
    let line = |raw: &str| Line::parse(raw).map_err(|error| error.to_string());
    let questions = wire
        .questions
        .iter()
        .map(|question| line(question))
        .collect::<Result<Vec<_>, String>>()?;
    let findings = wire
        .findings
        .map(|findings| {
            Ok::<_, String>(ReviewFindings {
                round: findings.round,
                findings: findings
                    .items
                    .into_iter()
                    .map(|finding| {
                        Ok(Finding {
                            file: finding.file.as_deref().map(line).transpose()?,
                            line: finding.line,
                            text: line(&finding.text)?,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            })
        })
        .transpose()?;
    Ok(RunDetail {
        item,
        rows,
        notes: RunNotes {
            questions,
            findings,
        },
    })
}

fn read_row(wire: WireRow) -> Result<DetailRow, String> {
    let lane = match wire.lane.as_str() {
        "alone" => Lane::Alone,
        "first" => Lane::First,
        "middle" => Lane::Middle,
        "last" => Lane::Last,
        other => return Err(format!("no lane {other:?}")),
    };
    let kind = match wire.looping {
        Some(looping) => RowKind::Loop {
            reached: looping.reached,
            selected: looping.selected,
            bound: looping.bound,
        },
        None => RowKind::Node,
    };
    let missing = |field: &str| {
        format!(
            "a {} row has no {field}",
            wire.status.as_deref().unwrap_or("?")
        )
    };
    let state = match (&wire.instance, &wire.status) {
        (Some(instance), Some(status)) => {
            let status = match status.as_str() {
                "ready" => InstanceStatus::Ready,
                "running" => InstanceStatus::Running,
                "waiting" => InstanceStatus::Waiting {
                    until: wire.until.ok_or_else(|| missing("until"))?,
                },
                "offered" => InstanceStatus::Offered {
                    queue: TaskQueue::parse(wire.queue.as_deref().ok_or_else(|| missing("queue"))?)
                        .map_err(|error| error.to_string())?,
                },
                "claimed" => InstanceStatus::Claimed {
                    worker: wire.worker.clone().ok_or_else(|| missing("worker"))?,
                    expires_at: wire.expires_at.ok_or_else(|| missing("expires_at"))?,
                },
                "looping" => InstanceStatus::Looping {
                    iteration: wire.iteration.ok_or_else(|| missing("iteration"))?,
                },
                "done" => InstanceStatus::Done {
                    branch: wire
                        .branch
                        .as_deref()
                        .map(BranchLabel::parse)
                        .transpose()
                        .map_err(|error| error.to_string())?,
                },
                "failed" => InstanceStatus::Failed {
                    error: wire.error.clone().unwrap_or_default(),
                },
                "skipped" => InstanceStatus::Skipped,
                "cancelled" => InstanceStatus::Cancelled,
                other => return Err(format!("no instance status {other:?}")),
            };
            Some(InstanceState {
                instance: NodeInstance::parse(instance).map_err(|error| error.to_string())?,
                status,
                attempt: wire.attempt,
                first_ready_at: wire.first_ready_at,
                finished_at: wire.finished_at,
            })
        }
        _ => None,
    };
    Ok(DetailRow {
        row: LaneRow {
            indent: wire.indent,
            node: NodeId::parse(&wire.node).map_err(|error| error.to_string())?,
            lane,
            kind,
            state,
        },
        mark: mark_named(&wire.mark)?,
    })
}

fn mark_named(name: &str) -> Result<Mark, String> {
    [
        Mark::Done,
        Mark::RunningHere,
        Mark::Elsewhere,
        Mark::NeedsYou,
        Mark::Failed,
        Mark::Next,
        Mark::NotReached,
    ]
    .into_iter()
    .find(|mark| mark_name(*mark) == name)
    .ok_or_else(|| format!("no mark {name:?}"))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use time::macros::datetime;
    use vetinari_core::domain::devflow::ids::WorkerName;
    use vetinari_core::domain::devflow::step::GateKind;

    use super::*;

    fn item() -> RunItem {
        RunItem {
            summary: RunSummary {
                run: RunId::parse("01JB7Z").expect("run"),
                status: RunStatus::Active,
                phases: vec![
                    PhaseCell {
                        phase: Phase::Pre,
                        mark: Mark::Done,
                        round: None,
                    },
                    PhaseCell {
                        phase: Phase::Impl,
                        mark: Mark::NeedsYou,
                        round: Some(Round {
                            iteration: NonZeroU32::new(2).expect("two"),
                            bound: NonZeroU32::new(3).expect("three"),
                        }),
                    },
                ],
                needs_you: Some(NeedsYou::Gate {
                    instance: NodeInstance::parse("ask-info@1").expect("instance"),
                    gate: GateKind::InfoNeeded,
                }),
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: Some(datetime!(2026-10-25 09:00 UTC)),
        }
    }

    fn line(value: &Outbound) -> Value {
        let line = value.to_line();
        assert!(line.ends_with('\n'), "{line:?}");
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        serde_json::from_str(&line).expect("JSON")
    }

    #[test]
    fn requests_are_read_by_method() {
        assert_eq!(
            parse_request(
                r#"{"id":1,"method":"hello","params":{"client":"vetinari 0.1.0","protocol":1}}"#
            ),
            Ok((
                1,
                Request::Hello {
                    client: "vetinari 0.1.0".to_owned(),
                    protocol: 1
                }
            ))
        );
        assert_eq!(
            parse_request(r#"{"id":2,"method":"runs.watch","params":{},"extra":true}"#),
            Ok((2, Request::RunsWatch))
        );
        assert_eq!(
            parse_request(r#"{"id":3,"method":"stream.close","params":{"stream":9}}"#),
            Ok((3, Request::StreamClose { stream: 9 }))
        );
    }

    #[test]
    fn a_run_is_watched_answered_and_cancelled_by_name() {
        assert_eq!(
            parse_request(r#"{"id":1,"method":"run.watch","params":{"run":"r1"}}"#),
            Ok((
                1,
                Request::RunWatch {
                    run: RunId::parse("r1").expect("run"),
                    selection: IterationSelection::default(),
                }
            ))
        );
        assert_eq!(
            parse_request(
                r#"{"id":2,"method":"run.watch","params":{"run":"r1","iterations":{"rework":1}}}"#
            ),
            Ok((
                2,
                Request::RunWatch {
                    run: RunId::parse("r1").expect("run"),
                    selection: IterationSelection::default()
                        .with(NodeId::parse("rework").expect("node"), NonZeroU32::MIN),
                }
            ))
        );
        assert_eq!(
            parse_request(
                r#"{"id":3,"method":"gate.answer","params":{"run":"r1","instance":"ask-info@1","answer":{"kind":"recheck","note":"Page size is 50"}}}"#
            ),
            Ok((
                3,
                Request::GateAnswer {
                    run: RunId::parse("r1").expect("run"),
                    instance: NodeInstance::parse("ask-info@1").expect("instance"),
                    answer: GateAnswer::Recheck {
                        note: Some(Line::parse("Page size is 50").expect("note")),
                    },
                }
            ))
        );
        assert_eq!(
            parse_request(
                r#"{"id":4,"method":"gate.answer","params":{"run":"r1","instance":"review-stuck","answer":{"kind":"open-anyway"}}}"#
            )
            .map(|(_, request)| request),
            Ok(Request::GateAnswer {
                run: RunId::parse("r1").expect("run"),
                instance: NodeInstance::parse("review-stuck").expect("instance"),
                answer: GateAnswer::OpenAnyway,
            })
        );
        assert_eq!(
            parse_request(r#"{"id":5,"method":"run.cancel","params":{"run":"r1"}}"#),
            Ok((
                5,
                Request::RunCancel {
                    run: RunId::parse("r1").expect("run")
                }
            ))
        );
        let bad = parse_request(
            r#"{"id":6,"method":"gate.answer","params":{"run":"r1","instance":"ask-info@1","answer":{"kind":"maybe"}}}"#,
        )
        .expect_err("invalid");
        assert_eq!((bad.id, bad.error.code()), (Some(6), "invalid"));
    }

    fn feature() -> DevWorkflow {
        DevWorkflow::FeatureFromTicket(FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse("jira").expect("source"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
                url: Some("https://acme.atlassian.net/browse/ENG-412".to_owned()),
            },
            repo: RepoTarget {
                path: std::path::PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: ProfileId::parse("work").expect("profile"),
            base: None,
            max_review_rounds: Rounds::new(3).expect("rounds"),
            max_intake_rounds: Rounds::new(3).expect("rounds"),
            agent: None,
        })
    }

    #[test]
    fn a_run_is_started_for_a_ticket_in_a_repository_under_a_profile() {
        let raw = r#"{"id":7,"method":"run.start","params":{
            "template":"feature-from-ticket",
            "ticket":{"source":"jira","key":"ENG-412","title":"Rate-limit retries",
                "url":"https://acme.atlassian.net/browse/ENG-412"},
            "repo":{"path":"/work/repos/web","remote":"acme/web"},
            "profile":"work"}}"#
            .replace('\n', "");
        assert_eq!(
            parse_request(&raw),
            Ok((7, Request::RunStart(Box::new(feature()))))
        );

        let rounds = raw.replace(
            r#""profile":"work""#,
            r#""profile":"work","max_review_rounds":5"#,
        );
        let Ok((_, Request::RunStart(workflow))) = parse_request(&rounds) else {
            panic!("a start");
        };
        let DevWorkflow::FeatureFromTicket(started) = *workflow;
        assert_eq!(started.max_review_rounds.get().get(), 5);
    }

    #[test]
    fn a_start_of_another_template_or_out_of_range_rounds_is_invalid() {
        let start = run_start_params(&feature());
        for (field, value) in [
            ("template", json!("deploy")),
            ("max_review_rounds", json!(21)),
            ("profile", json!("Not A Profile")),
        ] {
            let mut params = start.clone();
            params[field] = value;
            let raw = json!({ "id": 8, "method": "run.start", "params": params }).to_string();
            let error = parse_request(&raw).expect_err(field);
            assert_eq!(
                (error.id, error.error.code()),
                (Some(8), "invalid"),
                "{field}"
            );
            assert!(
                error.error.to_string().contains(field),
                "{field}: {}",
                error.error
            );
        }
    }

    #[test]
    fn the_daemon_reads_the_start_a_client_writes() {
        let raw = json!({ "id": 1, "method": "run.start", "params": run_start_params(&feature()) })
            .to_string();
        assert_eq!(
            parse_request(&raw),
            Ok((1, Request::RunStart(Box::new(feature()))))
        );
        let run = RunId::parse("01JB80").expect("run");
        assert_eq!(run_started_result(&run), json!({ "run": "01JB80" }));
        assert_eq!(read_run_started(&run_started_result(&run)), Ok(run));
    }

    #[test]
    fn a_run_is_dismissed_with_or_without_its_worktree() {
        assert_eq!(
            parse_request(
                r#"{"id":9,"method":"run.dismiss","params":{"run":"r1","remove_worktree":true}}"#
            ),
            Ok((
                9,
                Request::RunDismiss {
                    run: RunId::parse("r1").expect("run"),
                    remove_worktree: true,
                }
            ))
        );
        let bad = parse_request(r#"{"id":10,"method":"run.dismiss","params":{"run":"r1"}}"#)
            .expect_err("remove_worktree is required");
        assert_eq!((bad.id, bad.error.code()), (Some(10), "invalid"));
    }

    #[test]
    fn a_gate_answer_says_whether_it_was_sent() {
        assert_eq!(gate_result(GateReply::Sent), json!({ "sent": true }));
        assert_eq!(
            gate_result(GateReply::Queued),
            json!({ "sent": false, "queued": true })
        );
        assert_eq!(
            gate_result(GateReply::NotOpen),
            json!({ "sent": false, "queued": false })
        );
    }

    #[test]
    fn a_detail_event_carries_the_item_its_rows_and_its_notes() {
        let detail = RunDetail {
            item: item(),
            rows: vec![
                DetailRow {
                    row: LaneRow {
                        indent: 0,
                        node: NodeId::parse("intake").expect("node"),
                        lane: Lane::Alone,
                        kind: RowKind::Loop {
                            reached: 1,
                            selected: NonZeroU32::MIN,
                            bound: NonZeroU32::new(3).expect("three"),
                        },
                        state: None,
                    },
                    mark: Mark::NeedsYou,
                },
                DetailRow {
                    row: LaneRow {
                        indent: 1,
                        node: NodeId::parse("preflight").expect("node"),
                        lane: Lane::Alone,
                        kind: RowKind::Node,
                        state: Some(InstanceState {
                            instance: NodeInstance::parse("preflight@1").expect("instance"),
                            status: InstanceStatus::Done {
                                branch: Some(BranchLabel::parse("needs-info").expect("label")),
                            },
                            attempt: Some(NonZeroU32::MIN),
                            first_ready_at: Some(datetime!(2026-10-25 09:00 UTC)),
                            finished_at: Some(datetime!(2026-10-25 09:04 UTC)),
                        }),
                    },
                    mark: Mark::Done,
                },
            ],
            notes: RunNotes {
                questions: vec![Line::parse("Page size?").expect("question")],
                findings: None,
            },
        };
        let event = detail_event(&detail);
        assert_eq!(event["type"], json!("graph"));
        assert_eq!(event["run"]["ticket"], json!("ENG-412"));
        assert_eq!(
            event["rows"][0],
            json!({ "indent": 0, "node": "intake", "lane": "alone", "mark": "needs-you",
                    "glyph": "?", "loop": { "reached": 1, "selected": 1, "bound": 3 } })
        );
        assert_eq!(
            event["rows"][1],
            json!({ "indent": 1, "node": "preflight", "lane": "alone", "mark": "done",
                    "glyph": "✓", "instance": "preflight@1", "status": "done",
                    "branch": "needs-info", "attempt": 1,
                    "first_ready_at": "2026-10-25T09:00:00Z",
                    "finished_at": "2026-10-25T09:04:00Z" })
        );
        assert_eq!(event["questions"], json!(["Page size?"]));
        assert_eq!(event.get("findings"), None);
    }

    #[test]
    fn an_unknown_method_or_bad_params_are_invalid_with_the_id() {
        for raw in [
            r#"{"id":4,"method":"run.teleport","params":{}}"#,
            r#"{"id":4,"method":"hello","params":{"client":"x"}}"#,
        ] {
            let error = parse_request(raw).expect_err("invalid");
            assert_eq!(error.id, Some(4), "{raw}");
            assert_eq!(error.error.code(), "invalid", "{raw}");
        }
    }

    #[test]
    fn a_line_that_is_not_a_request_is_invalid_without_an_id() {
        let error = parse_request("not json").expect_err("invalid");
        assert_eq!(error.id, None);
        assert_eq!(error.error.code(), "invalid");
    }

    #[test]
    fn replies_errors_and_events_are_one_line_each() {
        assert_eq!(
            line(&Outbound::Reply {
                id: 7,
                result: stream_opened(9)
            }),
            json!({ "id": 7, "result": { "stream": 9 } })
        );
        assert_eq!(
            line(&Outbound::Error {
                id: Some(7),
                error: ApiError::Conflict("ENG-412 already has a run".to_owned()),
            }),
            json!({ "id": 7, "error": { "code": "conflict", "message": "conflict: ENG-412 already has a run" } })
        );
        assert_eq!(
            line(&Outbound::Error {
                id: None,
                error: ApiError::Invalid("not a frame".to_owned()),
            })["id"],
            Value::Null
        );
        assert_eq!(
            line(&Outbound::Event {
                stream: 9,
                event: json!({ "type": "removed", "run": "r1" })
            }),
            json!({ "stream": 9, "event": { "type": "removed", "run": "r1" } })
        );
    }

    #[test]
    fn hello_names_daemon_protocol_and_worker() {
        assert_eq!(
            hello_result(&Hello {
                daemon: "vetinarid 0.1.0".to_owned(),
                protocol: 1,
                worker: WorkerName::parse("hex").expect("worker"),
            }),
            json!({ "daemon": "vetinarid 0.1.0", "protocol": 1, "worker": "hex" })
        );
    }

    #[test]
    fn a_run_item_carries_its_strip_and_what_needs_you() {
        let expected = json!({
            "run": "01JB7Z",
            "ticket": "ENG-412",
            "title": "Rate-limit retries",
            "status": "active",
            "phases": [
                { "phase": "pre", "mark": "done", "glyph": "✓" },
                { "phase": "impl", "mark": "needs-you", "glyph": "?",
                  "round": { "iteration": 2, "bound": 3 } }
            ],
            "needs_you": { "kind": "gate", "instance": "ask-info@1", "gate": "info-needed" },
            "started_at": "2026-10-25T09:00:00Z"
        });
        let mut run = expected.clone();
        run["type"] = json!("run");
        assert_eq!(change_event(&BoardChange::Run(item())), run);
        assert_eq!(
            snapshot_event(&[item()]),
            json!({ "type": "snapshot", "runs": [expected] })
        );
    }

    #[test]
    fn a_run_without_a_label_or_start_leaves_them_out() {
        let mut item = item();
        item.label = None;
        item.started_at = None;
        item.summary.needs_you = Some(NeedsYou::Question {
            instance: NodeInstance::parse("implement@2").expect("instance"),
            question: "Which client?".to_owned(),
        });
        let event = change_event(&BoardChange::Run(item));
        assert_eq!(event.get("ticket"), None);
        assert_eq!(event.get("started_at"), None);
        assert_eq!(
            event["needs_you"],
            json!({ "kind": "question", "instance": "implement@2", "question": "Which client?" })
        );
        assert_eq!(
            change_event(&BoardChange::Removed(RunId::parse("r1").expect("run"))),
            json!({ "type": "removed", "run": "r1" })
        );
    }

    #[test]
    fn a_client_reads_back_the_run_items_the_daemon_writes() {
        let mut question = item();
        question.label = None;
        question.started_at = None;
        question.summary.needs_you = Some(NeedsYou::Question {
            instance: NodeInstance::parse("implement@2").expect("instance"),
            question: "Which client?".to_owned(),
        });
        for item in [item(), question] {
            let event = change_event(&BoardChange::Run(item.clone()));
            assert_eq!(read_run_item(&event), Ok(item));
        }
        assert!(read_run_item(&json!({ "run": "r1" })).is_err());
    }

    #[test]
    fn a_client_reads_back_a_runs_detail() {
        let claimed = |instance: &str| InstanceState {
            instance: NodeInstance::parse(instance).expect("instance"),
            status: InstanceStatus::Claimed {
                worker: "hex".to_owned(),
                expires_at: datetime!(2026-10-25 10:30 UTC),
            },
            attempt: Some(NonZeroU32::MIN),
            first_ready_at: Some(datetime!(2026-10-25 10:20 UTC)),
            finished_at: None,
        };
        let detail = RunDetail {
            item: item(),
            rows: vec![
                DetailRow {
                    row: LaneRow {
                        indent: 0,
                        node: NodeId::parse("rework").expect("node"),
                        lane: Lane::Alone,
                        kind: RowKind::Loop {
                            reached: 2,
                            selected: NonZeroU32::new(2).expect("two"),
                            bound: NonZeroU32::new(3).expect("three"),
                        },
                        state: Some(InstanceState {
                            instance: NodeInstance::parse("rework").expect("instance"),
                            status: InstanceStatus::Looping {
                                iteration: NonZeroU32::new(2).expect("two"),
                            },
                            attempt: None,
                            first_ready_at: None,
                            finished_at: None,
                        }),
                    },
                    mark: Mark::Elsewhere,
                },
                DetailRow {
                    row: LaneRow {
                        indent: 1,
                        node: NodeId::parse("review").expect("node"),
                        lane: Lane::First,
                        kind: RowKind::Node,
                        state: Some(claimed("review@2")),
                    },
                    mark: Mark::RunningHere,
                },
                DetailRow {
                    row: LaneRow {
                        indent: 0,
                        node: NodeId::parse("open-pr").expect("node"),
                        lane: Lane::Alone,
                        kind: RowKind::Node,
                        state: None,
                    },
                    mark: Mark::NotReached,
                },
            ],
            notes: RunNotes {
                questions: Vec::new(),
                findings: Some(ReviewFindings {
                    round: NonZeroU32::MIN,
                    findings: vec![Finding {
                        file: Some(Line::parse("src/retry.rs").expect("file")),
                        line: NonZeroU32::new(88),
                        text: Line::parse("Retry-After ignored on 503").expect("text"),
                    }],
                }),
            },
        };
        assert_eq!(read_detail(&detail_event(&detail)), Ok(detail));
    }

    #[test]
    fn sign_in_is_asked_about_and_begun_by_name() {
        assert_eq!(
            parse_request(r#"{"id":7,"method":"auth.status"}"#),
            Ok((7, Request::AuthStatus))
        );
        assert_eq!(
            parse_request(r#"{"id":8,"method":"auth.begin","params":{}}"#),
            Ok((8, Request::AuthBegin))
        );
    }

    #[test]
    fn a_client_reads_back_the_sign_in_state_and_challenge() {
        use vetinari_core::application::ports::DeviceCodeChallenge;

        for state in [
            AuthState::Missing,
            AuthState::Valid,
            AuthState::NearingExpiry,
            AuthState::Expired,
            AuthState::Corrupt,
        ] {
            assert_eq!(read_auth_status(&auth_status_result(state)), Ok(state));
        }
        let challenge = DeviceCodeChallenge {
            user_code: "BDWP-HQPK".to_owned(),
            verification_uri: "https://id.example.net/device".to_owned(),
            device_code: "held by the daemon".to_owned(),
            interval_secs: 5,
            expires_at: datetime!(2026-10-25 09:10 UTC),
        };
        let event = challenge_event(&challenge);
        assert_eq!(event["type"], "challenge");
        assert!(
            event.get("device_code").is_none(),
            "the device code stays in the daemon"
        );
        let read = read_challenge(&event).expect("challenge");
        assert_eq!(read.user_code, "BDWP-HQPK");
        assert_eq!(read.verification_uri, "https://id.example.net/device");
        assert_eq!(read.interval_secs, 5);
        assert_eq!(read.expires_at, datetime!(2026-10-25 09:10 UTC));
    }

    #[test]
    fn a_board_says_when_the_daemon_needs_a_sign_in() {
        assert_eq!(
            auth_event(true),
            json!({ "type": "auth", "needs_sign_in": true })
        );
        assert_eq!(sign_in_ended_event(None), json!({ "type": "granted" }));
        assert_eq!(
            sign_in_ended_event(Some("declined")),
            json!({ "type": "rejected", "reason": "declined" })
        );
    }
}
