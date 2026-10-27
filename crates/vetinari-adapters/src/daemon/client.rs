//! The TUI's side of the daemon's socket (design 10.4): `DaemonRuns` keeps
//! a `runs.watch` stream open and serves the latest runs as queue items,
//! and opens a connection of its own for each popup and each request.
//!
//! The TUI never starts the daemon. While none answers, `fetch` is
//! `Unavailable`, which the queue shows as a source problem and treats as
//! "keep what it reported".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use vetinari_core::application::auth::AuthState;
use vetinari_core::application::devflow::api::{PROTOCOL, RunDetail, RunItem};
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::application::ports::DeviceCodeChallenge;
use vetinari_core::application::ports::{AttentionSource, PortError, PortResult};
use vetinari_core::domain::attention::{AttentionItem, ItemContext, ItemState, SourceCategory};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::devflow::ids::{NodeInstance, RunId};
use vetinari_core::domain::devflow::lanes::IterationSelection;
use vetinari_core::domain::devflow::outcome::GateAnswer;
use vetinari_core::domain::devflow::template::DevWorkflow;
use vetinari_core::domain::ids::{Slug, SourceId};
use vetinari_core::domain::text::NonEmptyText;

use super::protocol::{
    read_auth_status, read_challenge, read_detail, read_run_item, read_run_started,
    run_start_params,
};

const WHAT: &str = "vetinarid";

/// `$XDG_RUNTIME_DIR/vetinari/vetinarid.sock`, or under the temporary
/// directory when there is no runtime directory: where the daemon listens
/// unless told otherwise.
#[must_use]
pub fn default_socket() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) if !runtime.is_empty() => Path::new(&runtime).join("vetinari"),
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
            std::env::temp_dir().join(format!("vetinari-{user}"))
        }
    };
    dir.join("vetinarid.sock")
}

/// What the watch has seen: nothing yet, every run, or why the picture
/// it had is gone. A daemon never reached queues nothing rather than an
/// error, so someone who does not run vetinarid sees no noise.
enum Board {
    Never,
    Down(String),
    Up(Seen),
}

/// The daemon's runs, and whether it waits on a sign-in to reach them.
#[derive(Clone, Default)]
struct Seen {
    runs: BTreeMap<RunId, RunItem>,
    needs_sign_in: bool,
}

/// The daemon's runs, as an attention source.
pub struct DaemonRuns {
    id: SourceId,
    socket: PathBuf,
    client: String,
    board: Mutex<Board>,
    changed: watch::Sender<u64>,
}

/// The longest wait between attempts to reach the daemon.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

impl DaemonRuns {
    /// The source id runs are queued under.
    pub const SOURCE_NAME: &'static str = "vetinarid";

