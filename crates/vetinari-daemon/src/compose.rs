//! The daemon's composition root: which adapters serve which ports, as
//! `vetinari-tui/src/startup.rs` is the TUI's.
//!
//! A scripted daemon serves the run board only. A gateway daemon also
//! claims and runs steps, with the profiles, agent engines and workspace
//! targets of vetinari's own configuration.

use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc;
use vetinari_adapters::autognomus::client::{AuthBearer, HttpGateway};
use vetinari_adapters::config::{candidate_paths, load_or_default};
use vetinari_adapters::credentials::KeyringStore;
use vetinari_adapters::daemon::client::DaemonRuns;
use vetinari_adapters::devflow::{FileLeaseJournal, FsStepFiles};
use vetinari_adapters::git::GitWorktreeManager;
use vetinari_adapters::oauth::OidcDeviceCodeFlow;
use vetinari_adapters::skills::FileSkillStore;
use vetinari_adapters::{HttpClient, Paths, Runtime};
use vetinari_core::application::auth::{AuthService, AuthState};
use vetinari_core::application::config::AutognomusConfig;
use vetinari_core::application::config::{Config, ParseContext};
use vetinari_core::application::devflow::api::ClientApi;
use vetinari_core::application::devflow::ports::PullRequestPublisher;
use vetinari_core::application::devflow::ports::{RunEvents, RunGateway, TaskGateway};
use vetinari_core::application::devflow::worker::{StepWorker, WorkerPorts, WorkerSettings};
use vetinari_core::application::ports::DeviceCodeChallenge;
use vetinari_core::application::ports::{Clock, PortError};
use vetinari_core::application::ports::{PortResult, WorktreeManager};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::devflow::compile::StepPolicies;
use vetinari_core::domain::devflow::compile::WorkerQueues;
use vetinari_core::domain::devflow::ids::{RunId, WorkerName};
use vetinari_core::domain::devflow::outcome::OpenPrOutcome;
use vetinari_core::domain::devflow::step::StepConfig;
use vetinari_core::domain::devflow::template::TemplateKind;
use vetinari_core::domain::profile::Profile;

use crate::gates::{Gates, NoGates};
use crate::hub::Hub;
use crate::runtime::{Feed, OwnedRun, TokioClock, feed_runs};
use crate::scripted::{Script, ScriptError};
use crate::signin::{SignIn, SignInOutcome};
use crate::socket::{self, Backend};
use crate::steps::{LocalBoard, StepParts, claim_loop, spawn_steps};

/// Where the daemon's runs come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Replay a script.
    Scripted(PathBuf),
    /// The gateway `[autognomus]` names, signed in to its issuer.
    Gateway,
}

/// How the daemon is to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The socket to serve.
    pub socket: PathBuf,
    /// The worker label, when given; otherwise `[autognomus] worker`, then
    /// the hostname.
    pub worker: Option<WorkerName>,
    /// Where runs come from.
    pub source: Source,
    /// vetinari's configuration file, when not the usual one.
    pub config: Option<PathBuf>,
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
    /// vetinari's configuration could not be read.
    #[error("configuration: {0}")]
    Config(String),
    /// Signing in did not finish.
    #[error("sign-in: {0}")]
    SignIn(String),
}

/// The socket the daemon listens on and the TUI reads, unless told
/// otherwise; both sides share one definition so they cannot drift.
pub use vetinari_adapters::daemon::client::default_socket;

/// The worker label from a hostname: its first label, when that is a valid
/// worker name, else `vetinari`.
#[must_use]
pub fn worker_from_hostname(hostname: Option<&str>) -> WorkerName {
    hostname
        .and_then(|name| name.trim().split('.').next())
        .and_then(|name| WorkerName::parse(name).ok())
        .unwrap_or_else(|| WorkerName::parse("vetinari").expect("a valid worker name"))
}

