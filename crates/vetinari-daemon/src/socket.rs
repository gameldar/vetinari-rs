//! The daemon's inbound adapter: a Unix socket speaking the frames of
//! `vetinari_adapters::daemon::protocol` (design 10.1).
//!
//! The filesystem is the authentication: the socket's directory is the
//! developer's alone, and so is everything that can connect.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use vetinari_adapters::daemon::protocol::{
    MAX_FRAME, Outbound, Request, change_event, detail_event, gate_result, hello_result,
    parse_request, snapshot_event, stream_opened,
};
use vetinari_core::application::devflow::api::{
    ApiError, BoardChange, ClientApi, RunDetail, RunItem,
};
use vetinari_core::application::devflow::ports::RunGateway;
use vetinari_core::domain::devflow::ids::RunId;
use vetinari_core::domain::devflow::lanes::IterationSelection;
use vetinari_core::domain::devflow::template::TemplateKind;

use crate::gates::Gates;
use crate::hub::Hub;
use crate::steps::LocalBoard;

/// Bind the socket at `path`, in a directory only this user can enter. A
/// socket file left by a daemon that is no longer running is replaced; one
/// a running daemon answers on is not.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a daemon is already listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// What the socket's methods beyond the board reach.
pub struct Backend {
    /// The run endpoints, for outputs and cancelling.
    pub runs: Arc<dyn RunGateway>,
    /// Who answers gates.
    pub gates: Arc<dyn Gates>,
    /// This machine's steps, for a run's detail.
    pub local: Arc<LocalBoard>,
    /// The template every run is of.
    pub template: TemplateKind,
}

/// Serve every connection until the listener fails.
pub async fn serve(
    listener: UnixListener,
    api: Arc<ClientApi>,
    hub: Arc<Hub>,
    backend: Arc<Backend>,
) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(connection(
            stream,
            api.clone(),
            hub.clone(),
            backend.clone(),
        ));
    }
}

/// How many frames may wait for a slow client before its streams wait too.
const OUTBOX: usize = 64;

/// One client, until it hangs up or sends a frame too large to read.
async fn connection(stream: UnixStream, api: Arc<ClientApi>, hub: Arc<Hub>, backend: Arc<Backend>) {
    let (reader, mut writer) = stream.into_split();
    let (outbox, mut outgoing) = mpsc::channel::<Outbound>(OUTBOX);
    let writing = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if writer.write_all(frame.to_line().as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(reader);
    let mut streams: BTreeMap<u64, JoinHandle<()>> = BTreeMap::new();
    let mut next_stream = 1;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let limit = u64::try_from(MAX_FRAME).unwrap_or(u64::MAX) + 1;
        match (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut buffer)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        } else if buffer.len() > MAX_FRAME {
            let refused = Outbound::Error {
                id: None,
                error: ApiError::Invalid(format!("a frame is at most {MAX_FRAME} bytes")),
            };
            let _ = outbox.send(refused).await;
            break;
        }
        let line = String::from_utf8_lossy(&buffer);
        if line.trim().is_empty() {
            continue;
        }
        let reply = match parse_request(&line) {
            Err(bad) => Outbound::Error {
                id: bad.id,
                error: bad.error,
            },
            Ok((id, Request::Hello { client, protocol })) => match api.hello(&client, protocol) {
                Ok(hello) => Outbound::Reply {
                    id,
                    result: hello_result(&hello),
                },
                Err(error) => Outbound::Error {
                    id: Some(id),
                    error,
                },
            },
            Ok((id, Request::RunsWatch)) => {
                let stream = next_stream;
                next_stream += 1;
                let (snapshot, changes) = hub.watch();
                let opened = Outbound::Reply {
                    id,
                    result: stream_opened(stream),
                };
                if outbox.send(opened).await.is_err() {
                    break;
                }
                let forwarding = forward(stream, snapshot, changes, hub.clone(), outbox.clone());
                streams.insert(stream, tokio::spawn(forwarding));
                continue;
            }
            Ok((id, Request::RunWatch { run, selection })) => {
                if hub.run(&run).is_none() {
                    Outbound::Error {
                        id: Some(id),
                        error: ApiError::NotFound(format!("run {run}")),
                    }
                } else {
                    let stream = next_stream;
                    next_stream += 1;
                    let touches = hub.touches();
                    let opened = Outbound::Reply {
                        id,
                        result: stream_opened(stream),
                    };
                    if outbox.send(opened).await.is_err() {
                        break;
                    }
                    let watching = Watching {
                        stream,
                        run,
                        selection,
                        hub: hub.clone(),
                        backend: backend.clone(),
                        outbox: outbox.clone(),
                    };
                    streams.insert(stream, tokio::spawn(watching.forward(touches)));
                    continue;
                }
            }
            Ok((
                id,
                Request::GateAnswer {
                    run,
                    instance,
                    answer,
                },
            )) => match backend.gates.answer(&run, &instance, answer).await {
                Ok(reply) => Outbound::Reply {
                    id,
                    result: gate_result(reply),
                },
                Err(error) => Outbound::Error {
                    id: Some(id),
                    error,
                },
            },
            Ok((id, Request::RunCancel { run })) => match backend.runs.cancel(&run).await {
                Ok(_) => Outbound::Reply {
                    id,
                    result: serde_json::json!({}),
                },
                Err(error) => Outbound::Error {
                    id: Some(id),
                    error: error.into(),
                },
            },
            Ok((id, Request::StreamClose { stream })) => match streams.remove(&stream) {
                Some(forwarding) => {
                    forwarding.abort();
                    Outbound::Reply {
                        id,
                        result: serde_json::json!({}),
                    }
                }
                None => Outbound::Error {
                    id: Some(id),
                    error: ApiError::NotFound(format!("stream {stream} is not open")),
                },
            },
        };
        if outbox.send(reply).await.is_err() {
            break;
        }
    }
    for forwarding in streams.values() {
        forwarding.abort();
    }
    drop(outbox);
    let _ = writing.await;
}

