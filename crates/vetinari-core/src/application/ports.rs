//! Ports: what the core needs from the outside world.
//!
//! Every trait here is defined in terms of domain types only. Infrastructure
//! lives in `vetinari-adapters` and depends on this crate, never the other way
//! round, so the application services can be exercised against in-memory
//! fakes.

use std::path::Path;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::domain::agent::{AgentSession, AgentStatus, ObservedAgent, TaskSpec};
use crate::domain::attention::{
    AttentionItem, IssueRef, MessageRef, PullRequestRef, SourceCategory,
};
use crate::domain::credentials::{CredentialKey, Provider, StoredCredential};
use crate::domain::git::{CommitId, Worktree, WorktreeSpec};
use crate::domain::ids::{AgentId, SessionId, Slug, SourceId, SpaceId};
use crate::domain::skill::SkillLibrary;
use crate::domain::workspace::{Space, SpaceName, TargetKind};

/// Anything an adapter can fail at, in terms the TUI can report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    /// The adapter has not been set up, e.g. no socket path configured.
    #[error("{what} is not configured: {detail}")]
    NotConfigured {
        /// The adapter or setting at fault.
        what: String,
        /// What is missing.
        detail: String,
    },
    /// No usable credential for the provider.
    #[error("{provider} needs authentication")]
    Unauthenticated {
        /// The provider to authenticate.
        provider: Provider,
    },
    /// The thing asked for does not exist.
    #[error("{what} {id} not found")]
    NotFound {
        /// The kind of thing.
        what: String,
        /// Its identifier.
        id: String,
    },
    /// The backend could not be reached.
    #[error("{what} unavailable: {detail}")]
    Unavailable {
        /// The backend.
        what: String,
        /// Why.
        detail: String,
    },
    /// The backend answered with something unexpected.
    #[error("{what} returned an unexpected response: {detail}")]
    Protocol {
        /// The backend.
        what: String,
        /// What was wrong with the response.
        detail: String,
    },
    /// The operation is not supported by this adapter.
    #[error("{what} does not support {operation}")]
    Unsupported {
        /// The adapter.
        what: String,
        /// The operation attempted.
        operation: String,
    },
    /// The request was refused.
    #[error("{what} refused the request: {detail}")]
    Refused {
        /// The backend.
        what: String,
        /// Why.
        detail: String,
    },
}

impl PortError {
    /// Build an [`PortError::Unavailable`].
    #[must_use]
    pub fn unavailable(what: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            what: what.into(),
            detail: detail.into(),
        }
    }

    /// Build a [`PortError::Protocol`].
    #[must_use]
    pub fn protocol(what: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Protocol {
            what: what.into(),
            detail: detail.into(),
        }
    }

    /// Build a [`PortError::NotFound`].
    #[must_use]
    pub fn not_found(what: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            what: what.into(),
            id: id.into(),
        }
    }

    /// Build a [`PortError::NotConfigured`].
    #[must_use]
    pub fn not_configured(what: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::NotConfigured {
            what: what.into(),
            detail: detail.into(),
        }
    }

    /// Build a [`PortError::Unsupported`].
    #[must_use]
    pub fn unsupported(what: impl Into<String>, operation: impl Into<String>) -> Self {
        Self::Unsupported {
            what: what.into(),
            operation: operation.into(),
        }
    }

    /// Whether retrying later might work.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, PortError::Unavailable { .. })
    }
}

/// The result of a port call.
pub type PortResult<T> = Result<T, PortError>;

/// Reading the clock, so time-dependent logic can be tested.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> OffsetDateTime;
}

/// A workspace orchestrator: Herdr, tmux, or an editor.
#[async_trait]
pub trait WorkspaceTarget: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> TargetKind;

    /// Create a space rooted at a path.
    async fn create_space(&self, name: &SpaceName, path: &Path) -> PortResult<SpaceId>;

    /// Bring a space to the foreground.
    async fn focus_space(&self, id: &SpaceId) -> PortResult<()>;

    /// Close a space.
    async fn close_space(&self, id: &SpaceId) -> PortResult<()>;

    /// Every space this target currently has.
    async fn list_spaces(&self) -> PortResult<Vec<Space>>;

    /// Run a command inside a space, which is how an agent is placed
    /// side-by-side with the developer's own shell.
    async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()>;
}

/// A target that also reports the agents already running inside it.
///
/// Herdr and tmux hold agents the developer started themselves, which
/// Vetinari can see but did not spawn and therefore cannot cancel, answer, or
/// re-run. Kept separate from [`AgentEngine`] for exactly that reason: the
/// type says what can be done with what it returns.
#[async_trait]
pub trait ExternalAgentSource: Send + Sync {
    /// Which target is being observed.
    fn target(&self) -> TargetKind;

    /// Every agent the target can currently see.
    async fn observed_agents(&self) -> PortResult<Vec<ObservedAgent>>;

    /// Wait until one of its agents changes or a new one appears.
    ///
    /// A target that cannot announce changes never returns, leaving its
    /// agents to the periodic poll.
    async fn changed(&self) -> PortResult<()> {
        std::future::pending().await
    }
}

/// An execution agent: a CLI, an API caller, or a script.
#[async_trait]
pub trait AgentEngine: Send + Sync {
    /// The engine's configuration id.
    fn id(&self) -> &AgentId;

    /// Whether the engine is usable right now, e.g. its CLI is installed.
    async fn is_available(&self) -> bool;

    /// Start a task.
    async fn spawn_task(&self, spec: &TaskSpec) -> PortResult<AgentSession>;

    /// Ask how a task is getting on.
    async fn check_status(&self, id: &SessionId) -> PortResult<AgentStatus>;

