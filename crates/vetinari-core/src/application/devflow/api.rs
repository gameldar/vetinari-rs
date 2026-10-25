//! `ClientApi`: what a client of the daemon may ask, in domain types
//! (design 10.3). The socket is only its first adapter; a web UI would be
//! another over the same service.

use std::collections::BTreeMap;

use time::OffsetDateTime;

use crate::application::ports::PortError;
use crate::domain::devflow::ids::{RunId, WorkerName};
use crate::domain::devflow::summary::RunSummary;
use crate::domain::text::NonEmptyText;

/// The protocol this daemon speaks. A client speaking another is told to
/// restart the daemon, since the two were built apart.
pub const PROTOCOL: u32 = 1;

/// What a run is about, for its queue row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLabel {
    /// The ticket's key.
    pub ticket: NonEmptyText,
    /// The ticket's title.
    pub title: NonEmptyText,
}

/// A run as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunItem {
    /// The strip and what needs the developer.
    pub summary: RunSummary,
    /// What it is about, when known: a run found on the gateway after a
    /// restart has none until its definition is read.
    pub label: Option<RunLabel>,
    /// When it was started, when known.
    pub started_at: Option<OffsetDateTime>,
}

/// A change a `runs.watch` stream pushes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardChange {
    /// A run is new or changed.
    Run(RunItem),
    /// A run left the queue.
    Removed(RunId),
}

/// The current item of every run, from which watchers get a snapshot and
/// then only what changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunBoard {
    items: BTreeMap<RunId, RunItem>,
}

impl RunBoard {
    /// Every run, in run id order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RunItem> {
        self.items.values().cloned().collect()
    }

    /// Record a run's item; the change to push, unless nothing changed.
    pub fn put(&mut self, item: RunItem) -> Option<BoardChange> {
        if self.items.get(&item.summary.run) == Some(&item) {
            return None;
        }
        self.items.insert(item.summary.run.clone(), item.clone());
        Some(BoardChange::Run(item))
    }

    /// Drop a run; the change to push, unless it was not there.
    pub fn remove(&mut self, run: &RunId) -> Option<BoardChange> {
        self.items
            .remove(run)
            .map(|_| BoardChange::Removed(run.clone()))
    }
}

/// Why a client's request failed: a closed set, because a client decides
/// what to do from the code alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The request is wrong: a bad method, bad params, a protocol mismatch.
    #[error("invalid: {0}")]
    Invalid(String),
    /// What it names does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// It conflicts with how things are, such as a second run for a ticket.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The gateway cannot be reached; worth retrying.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The daemon needs signing in.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
    /// The gateway answered with something unexpected.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl ApiError {
    /// The code a frame carries.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::NotFound(_) => "not-found",
            Self::Conflict(_) => "conflict",
            Self::Unavailable(_) => "unavailable",
            Self::Unauthenticated(_) => "unauthenticated",
            Self::Protocol(_) => "protocol",
        }
    }
}

impl From<PortError> for ApiError {
    fn from(error: PortError) -> Self {
        let message = error.to_string();
        match error {
            PortError::NotConfigured { .. } | PortError::Unavailable { .. } => {
                Self::Unavailable(message)
            }
            PortError::Unauthenticated { .. } => Self::Unauthenticated(message),
            PortError::NotFound { .. } => Self::NotFound(message),
            PortError::Protocol { .. } => Self::Protocol(message),
            PortError::Unsupported { .. } => Self::Invalid(message),
            PortError::Refused { .. } => Self::Conflict(message),
        }
    }
}

/// The answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The daemon's name and version.
    pub daemon: String,
    /// The protocol it speaks.
    pub protocol: u32,
    /// The worker label it claims as.
    pub worker: WorkerName,
}

/// What a client may ask the daemon.
#[derive(Debug, Clone)]
pub struct ClientApi {
    daemon: String,
    worker: WorkerName,
}

impl ClientApi {
    /// The API of a daemon with this name and worker label.
    #[must_use]
    pub fn new(daemon: impl Into<String>, worker: WorkerName) -> Self {
        Self {
            daemon: daemon.into(),
            worker,
        }
    }

