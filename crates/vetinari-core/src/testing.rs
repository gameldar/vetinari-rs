//! In-memory implementations of every port, for testing the application
//! services without a terminal, a network, or a keychain.
//!
//! Compiled for this crate's own tests, and for downstream crates that enable
//! the `testing` feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::application::ports::{
    AgentEngine, AttentionSource, ChatSource, Clock, CodeHostSource, CredentialStore,
    DeviceCodeChallenge, DeviceCodeFlow, DeviceCodePoll, ExternalAgentSource, IssueTrackerSource,
    PortError, PortResult, ScheduleSource, SkillStore, WorkspaceTarget, WorktreeManager,
};
use crate::domain::agent::{AgentSession, AgentStatus, ObservedAgent, TaskSpec};
use crate::domain::attention::{
    AttentionItem, IssueRef, MessageRef, PullRequestRef, SourceCategory,
};
use crate::domain::credentials::{CredentialKey, Provider, StoredCredential};
use crate::domain::git::{Worktree, WorktreeSpec};
use crate::domain::ids::{AgentId, SessionId, Slug, SourceId, SpaceId};
use crate::domain::skill::SkillLibrary;
use crate::domain::workspace::{Space, SpaceName, TargetKind};

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct FixedClock(Mutex<OffsetDateTime>);

impl FixedClock {
    /// Start the clock at an instant.
    #[must_use]
    pub fn new(now: OffsetDateTime) -> Self {
        Self(Mutex::new(now))
    }

    /// Move the clock forward.
    pub fn advance(&self, by: time::Duration) {
        let mut guard = self.0.lock().expect("clock lock");
        *guard += by;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().expect("clock lock")
    }
}

/// An attention source that returns whatever it was given.
#[derive(Debug)]
pub struct FakeSource {
    id: SourceId,
    category: SourceCategory,
    items: Mutex<Vec<AttentionItem>>,
    failure: Mutex<Option<PortError>>,
    fetches: Mutex<usize>,
    accounts: Mutex<Vec<Option<Slug>>>,
    pub(crate) calls: Mutex<Vec<String>>,
}

impl FakeSource {
    /// Build a source with items to hand out.
    #[must_use]
    pub fn new(id: SourceId, category: SourceCategory, items: Vec<AttentionItem>) -> Self {
        Self {
            id,
            category,
            items: Mutex::new(items),
            failure: Mutex::new(None),
            fetches: Mutex::new(0),
            accounts: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Build a source that always fails.
    #[must_use]
    pub fn failing(id: SourceId, category: SourceCategory, error: PortError) -> Self {
        let source = Self::new(id, category, Vec::new());
        *source.failure.lock().expect("failure lock") = Some(error);
        source
    }

    /// Replace the items it will hand out next.
    pub fn set_items(&self, items: Vec<AttentionItem>) {
        *self.items.lock().expect("items lock") = items;
    }

    /// How many times it has been fetched.
    #[must_use]
    pub fn fetch_count(&self) -> usize {
        *self.fetches.lock().expect("fetches lock")
    }

    /// The side-effecting calls it has received, each prefixed with the
    /// credential account it was made as, so a test can tell whether the
    /// active profile's account actually reached the adapter.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    /// The account each fetch was made as.
    #[must_use]
    pub fn accounts(&self) -> Vec<Option<Slug>> {
        self.accounts.lock().expect("accounts lock").clone()
    }

    fn record(&self, account: Option<&Slug>, call: String) {
        self.calls.lock().expect("calls lock").push(format!(
            "[{}] {call}",
            account.map_or("default", Slug::as_str)
        ));
    }
}

#[async_trait]
impl AttentionSource for FakeSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn category(&self) -> SourceCategory {
        self.category
    }

    async fn fetch(&self, account: Option<&Slug>) -> PortResult<Vec<AttentionItem>> {
        *self.fetches.lock().expect("fetches lock") += 1;
        self.accounts
            .lock()
            .expect("accounts lock")
            .push(account.cloned());
        if let Some(error) = self.failure.lock().expect("failure lock").clone() {
            return Err(error);
        }
        Ok(self.items.lock().expect("items lock").clone())
    }
}

#[async_trait]
impl CodeHostSource for FakeSource {
    async fn approve(
        &self,
        account: Option<&Slug>,
        pr: &PullRequestRef,
        comment: Option<&str>,
    ) -> PortResult<()> {
        self.record(account, format!("approve {} {:?}", pr.number, comment));
        Ok(())
    }

