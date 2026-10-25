//! Templates: the dev workflows vetinari knows how to run.
//!
//! A closed enum, because every template needs its own compile function, its
//! own phase mapping and its own tests. A free-form definition language would
//! be a second engine.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use super::ids::Rounds;
use crate::domain::git::BranchName;
use crate::domain::ids::{AgentId, ProfileId, SourceId};
use crate::domain::text::NonEmptyText;
use crate::domain::workspace::SpaceName;

/// Which template a run was compiled from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TemplateKind {
    /// [`FeatureFromTicket`].
    FeatureFromTicket,
}

/// A document named a template that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown template {name:?}")]
pub struct UnknownTemplate {
    /// The unrecognised name.
    pub name: String,
}

impl TemplateKind {
    /// The name used in step configs and on the socket.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            TemplateKind::FeatureFromTicket => "feature-from-ticket",
        }
    }

    /// The name its definitions are registered under, `vetinari-<short>-`.
    #[must_use]
    pub const fn short_name(&self) -> &'static str {
        match self {
            TemplateKind::FeatureFromTicket => "feature",
        }
    }
}

impl FromStr for TemplateKind {
    type Err = UnknownTemplate;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "feature-from-ticket" => Ok(TemplateKind::FeatureFromTicket),
            _ => Err(UnknownTemplate {
                name: raw.to_owned(),
            }),
        }
    }
}

impl fmt::Display for TemplateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The ticket a run works on, as the queue item that started it showed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRef {
    /// The attention source the ticket came from.
    pub source: SourceId,
    /// The tracker's own key, e.g. `ENG-412`.
    pub key: NonEmptyText,
    /// Its title.
    pub title: NonEmptyText,
    /// Where to read it.
    pub url: Option<String>,
}

/// The repository a run's worktree is cut from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTarget {
    /// The local clone.
    pub path: PathBuf,
    /// `owner/name` on the host, for opening the pull request.
    pub remote: NonEmptyText,
}

/// A dev workflow, with everything a run of it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevWorkflow {
    /// Ticket to pull request: preflight, implement, review, open the PR.
    FeatureFromTicket(FeatureFromTicket),
}

impl DevWorkflow {
    /// Which template this is.
    #[must_use]
    pub const fn kind(&self) -> TemplateKind {
        match self {
            DevWorkflow::FeatureFromTicket(_) => TemplateKind::FeatureFromTicket,
        }
    }
}

/// Ticket to pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureFromTicket {
    /// The ticket to implement.
    pub ticket: TicketRef,
    /// Where to implement it.
    pub repo: RepoTarget,
    /// Whose routing, worktree root and credentials to use.
    pub profile: ProfileId,
    /// What to branch from; the repository's default branch when absent.
    pub base: Option<BranchName>,
    /// How many implement and review rounds before asking a person.
    pub max_review_rounds: Rounds,
    /// How many preflight rechecks before the run stops at intake.
    pub max_intake_rounds: Rounds,
    /// Pins every agent step to one agent; routing decides when absent.
    pub agent: Option<AgentId>,
}

impl FeatureFromTicket {
    /// The run's one branch, `vetinari/<ticket-key>-<title>`, which every
    /// step of the run works on.
    #[must_use]
    pub fn branch(&self) -> BranchName {
        let title = SpaceName::sanitized(self.ticket.title.as_str());
        BranchName::sanitized(&format!("vetinari/{}-{title}", self.ticket.key))
    }
}

/// A ticket-to-PR workflow for tests elsewhere in the domain.
#[cfg(test)]
pub(crate) fn sample_feature() -> FeatureFromTicket {
    FeatureFromTicket {
        ticket: TicketRef {
            source: SourceId::parse("jira").expect("id"),
            key: NonEmptyText::parse("ENG-412").expect("key"),
            title: NonEmptyText::parse("Rate-limit retries / honour 503").expect("title"),
            url: None,
        },
        repo: RepoTarget {
            path: PathBuf::from("/work/repos/web"),
            remote: NonEmptyText::parse("acme/web").expect("remote"),
        },
        profile: ProfileId::parse("work").expect("id"),
        base: None,
        max_review_rounds: Rounds::new(3).expect("rounds"),
        max_intake_rounds: Rounds::new(3).expect("rounds"),
        agent: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_name_round_trips() {
        assert_eq!(
            "feature-from-ticket".parse::<TemplateKind>(),
            Ok(TemplateKind::FeatureFromTicket)
        );
        assert!("deploy".parse::<TemplateKind>().is_err());
    }

    #[test]
    fn the_run_branch_is_named_after_the_ticket() {
        assert_eq!(
            sample_feature().branch().as_str(),
            "vetinari/eng-412-rate-limit-retries-honour-503"
        );
    }
}
