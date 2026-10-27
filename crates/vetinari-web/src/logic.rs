//! What the page shows, decided without a browser.

use std::collections::BTreeMap;

use vetinari_web_model::{GateAnswer, ListEvent, Mark, NeedsYou, Point, RunEntry, RunStatus};

/// The runs the list shows, kept current from its event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunList {
    runs: Vec<RunEntry>,
    sign_in_needed: bool,
}

impl RunList {
    /// Take in one event from the stream.
    pub fn apply(&mut self, event: ListEvent) {
        match event {
            ListEvent::Snapshot { runs } => self.runs = runs,
            ListEvent::Run { entry } => {
                match self.runs.iter_mut().find(|known| known.run == entry.run) {
                    Some(known) => *known = entry,
                    None => self.runs.push(entry),
                }
            }
            ListEvent::Removed { run } => self.runs.retain(|known| known.run != run),
            ListEvent::SignIn { needed } => self.sign_in_needed = needed,
        }
    }

    /// The runs: those waiting on you first, then those still going, then
    /// the finished, each in the order they arrived.
    #[must_use]
    pub fn entries(&self) -> Vec<&RunEntry> {
        let mut entries: Vec<&RunEntry> = self.runs.iter().collect();
        entries.sort_by_key(|entry| match (&entry.needs_you, entry.status) {
            (Some(_), _) => 0,
            (None, RunStatus::Active) => 1,
            (None, _) => 2,
        });
        entries
    }

    /// Whether the daemon needs signing in to its gateway.
    #[must_use]
    pub const fn sign_in_needed(&self) -> bool {
        self.sign_in_needed
    }
}

/// The round each loop is looked at on; a loop not named shows its latest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rounds(BTreeMap<String, u32>);

impl Rounds {
    /// Look at `round` of `node`, within `1..=reached`.
    #[must_use]
    pub fn with(mut self, node: &str, round: u32, reached: u32) -> Self {
        self.0
            .insert(node.to_owned(), round.clamp(1, reached.max(1)));
        self
    }

    /// The `rounds` query value, if any loop is named.
    #[must_use]
    pub fn query(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| {
            self.0
                .iter()
                .map(|(node, round)| format!("{node}:{round}"))
                .collect::<Vec<_>>()
                .join(",")
        })
    }
}

/// One way to answer a gate, as a button offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAction {
    /// The button's text.
    pub label: &'static str,
    /// What it sends, before any note is added.
    pub answer: GateAnswer,
    /// Whether it asks for a note first.
    pub takes_note: bool,
}

/// The answers a waiting gate takes; none for an agent's question.
#[must_use]
pub fn gate_actions(needs: &NeedsYou) -> Vec<GateAction> {
    let recheck = |label, takes_note| GateAction {
        label,
        answer: GateAnswer::Recheck { note: None },
        takes_note,
    };
    match needs {
        NeedsYou::Gate { gate, .. } if gate == "info-needed" => {
            vec![
                recheck("Recheck", false),
                recheck("Recheck with a note", true),
            ]
        }
        NeedsYou::Gate { gate, .. } if gate == "review-stuck" => vec![GateAction {
            label: "Open the PR anyway",
            answer: GateAnswer::OpenAnyway,
            takes_note: false,
        }],
        NeedsYou::Gate { .. } | NeedsYou::Question { .. } => Vec::new(),
    }
}

/// An SVG path through `points`.
#[must_use]
pub fn path(points: &[Point]) -> String {
    points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let verb = if index == 0 { 'M' } else { 'L' };
            format!("{verb}{} {}", point.x, point.y)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where an edge's label goes: the middle of its middle segment.
#[must_use]
pub fn label_at(points: &[Point]) -> Option<Point> {
    let segments = points.len().checked_sub(1).filter(|count| *count > 0)?;
    let start = points[(segments - 1) / 2];
    let end = points[(segments - 1) / 2 + 1];
    Some(Point {
        x: start.x.midpoint(end.x),
        y: start.y.midpoint(end.y),
    })
}

/// The CSS class a mark is drawn with.
#[must_use]
pub const fn mark_class(mark: Mark) -> &'static str {
    match mark {
        Mark::Done => "mark-done",
        Mark::RunningHere => "mark-running",
        Mark::Elsewhere => "mark-elsewhere",
        Mark::NeedsYou => "mark-needs-you",
        Mark::Failed => "mark-failed",
        Mark::Next => "mark-next",
        Mark::NotReached => "mark-not-reached",
    }
}

