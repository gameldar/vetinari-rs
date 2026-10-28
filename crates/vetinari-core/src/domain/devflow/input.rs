//! What a caller asks a template to run: a JSON document each template
//! parses for itself, so starting a run needs no code per template outside
//! it. What vetinari can look up, such as a clone's `owner/name`, a caller
//! may leave out.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ids::Rounds;
use super::template::{DevWorkflow, FeatureFromTicket, RepoTarget, TemplateKind, TicketRef};
use crate::domain::ids::{ProfileId, SourceId};
use crate::domain::text::NonEmptyText;

/// One field a template's input takes, for callers building it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InputField {
    /// Its path in the document, e.g. `ticket.key`.
    pub name: &'static str,
    /// Whether a start without it is refused.
    pub required: bool,
    /// What it holds.
    pub about: &'static str,
}

/// A template's input did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{template} input: {problem}")]
pub struct InputError {
    /// The template that refused it.
    pub template: TemplateKind,
    /// Why, naming the field.
    pub problem: String,
}

/// A parsed start, before vetinari fills in what it looks up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowInput {
    /// [`TemplateKind::FeatureFromTicket`]'s input.
    FeatureFromTicket(FeatureInput),
}

/// `feature-from-ticket`'s input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureInput {
    /// The ticket to implement.
    pub ticket: TicketRef,
    /// Where to implement it.
    pub repo: RepoInput,
    /// Whose routing, worktree root and credentials to use.
    pub profile: ProfileId,
    /// How many implement and review rounds before asking a person.
    pub max_review_rounds: Rounds,
    /// How many preflight rechecks before the run stops at intake.
    pub max_intake_rounds: Rounds,
}

/// The repository a start names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoInput {
    /// A clone with its `owner/name` given.
    Named(RepoTarget),
    /// A clone whose `owner/name` is read from its `origin`.
    Clone(PathBuf),
}

impl TemplateKind {
    /// Every template, for listing.
    pub const ALL: [TemplateKind; 1] = [TemplateKind::FeatureFromTicket];

    /// What a run of it does.
    #[must_use]
    pub const fn about(&self) -> &'static str {
        match self {
            TemplateKind::FeatureFromTicket => {
                "Ticket to pull request: preflight, implement, review, open the PR"
            }
        }
    }

    /// The fields its input takes.
    #[must_use]
    pub const fn input_fields(&self) -> &'static [InputField] {
        match self {
            TemplateKind::FeatureFromTicket => FEATURE_FIELDS,
        }
    }

    /// Parse a start's input for this template.
    pub fn parse_input(&self, input: &Value) -> Result<WorkflowInput, InputError> {
        let refused = |problem: String| InputError {
            template: *self,
            problem,
        };
        match self {
            TemplateKind::FeatureFromTicket => feature_input(input)
                .map(WorkflowInput::FeatureFromTicket)
                .map_err(refused),
        }
    }
}

impl WorkflowInput {
    /// Which template it is for.
    #[must_use]
    pub const fn kind(&self) -> TemplateKind {
        match self {
            WorkflowInput::FeatureFromTicket(_) => TemplateKind::FeatureFromTicket,
        }
    }
}

impl FeatureInput {
    /// The workflow, run in `repo`.
    #[must_use]
    pub fn in_repo(self, repo: RepoTarget) -> FeatureFromTicket {
        FeatureFromTicket {
            ticket: self.ticket,
            repo,
            profile: self.profile,
            base: None,
            max_review_rounds: self.max_review_rounds,
            max_intake_rounds: self.max_intake_rounds,
            agent: None,
        }
    }
}

impl DevWorkflow {
    /// The input that starts this workflow, as a caller sends it.
    #[must_use]
    pub fn input(&self) -> Value {
        match self {
            DevWorkflow::FeatureFromTicket(feature) => json!({
                "ticket": {
                    "source": feature.ticket.source.as_str(),
                    "key": feature.ticket.key.as_str(),
                    "title": feature.ticket.title.as_str(),
                    "url": feature.ticket.url,
                },
                "repo": {
                    "path": feature.repo.path,
                    "remote": feature.repo.remote.as_str(),
                },
                "profile": feature.profile.as_str(),
                "max_review_rounds": feature.max_review_rounds.get().get(),
                "max_intake_rounds": feature.max_intake_rounds.get().get(),
            }),
        }
    }
}

