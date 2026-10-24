//! The step result contract: the JSON file a skill writes last.
//!
//! ```json
//! { "schema": 1, "outcome": "changes-requested",
//!   "findings": [ { "file": "src/retry.rs", "line": 88, "text": "Retry-After ignored" } ] }
//! ```
//!
//! `outcome` must be a label the step kind can produce, and each label has
//! its own required fields. Anything else is a permanent failure: running the
//! same skill again will not write a different file.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use super::outcome::{
    Finding, GateAnswer, ImplementOutcome, Line, ListError, OpenPrOutcome, PreflightOutcome,
    ReviewOutcome, ShortList, StepOutcome, Summary, label,
};
use super::step::StepKind;
use crate::domain::text::TextError;

/// Why a result file could not be read as an outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResultError {
    /// The bytes were not a JSON object of the expected shape.
    #[error("result is not valid: {0}")]
    Malformed(String),
    /// The file declared a schema this version does not read.
    #[error("result schema is {found:?}, expected {expected}", expected = StepResult::SCHEMA)]
    Schema {
        /// The schema it declared, if any.
        found: Option<u64>,
    },
    /// The outcome is not one the step can report.
    #[error("{step} cannot report {found:?}; expected one of {expected:?}", expected = step.labels())]
    Outcome {
        /// The step that wrote it.
        step: StepKind,
        /// What it reported.
        found: String,
    },
    /// A field the outcome requires was absent.
    #[error("{outcome} needs {field}")]
    Missing {
        /// The outcome.
        outcome: &'static str,
        /// The absent field.
        field: &'static str,
    },
    /// A piece of text was empty or too long.
    #[error("{field}: {error}")]
    Text {
        /// Which field.
        field: &'static str,
        /// What was wrong.
        error: TextError,
    },
    /// A list was empty or too long.
    #[error("{field}: {error}")]
    List {
        /// Which field.
        field: &'static str,
        /// What was wrong.
        error: ListError,
    },
}

/// Reads result files.
#[derive(Debug, Clone, Copy)]
pub struct StepResult;

impl StepResult {
    /// The schema this version writes into skill prompts and reads back.
    pub const SCHEMA: u64 = 1;

    /// Parse the bytes of a result file written by a step of `kind`.
    pub fn parse(kind: StepKind, bytes: &[u8]) -> Result<StepOutcome, ResultError> {
        let header: SchemaHeader = decode(bytes)?;
        if header.schema != Some(Self::SCHEMA) {
            return Err(ResultError::Schema {
                found: header.schema,
            });
        }
        let file: ResultFile = decode(bytes)?;
        file.fields.into_outcome(kind, &file.outcome)
    }
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ResultError> {
    serde_json::from_slice(bytes).map_err(|error| ResultError::Malformed(error.to_string()))
}

#[derive(Deserialize)]
struct SchemaHeader {
    schema: Option<u64>,
}

#[derive(Deserialize)]
struct ResultFile {
    outcome: String,
    #[serde(flatten)]
    fields: OutcomeFields,
}

/// The fields an outcome may carry, as both a result file and a step's
/// output to the engine spell them.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct OutcomeFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    questions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    findings: Option<Vec<FindingFields>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FindingFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<NonZeroU32>,
    text: String,
}

impl From<&StepOutcome> for OutcomeFields {
    fn from(outcome: &StepOutcome) -> Self {
        let lines = |lines: &ShortList<Line>| {
            lines
                .as_slice()
                .iter()
                .map(|line| line.as_str().to_owned())
                .collect()
        };
        match outcome {
            StepOutcome::Preflight(PreflightOutcome::Ready { summary })
            | StepOutcome::Implement(ImplementOutcome::Implemented { summary })
            | StepOutcome::Review(ReviewOutcome::Approved { summary }) => Self {
                summary: Some(summary.as_str().to_owned()),
                ..Self::default()
            },
            StepOutcome::Preflight(PreflightOutcome::NeedsInfo { questions }) => Self {
                questions: Some(lines(questions)),
                ..Self::default()
            },
            StepOutcome::Review(ReviewOutcome::ChangesRequested { findings }) => Self {
                findings: Some(
                    findings
                        .as_slice()
                        .iter()
                        .map(|finding| FindingFields {
                            file: finding.file.as_ref().map(|file| file.as_str().to_owned()),
                            line: finding.line,
                            text: finding.text.as_str().to_owned(),
                        })
                        .collect(),
                ),
                ..Self::default()
            },
            StepOutcome::OpenPr(OpenPrOutcome::Opened { number, url }) => Self {
                number: Some(*number),
                url: Some(url.as_str().to_owned()),
                ..Self::default()
            },
            StepOutcome::Gate(GateAnswer::Recheck { note }) => Self {
                note: note.as_ref().map(|note| note.as_str().to_owned()),
                ..Self::default()
            },
            StepOutcome::Gate(GateAnswer::OpenAnyway) => Self::default(),
        }
    }
}

