//! The daemon's composition root: which adapters serve which ports, as
//! `vetinari-tui/src/startup.rs` is the TUI's.
//!
//! This skeleton serves the run board. It does not yet claim steps: the
//! worker's claim, heartbeat and agent loops join the runtime with the
//! configuration and sign-in that slice 13 brings.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;
use vetinari_adapters::HttpClient;
use vetinari_adapters::autognomus::client::{HttpGateway, StaticBearer};
use vetinari_core::application::devflow::api::ClientApi;
use vetinari_core::application::devflow::ports::{RunEvents, RunGateway};
use vetinari_core::application::ports::PortError;
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_core::domain::devflow::template::TemplateKind;

use crate::hub::Hub;
use crate::runtime::{Feed, OwnedRun, TokioClock, feed_runs};
use crate::scripted::{Script, ScriptError};
use crate::socket;
use crate::steps::LocalBoard;

/// Where the daemon's runs come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Replay a script.
    Scripted(PathBuf),
    /// A gateway, with a fixed bearer token until sign-in exists.
    Gateway {
        /// The gateway's base URL.
        url: String,
        /// The bearer token.
        token: String,
    },
}

/// How the daemon is to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The socket to serve.
    pub socket: PathBuf,
    /// The worker label.
    pub worker: WorkerName,
    /// Where runs come from.
    pub source: Source,
}

