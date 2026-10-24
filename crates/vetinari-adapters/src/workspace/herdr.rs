//! The Herdr workspace target, over Herdr's Unix socket.
//!
//! # Protocol
//!
//! Herdr speaks newline-delimited JSON on a local socket, documented at
//! <https://herdr.dev/docs/socket-api/>. One request per line, and every
//! request carries an `id` the reply echoes:
//!
//! ```text
//! -> {"id":"vetinari-1","method":"workspace.create","params":{"cwd":"/trees/pr-142","label":"pr-142"}}
//! <- {"id":"vetinari-1","result":{"workspace":{"workspace_id":"w3"}}}
//! ```
//!
//! A failure comes back as `{"id":…,"error":{"code":…,"message":…}}`. A
//! request without an `id` is rejected, and because a connection stays open
//! for event subscriptions a frame addressed to someone else can arrive
//! first, so replies are matched on the id rather than simply read.
//!
//! # Vocabulary
//!
//! Herdr's *workspace* is Vetinari's space. Herdr's *pane* is a real terminal
//! inside one, which is what a command is typed into: there is no "run this
//! in a workspace" call, so `run_in_space` finds the workspace's focused pane
//! and sends it the command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use vetinari_core::application::ports::{
    ExternalAgentSource, PortError, PortResult, WorkspaceTarget,
};
use vetinari_core::domain::agent::{ObservedAgent, ObservedAgentState};
use vetinari_core::domain::ids::SpaceId;
use vetinari_core::domain::text::NonEmptyText;
use vetinari_core::domain::workspace::{Space, SpaceName, TargetKind};

use super::shell_join;

/// Where Herdr's socket is.
///
/// Herdr resolves this itself in a fixed order, and Vetinari has to agree
/// with it or it addresses a socket nobody is listening on: an explicitly
/// configured path, then `HERDR_SOCKET_PATH`, then the named session in
/// `HERDR_SESSION`, then the default session's socket under the *config*
/// directory - not the state directory, where Vetinari used to look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SocketLocation {
    configured: Option<PathBuf>,
    socket_env: Option<String>,
    session_env: Option<String>,
    config_dir: Option<PathBuf>,
}

impl SocketLocation {
    /// Herdr's directory inside the config directory.
    pub const DIR: &'static str = "herdr";
    /// The socket file Herdr listens on.
    pub const FILE: &'static str = "herdr.sock";

    /// Read the environment Herdr reads, under a config directory.
    #[must_use]
    pub fn from_env(config_dir: Option<&Path>) -> Self {
        Self {
            configured: None,
            socket_env: std::env::var("HERDR_SOCKET_PATH").ok(),
            session_env: std::env::var("HERDR_SESSION").ok(),
            config_dir: config_dir.map(Path::to_path_buf),
        }
    }

    /// Prefer a path the configuration named.
    #[must_use]
    pub fn configured(mut self, path: Option<PathBuf>) -> Self {
        self.configured = path;
        self
    }

    /// Override `HERDR_SOCKET_PATH`, for tests.
    #[must_use]
    pub fn socket_env(mut self, value: Option<String>) -> Self {
        self.socket_env = value;
        self
    }

    /// Override `HERDR_SESSION`, for tests.
    #[must_use]
    pub fn session_env(mut self, value: Option<String>) -> Self {
        self.session_env = value;
        self
    }

    /// Which path to connect to.
    #[must_use]
    pub fn resolve(&self) -> PathBuf {
        if let Some(path) = &self.configured {
            return path.clone();
        }
        if let Some(path) = self.socket_env.as_deref().filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
        let root = self
            .config_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(Self::DIR);
        match self.session_env.as_deref().filter(|name| !name.is_empty()) {
            Some(session) => root.join("sessions").join(session).join(Self::FILE),
            None => root.join(Self::FILE),
        }
    }
}

/// Talks to Herdr over its socket.
#[derive(Debug, Clone)]
pub struct HerdrSocketTarget {
    socket: PathBuf,
    timeout: Duration,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug, Serialize)]
