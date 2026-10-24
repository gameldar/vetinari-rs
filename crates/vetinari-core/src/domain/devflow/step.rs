//! Steps: the `External` nodes a vetinari worker executes.

use std::fmt;
use std::str::FromStr;

use super::outcome::label;

/// A decision the workflow author planned for, put to a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GateKind {
    /// Preflight found the ticket short of what an implementation needs.
    InfoNeeded,
    /// Review never approved within the allowed rounds.
    ReviewStuck,
}

/// What a step does, and so which outcomes it may report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepKind {
    /// Check the ticket carries what an implementation needs.
    Preflight,
    /// Implement the ticket, test first.
    Implement,
    /// Review the implementation.
    Review,
    /// Push the branch and open the pull request.
    OpenPr,
    /// Ask a person.
    Gate(GateKind),
}

/// A document named a step kind that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown step kind {name:?}")]
pub struct UnknownStepKind {
    /// The unrecognised name.
    pub name: String,
}

impl StepKind {
    /// Every step kind.
    pub const ALL: &'static [StepKind] = &[
        StepKind::Preflight,
        StepKind::Implement,
        StepKind::Review,
        StepKind::OpenPr,
        StepKind::Gate(GateKind::InfoNeeded),
        StepKind::Gate(GateKind::ReviewStuck),
    ];

    /// The name used in step configs, results and outputs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            StepKind::Preflight => "preflight",
            StepKind::Implement => "implement",
            StepKind::Review => "review",
            StepKind::OpenPr => "open-pr",
            StepKind::Gate(GateKind::InfoNeeded) => "info-needed",
            StepKind::Gate(GateKind::ReviewStuck) => "review-stuck",
        }
    }

    /// The branch labels this step's outcomes carry.
    #[must_use]
    pub const fn labels(&self) -> &'static [&'static str] {
        match self {
            StepKind::Preflight => &[label::READY, label::NEEDS_INFO],
            StepKind::Implement => &[label::IMPLEMENTED],
            StepKind::Review => &[label::APPROVED, label::CHANGES_REQUESTED],
            StepKind::OpenPr => &[label::OPENED],
            StepKind::Gate(GateKind::InfoNeeded) => &[label::RECHECK],
            StepKind::Gate(GateKind::ReviewStuck) => &[label::OPEN_ANYWAY],
        }
    }

    /// Whether a coding agent runs this step.
    #[must_use]
    pub const fn is_agent_step(&self) -> bool {
        matches!(
            self,
            StepKind::Preflight | StepKind::Implement | StepKind::Review
        )
    }
}

impl FromStr for StepKind {
    type Err = UnknownStepKind;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        StepKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.name() == raw)
            .ok_or_else(|| UnknownStepKind {
                name: raw.to_owned(),
            })
    }
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_kind_round_trips_through_its_name() {
        for kind in StepKind::ALL {
            assert_eq!(kind.name().parse::<StepKind>(), Ok(*kind));
        }
        assert_eq!(StepKind::OpenPr.name(), "open-pr");
        assert_eq!(StepKind::Gate(GateKind::InfoNeeded).name(), "info-needed");
        assert!("deploy".parse::<StepKind>().is_err());
    }

    #[test]
    fn each_step_kind_names_the_labels_it_can_report() {
        assert_eq!(
            StepKind::Preflight.labels(),
            &[label::READY, label::NEEDS_INFO]
        );
        assert_eq!(
            StepKind::Review.labels(),
            &[label::APPROVED, label::CHANGES_REQUESTED]
        );
        assert_eq!(
            StepKind::Gate(GateKind::ReviewStuck).labels(),
            &[label::OPEN_ANYWAY]
        );
    }

    #[test]
    fn only_preflight_implement_and_review_run_an_agent() {
        let agent: Vec<_> = StepKind::ALL
            .iter()
            .filter(|kind| kind.is_agent_step())
            .collect();
        assert_eq!(
            agent,
            [
                &StepKind::Preflight,
                &StepKind::Implement,
                &StepKind::Review
            ]
        );
    }
}
