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
    MAX_FRAME, Outbound, Request, change_event, hello_result, parse_request, snapshot_event,
    stream_opened,
};
use vetinari_core::application::devflow::api::{ApiError, BoardChange, ClientApi, RunItem};

use crate::hub::Hub;

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

/// Serve every connection until the listener fails.
pub async fn serve(listener: UnixListener, api: Arc<ClientApi>, hub: Arc<Hub>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(connection(stream, api.clone(), hub.clone()));
    }
}

/// How many frames may wait for a slow client before its streams wait too.
const OUTBOX: usize = 64;

/// One client, until it hangs up or sends a frame too large to read.
async fn connection(stream: UnixStream, api: Arc<ClientApi>, hub: Arc<Hub>) {
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use vetinari_core::application::devflow::api::RunLabel;
    use vetinari_core::domain::devflow::ids::{RunId, WorkerName};
    use vetinari_core::domain::devflow::run_view::RunStatus;
    use vetinari_core::domain::devflow::summary::RunSummary;
    use vetinari_core::domain::text::NonEmptyText;

    use super::*;

    struct Server {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        hub: Arc<Hub>,
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
        tokio::spawn(serve(listener, api, hub.clone()));
        Server {
            _dir: dir,
            path,
            hub,
        }
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