    /// A source reading the daemon at `socket`, introducing itself as
    /// `client`. Nothing is read until `start`.
    #[must_use]
    pub fn new(socket: PathBuf, client: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: SourceId::parse(Self::SOURCE_NAME).expect("built-in source name is valid"),
            socket,
            client: client.into(),
            board: Mutex::new(Board::Never),
            changed: watch::channel(0).0,
        })
    }

    /// Keep the watch open for as long as the task runs, reconnecting with
    /// backoff when the daemon goes away.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let reason = match this.watch_board().await {
                    Ok(()) => "the daemon closed the stream".to_owned(),
                    Err(error) => error.to_string(),
                };
                let was = match &*this.board.lock().expect("board lock") {
                    Board::Never => Board::Never,
                    Board::Up(_) => {
                        backoff = Duration::from_secs(1);
                        Board::Down(reason)
                    }
                    Board::Down(_) => Board::Down(reason),
                };
                this.set(was);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        })
    }

    /// Ticks whenever the runs change, so a client can redraw at once
    /// rather than at its next poll.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Open a run's popup: its detail now and on every change.
    pub async fn watch_run(
        &self,
        run: &RunId,
        selection: &IterationSelection,
    ) -> PortResult<RunWatch> {
        let mut connection = Connection::open(&self.socket, &self.client).await?;
        let iterations: BTreeMap<String, u32> = selection
            .iter()
            .map(|(node, iteration)| (node.as_str().to_owned(), iteration.get()))
            .collect();
        connection
            .request(
                "run.watch",
                json!({ "run": run.as_str(), "iterations": iterations }),
            )
            .await?;
        Ok(RunWatch { connection })
    }

    /// Answer a gate the daemon holds.
    pub async fn answer_gate(
        &self,
        run: &RunId,
        instance: &NodeInstance,
        answer: &GateAnswer,
    ) -> PortResult<GateReply> {
        let answer = match answer {
            GateAnswer::Recheck { note } => json!({
                "kind": "recheck",
                "note": note.as_ref().map(|note| note.as_str()),
            }),
            GateAnswer::OpenAnyway => json!({ "kind": "open-anyway" }),
        };
        let result = Connection::open(&self.socket, &self.client)
            .await?
            .request(
                "gate.answer",
                json!({ "run": run.as_str(), "instance": instance.to_string(), "answer": answer }),
            )
            .await?;
        Ok(
            match (
                result["sent"].as_bool().unwrap_or(false),
                result["queued"].as_bool().unwrap_or(false),
            ) {
                (true, _) => GateReply::Sent,
                (false, true) => GateReply::Queued,
                (false, false) => GateReply::NotOpen,
            },
        )
    }

    /// Cancel a run.
    pub async fn cancel_run(&self, run: &RunId) -> PortResult<()> {
        Connection::open(&self.socket, &self.client)
            .await?
            .request("run.cancel", json!({ "run": run.as_str() }))
            .await?;
        Ok(())
    }

    /// Start a run; the daemon refuses a ticket that already has one.
    pub async fn start_run(&self, workflow: &DevWorkflow) -> PortResult<RunId> {
        let result = Connection::open(&self.socket, &self.client)
            .await?
            .request("run.start", run_start_params(workflow))
            .await?;
        read_run_started(&result).map_err(protocol)
    }

    /// Take a finished run off the queue, and its worktree when asked.
    pub async fn dismiss_run(&self, run: &RunId, remove_worktree: bool) -> PortResult<()> {
        Connection::open(&self.socket, &self.client)
            .await?
            .request(
                "run.dismiss",
                json!({ "run": run.as_str(), "remove_worktree": remove_worktree }),
            )
            .await?;
        Ok(())
    }

    /// Begin signing the daemon in: the challenge to show. The daemon polls
    /// the issuer itself; ask `sign_in_state` to learn whether it was
    /// granted.
    pub async fn begin_sign_in(&self) -> PortResult<DeviceCodeChallenge> {
        let mut connection = Connection::open(&self.socket, &self.client).await?;
        connection.request("auth.begin", json!({})).await?;
        let event = connection
            .event()
            .await?
            .ok_or_else(|| PortError::unavailable(WHAT, "the daemon hung up"))?;
        read_challenge(&event).map_err(protocol)
    }

    /// Whether the daemon is signed in to its gateway.
    pub async fn sign_in_state(&self) -> PortResult<AuthState> {
        let result = Connection::open(&self.socket, &self.client)
            .await?
            .request("auth.status", json!({}))
            .await?;
        read_auth_status(&result).map_err(protocol)
    }

    fn sign_in_item(&self, now: OffsetDateTime) -> AttentionItem {
        AttentionItem::builder(
            &self.id,
            "sign-in",
            SourceCategory::AgentState,
            NonEmptyText::parse("autognomus needs sign-in").expect("a non-empty title"),
        )
        .state(ItemState::NeedsDecision)
        .occurred_at(now)
        .context(ItemContext::SignIn(Provider::Autognomus))
        .build()
    }

    async fn watch_board(&self) -> PortResult<()> {
        let mut connection = Connection::open(&self.socket, &self.client).await?;
        connection.request("runs.watch", json!({})).await?;
        while let Some(event) = connection.event().await? {
            let mut seen = match &*self.board.lock().expect("board lock") {
                Board::Up(seen) => seen.clone(),
                Board::Never | Board::Down(_) => Seen::default(),
            };
            let board = &mut seen.runs;
            match event["type"].as_str() {
                Some("auth") => {
                    seen.needs_sign_in = event["needs_sign_in"].as_bool().unwrap_or(false);
                }
                Some("snapshot") => {
                    board.clear();
                    for entry in event["runs"].as_array().into_iter().flatten() {
                        let item = read_run_item(entry).map_err(protocol)?;
                        board.insert(item.summary.run.clone(), item);
                    }
                }
                Some("run") => {
                    let item = read_run_item(&event).map_err(protocol)?;
                    board.insert(item.summary.run.clone(), item);
                }
                Some("removed") => {
                    if let Some(run) = event["run"].as_str().and_then(|run| RunId::parse(run).ok())
                    {
                        board.remove(&run);
                    }
                }
                _ => continue,
            }
            self.set(Board::Up(seen));
        }
        Ok(())
    }

    fn set(&self, board: Board) {
        *self.board.lock().expect("board lock") = board;
        self.changed.send_modify(|count| *count += 1);
    }
}

