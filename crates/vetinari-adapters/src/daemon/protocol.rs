//! The daemon's socket frames (design 10.2): newline-delimited JSON, one
//! frame per line.
//!
//! Both ends of the socket are adapters, so the frames live here, shared by
//! the server in `vetinari-daemon` and the TUI's client. Unknown fields are
//! ignored in both directions.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde::Deserialize;
use serde_json::{Value, json};
use vetinari_core::application::devflow::api::{
    ApiError, BoardChange, DetailRow, Hello, RunDetail, RunItem,
};
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::domain::devflow::ids::{NodeId, NodeInstance, RunId};
use vetinari_core::domain::devflow::lanes::{IterationSelection, Lane, RowKind};
use vetinari_core::domain::devflow::outcome::{GateAnswer, Line};
use vetinari_core::domain::devflow::run_view::InstanceStatus;
use vetinari_core::domain::devflow::run_view::RunStatus;
use vetinari_core::domain::devflow::step::StepKind;
use vetinari_core::domain::devflow::summary::{Mark, NeedsYou};

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
        other => return Err(invalid(format!("no method {other:?}"))),
    };
    Ok((frame.id, request))
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
            InstanceStatus::Claimed { worker, .. } => ("claimed", Some(("worker", json!(worker)))),
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use time::macros::datetime;
    use vetinari_core::application::devflow::api::RunLabel;
    use vetinari_core::domain::devflow::ids::WorkerName;
    use vetinari_core::domain::devflow::step::GateKind;
    use vetinari_core::domain::devflow::summary::{Phase, PhaseCell, Round, RunSummary};
    use vetinari_core::domain::text::NonEmptyText;

    use vetinari_core::domain::devflow::ids::BranchLabel;
    use vetinari_core::domain::devflow::lanes::LaneRow;
    use vetinari_core::domain::devflow::notes::RunNotes;
    use vetinari_core::domain::devflow::run_view::InstanceState;

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
}
