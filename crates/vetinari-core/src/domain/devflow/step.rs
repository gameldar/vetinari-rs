//! Steps: the `External` nodes a vetinari worker executes.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::outcome::label;
use super::template::{RepoTarget, TemplateKind, TicketRef};
use crate::domain::git::BranchName;
use crate::domain::ids::{AgentId, ProfileId, SkillId, Slug};
use crate::domain::text::NonEmptyText;

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

/// What every `External` node's `config` holds: written by `compile`, read
/// back from the claim, so a step carries everything it needs and no step
/// has to agree with another about where the work is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepConfig {
    /// What the step does.
    pub step: StepKind,
    /// The template the run was compiled from.
    pub template: TemplateKind,
    /// The ticket the run works on.
    pub ticket: TicketRef,
    /// The repository the work is in.
    pub repo: RepoTarget,
    /// Whose routing, worktree root and credentials to use.
    pub profile: ProfileId,
    /// The run's one branch, fixed at compile time.
    pub branch: BranchName,
    /// What the branch is cut from; the default branch when absent.
    pub base: Option<BranchName>,
    /// The skill an agent step runs.
    pub skill: Option<SkillId>,
    /// The agent an agent step is pinned to.
    pub agent: Option<AgentId>,
}

impl StepConfig {
    /// The schema this version writes and reads.
    pub const SCHEMA: u64 = 1;

    /// Read the config back from a claimed task. A config that does not
    /// parse is a task vetinari did not write, or a schema it no longer
    /// reads; neither improves on retry.
    pub fn parse(config: &serde_json::Value) -> Result<Self, ConfigError> {
        let header = SchemaHeader::deserialize(config)
            .map_err(|error| ConfigError::Malformed(error.to_string()))?;
        if header.schema != Some(Self::SCHEMA) {
            return Err(ConfigError::Schema {
                found: header.schema,
            });
        }
        let document = ConfigDocument::deserialize(config)
            .map_err(|error| ConfigError::Malformed(error.to_string()))?;
        Ok(Self {
            step: field("step", document.step.parse())?,
            template: field("template", document.template.parse())?,
            ticket: TicketRef {
                source: field("ticket.source", Slug::parse(&document.ticket.source))?,
                key: field("ticket.key", NonEmptyText::parse(&document.ticket.key))?,
                title: field("ticket.title", NonEmptyText::parse(&document.ticket.title))?,
                url: document.ticket.url,
            },
            repo: RepoTarget {
                path: document.repo.path.into(),
                remote: field("repo.remote", NonEmptyText::parse(&document.repo.remote))?,
            },
            profile: field("profile", Slug::parse(&document.profile))?,
            branch: field("branch", BranchName::parse(&document.branch))?,
            base: optional("base", document.base.as_deref(), BranchName::parse)?,
            skill: optional("skill", document.skill.as_deref(), Slug::parse)?,
            agent: optional("agent", document.agent.as_deref(), Slug::parse)?,
        })
    }

    /// The document an `External` node's `config` holds.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let document = ConfigDocument {
            schema: Self::SCHEMA,
            step: self.step.name().to_owned(),
            template: self.template.name().to_owned(),
            ticket: TicketDocument {
                source: self.ticket.source.to_string(),
                key: self.ticket.key.to_string(),
                title: self.ticket.title.to_string(),
                url: self.ticket.url.clone(),
            },
            repo: RepoDocument {
                path: self.repo.path.to_string_lossy().into_owned(),
                remote: self.repo.remote.to_string(),
            },
            profile: self.profile.to_string(),
            branch: self.branch.to_string(),
            base: self.base.as_ref().map(ToString::to_string),
            skill: self.skill.as_ref().map(ToString::to_string),
            agent: self.agent.as_ref().map(ToString::to_string),
        };
        serde_json::to_value(document).expect("a config document always serialises")
    }
}