    async fn comment(
        &self,
        account: Option<&Slug>,
        pr: &PullRequestRef,
        body: &str,
    ) -> PortResult<()> {
        self.record(account, format!("comment {} {body}", pr.number));
        Ok(())
    }
}

#[async_trait]
impl IssueTrackerSource for FakeSource {
    async fn transition(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        status: &str,
    ) -> PortResult<()> {
        self.record(account, format!("transition {} {status}", issue.key));
        Ok(())
    }

    async fn assign(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        assignee: &str,
    ) -> PortResult<()> {
        self.record(account, format!("assign {} {assignee}", issue.key));
        Ok(())
    }
}

#[async_trait]
impl ChatSource for FakeSource {
    async fn reply(
        &self,
        account: Option<&Slug>,
        message: &MessageRef,
        body: &str,
    ) -> PortResult<()> {
        self.record(account, format!("reply {} {body}", message.channel));
        Ok(())
    }
}

#[async_trait]
impl ScheduleSource for FakeSource {
    async fn agenda(
        &self,
        account: Option<&Slug>,
        _from: OffsetDateTime,
        _to: OffsetDateTime,
    ) -> PortResult<Vec<AttentionItem>> {
        self.fetch(account).await
    }
}

/// An agent engine that records what it was asked to run.
#[derive(Debug)]
pub struct FakeAgentEngine {
    id: AgentId,
    available: bool,
    spawned: Mutex<Vec<TaskSpec>>,
    statuses: Mutex<BTreeMap<String, AgentStatus>>,
    answers: Mutex<Vec<(String, String)>>,
    cancelled: Mutex<Vec<String>>,
    next_id: Mutex<usize>,
    spawn_failure: Mutex<Option<PortError>>,
}

impl FakeAgentEngine {
    /// Build an available engine.
    #[must_use]
    pub fn new(id: AgentId) -> Self {
        Self {
            id,
            available: true,
            spawned: Mutex::new(Vec::new()),
            statuses: Mutex::new(BTreeMap::new()),
            answers: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
            spawn_failure: Mutex::new(None),
        }
    }

    /// Build an engine that reports itself unavailable.
    #[must_use]
    pub fn unavailable(id: AgentId) -> Self {
        Self {
            available: false,
            ..Self::new(id)
        }
    }

    /// Make the next spawn fail.
    pub fn fail_spawn(&self, error: PortError) {
        *self.spawn_failure.lock().expect("failure lock") = Some(error);
    }

    /// Set the status a session will report.
    pub fn set_status(&self, id: &SessionId, status: AgentStatus) {
        self.statuses
            .lock()
            .expect("statuses lock")
            .insert(id.as_str().to_owned(), status);
    }

    /// Forget a session's status, as if the engine had lost it.
    pub fn forget_status(&self, id: &SessionId) {
        self.statuses
            .lock()
            .expect("statuses lock")
            .remove(id.as_str());
    }

    /// The tasks it was asked to run.
    #[must_use]
    pub fn spawned(&self) -> Vec<TaskSpec> {
        self.spawned.lock().expect("spawned lock").clone()
    }

    /// The answers it was given.
    #[must_use]
    pub fn answers(&self) -> Vec<(String, String)> {
        self.answers.lock().expect("answers lock").clone()
    }