impl OutcomeFields {
    /// Read the outcome a step of `kind` reported as `reported`.
    pub(super) fn into_outcome(
        self,
        kind: StepKind,
        reported: &str,
    ) -> Result<StepOutcome, ResultError> {
        let Some(outcome) = kind
            .labels()
            .iter()
            .copied()
            .find(|known| *known == reported)
        else {
            return Err(ResultError::Outcome {
                step: kind,
                found: reported.to_owned(),
            });
        };
        let missing = |field| ResultError::Missing { outcome, field };
        Ok(match outcome {
            label::READY => StepOutcome::Preflight(PreflightOutcome::Ready {
                summary: summary(self.summary.ok_or_else(|| missing("summary"))?)?,
            }),
            label::NEEDS_INFO => StepOutcome::Preflight(PreflightOutcome::NeedsInfo {
                questions: list(
                    "questions",
                    self.questions
                        .ok_or_else(|| missing("questions"))?
                        .iter()
                        .map(|question| text("questions", question))
                        .collect::<Result<_, _>>()?,
                )?,
            }),
            label::IMPLEMENTED => StepOutcome::Implement(ImplementOutcome::Implemented {
                summary: summary(self.summary.ok_or_else(|| missing("summary"))?)?,
            }),
            label::APPROVED => StepOutcome::Review(ReviewOutcome::Approved {
                summary: summary(self.summary.ok_or_else(|| missing("summary"))?)?,
            }),
            label::CHANGES_REQUESTED => StepOutcome::Review(ReviewOutcome::ChangesRequested {
                findings: list(
                    "findings",
                    self.findings
                        .ok_or_else(|| missing("findings"))?
                        .into_iter()
                        .map(FindingFields::into_finding)
                        .collect::<Result<_, _>>()?,
                )?,
            }),
            label::OPENED => StepOutcome::OpenPr(OpenPrOutcome::Opened {
                number: self.number.ok_or_else(|| missing("number"))?,
                url: text("url", &self.url.ok_or_else(|| missing("url"))?)?,
            }),
            label::RECHECK => StepOutcome::Gate(GateAnswer::Recheck {
                note: self.note.map(|note| text("note", &note)).transpose()?,
            }),
            label::OPEN_ANYWAY => StepOutcome::Gate(GateAnswer::OpenAnyway),
            _ => {
                return Err(ResultError::Outcome {
                    step: kind,
                    found: reported.to_owned(),
                });
            }
        })
    }
}

impl FindingFields {
    fn into_finding(self) -> Result<Finding, ResultError> {
        Ok(Finding {
            file: self
                .file
                .map(|file| text("findings.file", &file))
                .transpose()?,
            line: self.line,
            text: text("findings.text", &self.text)?,
        })
    }
}

fn summary(raw: String) -> Result<Summary, ResultError> {
    Summary::parse(&raw).map_err(|error| ResultError::Text {
        field: "summary",
        error,
    })
}

fn text(field: &'static str, raw: &str) -> Result<Line, ResultError> {
    Line::parse(raw).map_err(|error| ResultError::Text { field, error })
}