/// Why a step config could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The document was not shaped like a step config.
    #[error("step config is not valid: {0}")]
    Malformed(String),
    /// The document declared a schema this version does not read.
    #[error("step config schema is {found:?}, expected {expected}", expected = StepConfig::SCHEMA)]
    Schema {
        /// The schema it declared, if any.
        found: Option<u64>,
    },
    /// A field held a value that does not parse.
    #[error("step config {field}: {detail}")]
    Invalid {
        /// Which field.
        field: &'static str,
        /// What was wrong.
        detail: String,
    },
}

fn field<T, E: fmt::Display>(name: &'static str, parsed: Result<T, E>) -> Result<T, ConfigError> {
    parsed.map_err(|error| ConfigError::Invalid {
        field: name,
        detail: error.to_string(),
    })
}

fn optional<T, E: fmt::Display>(
    name: &'static str,
    raw: Option<&str>,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Option<T>, ConfigError> {
    raw.map(|raw| field(name, parse(raw))).transpose()
}

#[derive(Deserialize)]
struct SchemaHeader {
    schema: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct ConfigDocument {
    schema: u64,
    step: String,
    template: String,
    ticket: TicketDocument,
    repo: RepoDocument,
    profile: String,
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct TicketDocument {
    source: String,
    key: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RepoDocument {
    path: String,
    remote: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

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

    fn sample_config() -> StepConfig {
        StepConfig {
            step: StepKind::Implement,
            template: TemplateKind::FeatureFromTicket,
            ticket: TicketRef {
                source: Slug::parse("jira").expect("id"),
                key: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
                url: Some("https://acme.atlassian.net/browse/ENG-412".to_owned()),
            },
            repo: RepoTarget {
                path: PathBuf::from("/work/repos/web"),
                remote: NonEmptyText::parse("acme/web").expect("remote"),
            },
            profile: Slug::parse("work").expect("id"),
            branch: BranchName::sanitized("vetinari/eng-412-rate-limit-retries"),
            base: Some(BranchName::sanitized("main")),
            skill: Some(Slug::parse("implement-tdd").expect("id")),
            agent: Some(Slug::parse("claude-code").expect("id")),
        }
    }

    #[test]
    fn a_config_round_trips_through_its_document() {
        let config = sample_config();
        assert_eq!(StepConfig::parse(&config.to_json()), Ok(config));
        let mut bare = sample_config();
        bare.ticket.url = None;
        bare.base = None;
        bare.skill = None;
        bare.agent = None;
        assert_eq!(StepConfig::parse(&bare.to_json()), Ok(bare));
    }

    #[test]
    fn a_config_of_another_schema_is_refused() {
        let mut document = sample_config().to_json();
        document["schema"] = json!(2);
        assert_eq!(
            StepConfig::parse(&document),
            Err(ConfigError::Schema { found: Some(2) })
        );
        document.as_object_mut().expect("object").remove("schema");
        assert_eq!(
            StepConfig::parse(&document),
            Err(ConfigError::Schema { found: None })
        );
    }

    #[test]
    fn a_config_another_tool_wrote_is_malformed() {
        for document in [
            json!("deploy"),
            json!([1]),
            json!({ "schema": 1, "task": "deploy" }),
        ] {
            assert!(
                matches!(StepConfig::parse(&document), Err(ConfigError::Malformed(_))),
                "{document}"
            );
        }
    }

    #[test]
    fn each_field_is_parsed_into_its_type() {
        let cases = [
            ("step", json!("deploy")),
            ("template", json!("bugfix")),
            ("profile", json!("not a slug")),
            ("branch", json!("Not A Branch")),
            ("base", json!("a b")),
            ("skill", json!("")),
            ("agent", json!("a:b")),
        ];
        for (field, value) in cases {
            let mut document = sample_config().to_json();
            document[field] = value;
            assert!(
                matches!(
                    StepConfig::parse(&document),
                    Err(ConfigError::Invalid { field: found, .. }) if found == field
                ),
                "{field}"
            );
        }
        let mut document = sample_config().to_json();
        document["ticket"]["key"] = json!(" ");
        assert!(matches!(
            StepConfig::parse(&document),
            Err(ConfigError::Invalid {
                field: "ticket.key",
                ..
            })
        ));
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
