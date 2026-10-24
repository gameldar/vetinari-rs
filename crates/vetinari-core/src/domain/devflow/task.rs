//! Claimed tasks: what the engine hands a worker, and what the worker hands
//! back.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::ids::{LeaseToken, TaskId, TaskQueue};
use super::outcome::StepOutcome;
use super::result::{OutcomeFields, ResultError};
use super::step::{ConfigError, StepConfig, StepKind};
use crate::domain::git::BranchName;

/// A claim reply whose identifiers the adapter has parsed, with `config` and
/// `input` left as JSON so the domain, not the adapter, decides what a
/// vetinari step is.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTask {
    id: TaskId,
    lease: LeaseToken,
    expires_at: OffsetDateTime,
    queue: TaskQueue,
    config: serde_json::Value,
    input: serde_json::Value,
}

impl RawTask {
    /// Record a claim reply.
    #[must_use]
    pub const fn new(
        id: TaskId,
        lease: LeaseToken,
        expires_at: OffsetDateTime,
        queue: TaskQueue,
        config: serde_json::Value,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id,
            lease,
            expires_at,
            queue,
            config,
            input,
        }
    }

    /// The task, which is all a permanent failure of an unreadable claim needs
    /// beside the lease.
    #[must_use]
    pub const fn id(&self) -> &TaskId {
        &self.id
    }

    /// The lease the claim holds.
    #[must_use]
    pub const fn lease(&self) -> &LeaseToken {
        &self.lease
    }
}

/// Why a claimed task is not a vetinari step.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    /// Its config did not parse.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Its input did not parse.
    #[error(transparent)]
    Input(#[from] InputError),
}

/// A task this worker holds, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedTask {
    /// The task, which names its run, instance and attempt.
    pub id: TaskId,
    /// The lease the claim holds.
    pub lease: LeaseToken,
    /// When the lease runs out without a heartbeat.
    pub expires_at: OffsetDateTime,
    /// The queue it was claimed from.
    pub queue: TaskQueue,
    /// What the step needs.
    pub config: StepConfig,
    /// What came before it.
    pub input: StepInput,
}

impl ClaimedTask {
    /// Read a claim as a vetinari step.
    pub fn parse(raw: &RawTask) -> Result<Self, ClaimError> {
        Ok(Self {
            id: raw.id.clone(),
            lease: raw.lease.clone(),
            expires_at: raw.expires_at,
            queue: raw.queue.clone(),
            config: StepConfig::parse(&raw.config)?,
            input: StepInput::parse(&raw.input)?,
        })
    }
}

/// Why a task's input could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// A loop envelope's `iteration` was not a positive number.
    #[error("loop iteration is not a positive number: {0}")]
    Iteration(String),
    /// A loop envelope's `previous` was not a vetinari step's output.
    #[error("previous output is not valid: {0}")]
    Previous(#[from] OutputError),
}

/// What came before a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepInput {
    /// Outside a loop body's entry: the predecessor's output, unused by the
    /// current template.
    Plain,
    /// The first node of a loop body: the engine's envelope.
    Iteration {
        /// Which iteration this is.
        iteration: NonZeroU32,
        /// The exit node's outcome from the iteration before, when it
        /// produced one.
        previous: Option<StepOutcome>,
    },
}

impl StepInput {
    /// Read a task's input. An object with an `iteration` key is a loop
    /// envelope; no step's output carries that key.
    pub fn parse(input: &serde_json::Value) -> Result<Self, InputError> {
        let Some(iteration) = input.get("iteration") else {
            return Ok(Self::Plain);
        };
        let iteration = iteration
            .as_u64()
            .and_then(|iteration| u32::try_from(iteration).ok())
            .and_then(NonZeroU32::new)
            .ok_or_else(|| InputError::Iteration(iteration.to_string()))?;
        let previous = match input.get("previous") {
            None | Some(serde_json::Value::Null) => None,
            Some(previous) => Some(StepOutput::parse(previous)?.outcome),
        };
        Ok(Self::Iteration {
            iteration,
            previous,
        })
    }
}

/// Why a step's output could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    /// The document was not shaped like a step output.
    #[error("{0}")]
    Malformed(String),
    /// It named a step that does not exist.
    #[error("unknown step {0:?}")]
    Step(String),
    /// Its git branch was not one vetinari generates.
    #[error("git branch {0:?} is not valid")]
    GitBranch(String),
    /// Its outcome did not parse.
    #[error(transparent)]
    Outcome(#[from] ResultError),
}