struct Request<'params> {
    id: &'params str,
    method: &'static str,
    params: &'params serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    id: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ReplyError>,
}

#[derive(Debug, Deserialize)]
struct ReplyError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct RawPane {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    focused: bool,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    source: String,
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
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Target whichever socket Herdr itself would use.
    #[must_use]
    pub fn at(location: &SocketLocation) -> Self {
        Self::new(location.resolve())
    }

    /// Wait a different length of time for replies.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The socket path.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> PortResult<serde_json::Value> {
        let id = format!("vetinari-{}", self.next_id.fetch_add(1, Ordering::Relaxed));

        let stream = UnixStream::connect(&self.socket).await.map_err(|error| {
            PortError::unavailable(
                "herdr",
                format!("could not connect to {}: {error}", self.socket.display()),
            )
        })?;
        let (read_half, mut write_half) = stream.into_split();

        let payload = serde_json::to_string(&Request {
            id: &id,
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
        loop {
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
            // A subscribed connection carries event frames too, so anything
            // not answering this request is somebody else's.
            if reply.id != id {
                continue;
            }
            if let Some(error) = reply.error {
                return Err(map_error(method, &error));
            }
            return Ok(reply.result.unwrap_or(serde_json::Value::Null));
        }
    }

    async fn panes_of(&self, id: &SpaceId) -> PortResult<Vec<RawPane>> {
        let result = self
            .call(
                "pane.list",
                serde_json::json!({ "workspace_id": id.as_str() }),
            )
            .await?;
        list_of(&result, "panes", "pane list")
    }
}

/// Read a named list out of a reply.
///
/// A result that holds no such field is a protocol error rather than an empty
/// list: reading "nothing is running" out of a reply shaped differently than
/// expected is the failure that is hardest to notice, and Herdr sends an
/// explicit empty array when there is genuinely nothing.
fn list_of<T: serde::de::DeserializeOwned>(
    result: &serde_json::Value,
    field: &str,
    what: &str,
) -> PortResult<Vec<T>> {
    let Some(value) = result.get(field) else {
        // No result at all is how a bare acknowledgement comes back.
        if result.is_null() {
            return Ok(Vec::new());
        }
        return Err(PortError::protocol(
            "herdr",
            format!("the {what} reply has no {field:?} in it: {result}"),
        ));
    };
    serde_json::from_value(value.clone()).map_err(|error| {
        PortError::protocol("herdr", format!("could not read the {what}: {error}"))
    })
}

fn observed(raw: RawAgent) -> Option<ObservedAgent> {
    let pane = SpaceId::parse(&raw.pane_id).ok()?;
    let workspace = SpaceId::parse(&raw.workspace_id).ok();
    let name = if raw.agent.trim().is_empty() {
        "agent".to_owned()
    } else {
        raw.agent
    };
    let title = [raw.name, raw.title, raw.terminal_title_stripped]
        .into_iter()
        .flatten()
        .find_map(|title| NonEmptyText::parse(&title).ok());
    let agent = ObservedAgent::new(
        pane,
        workspace,
        name,
        ObservedAgentState::parse(&raw.agent_status),
        TargetKind::Herdr,
        (!raw.source.trim().is_empty()).then_some(raw.source),
    );
    Some(match title {
        Some(title) => agent.with_title(title),
        None => agent,
    })
}

/// Turn Herdr's error code into something the front-end can act on.
///
/// The codes are Herdr's own; what matters here is which of them the
/// developer can do something about and which are worth retrying.
fn map_error(method: &'static str, error: &ReplyError) -> PortError {
    let message = if error.message.trim().is_empty() {
        format!("{method} failed without saying why")
    } else {
        error.message.clone()
    };
    match error.code.as_str() {
        "not_found" => PortError::not_found("herdr workspace", message),
        "timeout" | "agent_prompt_stalled" => PortError::unavailable("herdr", message),
        "" => PortError::Refused {
            what: "herdr".to_owned(),
            detail: message,
        },
        code => PortError::Refused {
            what: "herdr".to_owned(),
            detail: format!("{code}: {message}"),
        },
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
                "workspace.create",
                serde_json::json!({
                    "cwd": path.display().to_string(),
                    "label": name.as_str(),
                }),
            )
            .await?;
        let id = result
            .get("workspace")
            .and_then(|workspace| workspace.get("workspace_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        SpaceId::parse(id).map_err(|_| {
            PortError::protocol(
                "herdr",
                "the new workspace came back without a workspace_id".to_owned(),
            )
        })
    }