impl From<DevWorkflow> for WorkflowInput {
    fn from(workflow: DevWorkflow) -> Self {
        match workflow {
            DevWorkflow::FeatureFromTicket(feature) => {
                WorkflowInput::FeatureFromTicket(FeatureInput {
                    ticket: feature.ticket,
                    repo: RepoInput::Named(feature.repo),
                    profile: feature.profile,
                    max_review_rounds: feature.max_review_rounds,
                    max_intake_rounds: feature.max_intake_rounds,
                })
            }
        }
    }
}

const FEATURE_FIELDS: &[InputField] = &[
    InputField {
        name: "ticket.source",
        required: true,
        about: "Where the ticket came from, e.g. jira",
    },
    InputField {
        name: "ticket.key",
        required: true,
        about: "The tracker's key, e.g. ENG-412",
    },
    InputField {
        name: "ticket.title",
        required: true,
        about: "The ticket's title; the run's branch is named after it",
    },
    InputField {
        name: "ticket.url",
        required: false,
        about: "Where to read the ticket",
    },
    InputField {
        name: "repo.path",
        required: true,
        about: "The absolute path of the local clone",
    },
    InputField {
        name: "repo.remote",
        required: false,
        about: "owner/name on the host; read from the clone's origin when left out",
    },
    InputField {
        name: "profile",
        required: true,
        about: "The vetinari profile whose routing and credentials to use",
    },
    InputField {
        name: "max_review_rounds",
        required: false,
        about: "Implement and review rounds before asking you (default 3)",
    },
    InputField {
        name: "max_intake_rounds",
        required: false,
        about: "Preflight rechecks before the run stops at intake (default 3)",
    },
];

