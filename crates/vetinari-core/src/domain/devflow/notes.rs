//! What a run's popup says beneath its rows (design 9.3, 8.2): the
//! questions the latest preflight asked, and what the latest review found.
//! Both come from the run's outputs, keyed by instance, since the graph view
//! carries no outputs.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde_json::Value;

use super::compile::node_step;
use super::ids::NodeInstance;
use super::outcome::{Finding, Line, PreflightOutcome, ReviewOutcome, StepOutcome};
use super::step::StepKind;
use super::task::StepOutput;
use super::template::TemplateKind;

/// What the popup says beneath its rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunNotes {
    /// What the latest preflight needs the ticket to answer; empty when it
    /// found the ticket ready.
    pub questions: Vec<Line>,
    /// What the latest review asked to change, and on which round.
    pub findings: Option<ReviewFindings>,
}

/// One review's findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFindings {
    /// The round the review was on.
    pub round: NonZeroU32,
    /// What it found.
    pub findings: Vec<Finding>,
}

/// Read the notes from a run's outputs. An output that does not read as a
/// step's is left out, as one from a step this version does not know.
#[must_use]
pub fn notes(template: TemplateKind, outputs: &BTreeMap<String, Value>) -> RunNotes {
    let mut preflight: Option<(Vec<NonZeroU32>, Vec<Line>)> = None;
    let mut review: Option<(Vec<NonZeroU32>, ReviewFindings)> = None;
    for (key, value) in outputs {
        let Ok(instance) = NodeInstance::parse(key) else {
            continue;
        };
        let Ok(output) = StepOutput::parse(value) else {
            continue;
        };
        let at = instance.iterations().to_vec();
        match (node_step(template, instance.node()), output.outcome) {
            (Some(StepKind::Preflight), StepOutcome::Preflight(outcome)) => {
                if preflight.as_ref().is_some_and(|(seen, _)| *seen > at) {
                    continue;
                }
                let questions = match outcome {
                    PreflightOutcome::NeedsInfo { questions } => questions.as_slice().to_vec(),
                    PreflightOutcome::Ready { .. } => Vec::new(),
                };
                preflight = Some((at, questions));
            }
            (
                Some(StepKind::Review),
                StepOutcome::Review(ReviewOutcome::ChangesRequested { findings }),
            ) => {
                if review.as_ref().is_some_and(|(seen, _)| *seen > at) {
                    continue;
                }
                let round = at.last().copied().unwrap_or(NonZeroU32::MIN);
                review = Some((
                    at,
                    ReviewFindings {
                        round,
                        findings: findings.as_slice().to_vec(),
                    },
                ));
            }
            _ => {}
        }
    }
    RunNotes {
        questions: preflight
            .map(|(_, questions)| questions)
            .unwrap_or_default(),
        findings: review.map(|(_, findings)| findings),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn outputs(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    fn needs_info(question: &str) -> Value {
        json!({
            "branch": "needs-info", "step": "preflight", "questions": [question],
            "git_branch": "vetinari/eng-412"
        })
    }

    fn changes(text: &str) -> Value {
        json!({
            "branch": "changes-requested", "step": "review",
            "findings": [{ "file": "src/retry.rs", "line": 88, "text": text }],
            "git_branch": "vetinari/eng-412"
        })
    }

    #[test]
    fn the_latest_preflight_says_what_the_ticket_must_answer() {
        let notes = notes(
            TemplateKind::FeatureFromTicket,
            &outputs(&[
                ("preflight@1", needs_info("Page size?")),
                ("preflight@2", needs_info("Cursor stable?")),
                (
                    "ask-info@1",
                    json!({ "branch": "recheck", "step": "info-needed",
                    "git_branch": "vetinari/eng-412" }),
                ),
            ]),
        );
        assert_eq!(
            notes.questions.iter().map(Line::as_str).collect::<Vec<_>>(),
            ["Cursor stable?"]
        );
    }

    #[test]
    fn a_ready_preflight_asks_nothing() {
        let notes = notes(
            TemplateKind::FeatureFromTicket,
            &outputs(&[
                ("preflight@1", needs_info("Page size?")),
                (
                    "preflight@2",
                    json!({ "branch": "ready", "step": "preflight",
                    "summary": "Paginate", "git_branch": "vetinari/eng-412" }),
                ),
            ]),
        );
        assert!(notes.questions.is_empty());
    }

    #[test]
    fn the_latest_review_that_asked_for_changes_is_kept_with_its_round() {
        let notes = notes(
            TemplateKind::FeatureFromTicket,
            &outputs(&[
                ("review@1", changes("Retry-After ignored")),
                ("review@2", changes("no test for the jitter cap")),
            ]),
        );
        let findings = notes.findings.expect("findings");
        assert_eq!(findings.round.get(), 2);
        assert_eq!(
            findings.findings[0].text.as_str(),
            "no test for the jitter cap"
        );
        assert_eq!(findings.findings[0].line.map(NonZeroU32::get), Some(88));
    }

    #[test]
    fn an_output_that_is_not_a_steps_is_left_out() {
        let notes = notes(
            TemplateKind::FeatureFromTicket,
            &outputs(&[
                ("preflight@1", json!({ "anything": true })),
                ("x@y@z", json!({})),
            ]),
        );
        assert_eq!(notes, RunNotes::default());
    }
}