fn protocol(detail: String) -> PortError {
    PortError::protocol(WHAT, detail)
}

#[async_trait]
impl AttentionSource for DaemonRuns {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        SourceCategory::AgentState
    }

    async fn fetch(&self, _account: Option<&Slug>) -> PortResult<Vec<AttentionItem>> {
        let now = OffsetDateTime::now_utc();
        match &*self.board.lock().expect("board lock") {
            Board::Up(seen) => {
                let mut items: Vec<AttentionItem> = seen
                    .runs
                    .values()
                    .map(|item| item.to_attention(&self.id, now))
                    .collect();
                if seen.needs_sign_in {
                    items.push(self.sign_in_item(now));
                }
                Ok(items)
            }
            Board::Never => Ok(Vec::new()),
            Board::Down(reason) => Err(PortError::unavailable(WHAT, reason.clone())),
        }
    }
}

/// An open popup's stream of a run's detail.
pub struct RunWatch {
    connection: Connection,
}

impl RunWatch {
    /// The next detail; `None` once the daemon closes the stream.
    pub async fn next(&mut self) -> PortResult<Option<RunDetail>> {
        loop {
            let Some(event) = self.connection.event().await? else {
                return Ok(None);
            };
            if event["type"] == "graph" {
                return read_detail(&event).map(Some).map_err(protocol);
            }
        }
    }
}

/// One connection to the daemon, greeted.
struct Connection {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Connection {
    async fn open(socket: &Path, client: &str) -> PortResult<Self> {
        let stream = UnixStream::connect(socket).await.map_err(|error| {
            PortError::unavailable(WHAT, format!("no daemon at {}: {error}", socket.display()))
        })?;
        let (reader, writer) = stream.into_split();
        let mut connection = Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 1,
        };
        connection
            .request("hello", json!({ "client": client, "protocol": PROTOCOL }))
            .await?;
        Ok(connection)
    }