/// Why the daemon stopped.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The socket could not be bound or served.
    #[error("socket {path}: {error}")]
    Socket {
        /// The socket's path.
        path: PathBuf,
        /// What went wrong.
        error: io::Error,
    },
    /// The script could not be read.
    #[error("script {path}: {error}")]
    ReadScript {
        /// The script's path.
        path: PathBuf,
        /// What went wrong.
        error: io::Error,
    },
    /// The script is not a script.
    #[error(transparent)]
    Script(#[from] ScriptError),
    /// The gateway could not be reached or set up.
    #[error(transparent)]
    Gateway(#[from] PortError),
}

/// `$XDG_RUNTIME_DIR/vetinari/vetinarid.sock`, or under the temporary
/// directory when there is no runtime directory.
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

/// The worker label from a hostname: its first label, when that is a valid
/// worker name, else `vetinari`.
#[must_use]
pub fn worker_from_hostname(hostname: Option<&str>) -> WorkerName {
    hostname
        .and_then(|name| name.trim().split('.').next())
        .and_then(|name| WorkerName::parse(name).ok())
        .unwrap_or_else(|| WorkerName::parse("vetinari").expect("a valid worker name"))
}

/// The runs a gateway holds for vetinari: every run of every live
/// vetinari definition, since a definition lives until its run is
/// dismissed (design 7.7).
pub async fn discover(runs: &dyn RunGateway) -> Result<Vec<OwnedRun>, PortError> {
    let mut owned = Vec::new();
    for definition in runs.definitions().await? {
        for header in runs.runs(&definition).await? {
            owned.push(OwnedRun {
                run: header.id,
                label: None,
                started_at: Some(header.created_at),
            });
        }
    }
    Ok(owned)
}

/// Run the daemon until it is interrupted or its socket fails.
pub async fn run(settings: Settings) -> Result<(), DaemonError> {
    let socket_error = |error| DaemonError::Socket {
        path: settings.socket.clone(),
        error,
    };
    let (runs, events, owned): (Arc<dyn RunGateway>, Arc<dyn RunEvents>, Vec<OwnedRun>) =
        match &settings.source {
            Source::Scripted(path) => {
                let text =
                    std::fs::read_to_string(path).map_err(|error| DaemonError::ReadScript {
                        path: path.clone(),
                        error,
                    })?;
                let script = Script::parse(&text)?;
                let owned = script.runs.clone();
                let (runs, events) = script.replay();
                (runs, events, owned)
            }
            Source::Gateway { url, token } => {
                let gateway = Arc::new(HttpGateway::new(
                    url,
                    HttpClient::new()?,
                    Arc::new(StaticBearer::new(token.clone())),
                ));
                let owned = discover(gateway.as_ref()).await?;
                (gateway.clone(), gateway, owned)
            }
        };

    let listener = socket::bind(&settings.socket).map_err(socket_error)?;
    let hub = Arc::new(Hub::default());
    let api = Arc::new(ClientApi::new(
        concat!("vetinarid ", env!("CARGO_PKG_VERSION")),
        settings.worker.clone(),
    ));
    let (local, touched) = LocalBoard::new();
    let feed = Feed {
        runs,
        events,
        clock: Arc::new(TokioClock::starting_at(OffsetDateTime::now_utc())),
        template: TemplateKind::FeatureFromTicket,
        hub: hub.clone(),
        local,
        steps: None,
    };
    let feeding = tokio::spawn(feed_runs(feed, owned, touched));
    let served = tokio::select! {
        served = socket::serve(listener, api, hub) => served.map_err(socket_error),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    feeding.abort();
    let _ = std::fs::remove_file(&settings.socket);
    served
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use time::macros::datetime;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use vetinari_core::application::devflow::ports::CancelReply;
    use vetinari_core::application::ports::PortResult;
    use vetinari_core::domain::devflow::compile::WorkflowGraph;
    use vetinari_core::domain::devflow::ids::{DefinitionId, RunId};
    use vetinari_core::domain::devflow::run_view::{RunGraph, RunHeader, RunStatus};

    use super::*;

    struct Listing;

    #[async_trait]
    impl RunGateway for Listing {
        async fn register(&self, _graph: &WorkflowGraph) -> PortResult<()> {
            unreachable!()
        }
        async fn start(&self, _definition: &DefinitionId, _input: &Value) -> PortResult<RunId> {
            unreachable!()
        }
        async fn remove(&self, _definition: &DefinitionId) -> PortResult<()> {
            unreachable!()
        }
        async fn definitions(&self) -> PortResult<Vec<DefinitionId>> {
            Ok(vec![
                DefinitionId::parse("vetinari-feature-01A").expect("id"),
                DefinitionId::parse("vetinari-feature-01B").expect("id"),
            ])
        }
        async fn runs(&self, definition: &DefinitionId) -> PortResult<Vec<RunHeader>> {
            Ok(vec![RunHeader {
                id: RunId::parse(&format!("run-of-{definition}")).expect("run"),
                definition: definition.clone(),
                status: RunStatus::Active,
                created_at: datetime!(2026-10-25 09:00 UTC),
            }])
        }
        async fn graph(&self, _run: &RunId) -> PortResult<RunGraph> {
            unreachable!()
        }
        async fn cancel(&self, _run: &RunId) -> PortResult<CancelReply> {
            unreachable!()
        }
        async fn outputs(&self, _run: &RunId) -> PortResult<BTreeMap<String, Value>> {
            unreachable!()
        }
    }

    #[test]
    fn a_worker_label_comes_from_the_hostname_or_falls_back() {
        assert_eq!(worker_from_hostname(Some("Hex.local\n")).as_str(), "hex");
        assert_eq!(worker_from_hostname(Some("my_box")).as_str(), "vetinari");
        assert_eq!(worker_from_hostname(None).as_str(), "vetinari");
    }

    #[test]
    fn the_socket_lives_in_a_vetinari_directory() {
        let socket = default_socket();
        assert_eq!(
            socket.file_name().and_then(|name| name.to_str()),
            Some("vetinarid.sock")
        );
        assert_eq!(
            socket
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("vetinari")),
            Some(true)
        );
    }

    #[tokio::test]
    async fn every_run_of_every_vetinari_definition_is_owned() {
        let owned = discover(&Listing).await.expect("discover");
        assert_eq!(
            owned
                .iter()
                .map(|run| run.run.as_str().to_owned())
                .collect::<Vec<_>>(),
            ["run-of-vetinari-feature-01A", "run-of-vetinari-feature-01B"]
        );
        assert_eq!(owned[0].label, None);
        assert_eq!(owned[0].started_at, Some(datetime!(2026-10-25 09:00 UTC)));
    }

    #[tokio::test]
    async fn a_scripted_daemon_serves_its_runs_over_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("run.json");
        let mut text: Value =
            serde_json::from_str(include_str!("../tests/fixtures/feature-run.script.json"))
                .expect("script");
        text["steps"] = json!([]);
        std::fs::write(&script, text.to_string()).expect("write script");
        let path = dir.path().join("vetinari").join("vetinarid.sock");
        let daemon = tokio::spawn(run(Settings {
            socket: path.clone(),
            worker: WorkerName::parse("hex").expect("worker"),
            source: Source::Scripted(script),
        }));
        let stream = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(!daemon.is_finished(), "the daemon stopped");
                if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the socket within five seconds");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(b"{\"id\":1,\"method\":\"runs.watch\",\"params\":{}}\n")
            .await
            .expect("write");
        let mut runs = Vec::new();
        while runs.is_empty() {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("a frame within five seconds")
                .expect("read")
                .expect("a line");
            let frame: Value = serde_json::from_str(&line).expect("JSON");
            let event = &frame["event"];
            if event["type"] == json!("snapshot") {
                runs = event["runs"].as_array().cloned().unwrap_or_default();
            } else if event["type"] == json!("run") {
                runs.push(event.clone());
            }
        }
        assert_eq!(runs[0]["ticket"], json!("ENG-412"));
        assert_eq!(runs[0]["phases"][0]["glyph"], json!("◐"));
    }
}