fn feature_input(input: &Value) -> Result<FeatureInput, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Document {
        ticket: Ticket,
        repo: Repo,
        profile: String,
        max_review_rounds: Option<u32>,
        max_intake_rounds: Option<u32>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ticket {
        source: String,
        key: String,
        title: String,
        url: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Repo {
        path: PathBuf,
        remote: Option<String>,
    }
    let document = Document::deserialize(input).map_err(|error| error.to_string())?;
    let text = |field: &str, raw: &str| {
        NonEmptyText::parse(raw).map_err(|error| format!("{field}: {error}"))
    };
    let rounds = |field: &str, raw: Option<u32>| {
        raw.map(Rounds::new)
            .transpose()
            .map(|rounds| rounds.unwrap_or(Rounds::DEFAULT))
            .map_err(|error| format!("{field}: {error}"))
    };
    let path = document.repo.path;
    if !path.is_absolute() {
        return Err(format!("repo.path: {} is not absolute", path.display()));
    }
    let repo = match document.repo.remote {
        Some(remote) => RepoInput::Named(
            RepoTarget::at(path, &remote).map_err(|error| format!("repo.remote: {error}"))?,
        ),
        None => RepoInput::Clone(path),
    };
    Ok(FeatureInput {
        ticket: TicketRef {
            source: SourceId::parse(&document.ticket.source)
                .map_err(|error| format!("ticket.source: {error}"))?,
            key: text("ticket.key", &document.ticket.key)?,
            title: text("ticket.title", &document.ticket.title)?,
            url: document.ticket.url,
        },
        repo,
        profile: ProfileId::parse(&document.profile)
            .map_err(|error| format!("profile: {error}"))?,
        max_review_rounds: rounds("max_review_rounds", document.max_review_rounds)?,
        max_intake_rounds: rounds("max_intake_rounds", document.max_intake_rounds)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::devflow::template::sample_feature;

    const FEATURE: TemplateKind = TemplateKind::FeatureFromTicket;

    fn full() -> Value {
        json!({
            "ticket": {
                "source": "jira",
                "key": "ENG-412",
                "title": "Rate-limit retries",
                "url": "https://acme.atlassian.net/browse/ENG-412",
            },
            "repo": { "path": "/src/web", "remote": "acme/web" },
            "profile": "work",
            "max_review_rounds": 5,
            "max_intake_rounds": 2,
        })
    }

    fn feature(input: &Value) -> FeatureInput {
        match FEATURE.parse_input(input) {
            Ok(WorkflowInput::FeatureFromTicket(feature)) => feature,
            Err(error) => panic!("{error}"),
        }
    }

    fn without(input: &Value, field: &str) -> Value {
        let mut input = input.clone();
        let (parent, leaf) = match field.rsplit_once('.') {
            Some((parent, leaf)) => (&mut input[parent], leaf),
            None => (&mut input, field),
        };
        parent.as_object_mut().expect("object").remove(leaf);
        input
    }

    fn leaves(value: &Value, prefix: &str, into: &mut Vec<String>) {
        match value.as_object() {
            Some(fields) => {
                for (name, value) in fields {
                    let path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    leaves(value, &path, into);
                }
            }
            None => into.push(prefix.to_owned()),
        }
    }

    #[test]
    fn a_feature_is_started_for_a_ticket_in_a_named_clone() {
        let started = feature(&full());
        assert_eq!(started.ticket.key.as_str(), "ENG-412");
        assert_eq!(started.ticket.source.as_str(), "jira");
        assert_eq!(
            started.ticket.url.as_deref(),
            Some("https://acme.atlassian.net/browse/ENG-412")
        );
        assert_eq!(
            started.repo,
            RepoInput::Named(RepoTarget::at(PathBuf::from("/src/web"), "acme/web").expect("repo"))
        );
        assert_eq!(started.profile.as_str(), "work");
        assert_eq!(started.max_review_rounds.get().get(), 5);
        assert_eq!(started.max_intake_rounds.get().get(), 2);
    }

    #[test]
    fn a_clone_without_a_remote_is_left_to_be_looked_up() {
        assert_eq!(
            feature(&without(&full(), "repo.remote")).repo,
            RepoInput::Clone(PathBuf::from("/src/web"))
        );
    }

    #[test]
    fn the_listed_fields_are_the_fields_the_input_takes() {
        let mut taken = Vec::new();
        leaves(&full(), "", &mut taken);
        taken.sort();
        let mut listed: Vec<String> = FEATURE
            .input_fields()
            .iter()
            .map(|field| field.name.to_owned())
            .collect();
        listed.sort();
        assert_eq!(listed, taken);
    }

    #[test]
    fn a_start_without_a_required_field_is_refused_naming_it() {
        for field in FEATURE.input_fields() {
            let leaf = field.name.rsplit('.').next().expect("leaf");
            let parsed = FEATURE.parse_input(&without(&full(), field.name));
            if field.required {
                let error = parsed.expect_err(field.name);
                assert!(error.to_string().contains(leaf), "{}: {error}", field.name);
            } else {
                assert!(parsed.is_ok(), "{}: {parsed:?}", field.name);
            }
        }
    }

    #[test]
    fn rounds_left_out_are_the_default() {
        let started = feature(&without(
            &without(&full(), "max_review_rounds"),
            "max_intake_rounds",
        ));
        assert_eq!(
            (started.max_review_rounds, started.max_intake_rounds),
            (Rounds::DEFAULT, Rounds::DEFAULT)
        );
    }

    #[test]
    fn a_bad_value_or_an_unknown_field_is_refused_naming_it() {
        for (field, value) in [
            ("max_review_rounds", json!(21)),
            ("profile", json!("Not A Profile")),
            (
                "ticket",
                json!({ "source": "jira", "key": "", "title": "t" }),
            ),
            ("repo", json!({ "path": "src/web" })),
            ("repo", json!({ "path": "/src/web", "remote": "web" })),
            ("max_review_round", json!(3)),
        ] {
            let mut input = full();
            input[field] = value;
            let error = FEATURE.parse_input(&input).expect_err(field);
            assert_eq!(error.template, FEATURE);
            let named = match field {
                "ticket" => "key",
                "repo" => "repo",
                other => other,
            };
            assert!(error.problem.contains(named), "{field}: {error}");
        }
        assert!(FEATURE.parse_input(&json!("ENG-412")).is_err());
    }

    #[test]
    fn a_workflow_reads_back_from_the_input_it_writes() {
        let workflow = DevWorkflow::FeatureFromTicket(sample_feature());
        let input = workflow.input();
        assert_eq!(
            FEATURE.parse_input(&input),
            Ok(WorkflowInput::from(workflow))
        );
    }

    #[test]
    fn a_named_feature_input_is_the_workflow_it_names() {
        let WorkflowInput::FeatureFromTicket(input) =
            WorkflowInput::from(DevWorkflow::FeatureFromTicket(sample_feature()));
        let RepoInput::Named(repo) = input.repo.clone() else {
            panic!("named");
        };
        assert_eq!(input.in_repo(repo), sample_feature());
    }

    #[test]
    fn every_template_says_what_it_does() {
        for template in TemplateKind::ALL {
            assert!(!template.about().is_empty());
            assert!(!template.input_fields().is_empty());
        }
    }
}