    /// Stop a task.
    async fn cancel(&self, id: &SessionId) -> PortResult<()>;

    /// Answer a question a task is blocked on.
    async fn answer(&self, id: &SessionId, answer: &str) -> PortResult<()>;

    /// The tail of a task's output.
    async fn output(&self, id: &SessionId, max_lines: usize) -> PortResult<Vec<String>>;

    /// The command line this engine would run for a task, for display in the
    /// TUI and for handing to a workspace target to run side-by-side.
    fn command_line(&self, spec: &TaskSpec) -> PortResult<Vec<String>>;
}

/// Something that produces attention items.
///
/// Every call names the credential account it should use rather than holding
/// one, because the account comes from the active profile and the profile
/// changes while Vetinari is running: a source built for `work` would keep
/// reading work's token after `<Space> p p` switched to home.
#[async_trait]
pub trait AttentionSource: Send + Sync {
    /// The source's configuration id.
    fn id(&self) -> &SourceId;

    /// Which category its items belong to.
    fn category(&self) -> SourceCategory;

    /// Fetch the current items as an account.
    async fn fetch(&self, account: Option<&Slug>) -> PortResult<Vec<AttentionItem>>;
}

/// A code host: pull requests, reviews, and CI.
#[async_trait]
pub trait CodeHostSource: AttentionSource {
    /// Approve a pull request, optionally with a comment.
    async fn approve(
        &self,
        account: Option<&Slug>,
        pr: &PullRequestRef,
        comment: Option<&str>,
    ) -> PortResult<()>;

    /// Comment on a pull request.
    async fn comment(
        &self,
        account: Option<&Slug>,
        pr: &PullRequestRef,
        body: &str,
    ) -> PortResult<()>;
}

/// An issue tracker.
#[async_trait]
pub trait IssueTrackerSource: AttentionSource {
    /// Move an issue to a different status.
    async fn transition(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        status: &str,
    ) -> PortResult<()>;

    /// Assign an issue.
    async fn assign(
        &self,
        account: Option<&Slug>,
        issue: &IssueRef,
        assignee: &str,
    ) -> PortResult<()>;
}

/// A chat system.
#[async_trait]
pub trait ChatSource: AttentionSource {
    /// Reply to a message, in its thread when it has one.
    async fn reply(
        &self,
        account: Option<&Slug>,
        message: &MessageRef,
        body: &str,
    ) -> PortResult<()>;
}

/// A calendar.
#[async_trait]
pub trait ScheduleSource: AttentionSource {
    /// Events between two instants, for the schedule view.
    async fn agenda(
        &self,
        account: Option<&Slug>,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> PortResult<Vec<AttentionItem>>;
}

/// Where secrets are kept.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Read a credential.
    async fn get(&self, key: &CredentialKey) -> PortResult<Option<StoredCredential>>;

    /// Write a credential.
    async fn set(&self, key: &CredentialKey, credential: &StoredCredential) -> PortResult<()>;

    /// Delete a credential, reporting whether one was there.
    async fn delete(&self, key: &CredentialKey) -> PortResult<bool>;
}

/// Git worktree management.
#[async_trait]
pub trait WorktreeManager: Send + Sync {
    /// Fetch a refspec, so a worktree can be based on a branch that only
    /// exists on the remote - a pull request's branch, typically.
    async fn fetch(&self, repo: &Path, remote: &str, refspec: &str) -> PortResult<()>;

    /// Create a worktree, or adopt the existing one if it is already there.
    async fn create(&self, spec: &WorktreeSpec) -> PortResult<Worktree>;

    /// Remove a worktree.
    async fn remove(&self, worktree: &Worktree) -> PortResult<()>;

    /// Every worktree of a repository.
    async fn list(&self, repo: &Path) -> PortResult<Vec<Worktree>>;

    /// The commit a worktree has checked out.
    async fn head(&self, worktree: &Path) -> PortResult<CommitId>;
}

/// Where the skill library is persisted.
#[async_trait]
pub trait SkillStore: Send + Sync {
    /// Load the library, falling back to the built-in skills when empty.
    async fn load(&self) -> PortResult<SkillLibrary>;

    /// Persist the library.
    async fn save(&self, library: &SkillLibrary) -> PortResult<()>;
}

/// A device-code challenge the developer must approve elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodeChallenge {
    /// The code to type on the verification page.
    pub user_code: String,
    /// The page to visit.
    pub verification_uri: String,
    /// The opaque code Vetinari polls with.
    pub device_code: String,
    /// How long to wait between polls, in seconds.
    pub interval_secs: u64,
    /// When the challenge stops being valid.
    pub expires_at: OffsetDateTime,
}

/// Where a device-code poll has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCodePoll {
    /// The developer has not approved it yet.
    Pending,
    /// Approved; here is the credential.
    Granted(StoredCredential),
    /// The challenge expired or was denied.
    Rejected {
        /// Why.
        reason: String,
    },
}

/// RFC 8628 device authorisation, used for calendar providers.
#[async_trait]
pub trait DeviceCodeFlow: Send + Sync {
    /// Begin a flow.
    async fn start(&self, provider: Provider) -> PortResult<DeviceCodeChallenge>;

    /// Poll once for the outcome.
    async fn poll(
        &self,
        provider: Provider,
        challenge: &DeviceCodeChallenge,
    ) -> PortResult<DeviceCodePoll>;

    /// Exchange a refresh token for a fresh credential.
    async fn refresh(
        &self,
        provider: Provider,
        credential: &StoredCredential,
    ) -> PortResult<StoredCredential>;
}
