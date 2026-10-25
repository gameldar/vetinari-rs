//! What a step reports when it finishes.
//!
//! Every piece of text an outcome carries is length-capped on construction.
//! Outputs are stored in the engine's run row without a bound of their own,
//! so this is where they are kept small.

use std::num::NonZeroU32;

use super::ids::BranchLabel;
use super::step::{GateKind, StepKind};
use crate::domain::text::TextError;

/// The branch labels vetinari's steps and the engine's loops produce. The
/// labels `compile` writes on edges and those `StepOutcome::branch` returns
/// are these constants, so the two cannot drift apart.
pub mod label {
    /// Preflight: the ticket is ready to implement.
    pub const READY: &str = "ready";
    /// Preflight: the ticket needs more information.
    pub const NEEDS_INFO: &str = "needs-info";
    /// Implement: the work is done.
    pub const IMPLEMENTED: &str = "implemented";
    /// Review: the work is approved.
    pub const APPROVED: &str = "approved";
    /// Review: the work needs another round.
    pub const CHANGES_REQUESTED: &str = "changes-requested";
    /// Open PR: the pull request is open.
    pub const OPENED: &str = "opened";
    /// Info-needed gate: the ticket was updated, check it again.
    pub const RECHECK: &str = "recheck";
    /// Review-stuck gate: open the pull request as it is.
    pub const OPEN_ANYWAY: &str = "open-anyway";
    /// A loop whose exit was met.
    pub const DONE: &str = "done";
    /// A loop that ran out of iterations.
    pub const EXHAUSTED: &str = "exhausted";

    /// Every label above.
    pub const ALL: &[&str] = &[
        READY,
        NEEDS_INFO,
        IMPLEMENTED,
        APPROVED,
        CHANGES_REQUESTED,
        OPENED,
        RECHECK,
        OPEN_ANYWAY,
        DONE,
        EXHAUSTED,
    ];
}

macro_rules! capped_text {
    ($(#[$meta:meta])* $name:ident, $max:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// The longest accepted text, in bytes.
            pub const MAX_LEN: usize = $max;

            /// Accept text, trimming the edges.
            pub fn parse(raw: &str) -> Result<Self, TextError> {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(TextError::Empty);
                }
                if trimmed.len() > Self::MAX_LEN {
                    return Err(TextError::TooLong {
                        len: trimmed.len(),
                        max: Self::MAX_LEN,
                    });
                }
                Ok(Self(trimmed.to_owned()))
            }

            /// Borrow the text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

capped_text!(
    /// A step's account of what it did, at most 2 KB.
    Summary,
    2048
);
capped_text!(
    /// One line a person will read: a question, a note, a finding.
    Line,
    500
);

/// Why a list could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListError {
    /// The list was empty.
    #[error("list is empty")]
    Empty,
    /// The list was longer than accepted.
    #[error("list has {len} entries, maximum is {max}")]
    TooMany {
        /// Its length.
        len: usize,
        /// The limit.
        max: usize,
    },
}

/// A list of one to twenty entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortList<T>(Vec<T>);

impl<T> ShortList<T> {
    /// The most entries accepted.
    pub const MAX_LEN: usize = 20;

    /// Accept a list of one to [`Self::MAX_LEN`] entries.
    pub fn parse(items: Vec<T>) -> Result<Self, ListError> {
        if items.is_empty() {
            return Err(ListError::Empty);
        }
        if items.len() > Self::MAX_LEN {
            return Err(ListError::TooMany {
                len: items.len(),
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(items))
    }

    /// The entries, of which there is at least one.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

/// One thing a review found, optionally pinned to a place in the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The file, relative to the worktree.
    pub file: Option<Line>,
    /// The line in that file.
    pub line: Option<NonZeroU32>,
    /// What is wrong.
    pub text: Line,
}

/// What preflight found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// The ticket carries what an implementation needs.
    Ready {
        /// What the implementation will do.
        summary: Summary,
    },
    /// The ticket does not.
    NeedsInfo {
        /// What a person must answer on the ticket.
        questions: ShortList<Line>,
    },
}

/// What implementation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImplementOutcome {
    /// The work is committed on the run's branch.
    Implemented {
        /// What was done.
        summary: Summary,
    },
}

/// What review concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// The work can go up for review by people.
    Approved {
        /// Why.
        summary: Summary,
    },
    /// The work needs another round.
    ChangesRequested {
        /// What the next round must address.
        findings: ShortList<Finding>,
    },
}

/// What opening the pull request produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrOutcome {
    /// The pull request is open.
    Opened {
        /// Its number.
        number: u64,
        /// Where to see it.
        url: Line,
    },
}

/// A person's answer to a gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAnswer {
    /// The ticket was updated: run preflight again. Answers `InfoNeeded`.
    Recheck {
        /// Anything the person wants preflight to know.
        note: Option<Line>,
    },
    /// Open the pull request as it is. Answers `ReviewStuck`.
    OpenAnyway,
}

impl GateAnswer {
    /// The gate this answers.
    #[must_use]
    pub const fn gate(&self) -> GateKind {
        match self {
            GateAnswer::Recheck { .. } => GateKind::InfoNeeded,
            GateAnswer::OpenAnyway => GateKind::ReviewStuck,
        }
    }
}