/// The worker label: the one asked for, else the configured one, else
/// the hostname's.
#[must_use]
pub fn choose_worker(
    explicit: Option<WorkerName>,
    configured: Option<WorkerName>,
    hostname: Option<&str>,
) -> WorkerName {
    explicit
        .or(configured)
        .unwrap_or_else(|| worker_from_hostname(hostname))
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
}

/// The gateway a daemon works for, which only `[autognomus]` names.
pub fn gateway_config(config: &Config) -> Result<&AutognomusConfig, DaemonError> {
    config.autognomus.as_ref().ok_or_else(|| {
        DaemonError::Config(
            "no gateway is configured: add [autognomus] with gateway_url, issuer and client_id"
                .to_owned(),
        )
    })
}

/// The credential service for the gateway's issuer. Only the daemon builds
/// one: refresh tokens rotate on use.
fn gateway_auth(
    autognomus: &AutognomusConfig,
    http: HttpClient,
    clock: Arc<dyn Clock>,
) -> Arc<AuthService> {
    Arc::new(AuthService::new(
        Arc::new(KeyringStore),
        Arc::new(OidcDeviceCodeFlow::new(
            Provider::Autognomus,
            autognomus.oidc.clone(),
            http,
            clock.clone(),
        )),
        clock,
    ))
}

/// Find the gateway's runs once the daemon is signed in, for a daemon that
/// started without a usable token.
async fn discover_once_signed_in(
    runs: Arc<dyn RunGateway>,
    sign_in: Arc<SignIn>,
    local: Arc<LocalBoard>,
) {
    sign_in.signed_in().await;
    match discover(runs.as_ref()).await {
        Ok(owned) => {
            for run in owned {
                local.adopt(run.run);
            }
        }
        Err(error) => tracing::warn!(%error, "cannot list the gateway's runs after signing in"),
    }
}

/// Sign the daemon in on stdout (design 12). A running daemon is asked to
/// sign itself in, since it holds the credential; with none running the
/// flow runs here and stores what it is granted for the next start.
pub async fn login(socket: PathBuf, config: Option<PathBuf>) -> Result<(), DaemonError> {
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        let daemon = DaemonRuns::new(socket, "vetinarid login");
        let challenge = daemon.begin_sign_in().await?;
        print_challenge(&challenge);
        loop {
            tokio::time::sleep(Duration::from_secs(challenge.interval_secs.max(1))).await;
            match daemon.sign_in_state().await? {
                AuthState::Valid | AuthState::NearingExpiry => break,
                _ if OffsetDateTime::now_utc() >= challenge.expires_at => {
                    return Err(DaemonError::SignIn("the code expired".to_owned()));
                }
                _ => {}
            }
        }
    } else {
        let (config, _) = load_config(config)?;
        let autognomus = gateway_config(&config)?.clone();
        let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(OffsetDateTime::now_utc()));
        let sign_in = SignIn::new(gateway_auth(&autognomus, HttpClient::new()?, clock));
        let (challenge, ended) = sign_in.begin().await?;
        print_challenge(&challenge);
        match ended.await {
            Ok(SignInOutcome::Granted) => {}
            Ok(SignInOutcome::Rejected(reason)) => return Err(DaemonError::SignIn(reason)),
            Err(_) => return Err(DaemonError::SignIn("the sign-in was abandoned".to_owned())),
        }
    }
    println!("vetinarid is signed in to autognomus");
    Ok(())
}