    /// The sessions it was asked to cancel.
    #[must_use]
    pub fn cancelled(&self) -> Vec<String> {
        self.cancelled.lock().expect("cancelled lock").clone()
    }
}

#[async_trait]
impl AgentEngine for FakeAgentEngine {
    fn id(&self) -> &AgentId {
        &self.id
    }

    async fn is_available(&self) -> bool {
        self.available
    }

    async fn spawn_task(&self, spec: &TaskSpec) -> PortResult<AgentSession> {
        if let Some(error) = self.spawn_failure.lock().expect("failure lock").take() {
            return Err(error);
        }
        let mut next = self.next_id.lock().expect("next id lock");
        *next += 1;
        let id = SessionId::parse(&format!("{}-{}", self.id, *next)).expect("generated id");
        self.spawned
            .lock()
            .expect("spawned lock")
            .push(spec.clone());
        self.statuses
            .lock()
            .expect("statuses lock")
            .insert(id.as_str().to_owned(), AgentStatus::Running);
        Ok(AgentSession::new(
            id,
            self.id.clone(),
            spec,
            OffsetDateTime::UNIX_EPOCH,
        ))
    }

    async fn check_status(&self, id: &SessionId) -> PortResult<AgentStatus> {
        self.statuses
            .lock()
            .expect("statuses lock")
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| PortError::not_found("session", id.as_str()))
    }

    async fn cancel(&self, id: &SessionId) -> PortResult<()> {
        self.cancelled
            .lock()
            .expect("cancelled lock")
            .push(id.as_str().to_owned());
        self.statuses
            .lock()
            .expect("statuses lock")
            .insert(id.as_str().to_owned(), AgentStatus::Cancelled);
        Ok(())
    }

    async fn answer(&self, id: &SessionId, answer: &str) -> PortResult<()> {
        self.answers
            .lock()
            .expect("answers lock")
            .push((id.as_str().to_owned(), answer.to_owned()));
        self.statuses
            .lock()
            .expect("statuses lock")
            .insert(id.as_str().to_owned(), AgentStatus::Running);
        Ok(())
    }

    async fn output(&self, _id: &SessionId, _max_lines: usize) -> PortResult<Vec<String>> {
        Ok(vec!["fake output".to_owned()])
    }

    fn command_line(&self, spec: &TaskSpec) -> PortResult<Vec<String>> {
        Ok(vec![
            self.id.as_str().to_owned(),
            spec.prompt().as_str().to_owned(),
        ])
    }
}

/// A workspace target that reports agents it did not start.
#[derive(Debug)]
pub struct FakeExternalAgentSource {
    target: TargetKind,
    agents: Mutex<Vec<ObservedAgent>>,
    failure: Mutex<Option<PortError>>,
    announces: bool,
}

impl FakeExternalAgentSource {
    /// A target that can see these agents.
    #[must_use]
    pub fn new(target: TargetKind, agents: Vec<ObservedAgent>) -> Self {
        Self {
            target,
            agents: Mutex::new(agents),
            failure: Mutex::new(None),
            announces: false,
        }
    }

    /// A target that cannot be reached.
    #[must_use]
    pub fn failing(target: TargetKind, error: PortError) -> Self {
        Self {
            target,
            agents: Mutex::new(Vec::new()),
            failure: Mutex::new(Some(error)),
            announces: false,
        }
    }

    /// Change what it can see.
    pub fn set_agents(&self, agents: Vec<ObservedAgent>) {
        *self.agents.lock().expect("agents lock") = agents;
    }

    /// Report a change whenever asked to wait for one.
    #[must_use]
    pub const fn announcing(mut self) -> Self {
        self.announces = true;
        self
    }
}

#[async_trait]
impl ExternalAgentSource for FakeExternalAgentSource {
    fn target(&self) -> TargetKind {
        self.target
    }

    async fn observed_agents(&self) -> PortResult<Vec<ObservedAgent>> {
        if let Some(error) = self.failure.lock().expect("failure lock").clone() {
            return Err(error);
        }
        Ok(self.agents.lock().expect("agents lock").clone())
    }