    /// Greet a client, refusing one that speaks another protocol.
    pub fn hello(&self, client: &str, protocol: u32) -> Result<Hello, ApiError> {
        if protocol != PROTOCOL {
            return Err(ApiError::Invalid(format!(
                "{client} speaks protocol {protocol} and {} speaks {PROTOCOL}: restart the daemon",
                self.daemon
            )));
        }
        Ok(Hello {
            daemon: self.daemon.clone(),
            protocol: PROTOCOL,
            worker: self.worker.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::credentials::Provider;
    use crate::domain::devflow::run_view::RunStatus;
    use crate::domain::devflow::summary::{Mark, Phase, PhaseCell};

    fn run(raw: &str) -> RunId {
        RunId::parse(raw).expect("run")
    }

    fn item(raw: &str, mark: Mark) -> RunItem {
        RunItem {
            summary: RunSummary {
                run: run(raw),
                status: RunStatus::Active,
                phases: vec![PhaseCell {
                    phase: Phase::Pre,
                    mark,
                    round: None,
                }],
                needs_you: None,
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: None,
        }
    }

    fn api() -> ClientApi {
        ClientApi::new("vetinarid 0.1.0", WorkerName::parse("hex").expect("worker"))
    }

    #[test]
    fn a_new_or_changed_run_is_pushed_and_an_unchanged_one_is_not() {
        let mut board = RunBoard::default();
        assert_eq!(
            board.put(item("r1", Mark::Next)),
            Some(BoardChange::Run(item("r1", Mark::Next)))
        );
        assert_eq!(board.put(item("r1", Mark::Next)), None);
        assert_eq!(
            board.put(item("r1", Mark::RunningHere)),
            Some(BoardChange::Run(item("r1", Mark::RunningHere)))
        );
    }

    #[test]
    fn a_snapshot_lists_every_run_and_a_removed_one_leaves_it() {
        let mut board = RunBoard::default();
        board.put(item("r2", Mark::Next));
        board.put(item("r1", Mark::Done));
        assert_eq!(
            board.snapshot(),
            [item("r1", Mark::Done), item("r2", Mark::Next)]
        );

        assert_eq!(
            board.remove(&run("r1")),
            Some(BoardChange::Removed(run("r1")))
        );
        assert_eq!(board.remove(&run("r1")), None);
        assert_eq!(board.snapshot(), [item("r2", Mark::Next)]);
    }

    #[test]
    fn hello_names_the_daemon_its_protocol_and_worker() {
        assert_eq!(
            api().hello("vetinari 0.1.0", PROTOCOL),
            Ok(Hello {
                daemon: "vetinarid 0.1.0".to_owned(),
                protocol: PROTOCOL,
                worker: WorkerName::parse("hex").expect("worker"),
            })
        );
    }

    #[test]
    fn a_client_speaking_another_protocol_is_told_to_restart_the_daemon() {
        let error = api().hello("vetinari 0.2.0", 2).expect_err("mismatch");
        assert_eq!(error.code(), "invalid");
        assert!(error.to_string().contains("restart"), "{error}");
    }

    #[test]
    fn port_errors_map_onto_the_closed_set_of_codes() {
        for (error, code) in [
            (PortError::unavailable("autognomus", "down"), "unavailable"),
            (PortError::protocol("autognomus", "odd"), "protocol"),
            (PortError::not_found("run", "r1"), "not-found"),
            (
                PortError::Unauthenticated {
                    provider: Provider::GitHub,
                },
                "unauthenticated",
            ),
            (
                PortError::Refused {
                    what: "autognomus".to_owned(),
                    detail: "exists".to_owned(),
                },
                "conflict",
            ),
            (
                PortError::not_configured("gateway", "no url"),
                "unavailable",
            ),
            (PortError::unsupported("fake", "start"), "invalid"),
        ] {
            assert_eq!(ApiError::from(error.clone()).code(), code, "{error:?}");
        }
    }
}