/// What a step hands the engine on completion. The engine reads only
/// `branch`; the rest is for `previous`, for the popup, and for
/// `GET /v1/runs/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutput {
    /// What the step reported.
    pub outcome: StepOutcome,
    /// The run's branch, where the work is.
    pub git_branch: BranchName,
}

impl StepOutput {
    /// The JSON object the engine stores.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let document = OutputDocument {
            branch: self.outcome.label().to_owned(),
            step: self.outcome.step().name().to_owned(),
            fields: OutcomeFields::from(&self.outcome),
            git_branch: self.git_branch.to_string(),
        };
        serde_json::to_value(document).expect("an output document always serialises")
    }

    /// Read an output back, as the engine hands it on in `previous`.
    pub fn parse(output: &serde_json::Value) -> Result<Self, OutputError> {
        let document = OutputDocument::deserialize(output)
            .map_err(|error| OutputError::Malformed(error.to_string()))?;
        let step: StepKind = document
            .step
            .parse()
            .map_err(|_| OutputError::Step(document.step.clone()))?;
        let git_branch = BranchName::parse(&document.git_branch)
            .map_err(|_| OutputError::GitBranch(document.git_branch.clone()))?;
        Ok(Self {
            outcome: document.fields.into_outcome(step, &document.branch)?,
            git_branch,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct OutputDocument {
    branch: String,
    step: String,
    #[serde(flatten)]
    fields: OutcomeFields,
    git_branch: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::devflow::outcome::{
        Finding, GateAnswer, ImplementOutcome, Line, OpenPrOutcome, PreflightOutcome,
        ReviewOutcome, ShortList, Summary,
    };
    use crate::domain::devflow::step::GateKind;

    fn branch() -> BranchName {
        BranchName::sanitized("vetinari/eng-412-rate-limit-retries")
    }

    fn findings() -> StepOutcome {
        StepOutcome::Review(ReviewOutcome::ChangesRequested {
            findings: ShortList::parse(vec![Finding {
                file: Some(Line::parse("src/retry.rs").expect("line")),
                line: NonZeroU32::new(88),
                text: Line::parse("Retry-After ignored on 503").expect("line"),
            }])
            .expect("list"),
        })
    }

    #[test]
    fn an_output_carries_its_branch_step_fields_and_git_branch() {
        let output = StepOutput {
            outcome: findings(),
            git_branch: branch(),
        };
        assert_eq!(
            output.to_json(),
            json!({
                "branch": "changes-requested",
                "step": "review",
                "findings": [ { "file": "src/retry.rs", "line": 88, "text": "Retry-After ignored on 503" } ],
                "git_branch": "vetinari/eng-412-rate-limit-retries"
            })
        );
    }

    #[test]
    fn every_outcome_round_trips_through_its_output() {
        let summary = || Summary::parse("s").expect("summary");
        let line = |text| Line::parse(text).expect("line");
        let outcomes = [
            StepOutcome::Preflight(PreflightOutcome::Ready { summary: summary() }),
            StepOutcome::Preflight(PreflightOutcome::NeedsInfo {
                questions: ShortList::parse(vec![line("Page size?")]).expect("list"),
            }),
            StepOutcome::Implement(ImplementOutcome::Implemented { summary: summary() }),
            StepOutcome::Review(ReviewOutcome::Approved { summary: summary() }),
            findings(),
            StepOutcome::OpenPr(OpenPrOutcome::Opened {
                number: 1204,
                url: line("https://github.com/acme/web/pull/1204"),
            }),
            StepOutcome::Gate(GateAnswer::Recheck {
                note: Some(line("Page size is 50")),
            }),
            StepOutcome::Gate(GateAnswer::Recheck { note: None }),
            StepOutcome::Gate(GateAnswer::OpenAnyway),
        ];
        for outcome in outcomes {
            let output = StepOutput {
                outcome,
                git_branch: branch(),
            };
            assert_eq!(StepOutput::parse(&output.to_json()), Ok(output));
        }
    }

    #[test]
    fn an_output_that_is_not_a_vetinari_steps_is_refused() {
        assert!(matches!(
            StepOutput::parse(&json!({ "branch": "ok" })),
            Err(OutputError::Malformed(_))
        ));
        assert_eq!(
            StepOutput::parse(&json!({ "branch": "ok", "step": "deploy", "git_branch": "main" })),
            Err(OutputError::Step("deploy".to_owned()))
        );
        assert_eq!(
            StepOutput::parse(&json!({
                "branch": "approved", "step": "review", "summary": "s", "git_branch": "Main"
            })),
            Err(OutputError::GitBranch("Main".to_owned()))
        );
        assert_eq!(
            StepOutput::parse(&json!({
                "branch": "approved", "step": "implement", "summary": "s", "git_branch": "main"
            })),
            Err(OutputError::Outcome(ResultError::Outcome {
                step: StepKind::Implement,
                found: "approved".to_owned()
            }))
        );
    }

    #[test]
    fn input_outside_a_loop_entry_is_plain() {
        assert_eq!(StepInput::parse(&json!({})), Ok(StepInput::Plain));
        assert_eq!(
            StepInput::parse(&json!({ "branch": "done", "iterations": 2, "last": {} })),
            Ok(StepInput::Plain)
        );
        assert_eq!(
            StepInput::parse(
                &StepOutput {
                    outcome: StepOutcome::Gate(GateAnswer::OpenAnyway),
                    git_branch: branch(),
                }
                .to_json()
            ),
            Ok(StepInput::Plain)
        );
    }

    #[test]
    fn the_first_iteration_of_a_loop_has_no_previous() {
        assert_eq!(
            StepInput::parse(&json!({ "input": {}, "iteration": 1 })),
            Ok(StepInput::Iteration {
                iteration: NonZeroU32::MIN,
                previous: None
            })
        );
        assert_eq!(
            StepInput::parse(&json!({ "input": {}, "iteration": 1, "previous": null })),
            Ok(StepInput::Iteration {
                iteration: NonZeroU32::MIN,
                previous: None
            })
        );
    }

    #[test]
    fn a_later_iteration_carries_the_exit_nodes_outcome() {
        let previous = StepOutput {
            outcome: findings(),
            git_branch: branch(),
        };
        assert_eq!(
            StepInput::parse(
                &json!({ "input": {}, "iteration": 2, "previous": previous.to_json() })
            ),
            Ok(StepInput::Iteration {
                iteration: NonZeroU32::new(2).expect("non-zero"),
                previous: Some(findings())
            })
        );
    }

    #[test]
    fn a_broken_envelope_is_refused() {
        for iteration in [json!(0), json!(-1), json!("2"), json!(1.5)] {
            assert!(
                matches!(
                    StepInput::parse(&json!({ "input": {}, "iteration": iteration })),
                    Err(InputError::Iteration(_))
                ),
                "{iteration}"
            );
        }
        assert!(matches!(
            StepInput::parse(
                &json!({ "input": {}, "iteration": 2, "previous": { "branch": "x" } })
            ),
            Err(InputError::Previous(_))
        ));
    }

    #[test]
    fn a_claim_is_read_as_a_step_or_refused_whole() {
        let config = json!({
            "schema": 1, "step": "info-needed", "template": "feature-from-ticket",
            "ticket": { "source": "jira", "key": "ENG-412", "title": "Retries" },
            "repo": { "path": "/work/repos/web", "remote": "acme/web" },
            "profile": "work", "branch": "vetinari/eng-412-retries"
        });
        let raw = |config: serde_json::Value, input: serde_json::Value| {
            RawTask::new(
                TaskId::parse("01JB7Z~ask-info@1~1").expect("id"),
                LeaseToken::parse("01JB80").expect("lease"),
                OffsetDateTime::UNIX_EPOCH,
                TaskQueue::parse("vetinari-hex-human").expect("queue"),
                config,
                input,
            )
        };
        let task = ClaimedTask::parse(&raw(config.clone(), json!({}))).expect("claim");
        assert_eq!(task.id.to_string(), "01JB7Z~ask-info@1~1");
        assert_eq!(task.lease.as_str(), "01JB80");
        assert_eq!(task.expires_at, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(task.queue.as_str(), "vetinari-hex-human");
        assert_eq!(task.config.step, StepKind::Gate(GateKind::InfoNeeded));
        assert_eq!(task.input, StepInput::Plain);

        assert!(matches!(
            ClaimedTask::parse(&raw(json!({ "job": "deploy" }), json!({}))),
            Err(ClaimError::Config(_))
        ));
        assert!(matches!(
            ClaimedTask::parse(&raw(config, json!({ "iteration": 0 }))),
            Err(ClaimError::Input(_))
        ));
    }
}