    /// Send a request and read until its reply.
    async fn request(&mut self, method: &str, params: Value) -> PortResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({ "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|error| PortError::unavailable(WHAT, error.to_string()))?;
        loop {
            let frame = self
                .frame()
                .await?
                .ok_or_else(|| PortError::unavailable(WHAT, "the daemon hung up"))?;
            if frame["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(daemon_error(error));
            }
            return Ok(frame["result"].clone());
        }
    }

    /// The next stream event.
    async fn event(&mut self) -> PortResult<Option<Value>> {
        loop {
            let Some(frame) = self.frame().await? else {
                return Ok(None);
            };
            if frame.get("stream").is_some() {
                return Ok(Some(frame["event"].clone()));
            }
        }
    }

    async fn frame(&mut self) -> PortResult<Option<Value>> {
        let line = self
            .lines
            .next_line()
            .await
            .map_err(|error| PortError::unavailable(WHAT, error.to_string()))?;
        line.map(|line| serde_json::from_str(&line).map_err(|error| protocol(error.to_string())))
            .transpose()
    }
}

fn daemon_error(error: &Value) -> PortError {
    let message = error["message"].as_str().unwrap_or("no message").to_owned();
    match error["code"].as_str() {
        Some("not-found") => PortError::not_found(WHAT, message),
        Some("conflict") => PortError::Refused {
            what: WHAT.to_owned(),
            detail: message,
        },
        Some("unauthenticated") => PortError::Unauthenticated {
            provider: Provider::Autognomus,
        },
        Some("unavailable") => PortError::unavailable(WHAT, message),
        _ => protocol(message),
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;
    use vetinari_core::domain::devflow::outcome::Line;

    use super::*;

    /// A daemon played by the test: each connection is handed over to be
    /// answered by hand.
    struct FakeDaemon {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        connections: mpsc::UnboundedReceiver<Peer>,
    }

    struct Peer {
        lines: Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl Peer {
        async fn read(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("a request within five seconds")
                .expect("read")
                .expect("a line");
            serde_json::from_str(&line).expect("JSON")
        }

        async fn send(&mut self, frame: Value) {
            let mut line = frame.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.expect("write");
        }

        /// Read a request, check its method and answer it.
        async fn answer(&mut self, method: &str, result: Value) -> Value {
            let request = self.read().await;
            assert_eq!(request["method"], json!(method));
            self.send(json!({ "id": request["id"], "result": result }))
                .await;
            request
        }

        async fn greet(&mut self) {
            self.answer(
                "hello",
                json!({ "daemon": "vetinarid test", "protocol": 1, "worker": "hex" }),
            )
            .await;
        }
    }

    impl FakeDaemon {
        fn start() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket = dir.path().join("vetinarid.sock");
            let listener = UnixListener::bind(&socket).expect("bind");
            let (sender, connections) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (reader, writer) = stream.into_split();
                    let _ = sender.send(Peer {
                        lines: BufReader::new(reader).lines(),
                        writer,
                    });
                }
            });
            Self {
                _dir: dir,
                socket,
                connections,
            }
        }