fn print_challenge(challenge: &DeviceCodeChallenge) {
    println!(
        "Visit {} and enter {}",
        challenge.verification_uri, challenge.user_code
    );
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

/// The claiming half of a daemon: the task endpoints, the worker's queues,
/// and the worker with its agents.
pub struct Claims {
    /// The engine's task endpoints.
    pub tasks: Arc<dyn TaskGateway>,
    /// The queues to claim from.
    pub queues: WorkerQueues,
    /// The worker and what drives it.
    pub parts: StepParts,
}

/// Everything a daemon serves from, built by `run` or by a test.
pub struct Services {
    /// The run endpoints.
    pub runs: Arc<dyn RunGateway>,
    /// The run stream.
    pub events: Arc<dyn RunEvents>,
    /// The runs owned at start.
    pub owned: Vec<OwnedRun>,
    /// The worker label clients are greeted with.
    pub worker: WorkerName,
    /// The application's clock.
    pub clock: Arc<dyn Clock>,
    /// The worker, when this daemon claims steps; none for a script.
    pub claims: Option<Claims>,
    /// Who answers gates when there is no worker: a script's, or nobody.
    pub gates: Arc<dyn Gates>,
    /// This machine's steps, and the receiver that hears which changed.
    pub local: (Arc<LocalBoard>, mpsc::UnboundedReceiver<RunId>),
    /// How the daemon signs in to its gateway; none for a script or a
    /// fixed token.
    pub sign_in: Option<Arc<SignIn>>,
}

/// How many agent steps run at once until `[daemon] max_agent_steps` is
/// configurable (design 7.2).
const MAX_AGENT_STEPS: NonZeroUsize = NonZeroUsize::new(2).expect("non-zero");

/// Stands in for the pull request publisher until its GitHub adapter
/// exists (slice 15): an `open-pr` step fails, transiently, so a later
/// daemon can still open it.
struct NoPublisher;

#[async_trait::async_trait]
impl PullRequestPublisher for NoPublisher {
    async fn publish(&self, _config: &StepConfig, _profile: &Profile) -> PortResult<OpenPrOutcome> {
        Err(PortError::unavailable(
            "pull request publisher",
            "this daemon cannot open pull requests yet",
        ))
    }
}

/// The worker for a gateway, from vetinari's own configuration: its
/// profiles, agent engines and workspace targets, as the TUI reads them.
pub fn claims(
    config: Config,
    paths: &Paths,
    gateway: Arc<HttpGateway>,
    worker: &WorkerName,
    clock: Arc<dyn Clock>,
) -> Result<Claims, DaemonError> {
    let tick = Duration::from_millis(config.general.tick_rate_ms);
    let profiles = config.profiles.clone();
    let runtime = Runtime::build(config, paths.clone())?;
    for problem in &runtime.problems {
        tracing::warn!(%problem, "configuration");
    }
    let worktrees: Arc<dyn WorktreeManager> = Arc::new(GitWorktreeManager::default());
    let queues = WorkerQueues::for_worker(worker);
    let step_worker = StepWorker::new(
        WorkerPorts {
            tasks: gateway.clone(),
            runs: gateway.clone(),
            journal: Arc::new(FileLeaseJournal::new(FileLeaseJournal::default_path(
                &paths.state_dir,
            ))),
            files: Arc::new(FsStepFiles::default()),
            worktrees,
            skills: Arc::new(FileSkillStore::new(FileSkillStore::default_path(
                &paths.config_dir,
            ))),
            publisher: Arc::new(NoPublisher),
            clock: clock.clone(),
        },
        Arc::new(runtime.workflows),
        profiles,
        WorkerSettings {
            name: worker.clone(),
            queues: queues.clone(),
            max_agent_steps: MAX_AGENT_STEPS,
            policies: StepPolicies::default(),
        },
    );
    Ok(Claims {
        tasks: gateway,
        queues,
        parts: StepParts {
            worker: step_worker,
            registry: runtime.agents,
            clock,
            tick,
        },
    })
}

/// vetinari's configuration and directories, found as the TUI finds them.
fn load_config(explicit: Option<PathBuf>) -> Result<(Config, Paths), DaemonError> {
    let dirs = directories::BaseDirs::new();
    let config_dir = dirs.as_ref().map(|dirs| dirs.config_dir().to_path_buf());
    let state_dir = dirs.as_ref().map(|dirs| {
        dirs.state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf()
    });
    let home = dirs.as_ref().map(|dirs| dirs.home_dir().to_path_buf());
    let candidates = match explicit {
        Some(path) => vec![path],
        None => config_dir
            .as_deref()
            .map(candidate_paths)
            .unwrap_or_default(),
    };
    let loaded = load_or_default(&candidates, &ParseContext { home })
        .map_err(|error| DaemonError::Config(error.to_string()))?;
    Ok((
        loaded.config,
        Paths {
            config_dir: config_dir.unwrap_or_else(|| PathBuf::from(".")),
            state_dir: state_dir.unwrap_or_else(|| PathBuf::from(".")),
        },
    ))
}

/// Run the daemon until it is interrupted or its socket fails.
pub async fn run(settings: Settings) -> Result<(), DaemonError> {
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(OffsetDateTime::now_utc()));
    let services = match &settings.source {
        Source::Scripted(path) => {
            let text = std::fs::read_to_string(path).map_err(|error| DaemonError::ReadScript {
                path: path.clone(),
                error,
            })?;
            let script = Script::parse(&text)?;
            let owned = script.runs.clone();
            let local = LocalBoard::new();
            let (runs, events, gates) = script.replay(local.0.clone());
            Services {
                runs,
                events,
                owned,
                worker: choose_worker(settings.worker.clone(), None, hostname().as_deref()),
                clock,
                claims: None,
                gates,
                local,
                sign_in: None,
            }
        }
        Source::Gateway => {
            let (config, paths) = load_config(settings.config.clone())?;
            let autognomus = gateway_config(&config)?.clone();
            let worker = choose_worker(
                settings.worker.clone(),
                autognomus.worker.clone(),
                hostname().as_deref(),
            );
            let http = HttpClient::new()?;
            let auth = gateway_auth(&autognomus, http.clone(), clock.clone());
            let sign_in = SignIn::new(auth.clone());
            sign_in.check().await;
            let gateway = Arc::new(HttpGateway::new(
                autognomus.gateway_url.as_str(),
                http,
                Arc::new(AuthBearer::new(auth)),
            ));
            let local = LocalBoard::new();
            let owned = match discover(gateway.as_ref()).await {
                Ok(owned) => owned,
                Err(PortError::Unauthenticated { .. }) => {
                    tracing::warn!("autognomus needs a sign-in; serving until it has one");
                    sign_in.lost();
                    tokio::spawn(discover_once_signed_in(
                        gateway.clone(),
                        sign_in.clone(),
                        local.0.clone(),
                    ));
                    Vec::new()
                }
                Err(error) => return Err(error.into()),
            };
            let claims = claims(config, &paths, gateway.clone(), &worker, clock.clone())?;
            Services {
                runs: gateway.clone(),
                events: gateway,
                owned,
                worker,
                clock,
                claims: Some(claims),
                gates: Arc::new(NoGates),
                local,
                sign_in: Some(sign_in),
            }
        }
    };
    serve(settings.socket, services).await
}

