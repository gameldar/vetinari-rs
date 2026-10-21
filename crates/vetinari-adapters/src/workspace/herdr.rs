//! The Herdr workspace target, over Herdr's Unix socket.
//!
//! # Protocol
//!
//! Herdr is addressed with one newline-delimited JSON request per connection:
//!
//! ```text
//! -> {"method":"create_space","params":{"name":"pr-142","path":"/trees/pr-142"}}
//! <- {"ok":true,"result":{"id":"space-3"}}
//! ```
//!
//! and a failure comes back as `{"ok":false,"error":"..."}`. The methods used
//! are `create_space`, `focus_space`, `close_space`, `list_spaces`, and
//! `run_in_space`. This is the shape Vetinari speaks; if Herdr's own IPC
//! differs, this module is the only place that needs to change.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use vetinari_core::application::ports::{PortError, PortResult, WorkspaceTarget};
use vetinari_core::domain::ids::SpaceId;
use vetinari_core::domain::workspace::{Space, SpaceName, TargetKind};

/// Talks to Herdr over its socket.
#[derive(Debug, Clone)]
pub struct HerdrSocketTarget {
    socket: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Serialize)]
struct Request<'params> {
    method: &'static str,
    params: &'params serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RawSpace {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
}

impl HerdrSocketTarget {
    /// How long to wait for Herdr to answer.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Target a Herdr socket.
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Wait a different length of time for replies.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The socket path.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Where Herdr's socket lives by default, given an XDG state directory.
    #[must_use]
    pub fn default_socket(state_dir: Option<&Path>) -> PathBuf {
        state_dir
            .unwrap_or(Path::new("."))
            .join("herdr")
            .join("herdr.sock")
    }

    async fn call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> PortResult<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket).await.map_err(|error| {
            PortError::unavailable(
                "herdr",
                format!("could not connect to {}: {error}", self.socket.display()),
            )
        })?;
        let (read_half, mut write_half) = stream.into_split();

        let payload = serde_json::to_string(&Request {
            method,
            params: &params,
        })
        .map_err(|error| PortError::protocol("herdr", error.to_string()))?;

        write_half
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .map_err(|error| PortError::unavailable("herdr", error.to_string()))?;
        write_half
            .flush()
            .await
            .map_err(|error| PortError::unavailable("herdr", error.to_string()))?;

        let mut lines = BufReader::new(read_half).lines();
        let line = tokio::time::timeout(self.timeout, lines.next_line())
            .await
            .map_err(|_| {
                PortError::unavailable(
                    "herdr",
                    format!("no reply to {method} within {:?}", self.timeout),
                )
            })?
            .map_err(|error| PortError::unavailable("herdr", error.to_string()))?
            .ok_or_else(|| {
                PortError::unavailable("herdr", format!("the socket closed during {method}"))
            })?;

        let reply: Reply = serde_json::from_str(&line).map_err(|error| {
            PortError::protocol("herdr", format!("could not read the reply: {error}"))
        })?;
        if !reply.ok {
            return Err(PortError::Refused {
                what: "herdr".to_owned(),
                detail: reply
                    .error
                    .unwrap_or_else(|| format!("{method} was refused without saying why")),
            });
        }
        Ok(reply.result)
    }
}

#[async_trait]
impl WorkspaceTarget for HerdrSocketTarget {
    fn kind(&self) -> TargetKind {
        TargetKind::Herdr
    }

    async fn create_space(&self, name: &SpaceName, path: &Path) -> PortResult<SpaceId> {
        let result = self
            .call(
                "create_space",
                serde_json::json!({ "name": name.as_str(), "path": path.display().to_string() }),
            )
            .await?;
        let id = result
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        SpaceId::parse(id).map_err(|_| {
            PortError::protocol("herdr", "the new space came back without an id".to_owned())
        })
    }