fn list<T>(field: &'static str, items: Vec<T>) -> Result<ShortList<T>, ResultError> {
    ShortList::parse(items).map_err(|error| ResultError::List { field, error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::devflow::step::GateKind;

    fn parse(kind: StepKind, json: &str) -> Result<StepOutcome, ResultError> {
        StepResult::parse(kind, json.as_bytes())
    }

    fn line(text: &str) -> Line {
        Line::parse(text).expect("line")
    }

    #[test]
    fn preflight_reports_ready_with_a_summary() {
        assert_eq!(
            parse(
                StepKind::Preflight,
                r#"{ "schema": 1, "outcome": "ready", "summary": "Add a retry header" }"#
            ),
            Ok(StepOutcome::Preflight(PreflightOutcome::Ready {
                summary: Summary::parse("Add a retry header").expect("summary")
            }))
        );
    }

    #[test]
    fn preflight_reports_needs_info_with_its_questions() {
        assert_eq!(
            parse(
                StepKind::Preflight,
                r#"{ "schema": 1, "outcome": "needs-info", "questions": ["Page size?", "Cursor?"] }"#
            ),
            Ok(StepOutcome::Preflight(PreflightOutcome::NeedsInfo {
                questions: ShortList::parse(vec![line("Page size?"), line("Cursor?")])
                    .expect("list")
            }))
        );
    }

    #[test]
    fn implement_reports_implemented() {
        assert_eq!(
            parse(
                StepKind::Implement,
                r#"{ "schema": 1, "outcome": "implemented", "summary": "Honour Retry-After" }"#
            ),
            Ok(StepOutcome::Implement(ImplementOutcome::Implemented {
                summary: Summary::parse("Honour Retry-After").expect("summary")
            }))
        );
    }

    #[test]
    fn review_reports_approved_or_its_findings() {
        assert_eq!(
            parse(
                StepKind::Review,
                r#"{ "schema": 1, "outcome": "approved", "summary": "Looks right" }"#
            ),
            Ok(StepOutcome::Review(ReviewOutcome::Approved {
                summary: Summary::parse("Looks right").expect("summary")
            }))
        );
        assert_eq!(
            parse(
                StepKind::Review,
                r#"{ "schema": 1, "outcome": "changes-requested",
                     "summary": "Retry header is parsed but never honoured",
                     "findings": [ { "file": "src/retry.rs", "line": 88, "text": "Retry-After ignored on 503" },
                                   { "text": "no test for the jitter cap" } ] }"#
            ),
            Ok(StepOutcome::Review(ReviewOutcome::ChangesRequested {
                findings: ShortList::parse(vec![
                    Finding {
                        file: Some(line("src/retry.rs")),
                        line: NonZeroU32::new(88),
                        text: line("Retry-After ignored on 503"),
                    },
                    Finding {
                        file: None,
                        line: None,
                        text: line("no test for the jitter cap"),
                    },
                ])
                .expect("list")
            }))
        );
    }

    #[test]
    fn open_pr_reports_the_pull_request() {
        assert_eq!(
            parse(
                StepKind::OpenPr,
                r#"{ "schema": 1, "outcome": "opened", "number": 1204, "url": "https://github.com/o/r/pull/1204" }"#
            ),
            Ok(StepOutcome::OpenPr(OpenPrOutcome::Opened {
                number: 1204,
                url: line("https://github.com/o/r/pull/1204")
            }))
        );
    }

    #[test]
    fn gates_report_their_answers() {
        assert_eq!(
            parse(
                StepKind::Gate(GateKind::InfoNeeded),
                r#"{ "schema": 1, "outcome": "recheck", "note": "Page size is 50" }"#
            ),
            Ok(StepOutcome::Gate(GateAnswer::Recheck {
                note: Some(line("Page size is 50"))
            }))
        );
        assert_eq!(
            parse(
                StepKind::Gate(GateKind::InfoNeeded),
                r#"{ "schema": 1, "outcome": "recheck" }"#
            ),
            Ok(StepOutcome::Gate(GateAnswer::Recheck { note: None }))
        );
        assert_eq!(
            parse(
                StepKind::Gate(GateKind::ReviewStuck),
                r#"{ "schema": 1, "outcome": "open-anyway" }"#
            ),
            Ok(StepOutcome::Gate(GateAnswer::OpenAnyway))
        );
    }

    #[test]
    fn an_outcome_the_step_cannot_report_is_refused() {
        assert_eq!(
            parse(
                StepKind::Implement,
                r#"{ "schema": 1, "outcome": "approved", "summary": "s" }"#
            ),
            Err(ResultError::Outcome {
                step: StepKind::Implement,
                found: "approved".to_owned()
            })
        );
        assert_eq!(
            parse(
                StepKind::Gate(GateKind::InfoNeeded),
                r#"{ "schema": 1, "outcome": "open-anyway" }"#
            ),
            Err(ResultError::Outcome {
                step: StepKind::Gate(GateKind::InfoNeeded),
                found: "open-anyway".to_owned()
            })
        );
    }

    #[test]
    fn every_required_field_is_required() {
        let cases = [
            (StepKind::Preflight, "ready", "summary"),
            (StepKind::Preflight, "needs-info", "questions"),
            (StepKind::Implement, "implemented", "summary"),
            (StepKind::Review, "approved", "summary"),
            (StepKind::Review, "changes-requested", "findings"),
            (StepKind::OpenPr, "opened", "number"),
        ];
        for (kind, outcome, field) in cases {
            let json = format!(r#"{{ "schema": 1, "outcome": "{outcome}" }}"#);
            assert_eq!(
                parse(kind, &json),
                Err(ResultError::Missing {
                    outcome: label_of(outcome),
                    field
                }),
                "{outcome}"
            );
        }
        assert_eq!(
            parse(
                StepKind::OpenPr,
                r#"{ "schema": 1, "outcome": "opened", "number": 3 }"#
            ),
            Err(ResultError::Missing {
                outcome: label::OPENED,
                field: "url"
            })
        );
    }

    fn label_of(outcome: &str) -> &'static str {
        label::ALL
            .iter()
            .copied()
            .find(|known| *known == outcome)
            .expect("known label")
    }

    #[test]
    fn empty_lists_are_refused() {
        assert_eq!(
            parse(
                StepKind::Preflight,
                r#"{ "schema": 1, "outcome": "needs-info", "questions": [] }"#
            ),
            Err(ResultError::List {
                field: "questions",
                error: ListError::Empty
            })
        );
        assert_eq!(
            parse(
                StepKind::Review,
                r#"{ "schema": 1, "outcome": "changes-requested", "findings": [] }"#
            ),
            Err(ResultError::List {
                field: "findings",
                error: ListError::Empty
            })
        );
    }

    #[test]
    fn oversize_text_and_lists_are_refused() {
        let summary = "x".repeat(Summary::MAX_LEN + 1);
        assert_eq!(
            parse(
                StepKind::Implement,
                &format!(r#"{{ "schema": 1, "outcome": "implemented", "summary": "{summary}" }}"#)
            ),
            Err(ResultError::Text {
                field: "summary",
                error: TextError::TooLong {
                    len: Summary::MAX_LEN + 1,
                    max: Summary::MAX_LEN
                }
            })
        );
        let finding = format!(r#"{{ "text": "{}" }}"#, "y".repeat(Line::MAX_LEN + 1));
        assert!(matches!(
            parse(
                StepKind::Review,
                &format!(
                    r#"{{ "schema": 1, "outcome": "changes-requested", "findings": [{finding}] }}"#
                )
            ),
            Err(ResultError::Text {
                field: "findings.text",
                ..
            })
        ));
        let findings = vec![r#"{ "text": "t" }"#; 21].join(",");
        assert_eq!(
            parse(
                StepKind::Review,
                &format!(
                    r#"{{ "schema": 1, "outcome": "changes-requested", "findings": [{findings}] }}"#
                )
            ),
            Err(ResultError::List {
                field: "findings",
                error: ListError::TooMany { len: 21, max: 20 }
            })
        );
    }

    #[test]
    fn a_wrong_or_missing_schema_is_refused() {
        assert_eq!(
            parse(
                StepKind::Implement,
                r#"{ "schema": 2, "outcome": "implemented", "summary": "s" }"#
            ),
            Err(ResultError::Schema { found: Some(2) })
        );
        assert_eq!(
            parse(
                StepKind::Implement,
                r#"{ "outcome": "implemented", "summary": "s" }"#
            ),
            Err(ResultError::Schema { found: None })
        );
    }

    #[test]
    fn bytes_that_are_not_a_result_object_are_malformed() {
        for json in [
            "",
            "not json",
            "[]",
            r#"{ "schema": 1 }"#,
            r#"{ "schema": 1, "outcome": 3 }"#,
        ] {
            assert!(
                matches!(
                    parse(StepKind::Review, json),
                    Err(ResultError::Malformed(_))
                ),
                "{json}"
            );
        }
    }
}