/// Serve `services` on `socket` until interrupted or the socket fails: the
/// run board, and the worker's claim loops when there is a worker.
pub async fn serve(socket: PathBuf, services: Services) -> Result<(), DaemonError> {
    let socket_error = |error| DaemonError::Socket {
        path: socket.clone(),
        error,
    };
    let listener = socket::bind(&socket).map_err(socket_error)?;
    let hub = Arc::new(Hub::default());
    let api = Arc::new(ClientApi::new(
        concat!("vetinarid ", env!("CARGO_PKG_VERSION")),
        services.worker.clone(),
    ));
    let (local, touched) = services.local;
    let mut tasks = Vec::new();
    let steps = services.claims.map(|claims| {
        let (steps, owner) = spawn_steps(claims.parts, local.clone());
        tasks.push(owner);
        for queue in [claims.queues.agent, claims.queues.human] {
            tasks.push(tokio::spawn(claim_loop(
                claims.tasks.clone(),
                queue,
                services.worker.clone(),
                steps.clone(),
                services.sign_in.clone(),
            )));
        }
        steps
    });
    let gates: Arc<dyn Gates> = match &steps {
        Some(steps) => Arc::new(steps.clone()),
        None => services.gates,
    };
    let backend = Arc::new(Backend {
        runs: services.runs.clone(),
        gates,
        local: local.clone(),
        template: TemplateKind::FeatureFromTicket,
        sign_in: services.sign_in.clone(),
    });
    let feed = Feed {
        runs: services.runs,
        events: services.events,
        clock: services.clock,
        template: TemplateKind::FeatureFromTicket,
        hub: hub.clone(),
        local,
        steps,
    };
    tasks.push(tokio::spawn(feed_runs(feed, services.owned, touched)));
    let served = tokio::select! {
        served = socket::serve(listener, api, hub, backend) => served.map_err(socket_error),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    for task in tasks {
        task.abort();
    }
    let _ = std::fs::remove_file(&socket);
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
    use vetinari_core::domain::devflow::ids::DefinitionId;
    use vetinari_core::domain::devflow::run_view::{RunGraph, RunHeader, RunStatus};

    use vetinari_core::application::config::RawConfig;

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

    #[tokio::test]
    async fn a_gateway_daemon_claims_from_its_own_queues_at_the_configured_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::parse(RawConfig::default(), &ParseContext::default())
            .expect("the default config");
        config.general.tick_rate_ms = 250;
        let gateway = Arc::new(HttpGateway::new(
            "http://127.0.0.1:1",
            HttpClient::new().expect("client"),
            Arc::new(vetinari_adapters::autognomus::client::StaticBearer::new(
                "token",
            )),
        ));
        let clock: Arc<dyn Clock> =
            Arc::new(TokioClock::starting_at(datetime!(2026-10-25 09:00 UTC)));

        let claims = claims(
            config,
            &Paths {
                config_dir: dir.path().join("config"),
                state_dir: dir.path().join("state"),
            },
            gateway,
            &WorkerName::parse("hex").expect("worker"),
            clock,
        )
        .expect("claims");

        assert_eq!(claims.queues.agent.as_str(), "vetinari-hex-agent");
        assert_eq!(claims.queues.human.as_str(), "vetinari-hex-human");
        assert_eq!(claims.parts.tick, Duration::from_millis(250));
        assert!(
            claims
                .parts
                .registry
                .engine_ids()
                .iter()
                .any(|id| id.as_str() == "claude-code"),
            "the configured engines are registered"
        );
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
            worker: Some(WorkerName::parse("hex").expect("worker")),
            source: Source::Scripted(script),
            config: None,
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

    fn configured(toml_text: &str) -> Config {
        let raw: RawConfig = toml::from_str(toml_text).expect("TOML");
        Config::parse(raw, &ParseContext::default()).expect("config")
    }

    #[test]
    fn the_worker_label_is_the_flags_then_the_configs_then_the_hostnames() {
        let flag = WorkerName::parse("flag").expect("worker");
        let file = WorkerName::parse("file").expect("worker");
        assert_eq!(
            choose_worker(Some(flag.clone()), Some(file.clone()), Some("host")),
            flag
        );
        assert_eq!(choose_worker(None, Some(file.clone()), Some("host")), file);
        assert_eq!(
            choose_worker(None, None, Some("host.local")).as_str(),
            "host"
        );
    }

    #[test]
    fn a_gateway_daemon_needs_the_autognomus_table() {
        let error = gateway_config(&configured("")).expect_err("no table");
        assert!(error.to_string().contains("[autognomus]"), "{error}");
        let config = configured(
            r#"
            [autognomus]
            gateway_url = "https://autognomus.example.net"
            issuer = "https://id.example.net"
            client_id = "vetinari"
            "#,
        );
        assert_eq!(
            gateway_config(&config).expect("table").gateway_url.as_str(),
            "https://autognomus.example.net"
        );
    }
}