/// What a step reports when it finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// From preflight.
    Preflight(PreflightOutcome),
    /// From implement.
    Implement(ImplementOutcome),
    /// From review.
    Review(ReviewOutcome),
    /// From open-pr.
    OpenPr(OpenPrOutcome),
    /// From a gate.
    Gate(GateAnswer),
}

impl StepOutcome {
    /// The step kind that reports this outcome.
    #[must_use]
    pub const fn step(&self) -> StepKind {
        match self {
            StepOutcome::Preflight(_) => StepKind::Preflight,
            StepOutcome::Implement(_) => StepKind::Implement,
            StepOutcome::Review(_) => StepKind::Review,
            StepOutcome::OpenPr(_) => StepKind::OpenPr,
            StepOutcome::Gate(answer) => StepKind::Gate(answer.gate()),
        }
    }

    /// The label the engine matches against edges.
    #[must_use]
    pub fn branch(&self) -> BranchLabel {
        BranchLabel::known(self.label())
    }

    /// The label as one of the [`label`] constants.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            StepOutcome::Preflight(PreflightOutcome::Ready { .. }) => label::READY,
            StepOutcome::Preflight(PreflightOutcome::NeedsInfo { .. }) => label::NEEDS_INFO,
            StepOutcome::Implement(ImplementOutcome::Implemented { .. }) => label::IMPLEMENTED,
            StepOutcome::Review(ReviewOutcome::Approved { .. }) => label::APPROVED,
            StepOutcome::Review(ReviewOutcome::ChangesRequested { .. }) => label::CHANGES_REQUESTED,
            StepOutcome::OpenPr(OpenPrOutcome::Opened { .. }) => label::OPENED,
            StepOutcome::Gate(GateAnswer::Recheck { .. }) => label::RECHECK,
            StepOutcome::Gate(GateAnswer::OpenAnyway) => label::OPEN_ANYWAY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Line {
        Line::parse(text).expect("line")
    }

    #[test]
    fn every_known_label_is_a_valid_branch_label() {
        for known in label::ALL {
            assert_eq!(BranchLabel::known(known).as_str(), *known);
        }
    }

    #[test]
    fn capped_text_is_trimmed_and_bounded() {
        assert_eq!(Summary::parse("  done \n").expect("valid").as_str(), "done");
        assert_eq!(Summary::parse(" "), Err(TextError::Empty));
        assert!(Summary::parse(&"x".repeat(2048)).is_ok());
        assert_eq!(
            Summary::parse(&"x".repeat(2049)),
            Err(TextError::TooLong {
                len: 2049,
                max: 2048
            })
        );
        assert_eq!(
            Line::parse(&"x".repeat(501)),
            Err(TextError::TooLong { len: 501, max: 500 })
        );
    }

    #[test]
    fn a_short_list_holds_one_to_twenty_entries() {
        assert_eq!(ShortList::parse(vec![1]).expect("valid").as_slice(), &[1]);
        assert_eq!(ShortList::<u8>::parse(vec![]), Err(ListError::Empty));
        assert!(ShortList::parse(vec![0; 20]).is_ok());
        assert_eq!(
            ShortList::parse(vec![0; 21]),
            Err(ListError::TooMany { len: 21, max: 20 })
        );
    }

    #[test]
    fn each_outcome_carries_its_own_label_and_step() {
        let summary = Summary::parse("s").expect("summary");
        let cases = [
            (
                StepOutcome::Preflight(PreflightOutcome::Ready {
                    summary: summary.clone(),
                }),
                StepKind::Preflight,
                label::READY,
            ),
            (
                StepOutcome::Preflight(PreflightOutcome::NeedsInfo {
                    questions: ShortList::parse(vec![line("page size?")]).expect("list"),
                }),
                StepKind::Preflight,
                label::NEEDS_INFO,
            ),
            (
                StepOutcome::Implement(ImplementOutcome::Implemented {
                    summary: summary.clone(),
                }),
                StepKind::Implement,
                label::IMPLEMENTED,
            ),
            (
                StepOutcome::Review(ReviewOutcome::Approved { summary }),
                StepKind::Review,
                label::APPROVED,
            ),
            (
                StepOutcome::Review(ReviewOutcome::ChangesRequested {
                    findings: ShortList::parse(vec![Finding {
                        file: None,
                        line: None,
                        text: line("no test"),
                    }])
                    .expect("list"),
                }),
                StepKind::Review,
                label::CHANGES_REQUESTED,
            ),
            (
                StepOutcome::OpenPr(OpenPrOutcome::Opened {
                    number: 7,
                    url: line("https://example.com/pr/7"),
                }),
                StepKind::OpenPr,
                label::OPENED,
            ),
            (
                StepOutcome::Gate(GateAnswer::Recheck { note: None }),
                StepKind::Gate(GateKind::InfoNeeded),
                label::RECHECK,
            ),
            (
                StepOutcome::Gate(GateAnswer::OpenAnyway),
                StepKind::Gate(GateKind::ReviewStuck),
                label::OPEN_ANYWAY,
            ),
        ];
        for (outcome, step, expected) in cases {
            assert_eq!(outcome.step(), step);
            assert_eq!(outcome.branch().as_str(), expected);
            assert!(step.labels().contains(&expected));
        }
    }
}