/// A `run.watch` stream: the run's detail now, and again whenever its item
/// or graph changes.
struct Watching {
    stream: u64,
    run: RunId,
    selection: IterationSelection,
    hub: Arc<Hub>,
    backend: Arc<Backend>,
    outbox: mpsc::Sender<Outbound>,
}

impl Watching {
    async fn forward(self, mut touches: broadcast::Receiver<RunId>) {
        loop {
            if let Some(detail) = self.detail().await {
                let event = Outbound::Event {
                    stream: self.stream,
                    event: detail_event(&detail),
                };
                if self.outbox.send(event).await.is_err() {
                    return;
                }
            }
            loop {
                match touches.recv().await {
                    Ok(run) if run == self.run => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }

    async fn detail(&self) -> Option<RunDetail> {
        let (item, graph) = self.hub.run(&self.run)?;
        let outputs = match self.backend.runs.outputs(&self.run).await {
            Ok(outputs) => outputs,
            Err(error) => {
                tracing::warn!(%error, run = %self.run, "cannot read a run's outputs");
                BTreeMap::new()
            }
        };
        Some(RunDetail::of(
            item,
            &graph,
            self.backend.template,
            &self.selection,
            &self.backend.local.get(&self.run),
            &outputs,
        ))
    }
}

/// A `runs.watch` stream: the snapshot, then each change. A watcher too slow
/// to keep up is sent a fresh snapshot rather than a gap.
async fn forward(
    stream: u64,
    snapshot: Vec<RunItem>,
    mut changes: broadcast::Receiver<BoardChange>,
    hub: Arc<Hub>,
    outbox: mpsc::Sender<Outbound>,
) {
    let mut event = snapshot_event(&snapshot);
    loop {
        if outbox
            .send(Outbound::Event { stream, event })
            .await
            .is_err()
        {
            return;
        }
        event = match changes.recv().await {
            Ok(change) => change_event(&change),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let (snapshot, fresh) = hub.watch();
                changes = fresh;
                snapshot_event(&snapshot)
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    use async_trait::async_trait;
    use vetinari_core::application::devflow::api::RunLabel;
    use vetinari_core::application::devflow::worker::GateReply;
    use vetinari_core::domain::devflow::ids::{NodeId, NodeInstance, TaskQueue, WorkerName};
    use vetinari_core::domain::devflow::outcome::GateAnswer;
    use vetinari_core::domain::devflow::run_view::{
        InstanceState, InstanceStatus, RunGraph, RunNode, RunNodeKind, RunStatus,
    };
    use vetinari_core::domain::devflow::summary::RunSummary;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::testing::FakeRunGateway;

    use super::*;

    struct Server {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        hub: Arc<Hub>,
        runs: Arc<FakeRunGateway>,
        gates: Arc<RecordingGates>,
    }

    #[derive(Default)]
    struct RecordingGates(Mutex<Vec<(String, String, GateAnswer)>>);

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

    async fn server() -> Server {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vetinari").join("vetinarid.sock");
        let listener = bind(&path).expect("bind");
        let hub = Arc::new(Hub::default());
        let api = Arc::new(ClientApi::new(
            "vetinarid test",
            WorkerName::parse("hex").expect("worker"),
        ));
        let runs = Arc::new(FakeRunGateway::default());
        let gates = Arc::new(RecordingGates::default());
        let backend = Arc::new(Backend {
            runs: runs.clone(),
            gates: gates.clone(),
            local: LocalBoard::new().0,
            template: TemplateKind::FeatureFromTicket,
        });
        tokio::spawn(serve(listener, api, hub.clone(), backend));
        Server {
            _dir: dir,
            path,
            hub,
            runs,
            gates,
        }
    }

    fn graph(run: &str, status: InstanceStatus) -> RunGraph {
        RunGraph {
            run: RunId::parse(run).expect("run"),
            status: RunStatus::Active,
            seq: 3,
            nodes: vec![RunNode {
                id: NodeId::parse("open-pr").expect("id"),
                kind: RunNodeKind::External {
                    queue: TaskQueue::parse("vetinari-hex-agent").expect("queue"),
                },
                scope: None,
            }],
            edges: Vec::new(),
            instances: vec![InstanceState {
                instance: NodeInstance::parse("open-pr").expect("instance"),
                status,
                attempt: None,
                first_ready_at: None,
                finished_at: None,
            }],
        }
    }

    #[tokio::test]
    async fn a_watched_run_streams_its_detail_on_every_change() {
        let server = server().await;
        server.hub.put(item("r1"));
        server.hub.put_graph(graph("r1", InstanceStatus::Running));
        let mut client = Client::connect(&server).await;
        client
            .send(r#"{"id":1,"method":"run.watch","params":{"run":"r1"}}"#)
            .await;
        let stream = client.frame().await["result"]["stream"].clone();
        let first = client.frame().await;
        assert_eq!(first["stream"], stream);
        assert_eq!(first["event"]["type"], json!("graph"));
        assert_eq!(first["event"]["rows"][0]["status"], json!("running"));

        server
            .hub
            .put_graph(graph("r1", InstanceStatus::Done { branch: None }));
        let second = client.frame().await;
        assert_eq!(second["event"]["rows"][0]["status"], json!("done"));
    }

    #[tokio::test]
    async fn watching_a_run_the_daemon_does_not_have_is_not_found() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        client
            .send(r#"{"id":2,"method":"run.watch","params":{"run":"r9"}}"#)
            .await;
        assert_eq!(client.frame().await["error"]["code"], json!("not-found"));
    }

    #[tokio::test]
    async fn a_gate_answer_goes_to_the_gates_and_a_cancel_to_the_gateway() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        client
            .send(r#"{"id":3,"method":"gate.answer","params":{"run":"r1","instance":"ask-info@1","answer":{"kind":"recheck"}}}"#)
            .await;
        assert_eq!(
            client.frame().await,
            json!({ "id": 3, "result": { "sent": true } })
        );
        assert_eq!(
            server.gates.0.lock().expect("lock").clone(),
            [(
                "r1".to_owned(),
                "ask-info@1".to_owned(),
                GateAnswer::Recheck { note: None }
            )]
        );

        client
            .send(r#"{"id":4,"method":"run.cancel","params":{"run":"r1"}}"#)
            .await;
        assert_eq!(client.frame().await, json!({ "id": 4, "result": {} }));
        assert_eq!(server.runs.cancelled(), ["r1"]);
    }

    struct Client {
        lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl Client {
        async fn connect(server: &Server) -> Self {
            let stream = UnixStream::connect(&server.path).await.expect("connect");
            let (reader, writer) = stream.into_split();
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn send(&mut self, frame: &str) {
            self.writer
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .expect("write");
        }

        async fn next(&mut self) -> Option<Value> {
            let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("a frame within five seconds")
                .expect("read")?;
            Some(serde_json::from_str(&line).expect("a JSON frame"))
        }

        async fn frame(&mut self) -> Value {
            self.next().await.expect("a frame")
        }

        async fn hello(&mut self, id: u64) -> Value {
            self.send(&format!(
                r#"{{"id":{id},"method":"hello","params":{{"client":"test","protocol":1}}}}"#
            ))
            .await;
            self.frame().await
        }

        async fn watch(&mut self, id: u64) -> u64 {
            self.send(&format!(
                r#"{{"id":{id},"method":"runs.watch","params":{{}}}}"#
            ))
            .await;
            self.frame().await["result"]["stream"]
                .as_u64()
                .expect("a stream id")
        }
    }

    fn item(run: &str) -> RunItem {
        RunItem {
            summary: RunSummary {
                run: RunId::parse(run).expect("run"),
                status: RunStatus::Active,
                phases: Vec::new(),
                needs_you: None,
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: None,
        }
    }

    #[tokio::test]
    async fn the_socket_directory_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let server = server().await;
        let mode = std::fs::metadata(server.path.parent().expect("dir"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn a_stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vetinarid.sock");
        drop(bind(&path).expect("first"));
        bind(&path).expect("a second daemon replaces the stale file");
    }

    #[tokio::test]
    async fn hello_is_answered_with_the_daemon_and_its_worker() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        assert_eq!(
            client.hello(1).await,
            json!({ "id": 1, "result": { "daemon": "vetinarid test", "protocol": 1, "worker": "hex" } })
        );
    }

    #[tokio::test]
    async fn a_client_on_another_protocol_is_refused() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        client
            .send(r#"{"id":2,"method":"hello","params":{"client":"test","protocol":9}}"#)
            .await;
        let reply = client.frame().await;
        assert_eq!(reply["id"], json!(2));
        assert_eq!(reply["error"]["code"], json!("invalid"));
    }

    #[tokio::test]
    async fn a_bad_line_is_answered_and_the_connection_kept() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        client.send("not json").await;
        let reply = client.frame().await;
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["error"]["code"], json!("invalid"));
        assert_eq!(client.hello(3).await["id"], json!(3));
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_and_the_connection_closed() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        let huge = "x".repeat(MAX_FRAME + 1);
        client.send(&huge).await;
        let reply = client.frame().await;
        assert_eq!(reply["error"]["code"], json!("invalid"));
        assert_eq!(client.next().await, None);
    }

    #[tokio::test]
    async fn a_watch_streams_a_snapshot_then_each_change() {
        let server = server().await;
        server.hub.put(item("r1"));
        let mut client = Client::connect(&server).await;
        let stream = client.watch(4).await;

        let snapshot = client.frame().await;
        assert_eq!(snapshot["stream"], json!(stream));
        assert_eq!(snapshot["event"]["type"], json!("snapshot"));
        assert_eq!(snapshot["event"]["runs"][0]["run"], json!("r1"));

        server.hub.put(item("r2"));
        let added = client.frame().await;
        assert_eq!(
            (
                added["event"]["type"].clone(),
                added["event"]["run"].clone()
            ),
            (json!("run"), json!("r2"))
        );
        server.hub.remove(&RunId::parse("r1").expect("run"));
        assert_eq!(
            client.frame().await["event"],
            json!({ "type": "removed", "run": "r1" })
        );
    }

    #[tokio::test]
    async fn a_closed_stream_sends_nothing_more() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        let stream = client.watch(5).await;
        client.frame().await;

        client
            .send(&format!(
                r#"{{"id":6,"method":"stream.close","params":{{"stream":{stream}}}}}"#
            ))
            .await;
        assert_eq!(client.frame().await, json!({ "id": 6, "result": {} }));
        server.hub.put(item("r3"));
        assert_eq!(client.hello(7).await["id"], json!(7));
    }

    #[tokio::test]
    async fn closing_a_stream_that_is_not_open_is_not_found() {
        let server = server().await;
        let mut client = Client::connect(&server).await;
        client
            .send(r#"{"id":8,"method":"stream.close","params":{"stream":41}}"#)
            .await;
        assert_eq!(client.frame().await["error"]["code"], json!("not-found"));
    }

    #[tokio::test]
    async fn two_clients_watch_at_once() {
        let server = server().await;
        let mut first = Client::connect(&server).await;
        let mut second = Client::connect(&server).await;
        first.watch(1).await;
        second.watch(1).await;
        first.frame().await;
        second.frame().await;

        server.hub.put(item("r4"));
        assert_eq!(first.frame().await["event"]["run"], json!("r4"));
        assert_eq!(second.frame().await["event"]["run"], json!("r4"));
    }
}