        async fn accept(&mut self) -> Peer {
            tokio::time::timeout(Duration::from_secs(5), self.connections.recv())
                .await
                .expect("a connection within five seconds")
                .expect("a connection")
        }
    }

    fn run_json(run: &str, needs_you: Value) -> Value {
        json!({
            "run": run, "ticket": "ENG-412", "title": "Rate-limit retries", "status": "active",
            "phases": [{ "phase": "pre", "mark": "done", "glyph": "✓" }],
            "needs_you": needs_you,
        })
    }

    async fn until_changed(changes: &mut watch::Receiver<u64>) {
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("a change within five seconds")
            .expect("changes");
    }

    #[tokio::test]
    async fn the_runs_are_queued_from_the_watch_and_unavailable_without_it() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        assert!(
            runs.fetch(None).await.expect("nothing yet").is_empty(),
            "nothing read before start"
        );
        let mut changes = runs.changes();
        let _task = runs.start();

        let mut peer = daemon.accept().await;
        peer.greet().await;
        peer.answer("runs.watch", json!({ "stream": 1 })).await;
        peer.send(json!({ "stream": 1, "event": { "type": "snapshot",
            "runs": [run_json("r1", Value::Null)] } }))
            .await;
        until_changed(&mut changes).await;
        let items = runs.fetch(None).await.expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].row_label(),
            "[run] ENG-412 Rate-limit retries  pre✓"
        );

        let mut gated = run_json(
            "r1",
            json!({ "kind": "gate", "instance": "ask-info@1",
            "gate": "info-needed" }),
        );
        gated["type"] = json!("run");
        peer.send(json!({ "stream": 1, "event": gated })).await;
        until_changed(&mut changes).await;
        assert_eq!(
            runs.fetch(None).await.expect("items")[0].state(),
            ItemState::NeedsDecision
        );

        peer.send(json!({ "stream": 1, "event": { "type": "removed", "run": "r1" } }))
            .await;
        until_changed(&mut changes).await;
        assert!(runs.fetch(None).await.expect("items").is_empty());

        drop(peer);
        until_changed(&mut changes).await;
        assert!(matches!(
            runs.fetch(None).await,
            Err(PortError::Unavailable { .. })
        ));
        let mut again = daemon.accept().await;
        again.greet().await;
    }

    #[tokio::test]
    async fn a_daemon_never_reached_queues_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = DaemonRuns::new(dir.path().join("absent.sock"), "vetinari test");
        let mut changes = runs.changes();
        let _task = runs.start();
        until_changed(&mut changes).await;
        assert!(runs.fetch(None).await.expect("no error").is_empty());
    }

    #[tokio::test]
    async fn a_gate_answer_and_a_cancel_each_go_on_a_connection_of_their_own() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let run = RunId::parse("r1").expect("run");
        let instance = NodeInstance::parse("ask-info@1").expect("instance");
        let answer = GateAnswer::Recheck {
            note: Some(Line::parse("Page size is 50").expect("note")),
        };

        let answering = tokio::spawn({
            let runs = runs.clone();
            let run = run.clone();
            async move { runs.answer_gate(&run, &instance, &answer).await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.answer("gate.answer", json!({ "sent": true })).await;
        assert_eq!(
            request["params"],
            json!({ "run": "r1", "instance": "ask-info@1",
                    "answer": { "kind": "recheck", "note": "Page size is 50" } })
        );
        assert_eq!(answering.await.expect("join"), Ok(GateReply::Sent));

        let cancelling = tokio::spawn({
            let runs = runs.clone();
            async move { runs.cancel_run(&run).await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.read().await;
        assert_eq!(request["method"], json!("run.cancel"));
        peer.send(json!({ "id": request["id"],
            "error": { "code": "not-found", "message": "not found: run r1" } }))
            .await;
        assert!(matches!(
            cancelling.await.expect("join"),
            Err(PortError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn a_start_names_its_run_and_a_refusal_says_why() {
        use vetinari_core::domain::devflow::ids::Rounds;
        use vetinari_core::domain::devflow::template::{FeatureFromTicket, RepoTarget, TicketRef};
        use vetinari_core::domain::ids::ProfileId;

        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let workflow = DevWorkflow::FeatureFromTicket(FeatureFromTicket {
            ticket: TicketRef {
                source: SourceId::parse("jira").expect("source"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
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
        });

        let starting = tokio::spawn({
            let runs = runs.clone();
            let workflow = workflow.clone();
            async move { runs.start_run(&workflow).await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.answer("run.start", json!({ "run": "01JB80" })).await;
        assert_eq!(
            request["params"],
            super::super::protocol::run_start_params(&workflow)
        );
        assert_eq!(
            starting.await.expect("join"),
            Ok(RunId::parse("01JB80").expect("run"))
        );

        let refused = tokio::spawn({
            let runs = runs.clone();
            async move { runs.start_run(&workflow).await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.read().await;
        peer.send(json!({ "id": request["id"],
            "error": { "code": "conflict", "message": "ENG-412 already has a run" } }))
            .await;
        assert!(matches!(
            refused.await.expect("join"),
            Err(PortError::Refused { detail, .. }) if detail.contains("already has a run")
        ));
    }

    #[tokio::test]
    async fn a_dismissal_says_whether_the_worktree_goes_too() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let dismissing = tokio::spawn({
            let runs = runs.clone();
            async move {
                runs.dismiss_run(&RunId::parse("r1").expect("run"), true)
                    .await
            }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.answer("run.dismiss", json!({})).await;
        assert_eq!(
            request["params"],
            json!({ "run": "r1", "remove_worktree": true })
        );
        assert_eq!(dismissing.await.expect("join"), Ok(()));
    }

    #[tokio::test]
    async fn a_popup_reads_each_detail_its_stream_sends() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let watching = tokio::spawn({
            let runs = runs.clone();
            async move {
                let mut watch = runs
                    .watch_run(
                        &RunId::parse("r1").expect("run"),
                        &IterationSelection::default(),
                    )
                    .await
                    .expect("watch");
                watch.next().await
            }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        let request = peer.answer("run.watch", json!({ "stream": 4 })).await;
        assert_eq!(request["params"]["run"], json!("r1"));
        peer.send(json!({ "stream": 4, "event": { "type": "graph",
            "run": run_json("r1", Value::Null),
            "rows": [{ "indent": 0, "node": "open-pr", "lane": "alone", "mark": "not-reached",
                       "glyph": "·" }],
            "questions": ["Page size?"] } }))
            .await;
        let detail = watching.await.expect("join").expect("detail").expect("one");
        assert_eq!(detail.rows[0].row.node.as_str(), "open-pr");
        assert_eq!(detail.notes.questions[0].as_str(), "Page size?");
    }

    #[tokio::test]
    async fn a_daemon_that_needs_a_sign_in_queues_one_item_saying_so() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let mut changes = runs.changes();
        let _task = runs.start();
        let mut peer = daemon.accept().await;
        peer.greet().await;
        peer.answer("runs.watch", json!({ "stream": 1 })).await;
        peer.send(json!({ "stream": 1, "event": { "type": "snapshot", "runs": [] } }))
            .await;
        until_changed(&mut changes).await;
        peer.send(json!({ "stream": 1, "event": { "type": "auth", "needs_sign_in": true } }))
            .await;
        until_changed(&mut changes).await;

        let items = runs.fetch(None).await.expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title().as_str(), "autognomus needs sign-in");
        assert_eq!(items[0].state(), ItemState::NeedsDecision);
        assert_eq!(
            items[0].context(),
            &vetinari_core::domain::attention::ItemContext::SignIn(Provider::Autognomus)
        );

        peer.send(json!({ "stream": 1, "event": { "type": "auth", "needs_sign_in": false } }))
            .await;
        until_changed(&mut changes).await;
        assert!(runs.fetch(None).await.expect("items").is_empty());
    }

    #[tokio::test]
    async fn a_sign_in_is_begun_in_the_daemon_and_its_state_asked_after() {
        let mut daemon = FakeDaemon::start();
        let runs = DaemonRuns::new(daemon.socket.clone(), "vetinari test");
        let asking = tokio::spawn({
            let runs = runs.clone();
            async move { runs.begin_sign_in().await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        peer.answer("auth.begin", json!({ "stream": 1 })).await;
        peer.send(json!({ "stream": 1, "event": {
            "type": "challenge", "user_code": "BDWP-HQPK",
            "verification_uri": "https://id.example.net/device", "interval": 5,
            "expires_at": "2026-10-25T09:10:00Z" } }))
            .await;
        let challenge = asking.await.expect("joined").expect("challenge");
        assert_eq!(challenge.user_code.as_deref(), Some("BDWP-HQPK"));

        let asking = tokio::spawn({
            let runs = runs.clone();
            async move { runs.sign_in_state().await }
        });
        let mut peer = daemon.accept().await;
        peer.greet().await;
        peer.answer("auth.status", json!({ "state": "valid" }))
            .await;
        assert_eq!(
            asking.await.expect("joined").expect("state"),
            vetinari_core::application::auth::AuthState::Valid
        );
    }
}