/// An API URL carrying the token and any further query.
#[must_use]
pub fn api_url(path: &str, token: &str, query: &[(&str, String)]) -> String {
    std::iter::once(format!("{path}?token={}", escape(token)))
        .chain(
            query
                .iter()
                .map(|(key, value)| format!("{key}={}", escape(value))),
        )
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything but unreserved characters.
fn escape(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The token in a page URL's query.
#[must_use]
pub fn token_in(search: &str) -> Option<String> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(run: &str, status: RunStatus, waiting: bool) -> RunEntry {
        RunEntry {
            run: run.to_owned(),
            ticket: None,
            title: None,
            status,
            phases: Vec::new(),
            needs_you: waiting.then(|| NeedsYou::Gate {
                instance: "ask-info@1".to_owned(),
                gate: "info-needed".to_owned(),
            }),
            started_at: None,
        }
    }

    fn runs(list: &RunList) -> Vec<&str> {
        list.entries()
            .iter()
            .map(|entry| entry.run.as_str())
            .collect()
    }

    #[test]
    fn a_snapshot_replaces_the_list_and_changes_update_it_in_place() {
        let mut list = RunList::default();
        list.apply(ListEvent::Snapshot {
            runs: vec![
                entry("a", RunStatus::Active, false),
                entry("b", RunStatus::Active, false),
            ],
        });
        list.apply(ListEvent::Run {
            entry: entry("a", RunStatus::Completed, false),
        });
        list.apply(ListEvent::Run {
            entry: entry("c", RunStatus::Active, false),
        });
        list.apply(ListEvent::Removed {
            run: "b".to_owned(),
        });

        assert_eq!(runs(&list), ["c", "a"]);
        assert_eq!(list.entries()[1].status, RunStatus::Completed);

        list.apply(ListEvent::Snapshot { runs: Vec::new() });
        assert!(list.entries().is_empty());
    }

    #[test]
    fn runs_waiting_on_you_come_first_then_the_running_then_the_finished() {
        let mut list = RunList::default();
        list.apply(ListEvent::Snapshot {
            runs: vec![
                entry("done", RunStatus::Completed, false),
                entry("going", RunStatus::Active, false),
                entry("waiting", RunStatus::Active, true),
            ],
        });

        assert_eq!(runs(&list), ["waiting", "going", "done"]);
    }

    #[test]
    fn the_list_says_when_the_daemon_needs_signing_in() {
        let mut list = RunList::default();
        list.apply(ListEvent::SignIn { needed: true });
        assert!(list.sign_in_needed());
        list.apply(ListEvent::SignIn { needed: false });
        assert!(!list.sign_in_needed());
    }

    #[test]
    fn rounds_are_kept_within_what_the_loop_reached_and_written_as_a_query() {
        assert_eq!(Rounds::default().query(), None);
        let rounds = Rounds::default()
            .with("rework", 1, 2)
            .with("intake", 9, 2)
            .with("rework", 0, 2);

        assert_eq!(rounds.query().as_deref(), Some("intake:2,rework:1"));
    }

    #[test]
    fn each_gate_offers_its_own_answers_and_a_question_none() {
        let info = gate_actions(&NeedsYou::Gate {
            instance: "ask-info@1".to_owned(),
            gate: "info-needed".to_owned(),
        });
        assert_eq!(
            info.iter()
                .map(|action| (action.label, action.takes_note))
                .collect::<Vec<_>>(),
            [("Recheck", false), ("Recheck with a note", true)]
        );
        assert!(
            info.iter()
                .all(|action| matches!(action.answer, GateAnswer::Recheck { note: None }))
        );

        let stuck = gate_actions(&NeedsYou::Gate {
            instance: "review-stuck".to_owned(),
            gate: "review-stuck".to_owned(),
        });
        assert_eq!(
            stuck,
            [GateAction {
                label: "Open the PR anyway",
                answer: GateAnswer::OpenAnyway,
                takes_note: false,
            }]
        );

        assert!(
            gate_actions(&NeedsYou::Question {
                instance: "implement@1".to_owned(),
                question: "Which crate?".to_owned(),
            })
            .is_empty()
        );
    }

    #[test]
    fn a_path_moves_to_the_first_point_and_draws_lines_through_the_rest() {
        let points = [
            Point { x: 150, y: 20 },
            Point { x: 180, y: 20 },
            Point { x: 180, y: 84 },
            Point { x: 210, y: 84 },
        ];

        assert_eq!(path(&points), "M150 20 L180 20 L180 84 L210 84");
        assert_eq!(path(&[]), "");
    }

    #[test]
    fn a_label_sits_on_the_middle_of_the_middle_segment() {
        let bent = [
            Point { x: 150, y: 20 },
            Point { x: 180, y: 20 },
            Point { x: 180, y: 84 },
            Point { x: 210, y: 84 },
        ];
        assert_eq!(label_at(&bent), Some(Point { x: 180, y: 52 }));
        assert_eq!(
            label_at(&[Point { x: 0, y: 10 }, Point { x: 60, y: 10 }]),
            Some(Point { x: 30, y: 10 })
        );
        assert_eq!(label_at(&[Point { x: 0, y: 10 }]), None);
    }

    #[test]
    fn every_mark_has_its_own_class() {
        let classes: std::collections::BTreeSet<&str> = [
            Mark::Done,
            Mark::RunningHere,
            Mark::Elsewhere,
            Mark::NeedsYou,
            Mark::Failed,
            Mark::Next,
            Mark::NotReached,
        ]
        .into_iter()
        .map(mark_class)
        .collect();

        assert_eq!(classes.len(), 7);
        assert!(classes.iter().all(|class| class.starts_with("mark-")));
    }

    #[test]
    fn an_api_url_carries_the_token_and_its_query_escaped() {
        assert_eq!(api_url("/api/runs", "abc", &[]), "/api/runs?token=abc");
        assert_eq!(
            api_url(
                "/api/runs/r1",
                "abc",
                &[("rounds", "rework:1,intake:2".to_owned())]
            ),
            "/api/runs/r1?token=abc&rounds=rework%3A1%2Cintake%3A2"
        );
    }

    #[test]
    fn the_token_is_read_from_the_pages_query() {
        assert_eq!(token_in("?token=abc").as_deref(), Some("abc"));
        assert_eq!(token_in("?x=1&token=abc").as_deref(), Some("abc"));
        assert_eq!(token_in(""), None);
        assert_eq!(token_in("?token="), None);
    }
}