    async fn changed(&self) -> PortResult<()> {
        if self.announces {
            Ok(())
        } else {
            std::future::pending().await
        }
    }
}

/// A workspace target that keeps its spaces in memory.
#[derive(Debug)]
pub struct FakeWorkspaceTarget {
    kind: TargetKind,
    spaces: Mutex<Vec<Space>>,
    focused: Mutex<Option<SpaceId>>,
    commands: Mutex<Vec<(String, Vec<String>)>>,
    next_id: Mutex<usize>,
    failure: Mutex<Option<PortError>>,
}

impl FakeWorkspaceTarget {
    /// Build a target of a kind.
    #[must_use]
    pub fn new(kind: TargetKind) -> Self {
        Self {
            kind,
            spaces: Mutex::new(Vec::new()),
            focused: Mutex::new(None),
            commands: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
            failure: Mutex::new(None),
        }
    }

    /// Make the next space creation fail.
    pub fn fail_create(&self, error: PortError) {
        *self.failure.lock().expect("failure lock") = Some(error);
    }

    /// The spaces it holds.
    #[must_use]
    pub fn spaces(&self) -> Vec<Space> {
        self.spaces.lock().expect("spaces lock").clone()
    }

    /// The space currently focused.
    #[must_use]
    pub fn focused(&self) -> Option<SpaceId> {
        self.focused.lock().expect("focused lock").clone()
    }

    /// The commands it was asked to run.
    #[must_use]
    pub fn commands(&self) -> Vec<(String, Vec<String>)> {
        self.commands.lock().expect("commands lock").clone()
    }
}

#[async_trait]
impl WorkspaceTarget for FakeWorkspaceTarget {
    fn kind(&self) -> TargetKind {
        self.kind
    }

    async fn create_space(&self, name: &SpaceName, path: &Path) -> PortResult<SpaceId> {
        if let Some(error) = self.failure.lock().expect("failure lock").take() {
            return Err(error);
        }
        let mut next = self.next_id.lock().expect("next id lock");
        *next += 1;
        let id = SpaceId::parse(&format!("space-{}", *next)).expect("generated id");
        self.spaces.lock().expect("spaces lock").push(Space::new(
            id.clone(),
            name.clone(),
            path.to_path_buf(),
            self.kind,
        ));
        Ok(id)
    }

    async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        if !self
            .spaces
            .lock()
            .expect("spaces lock")
            .iter()
            .any(|space| space.id() == id)
        {
            return Err(PortError::not_found("space", id.as_str()));
        }
        *self.focused.lock().expect("focused lock") = Some(id.clone());
        Ok(())
    }

    async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        let mut spaces = self.spaces.lock().expect("spaces lock");
        let before = spaces.len();
        spaces.retain(|space| space.id() != id);
        if spaces.len() == before {
            return Err(PortError::not_found("space", id.as_str()));
        }
        Ok(())
    }

    async fn list_spaces(&self) -> PortResult<Vec<Space>> {
        Ok(self.spaces.lock().expect("spaces lock").clone())
    }

    async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()> {
        self.commands
            .lock()
            .expect("commands lock")
            .push((id.as_str().to_owned(), command.to_vec()));
        Ok(())
    }
}

/// A worktree manager that pretends to create directories.
#[derive(Debug, Default)]
pub struct FakeWorktreeManager {
    created: Mutex<Vec<WorktreeSpec>>,
    removed: Mutex<Vec<PathBuf>>,
    fetched: Mutex<Vec<(PathBuf, String, String)>>,
    failure: Mutex<Option<PortError>>,
    fetch_failure: Mutex<Option<PortError>>,
}

impl FakeWorktreeManager {
    /// Make the next creation fail.
    pub fn fail_create(&self, error: PortError) {
        *self.failure.lock().expect("failure lock") = Some(error);
    }