    async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        self.call(
            "workspace.focus",
            serde_json::json!({ "workspace_id": id.as_str() }),
        )
        .await?;
        Ok(())
    }

    async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        self.call(
            "workspace.close",
            serde_json::json!({ "workspace_id": id.as_str() }),
        )
        .await?;
        Ok(())
    }

    async fn list_spaces(&self) -> PortResult<Vec<Space>> {
        let result = self.call("workspace.list", serde_json::json!({})).await?;
        let raw: Vec<RawWorkspace> = list_of(&result, "workspaces", "workspace list")?;

        Ok(raw
            .into_iter()
            .filter_map(|workspace| {
                let id = SpaceId::parse(&workspace.workspace_id).ok()?;
                Some(Space::new(
                    id,
                    SpaceName::sanitized(&workspace.label),
                    PathBuf::from(workspace.cwd),
                    TargetKind::Herdr,
                ))
            })
            .collect())
    }

    async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()> {
        let panes = self.panes_of(id).await?;
        let pane = panes
            .iter()
            .find(|pane| pane.focused)
            .or_else(|| panes.first())
            .ok_or_else(|| PortError::not_found("pane in herdr workspace", id.as_str()))?;

        // A pane is a terminal, so the command is typed at its shell and
        // submitted, rather than handed over as an argument vector.
        self.call(
            "pane.send_input",
            serde_json::json!({
                "pane_id": pane.pane_id,
                "text": format!("{}\n", shell_join(command)),
            }),
        )
        .await?;
        Ok(())
    }
}
/// Herdr's own agents, whichever pane they are running in.
///
/// Vetinari did not start these, so it cannot cancel or re-run them; what it
/// can do is rank them, which is the whole point of the feed when one of them
/// is blocked.
#[async_trait]
impl ExternalAgentSource for HerdrSocketTarget {
    fn target(&self) -> TargetKind {
        TargetKind::Herdr
    }

    async fn observed_agents(&self) -> PortResult<Vec<ObservedAgent>> {
        let result = self.call("agent.list", serde_json::json!({})).await?;
        let raw: Vec<RawAgent> = list_of(&result, "agents", "agent list")?;
        Ok(raw.into_iter().filter_map(observed).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A stand-in for Herdr: speaks the documented protocol, answering each
    /// request on a line with a reply echoing its `id`.
    ///
    /// Keyed by method, because the adapter is free to choose how many
    /// requests one operation takes; what matters is which methods it calls
    /// and what it sends in them.
    struct FakeHerdr {
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl FakeHerdr {
        /// Serve `results` by method name, on a socket, until dropped.
        fn serve(socket: PathBuf, results: HashMap<&'static str, serde_json::Value>) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let listener = UnixListener::bind(&socket).expect("bind");
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    let results = results.clone();
                    tokio::spawn(async move {
                        let (read_half, mut write_half) = stream.into_split();
                        let mut lines = BufReader::new(read_half).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let request: serde_json::Value =
                                serde_json::from_str(&line).expect("herdr is sent valid json");
                            let id = request["id"].as_str().expect("every request has an id");
                            let method = request["method"].as_str().expect("method").to_owned();
                            seen.lock().expect("lock").push(request.clone());

                            let reply = match results.get(method.as_str()) {
                                Some(result) => {
                                    serde_json::json!({ "id": id, "result": result })
                                }
                                None => serde_json::json!({
                                    "id": id,
                                    "error": {
                                        "code": "unknown_method",
                                        "message": format!("no such method {method}")
                                    }
                                }),
                            };
                            let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
                            let _ = write_half.flush().await;
                        }
                    });
                }
            });
            Self { requests }
        }

