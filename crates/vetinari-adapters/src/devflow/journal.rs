//! The lease journal as a JSON file, written through a temporary file and a
//! rename so a crash mid-write leaves the previous journal whole.
//!
//! An entry that does not parse fails the whole load rather than being
//! dropped: a dropped entry is a lease nobody heartbeats, and an attempt
//! burnt for nothing.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use vetinari_core::application::devflow::ports::LeaseJournal;
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::devflow::held::{HeldLease, HeldState};
use vetinari_core::domain::devflow::ids::{LeaseToken, TaskId, TaskQueue};
use vetinari_core::domain::devflow::task::{
    ClaimedTask, FailureKind, RawTask, StepFailure, StepOutput,
};
use vetinari_core::domain::ids::{SessionId, SpaceId};

/// How errors from this adapter name it.
const WHAT: &str = "lease journal";

/// Persists held leases in a JSON file.
#[derive(Debug, Clone)]
pub struct FileLeaseJournal {
    path: PathBuf,
}

impl FileLeaseJournal {
    /// Keep the journal at a path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the journal lives, relative to a state directory.
    #[must_use]
    pub fn default_path(state_dir: &Path) -> PathBuf {
        state_dir
            .join(super::super::config::loader::APP_DIR)
            .join("leases.json")
    }
}

#[async_trait]
impl LeaseJournal for FileLeaseJournal {
    async fn load(&self) -> PortResult<Vec<HeldLease>> {
        let text = match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(PortError::unavailable(
                    WHAT,
                    format!("could not read {}: {error}", self.path.display()),
                ));
            }
        };
        let malformed = |detail: String| {
            PortError::protocol(
                WHAT,
                format!("{} is malformed: {detail}", self.path.display()),
            )
        };
        let file: JournalFile =
            serde_json::from_str(&text).map_err(|error| malformed(error.to_string()))?;
        if file.version != JournalFile::VERSION {
            return Err(malformed(format!("unknown version {}", file.version)));
        }
        file.leases
            .into_iter()
            .map(LeaseEntry::into_held)
            .collect::<Result<_, _>>()
            .map_err(malformed)
    }

    async fn save(&self, held: &[HeldLease]) -> PortResult<()> {
        let file = JournalFile {
            version: JournalFile::VERSION,
            leases: held.iter().map(LeaseEntry::from).collect(),
        };
        let text = serde_json::to_string_pretty(&file)
            .map_err(|error| PortError::protocol(WHAT, format!("could not encode: {error}")))?;
        let unavailable = |action: &str, path: &Path, error: std::io::Error| {
            PortError::unavailable(
                WHAT,
                format!("could not {action} {}: {error}", path.display()),
            )
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| unavailable("create", parent, error))?;
        }
        let temporary = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary, text)
            .await
            .map_err(|error| unavailable("write", &temporary, error))?;
        tokio::fs::rename(&temporary, &self.path)
            .await
            .map_err(|error| unavailable("replace", &self.path, error))
    }
}

#[derive(Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    leases: Vec<LeaseEntry>,
}

impl JournalFile {
    const VERSION: u32 = 1;
}

#[derive(Serialize, Deserialize)]
struct LeaseEntry {
    task: TaskEntry,
    state: StateEntry,
}