    /// Make every fetch fail, as it would with no remote configured.
    pub fn fail_fetch(&self, error: PortError) {
        *self.fetch_failure.lock().expect("fetch failure lock") = Some(error);
    }

    /// The refspecs it was asked to fetch.
    #[must_use]
    pub fn fetched(&self) -> Vec<(PathBuf, String, String)> {
        self.fetched.lock().expect("fetched lock").clone()
    }

    /// The specs it was asked to create.
    #[must_use]
    pub fn created(&self) -> Vec<WorktreeSpec> {
        self.created.lock().expect("created lock").clone()
    }

    /// The worktrees it was asked to remove.
    #[must_use]
    pub fn removed(&self) -> Vec<PathBuf> {
        self.removed.lock().expect("removed lock").clone()
    }
}

#[async_trait]
impl WorktreeManager for FakeWorktreeManager {
    async fn fetch(&self, repo: &Path, remote: &str, refspec: &str) -> PortResult<()> {
        self.fetched.lock().expect("fetched lock").push((
            repo.to_path_buf(),
            remote.to_owned(),
            refspec.to_owned(),
        ));
        match self
            .fetch_failure
            .lock()
            .expect("fetch failure lock")
            .clone()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn create(&self, spec: &WorktreeSpec) -> PortResult<Worktree> {
        if let Some(error) = self.failure.lock().expect("failure lock").take() {
            return Err(error);
        }
        self.created
            .lock()
            .expect("created lock")
            .push(spec.clone());
        Ok(Worktree::new(
            spec.path().to_path_buf(),
            spec.branch().clone(),
            spec.repo().to_path_buf(),
        ))
    }

    async fn remove(&self, worktree: &Worktree) -> PortResult<()> {
        self.removed
            .lock()
            .expect("removed lock")
            .push(worktree.path().to_path_buf());
        Ok(())
    }

    async fn list(&self, repo: &Path) -> PortResult<Vec<Worktree>> {
        Ok(self
            .created
            .lock()
            .expect("created lock")
            .iter()
            .filter(|spec| spec.repo() == repo)
            .map(|spec| {
                Worktree::new(
                    spec.path().to_path_buf(),
                    spec.branch().clone(),
                    spec.repo().to_path_buf(),
                )
            })
            .collect())
    }
}

/// A credential store backed by a map.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore(Mutex<BTreeMap<String, StoredCredential>>);

impl MemoryCredentialStore {
    /// Seed a credential.
    pub fn seed(&self, key: &CredentialKey, credential: StoredCredential) {
        self.0
            .lock()
            .expect("store lock")
            .insert(key.to_string(), credential);
    }

    /// How many credentials are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().expect("store lock").len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn get(&self, key: &CredentialKey) -> PortResult<Option<StoredCredential>> {
        Ok(self
            .0
            .lock()
            .expect("store lock")
            .get(&key.to_string())
            .cloned())
    }

    async fn set(&self, key: &CredentialKey, credential: &StoredCredential) -> PortResult<()> {
        self.0
            .lock()
            .expect("store lock")
            .insert(key.to_string(), credential.clone());
        Ok(())
    }

    async fn delete(&self, key: &CredentialKey) -> PortResult<bool> {
        Ok(self
            .0
            .lock()
            .expect("store lock")
            .remove(&key.to_string())
            .is_some())
    }
}

/// A credential store that cannot be reached, as on a machine with no
/// keychain service running.
#[derive(Debug)]
pub struct UnavailableCredentialStore;

#[async_trait]
impl CredentialStore for UnavailableCredentialStore {
    async fn get(&self, _key: &CredentialKey) -> PortResult<Option<StoredCredential>> {
        Err(PortError::unavailable(
            "keychain",
            "no usable credential store",
        ))
    }

    async fn set(&self, _key: &CredentialKey, _credential: &StoredCredential) -> PortResult<()> {
        Err(PortError::unavailable(
            "keychain",
            "no usable credential store",
        ))
    }