        fn requests(&self) -> Vec<serde_json::Value> {
            self.requests.lock().expect("lock").clone()
        }

        fn method(&self, name: &str) -> serde_json::Value {
            self.requests()
                .into_iter()
                .find(|request| request["method"] == name)
                .unwrap_or_else(|| panic!("{name} was never called"))
        }

        fn methods(&self) -> Vec<String> {
            self.requests()
                .iter()
                .map(|request| request["method"].as_str().expect("method").to_owned())
                .collect()
        }
    }

    fn socket_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("herdr.sock");
        (dir, path)
    }

    fn results(
        pairs: Vec<(&'static str, serde_json::Value)>,
    ) -> HashMap<&'static str, serde_json::Value> {
        pairs.into_iter().collect()
    }

    #[tokio::test]
    async fn every_request_carries_an_id_the_reply_is_matched_on() {
        let (_dir, socket) = socket_path();
        let fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.list",
                serde_json::json!({ "workspaces": [] }),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        target.list_spaces().await.expect("list");

        let request = fake.method("workspace.list");
        assert!(
            request["id"].as_str().is_some_and(|id| !id.is_empty()),
            "herdr rejects a request without an id, got {request}"
        );
    }

    #[tokio::test]
    async fn a_reply_without_an_ok_field_is_a_success_rather_than_a_refusal() {
        // Herdr answers {"id":…,"result":…} with no "ok" anywhere. Treating a
        // missing "ok" as false made every call fail.
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.list",
                serde_json::json!({ "workspaces": [
                    { "workspace_id": "w1", "label": "pr-142", "cwd": "/trees/pr-142" }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 1);
    }

    #[tokio::test]
    async fn spaces_are_listed_from_herdrs_workspaces() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.list",
                serde_json::json!({ "workspaces": [
                    { "workspace_id": "w1", "label": "pr-142", "cwd": "/trees/pr-142" },
                    { "workspace_id": "w2", "label": "feedback", "cwd": "/trees/feedback",
                      "focused": true }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].id().as_str(), "w1");
        assert_eq!(spaces[0].name().as_str(), "pr-142");
        assert_eq!(spaces[0].path(), Path::new("/trees/pr-142"));
        assert_eq!(spaces[0].target(), TargetKind::Herdr);
        assert_eq!(spaces[1].id().as_str(), "w2");
    }

    #[tokio::test]
    async fn creating_a_space_sends_the_cwd_and_label_and_returns_the_workspace_id() {
        let (_dir, socket) = socket_path();
        let fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.create",
                serde_json::json!({ "workspace": { "workspace_id": "w3" } }),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let id = target
            .create_space(&SpaceName::sanitized("pr-142"), Path::new("/trees/pr-142"))
            .await
            .expect("create");

        assert_eq!(id.as_str(), "w3");
        let request = fake.method("workspace.create");
        assert_eq!(request["params"]["cwd"], "/trees/pr-142");
        assert_eq!(request["params"]["label"], "pr-142");
    }

    #[tokio::test]
    async fn focusing_and_closing_name_the_workspace_herdr_was_given() {
        let (_dir, socket) = socket_path();
        let fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![
                ("workspace.focus", serde_json::json!({})),
                ("workspace.close", serde_json::json!({})),
            ]),
        );
        let target = HerdrSocketTarget::new(socket);
        let id = SpaceId::parse("w9").expect("id");

        target.focus_space(&id).await.expect("focus");
        target.close_space(&id).await.expect("close");

        assert_eq!(fake.methods(), vec!["workspace.focus", "workspace.close"]);
        assert_eq!(
            fake.method("workspace.focus")["params"]["workspace_id"],
            "w9"
        );
        assert_eq!(
            fake.method("workspace.close")["params"]["workspace_id"],
            "w9"
        );
    }

    #[tokio::test]
    async fn running_a_command_types_it_into_a_pane_of_that_workspace() {
        let (_dir, socket) = socket_path();
        let fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![
                (
                    "pane.list",
                    serde_json::json!({ "panes": [
                        { "pane_id": "w9:p1", "workspace_id": "w9" }
                    ]}),
                ),
                ("pane.send_input", serde_json::json!({})),
            ]),
        );
        let target = HerdrSocketTarget::new(socket);
        let id = SpaceId::parse("w9").expect("id");

        target
            .run_in_space(
                &id,
                &[
                    "claude".to_owned(),
                    "--print".to_owned(),
                    "do it".to_owned(),
                ],
            )
            .await
            .expect("run");

        assert_eq!(fake.method("pane.list")["params"]["workspace_id"], "w9");
        let sent = fake.method("pane.send_input");
        assert_eq!(sent["params"]["pane_id"], "w9:p1");
        assert_eq!(
            sent["params"]["text"], "claude --print 'do it'\n",
            "the command is typed at a shell, so it must be quoted and submitted"
        );
    }

    #[tokio::test]
    async fn the_focused_pane_is_preferred_when_a_workspace_has_several() {
        let (_dir, socket) = socket_path();
        let fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![
                (
                    "pane.list",
                    serde_json::json!({ "panes": [
                        { "pane_id": "w9:p1", "workspace_id": "w9" },
                        { "pane_id": "w9:p2", "workspace_id": "w9", "focused": true }
                    ]}),
                ),
                ("pane.send_input", serde_json::json!({})),
            ]),
        );
        let target = HerdrSocketTarget::new(socket);

        target
            .run_in_space(&SpaceId::parse("w9").expect("id"), &["ls".to_owned()])
            .await
            .expect("run");

        assert_eq!(fake.method("pane.send_input")["params"]["pane_id"], "w9:p2");
    }

    #[tokio::test]
    async fn a_workspace_with_no_pane_to_type_into_is_reported_rather_than_silently_dropped() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![("pane.list", serde_json::json!({ "panes": [] }))]),
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .run_in_space(&SpaceId::parse("w9").expect("id"), &["ls".to_owned()])
            .await
            .expect_err("no pane");

        assert!(matches!(error, PortError::NotFound { .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn herdrs_error_object_is_reported_with_its_code_and_message() {
        let (_dir, socket) = socket_path();
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let line = lines.next_line().await.expect("read").expect("a request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let reply = serde_json::json!({
                "id": request["id"],
                "error": { "code": "not_found", "message": "workspace not found" }
            });
            let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
            let _ = write_half.flush().await;
        });
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .focus_space(&SpaceId::parse("w1").expect("id"))
            .await
            .expect_err("refused");

        match error {
            PortError::NotFound { what, id } => {
                assert_eq!(what, "herdr workspace");
                assert_eq!(id, "workspace not found");
            }
            other => panic!("expected a not-found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_code_herdr_cannot_retry_is_a_refusal() {
        let (_dir, socket) = socket_path();
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let line = lines.next_line().await.expect("read").expect("a request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let reply = serde_json::json!({
                "id": request["id"],
                "error": {
                    "code": "workspace_group_close_required",
                    "message": "linked worktree workspaces are open"
                }
            });
            let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
            let _ = write_half.flush().await;
        });
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .close_space(&SpaceId::parse("w1").expect("id"))
            .await
            .expect_err("refused");

        match error {
            PortError::Refused { what, detail } => {
                assert_eq!(what, "herdr");
                assert!(
                    detail.contains("workspace_group_close_required"),
                    "{detail}"
                );
                assert!(
                    detail.contains("linked worktree workspaces are open"),
                    "{detail}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_reply_meant_for_another_request_is_skipped_rather_than_mistaken_for_this_one() {
        // Herdr keeps a subscribed connection open, so an event frame can
        // arrive between a request and its reply.
        let (_dir, socket) = socket_path();
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let line = lines.next_line().await.expect("read").expect("a request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let noise = serde_json::json!({
                "id": "someone-elses-request",
                "result": { "workspaces": [] }
            });
            let reply = serde_json::json!({
                "id": request["id"],
                "result": { "workspaces": [
                    { "workspace_id": "w1", "label": "mine", "cwd": "/mine" }
                ]}
            });
            for frame in [noise, reply] {
                let _ = write_half.write_all(format!("{frame}\n").as_bytes()).await;
            }
            let _ = write_half.flush().await;
        });
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id().as_str(), "w1");
    }

    #[tokio::test]
    async fn a_workspace_herdr_cannot_describe_is_skipped_rather_than_failing_the_list() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.list",
                serde_json::json!({ "workspaces": [
                    { "workspace_id": "", "label": "broken", "cwd": "/x" },
                    { "workspace_id": "w2", "label": "fine", "cwd": "/y" }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let spaces = target.list_spaces().await.expect("list");

        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id().as_str(), "w2");
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
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(stream);
        });
        let target = HerdrSocketTarget::new(socket).with_timeout(Duration::from_millis(100));

        let error = target.list_spaces().await.expect_err("no reply");

        assert!(error.is_transient(), "got {error:?}");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_reported_as_a_protocol_error() {
        let (_dir, socket) = socket_path();
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
            let _ = write_half.write_all(b"not json at all\n").await;
            let _ = write_half.flush().await;
        });
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .focus_space(&SpaceId::parse("w1").expect("id"))
            .await
            .expect_err("bad reply");

        assert!(matches!(error, PortError::Protocol { .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn a_created_workspace_that_reports_no_id_is_a_protocol_error() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.create",
                serde_json::json!({ "workspace": {} }),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target
            .create_space(&SpaceName::sanitized("x"), Path::new("/tmp"))
            .await
            .expect_err("no id");

        assert!(matches!(error, PortError::Protocol { .. }), "got {error:?}");
    }

    #[test]
    fn the_target_reports_which_kind_it_is() {
        let (_dir, socket) = socket_path();
        assert_eq!(HerdrSocketTarget::new(socket).kind(), TargetKind::Herdr);
    }
    #[tokio::test]
    async fn herdrs_own_agents_are_observed_with_where_they_are_and_what_they_are_doing() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "agent.list",
                serde_json::json!({ "agents": [
                    { "pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude",
                      "agent_status": "blocked", "source": "integration" },
                    { "pane_id": "w2:p1", "workspace_id": "w2", "agent": "codex",
                      "agent_status": "working" },
                    { "pane_id": "w3:p1", "workspace_id": "w3", "agent": "opencode",
                      "agent_status": "idle" }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let agents = target.observed_agents().await.expect("list");

        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].pane().as_str(), "w1:p1");
        assert_eq!(agents[0].workspace().map(SpaceId::as_str), Some("w1"));
        assert_eq!(agents[0].name(), "claude");
        assert_eq!(agents[0].state(), ObservedAgentState::Blocked);
        assert_eq!(agents[0].target(), TargetKind::Herdr);
        assert_eq!(agents[0].source(), Some("integration"));
        assert_eq!(agents[1].state(), ObservedAgentState::Working);
        assert_eq!(agents[1].source(), None);
        assert_eq!(agents[2].state(), ObservedAgentState::Idle);
    }

    #[tokio::test]
    async fn an_agent_is_titled_by_its_name_then_herdrs_title_then_its_terminals() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "agent.list",
                serde_json::json!({ "agents": [
                    { "pane_id": "w1:p1", "agent": "claude", "agent_status": "done",
                      "name": "auth work", "title": "Fix login",
                      "terminal_title_stripped": "Claude Code" },
                    { "pane_id": "w2:p1", "agent": "claude", "agent_status": "done",
                      "name": null, "title": "  ",
                      "terminal_title_stripped": "Herdr attention queue" },
                    { "pane_id": "w3:p1", "agent": "claude", "agent_status": "idle" }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let agents = target.observed_agents().await.expect("list");

        let titles: Vec<Option<&str>> = agents
            .iter()
            .map(|agent| agent.title().map(NonEmptyText::as_str))
            .collect();
        assert_eq!(
            titles,
            vec![Some("auth work"), Some("Herdr attention queue"), None]
        );
    }

    #[tokio::test]
    async fn an_agent_herdr_cannot_place_in_a_pane_is_skipped_rather_than_failing_the_list() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "agent.list",
                serde_json::json!({ "agents": [
                    { "pane_id": "", "agent": "ghost", "agent_status": "working" },
                    { "pane_id": "w2:p1", "agent": "", "agent_status": "mystery" }
                ]}),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        let agents = target.observed_agents().await.expect("list");

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane().as_str(), "w2:p1");
        assert_eq!(agents[0].workspace(), None);
        assert_eq!(
            agents[0].name(),
            "agent",
            "an unnamed agent still has to be nameable in the monitor"
        );
        assert_eq!(agents[0].state(), ObservedAgentState::Unknown);
    }

    #[tokio::test]
    async fn no_agents_running_is_an_empty_list_rather_than_an_error() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![("agent.list", serde_json::json!({ "agents": [] }))]),
        );
        let target = HerdrSocketTarget::new(socket);

        assert!(target.observed_agents().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn an_agent_list_with_no_agents_field_is_reported_rather_than_read_as_quiet() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![("agent.list", serde_json::json!({}))]),
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target.observed_agents().await.expect_err("unknown shape");

        assert!(matches!(error, PortError::Protocol { .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn a_result_that_does_not_hold_the_expected_list_is_reported_not_read_as_empty() {
        // A guessed-wrong field name must not look like "nothing is running".
        // That silence is exactly what made the first version of this adapter
        // so hard to diagnose.
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![("workspace.list", serde_json::json!({ "items": [] }))]),
        );
        let target = HerdrSocketTarget::new(socket);

        let error = target.list_spaces().await.expect_err("unknown shape");

        match error {
            PortError::Protocol { what, detail } => {
                assert_eq!(what, "herdr");
                assert!(detail.contains("workspaces"), "{detail}");
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_list_herdr_does_send_is_still_empty() {
        let (_dir, socket) = socket_path();
        let _fake = FakeHerdr::serve(
            socket.clone(),
            results(vec![(
                "workspace.list",
                serde_json::json!({ "workspaces": [] }),
            )]),
        );
        let target = HerdrSocketTarget::new(socket);

        assert!(target.list_spaces().await.expect("list").is_empty());
    }

    #[test]
    fn a_configured_socket_path_wins_over_everything_else() {
        let location = SocketLocation::from_env(Some(Path::new("/home/dev/.config")))
            .configured(Some(PathBuf::from("/run/herdr.sock")))
            .socket_env(Some("/env/herdr.sock".to_owned()))
            .session_env(Some("other".to_owned()));

        assert_eq!(location.resolve(), PathBuf::from("/run/herdr.sock"));
    }

    #[test]
    fn herdr_socket_path_is_honoured_when_nothing_is_configured() {
        let location = SocketLocation::from_env(Some(Path::new("/home/dev/.config")))
            .socket_env(Some("/env/herdr.sock".to_owned()))
            .session_env(Some("other".to_owned()));

        assert_eq!(location.resolve(), PathBuf::from("/env/herdr.sock"));
    }

    #[test]
    fn a_named_session_has_its_own_socket() {
        let location = SocketLocation::from_env(Some(Path::new("/home/dev/.config")))
            .socket_env(None)
            .session_env(Some("review".to_owned()));

        assert_eq!(
            location.resolve(),
            PathBuf::from("/home/dev/.config/herdr/sessions/review/herdr.sock")
        );
    }

    #[test]
    fn the_default_socket_is_the_default_sessions_one_under_the_config_directory() {
        // Not the state directory: Vetinari looked under
        // ~/.local/state/herdr and found nothing listening there.
        let location = SocketLocation::from_env(Some(Path::new("/home/dev/.config")))
            .socket_env(None)
            .session_env(None);

        assert_eq!(
            location.resolve(),
            PathBuf::from("/home/dev/.config/herdr/herdr.sock")
        );
    }

    #[test]
    fn an_empty_environment_variable_is_not_a_socket_path() {
        let location = SocketLocation::from_env(Some(Path::new("/home/dev/.config")))
            .socket_env(Some(String::new()))
            .session_env(Some(String::new()));

        assert_eq!(
            location.resolve(),
            PathBuf::from("/home/dev/.config/herdr/herdr.sock")
        );
    }
}
