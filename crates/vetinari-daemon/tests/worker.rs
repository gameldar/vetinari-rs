//! vetinarid against a gateway played by a local HTTP server: a claimed
//! preflight runs in a real worktree, its agent writes the step result,
//! and the daemon completes the task with the parsed outcome.

mod gateway;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use vetinari_adapters::HttpClient;
use vetinari_adapters::autognomus::client::{AuthBearer, HttpGateway, StaticBearer};
use vetinari_adapters::daemon::client::DaemonRuns;
use vetinari_adapters::devflow::{FileLeaseJournal, FsStepFiles};
use vetinari_adapters::git::GitWorktreeManager;
use vetinari_core::application::agents::AgentRegistry;
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::devflow::worker::{StepWorker, WorkerPorts, WorkerSettings};
use vetinari_core::application::ports::AttentionSource;
use vetinari_core::application::ports::{AgentEngine, Clock, PortResult};
use vetinari_core::application::workflows::WorkflowEngine;
use vetinari_core::application::workspaces::WorkspaceDirectory;
use vetinari_core::domain::agent::{AgentSession, AgentStatus, TaskSpec};
use vetinari_core::domain::credentials::{CredentialKey, Provider, Secret, StoredCredential};
use vetinari_core::domain::devflow::compile::{StepPolicies, WorkerQueues};
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_core::domain::ids::{AgentId, ProfileId, SessionId};
use vetinari_core::domain::profile::{Profile, ProfileSet};
use vetinari_core::domain::text::NonEmptyText;
use vetinari_core::domain::workspace::TargetKind;
use vetinari_core::testing::{
    FakeDeviceCodeFlow, FakePullRequestPublisher, MemoryCredentialStore, MemorySkillStore,
};
use vetinari_daemon::compose::{Claims, Services, serve};
use vetinari_daemon::gates::NoGates;
use vetinari_daemon::launches::NoLaunches;
use vetinari_daemon::runtime::TokioClock;
use vetinari_daemon::signin::SignIn;
use vetinari_daemon::steps::{LocalBoard, StepParts};

use gateway::Gateway;

/// An agent that does its work at once: it writes the result it was told
/// to, then reports success.
struct WritesItsResult {
    id: AgentId,
    result: Value,
}

#[async_trait]
impl AgentEngine for WritesItsResult {
    fn id(&self) -> &AgentId {
        &self.id
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn spawn_task(&self, spec: &TaskSpec) -> PortResult<AgentSession> {
        let path = spec
            .env()
            .iter()
            .find(|(key, _)| key == "VETINARI_STEP_RESULT")
            .map(|(_, value)| PathBuf::from(value))
            .expect("the step names its result file");
        std::fs::create_dir_all(path.parent().expect("a directory")).expect("mkdir");
        std::fs::write(&path, self.result.to_string()).expect("write the result");
        Ok(AgentSession::new(
            SessionId::parse("writer-1").expect("id"),
            self.id.clone(),
            spec,
            OffsetDateTime::now_utc(),
        ))
    }

    async fn check_status(&self, _id: &SessionId) -> PortResult<AgentStatus> {
        Ok(AgentStatus::Succeeded)
    }

    async fn cancel(&self, _id: &SessionId) -> PortResult<()> {
        Ok(())
    }

    async fn answer(&self, _id: &SessionId, _answer: &str) -> PortResult<()> {
        Ok(())
    }

    async fn output(&self, _id: &SessionId, _max_lines: usize) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }

    fn command_line(&self, _spec: &TaskSpec) -> PortResult<Vec<String>> {
        Ok(vec!["writer".to_owned()])
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) -> PathBuf {
    let repo = root.join("web");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "dev@example.test"]);
    git(&repo, &["config", "user.name", "Dev"]);
    std::fs::write(repo.join("README.md"), "web\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

fn claim(repo: &Path) -> Value {
    let mut claim: Value = serde_json::from_str(include_str!(
        "../../vetinari-adapters/tests/fixtures/autognomus/claim-preflight-first.json"
    ))
    .expect("claim");
    claim["config"]["repo"]["path"] = json!(repo.display().to_string());
    claim["expires_at"] = json!("2999-01-01T00:00:00Z");
    claim
}

/// The claiming half of a daemon for `repo`'s gateway, whose one agent
/// writes `result` at once.
fn claims(dir: &Path, http: Arc<HttpGateway>, clock: Arc<dyn Clock>, result: Value) -> Claims {
    let worktrees = Arc::new(GitWorktreeManager::default());
    let worker_name = WorkerName::parse("hex").expect("worker");
    let mut registry = AgentRegistry::new(clock.clone());
    registry.register(Arc::new(WritesItsResult {
        id: AgentId::parse("claude-code").expect("id"),
        result,
    }));
    let profile = Profile::new(
        ProfileId::parse("work").expect("id"),
        NonEmptyText::parse("Work").expect("name"),
        TargetKind::Herdr,
    )
    .with_worktree_root(dir.join("trees"));
    let worker = StepWorker::new(
        WorkerPorts {
            tasks: http.clone(),
            runs: http.clone(),
            journal: Arc::new(FileLeaseJournal::new(dir.join("leases.json"))),
            files: Arc::new(FsStepFiles::default()),
            worktrees: worktrees.clone(),
            skills: Arc::new(MemorySkillStore::default()),
            publisher: Arc::new(FakePullRequestPublisher::default()),
            clock: clock.clone(),
        },
        Arc::new(WorkflowEngine::new(
            worktrees,
            WorkspaceDirectory::default(),
        )),
        ProfileSet::new(profile, Vec::new()).expect("profiles"),
        WorkerSettings {
            name: worker_name.clone(),
            queues: WorkerQueues::for_worker(&worker_name),
            max_agent_steps: NonZeroUsize::new(2).expect("non-zero"),
            policies: StepPolicies::default(),
        },
    );
    Claims {
        tasks: http,
        queues: WorkerQueues::for_worker(&worker_name),
        parts: StepParts {
            worker,
            registry,
            clock,
            tick: Duration::from_millis(100),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claimed_preflight_is_run_and_completed_with_its_outcome() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repository(dir.path());
    let gateway = Gateway::start(claim(&repo)).await;

    let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(OffsetDateTime::now_utc()));
    let http = Arc::new(HttpGateway::new(
        &gateway.base_url,
        HttpClient::new().expect("client"),
        Arc::new(StaticBearer::new("token")),
    ));
    let socket = dir.path().join("vetinarid.sock");
    let daemon = tokio::spawn(serve(
        socket,
        None,
        Services {
            runs: http.clone(),
            events: http.clone(),
            owned: Vec::new(),
            worker: WorkerName::parse("hex").expect("worker"),
            clock: clock.clone(),
            gates: Arc::new(NoGates),
            local: LocalBoard::new(),
            sign_in: None,
            hub: Arc::default(),
            launches: Arc::new(NoLaunches),
            changes: tokio::sync::mpsc::unbounded_channel().1,
            claims: Some(claims(
                dir.path(),
                http,
                clock,
                json!({ "schema": 1, "outcome": "ready", "summary": "Retry with backoff" }),
            )),
        },
    ));

    let completed = tokio::time::timeout(Duration::from_secs(20), gateway.completed())
        .await
        .expect("a completion within twenty seconds");
    daemon.abort();

    assert_eq!(
        completed.target,
        "/v1/tasks/01JB7ZR2M4Q9V8C6T3N5HKD0WX~preflight@1~1/complete"
    );
    let body = completed.json();
    assert_eq!(body["lease"], json!("01JB7ZS9D1F4G7H2J5K8M3N6P0"));
    assert_eq!(body["output"]["branch"], json!("ready"));
    assert_eq!(body["output"]["step"], json!("preflight"));
    assert_eq!(body["output"]["summary"], json!("Retry with backoff"));
    assert_eq!(
        body["output"]["git_branch"],
        json!("vetinari/eng-412-rate-limit-webhook-retries")
    );
    assert!(
        dir.path()
            .join("trees/web-eng-412-rate-limit-webhook-retries")
            .is_dir(),
        "the step ran in the run's worktree"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_whose_token_is_refused_keeps_serving_and_asks_for_a_sign_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gateway = Gateway::refusing().await;
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryCredentialStore::default());
    store.seed(
        &CredentialKey::new(Provider::Autognomus, None),
        StoredCredential::oauth(Secret::new("revoked"), None, None),
    );
    let auth = Arc::new(AuthService::new(
        store,
        Arc::new(FakeDeviceCodeFlow::new(0, OffsetDateTime::now_utc())),
        clock.clone(),
    ));
    let sign_in = SignIn::new(auth.clone());
    let http = Arc::new(HttpGateway::new(
        &gateway.base_url,
        HttpClient::new().expect("client"),
        Arc::new(AuthBearer::new(auth)),
    ));
    let socket = dir.path().join("vetinarid.sock");
    let daemon = tokio::spawn(serve(
        socket.clone(),
        None,
        Services {
            runs: http.clone(),
            events: http.clone(),
            owned: Vec::new(),
            worker: WorkerName::parse("hex").expect("worker"),
            clock: clock.clone(),
            gates: Arc::new(NoGates),
            local: LocalBoard::new(),
            sign_in: Some(sign_in),
            hub: Arc::default(),
            launches: Arc::new(NoLaunches),
            changes: tokio::sync::mpsc::unbounded_channel().1,
            claims: Some(claims(dir.path(), http, clock, json!({}))),
        },
    ));

    let runs = DaemonRuns::new(socket, "vetinari test");
    let _watching = runs.start();
    let mut changes = runs.changes();
    let asked = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let items = runs.fetch(None).await.unwrap_or_default();
            if items
                .iter()
                .any(|item| item.title().as_str() == "autognomus needs sign-in")
            {
                return;
            }
            let _ = changes.changed().await;
        }
    })
    .await;
    daemon.abort();
    asked.expect("the board asks for a sign-in within twenty seconds");
}