    async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        self.call("focus_space", serde_json::json!({ "id": id.as_str() }))
            .await?;
        Ok(())
    }

    async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        self.call("close_space", serde_json::json!({ "id": id.as_str() }))
            .await?;
        Ok(())
    }

    async fn list_spaces(&self) -> PortResult<Vec<Space>> {
        let result = self.call("list_spaces", serde_json::json!({})).await?;
        let raw: Vec<RawSpace> = serde_json::from_value(
            result
                .get("spaces")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new())),
        )
        .map_err(|error| {
            PortError::protocol("herdr", format!("could not read the space list: {error}"))
        })?;

        Ok(raw
            .into_iter()
            .filter_map(|space| {
                let id = SpaceId::parse(&space.id).ok()?;
                Some(Space::new(
                    id,
                    SpaceName::sanitized(&space.name),
                    PathBuf::from(space.path),
                    TargetKind::Herdr,
                ))
            })
            .collect())
    }

    async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()> {
        self.call(
            "run_in_space",
            serde_json::json!({ "id": id.as_str(), "command": command }),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A stand-in for Herdr: reads one request per connection and answers
    /// with whatever the test told it to.
    fn serve(socket: PathBuf, replies: Vec<String>) -> Arc<Mutex<Vec<String>>> {
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = received.clone();
        // Bind before returning, so a caller cannot connect before the
        // socket exists.
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            for reply in replies {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (read_half, mut write_half) = stream.into_split();
                let mut lines = BufReader::new(read_half).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    seen.lock().expect("lock").push(line);
                }
                let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
                let _ = write_half.flush().await;
            }
        });
        received
    }

    fn socket_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("herdr.sock");
        (dir, path)
    }

    #[tokio::test]
    async fn creating_a_space_sends_the_name_and_path_and_returns_herdrs_id() {
        let (_dir, socket) = socket_path();
        let received = serve(
            socket.clone(),
            vec![r#"{"ok":true,"result":{"id":"space-3"}}"#.to_owned()],
        );
        let target = HerdrSocketTarget::new(socket);

        let id = target
            .create_space(&SpaceName::sanitized("pr-142"), Path::new("/trees/pr-142"))
            .await
            .expect("create");

        assert_eq!(id.as_str(), "space-3");
        let request: serde_json::Value =
            serde_json::from_str(&received.lock().expect("lock")[0]).expect("json");
        assert_eq!(request["method"], "create_space");
        assert_eq!(request["params"]["name"], "pr-142");
        assert_eq!(request["params"]["path"], "/trees/pr-142");
    }

    #[tokio::test]
    async fn an_error_from_herdr_is_reported_as_a_refusal() {
        let (_dir, socket) = socket_path();
        serve(
            socket.clone(),
            vec![r#"{"ok":false,"error":"no such workspace root"}"#.to_owned()],
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .create_space(&SpaceName::sanitized("x"), Path::new("/nope"))
            .await
            .expect_err("refused");

        match error {
            PortError::Refused { what, detail } => {
                assert_eq!(what, "herdr");
                assert_eq!(detail, "no such workspace root");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_reported_as_a_protocol_error() {
        let (_dir, socket) = socket_path();
        serve(socket.clone(), vec!["not json at all".to_owned()]);
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .focus_space(&SpaceId::parse("space-1").expect("id"))
            .await
            .expect_err("bad reply");

        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[tokio::test]
    async fn a_successful_reply_without_an_id_is_a_protocol_error() {
        let (_dir, socket) = socket_path();
        serve(
            socket.clone(),
            vec![r#"{"ok":true,"result":{}}"#.to_owned()],
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .create_space(&SpaceName::sanitized("x"), Path::new("/tmp"))
            .await
            .expect_err("no id");

        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[tokio::test]
    async fn spaces_are_listed_from_herdr() {
        let (_dir, socket) = socket_path();
        serve(
            socket.clone(),
            vec![
                concat!(
                    r#"{"ok":true,"result":{"spaces":["#,
                    r#"{"id":"space-1","name":"pr-142","path":"/trees/pr-142"},"#,
                    r#"{"id":"space-2","name":"feedback","path":"/trees/feedback"}"#,
                    "]}}"
                )
                .to_owned(),
            ],
        );
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].id().as_str(), "space-1");
        assert_eq!(spaces[0].name().as_str(), "pr-142");
        assert_eq!(spaces[1].path(), Path::new("/trees/feedback"));
        assert_eq!(spaces[0].target(), TargetKind::Herdr);
    }

    #[tokio::test]
    async fn an_entry_herdr_cannot_describe_is_skipped_rather_than_failing_the_list() {
        let (_dir, socket) = socket_path();
        serve(
            socket.clone(),
            vec![
                concat!(
                    r#"{"ok":true,"result":{"spaces":["#,
                    r#"{"id":"","name":"broken","path":"/x"},"#,
                    r#"{"id":"space-2","name":"fine","path":"/y"}"#,
                    "]}}"
                )
                .to_owned(),
            ],
        );
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id().as_str(), "space-2");
    }

    #[tokio::test]
    async fn focusing_closing_and_running_send_the_space_id() {
        let (_dir, socket) = socket_path();
        let ok = r#"{"ok":true}"#.to_owned();
        let received = serve(socket.clone(), vec![ok.clone(), ok.clone(), ok]);
        let target = HerdrSocketTarget::new(socket);
        let id = SpaceId::parse("space-9").expect("id");

        target.focus_space(&id).await.expect("focus");
        target.close_space(&id).await.expect("close");
        target
            .run_in_space(&id, &["claude".to_owned(), "--print".to_owned()])
            .await
            .expect("run");

        let requests = received.lock().expect("lock").clone();
        let methods: Vec<String> = requests
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("json")["method"]
                    .as_str()
                    .expect("method")
                    .to_owned()
            })
            .collect();
        assert_eq!(methods, vec!["focus_space", "close_space", "run_in_space"]);

        let last: serde_json::Value = serde_json::from_str(&requests[2]).expect("json");
        assert_eq!(last["params"]["id"], "space-9");
        assert_eq!(last["params"]["command"][0], "claude");
    }

    #[tokio::test]
    async fn a_socket_that_is_not_there_is_reported_as_unavailable() {
        let (_dir, socket) = socket_path();
        let target = HerdrSocketTarget::new(socket);

        let error = target.list_spaces().await.expect_err("no socket");

        assert!(error.is_transient(), "got {error:?}");
        assert!(error.to_string().contains("herdr"));
    }

    #[tokio::test]
    async fn a_server_that_says_nothing_is_reported_rather_than_waited_on_forever() {
        let (_dir, socket) = socket_path();
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn({
            async move {
                let (stream, _) = listener.accept().await.expect("accept");
                // Hold the connection open without replying.
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                drop(stream);
            }
        });
        let target = HerdrSocketTarget::new(socket).with_timeout(Duration::from_millis(100));

        let error = target.list_spaces().await.expect_err("no reply");

        assert!(error.is_transient(), "got {error:?}");
    }

    #[test]
    fn the_target_reports_which_kind_it_is() {
        let (_dir, socket) = socket_path();
        assert_eq!(HerdrSocketTarget::new(socket).kind(), TargetKind::Herdr);
    }

    #[test]
    fn the_default_socket_path_follows_the_xdg_state_directory() {
        let path = HerdrSocketTarget::default_socket(Some(Path::new("/home/dev/.local/state")));
        assert_eq!(
            path,
            PathBuf::from("/home/dev/.local/state/herdr/herdr.sock")
        );
    }
}