/// The claim as the gateway sent it, so loading reads it with the same
/// parser a claim goes through.
#[derive(Serialize, Deserialize)]
struct TaskEntry {
    id: String,
    lease: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    queue: String,
    config: serde_json::Value,
    input: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum StateEntry {
    Starting,
    Running {
        session: String,
        worktree: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<String>,
    },
    AwaitingPerson,
    Completing {
        output: serde_json::Value,
    },
    Failing {
        permanent: bool,
        message: String,
    },
}

impl From<&HeldLease> for LeaseEntry {
    fn from(held: &HeldLease) -> Self {
        let task = &held.task;
        Self {
            task: TaskEntry {
                id: task.id.to_string(),
                lease: task.lease.to_string(),
                expires_at: task.expires_at,
                queue: task.queue.to_string(),
                config: task.config.to_json(),
                input: task.input.to_json(),
            },
            state: match &held.state {
                HeldState::Starting => StateEntry::Starting,
                HeldState::Running {
                    session,
                    worktree,
                    space,
                } => StateEntry::Running {
                    session: session.to_string(),
                    worktree: worktree.clone(),
                    space: space.as_ref().map(ToString::to_string),
                },
                HeldState::AwaitingPerson => StateEntry::AwaitingPerson,
                HeldState::Completing { output } => StateEntry::Completing {
                    output: output.to_json(),
                },
                HeldState::Failing { failure } => StateEntry::Failing {
                    permanent: failure.kind == FailureKind::Permanent,
                    message: failure.message.clone(),
                },
            },
        }
    }
}

impl LeaseEntry {
    fn into_held(self) -> Result<HeldLease, String> {
        let field = |name: &str, error: &dyn std::fmt::Display| format!("{name}: {error}");
        let raw = RawTask::new(
            TaskId::parse(&self.task.id).map_err(|error| field("task", &error))?,
            LeaseToken::parse(&self.task.lease).map_err(|error| field("lease", &error))?,
            self.task.expires_at,
            TaskQueue::parse(&self.task.queue).map_err(|error| field("queue", &error))?,
            self.task.config,
            self.task.input,
        );
        Ok(HeldLease {
            task: ClaimedTask::parse(&raw).map_err(|error| field("task", &error))?,
            state: match self.state {
                StateEntry::Starting => HeldState::Starting,
                StateEntry::Running {
                    session,
                    worktree,
                    space,
                } => HeldState::Running {
                    session: SessionId::parse(&session)
                        .map_err(|error| field("session", &error))?,
                    worktree,
                    space: space
                        .map(|space| SpaceId::parse(&space))
                        .transpose()
                        .map_err(|error| field("space", &error))?,
                },
                StateEntry::AwaitingPerson => HeldState::AwaitingPerson,
                StateEntry::Completing { output } => HeldState::Completing {
                    output: StepOutput::parse(&output).map_err(|error| field("output", &error))?,
                },
                StateEntry::Failing { permanent, message } => HeldState::Failing {
                    failure: if permanent {
                        StepFailure::permanent(message)
                    } else {
                        StepFailure::transient(message)
                    },
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use vetinari_core::domain::devflow::ids::{LeaseToken, TaskId, TaskQueue};
    use vetinari_core::domain::devflow::outcome::{GateAnswer, Line, StepOutcome};
    use vetinari_core::domain::devflow::step::{GateKind, StepKind};
    use vetinari_core::domain::git::BranchName;
    use vetinari_core::domain::ids::{SessionId, SpaceId};

    use super::*;

    fn held(instance: &str, step: StepKind, state: HeldState) -> HeldLease {
        let raw = RawTask::new(
            TaskId::parse(&format!("01JB7Z~{instance}~1")).expect("id"),
            LeaseToken::parse("01JB80").expect("lease"),
            datetime!(2026-10-25 10:05:00 UTC),
            TaskQueue::parse("vetinari-hex-agent").expect("queue"),
            serde_json::json!({
                "schema": 1, "step": step.name(), "template": "feature-from-ticket",
                "ticket": { "source": "jira", "key": "ENG-412", "title": "Retries" },
                "repo": { "path": "/work/repos/web", "remote": "acme/web" },
                "profile": "work", "branch": "vetinari/eng-412-retries"
            }),
            serde_json::json!({ "input": {}, "iteration": 1 }),
        );
        HeldLease {
            task: ClaimedTask::parse(&raw).expect("claim"),
            state,
        }
    }

    fn every_state() -> Vec<HeldLease> {
        vec![
            held("preflight@1", StepKind::Preflight, HeldState::Starting),
            held(
                "implement@2",
                StepKind::Implement,
                HeldState::Running {
                    session: SessionId::parse("4f2a").expect("session"),
                    worktree: PathBuf::from("/work/trees/web-eng-412"),
                    space: Some(SpaceId::parse("herdr/3").expect("space")),
                },
            ),
            held(
                "review@1",
                StepKind::Review,
                HeldState::Running {
                    session: SessionId::parse("5e1b").expect("session"),
                    worktree: PathBuf::from("/work/trees/web-eng-412"),
                    space: None,
                },
            ),
            held(
                "ask-info@1",
                StepKind::Gate(GateKind::InfoNeeded),
                HeldState::AwaitingPerson,
            ),
            held(
                "ask-info@2",
                StepKind::Gate(GateKind::InfoNeeded),
                HeldState::Completing {
                    output: StepOutput {
                        outcome: StepOutcome::Gate(GateAnswer::Recheck {
                            note: Some(Line::parse("Page size is 50").expect("line")),
                        }),
                        git_branch: BranchName::sanitized("vetinari/eng-412-retries"),
                    },
                },
            ),
            held(
                "review@1",
                StepKind::Review,
                HeldState::Failing {
                    failure: StepFailure::transient("claude-code exited 1"),
                },
            ),
            held(
                "implement@1",
                StepKind::Implement,
                HeldState::Failing {
                    failure: StepFailure::permanent("the skill wrote no result"),
                },
            ),
        ]
    }

    #[tokio::test]
    async fn every_held_state_survives_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = FileLeaseJournal::new(dir.path().join("state/leases.json"));
        journal.save(&every_state()).await.expect("save");
        assert_eq!(journal.load().await.expect("load"), every_state());
    }

    #[tokio::test]
    async fn a_missing_journal_holds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = FileLeaseJournal::new(dir.path().join("leases.json"));
        assert_eq!(journal.load().await.expect("load"), Vec::new());
    }

    #[tokio::test]
    async fn a_save_replaces_what_was_there_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = FileLeaseJournal::new(dir.path().join("leases.json"));
        journal.save(&every_state()).await.expect("save");
        journal.save(&every_state()[..1]).await.expect("save");
        assert_eq!(journal.load().await.expect("load"), every_state()[..1]);
        journal.save(&[]).await.expect("save");
        assert_eq!(journal.load().await.expect("load"), Vec::new());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["leases.json"]);
    }

    #[tokio::test]
    async fn a_corrupt_journal_is_reported_not_emptied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("leases.json");
        let journal = FileLeaseJournal::new(&path);
        std::fs::write(&path, "{ not json").expect("write");
        assert!(matches!(
            journal.load().await,
            Err(PortError::Protocol { .. })
        ));
        journal.save(&every_state()).await.expect("save");
        let text = std::fs::read_to_string(&path)
            .expect("read")
            .replace("01JB7Z~preflight@1~1", "not-a-task-id");
        assert!(text.contains("not-a-task-id"));
        std::fs::write(&path, text).expect("write");
        assert!(matches!(
            journal.load().await,
            Err(PortError::Protocol { .. })
        ));
    }

    #[test]
    fn the_journal_lives_in_vetinaris_state_directory() {
        assert_eq!(
            FileLeaseJournal::default_path(Path::new("/home/dev/.local/state")),
            PathBuf::from("/home/dev/.local/state/vetinari/leases.json")
        );
    }
}