    async fn delete(&self, _key: &CredentialKey) -> PortResult<bool> {
        Err(PortError::unavailable(
            "keychain",
            "no usable credential store",
        ))
    }
}

/// A skill store backed by memory.
#[derive(Debug)]
pub struct MemorySkillStore(Mutex<SkillLibrary>);

impl Default for MemorySkillStore {
    fn default() -> Self {
        Self(Mutex::new(SkillLibrary::builtin()))
    }
}

impl MemorySkillStore {
    /// Build a store holding a specific library.
    #[must_use]
    pub fn with_library(library: SkillLibrary) -> Self {
        Self(Mutex::new(library))
    }

    /// The library as it stands.
    #[must_use]
    pub fn library(&self) -> SkillLibrary {
        self.0.lock().expect("library lock").clone()
    }
}

#[async_trait]
impl SkillStore for MemorySkillStore {
    async fn load(&self) -> PortResult<SkillLibrary> {
        Ok(self.0.lock().expect("library lock").clone())
    }

    async fn save(&self, library: &SkillLibrary) -> PortResult<()> {
        *self.0.lock().expect("library lock") = library.clone();
        Ok(())
    }
}

/// A device-code flow that grants on the second poll.
#[derive(Debug)]
pub struct FakeDeviceCodeFlow {
    polls: Mutex<usize>,
    polls_before_grant: usize,
    challenge_expiry: OffsetDateTime,
}

impl FakeDeviceCodeFlow {
    /// Build a flow that grants after a number of polls.
    #[must_use]
    pub fn new(polls_before_grant: usize, challenge_expiry: OffsetDateTime) -> Self {
        Self {
            polls: Mutex::new(0),
            polls_before_grant,
            challenge_expiry,
        }
    }

    /// How many times it was polled.
    #[must_use]
    pub fn poll_count(&self) -> usize {
        *self.polls.lock().expect("polls lock")
    }
}

#[async_trait]
impl DeviceCodeFlow for FakeDeviceCodeFlow {
    async fn start(&self, _provider: Provider) -> PortResult<DeviceCodeChallenge> {
        Ok(DeviceCodeChallenge {
            user_code: "WXYZ-1234".to_owned(),
            verification_uri: "https://example.test/device".to_owned(),
            device_code: "dev-code".to_owned(),
            interval_secs: 5,
            expires_at: self.challenge_expiry,
        })
    }

    async fn poll(
        &self,
        _provider: Provider,
        _challenge: &DeviceCodeChallenge,
    ) -> PortResult<DeviceCodePoll> {
        let mut polls = self.polls.lock().expect("polls lock");
        *polls += 1;
        if *polls > self.polls_before_grant {
            Ok(DeviceCodePoll::Granted(StoredCredential::oauth(
                crate::domain::credentials::Secret::new("granted-access"),
                Some(crate::domain::credentials::Secret::new("granted-refresh")),
                Some(self.challenge_expiry),
            )))
        } else {
            Ok(DeviceCodePoll::Pending)
        }
    }

    async fn refresh(
        &self,
        _provider: Provider,
        _credential: &StoredCredential,
    ) -> PortResult<StoredCredential> {
        Ok(StoredCredential::oauth(
            crate::domain::credentials::Secret::new("refreshed-access"),
            Some(crate::domain::credentials::Secret::new("granted-refresh")),
            Some(self.challenge_expiry + time::Duration::hours(1)),
        ))
    }
}

/// Items as every source that reported them would answer a poll.
#[must_use]
pub fn polled(
    items: Vec<crate::domain::attention::AttentionItem>,
) -> Vec<crate::application::attention::Polled> {
    let mut by_source: BTreeMap<SourceId, Vec<_>> = BTreeMap::new();
    for item in items {
        by_source
            .entry(item.source().clone())
            .or_default()
            .push(item);
    }
    by_source
        .into_iter()
        .map(|(source, items)| crate::application::attention::Polled {
            source,
            outcome: Ok(items),
        })
        .collect()
}
