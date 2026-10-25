//! A run, as the daemon would describe it, for the tests that draw and
//! drive its popup.

use std::num::NonZeroU32;

use time::OffsetDateTime;
use time::macros::datetime;
use vetinari_core::application::devflow::api::{DetailRow, RunDetail, RunItem, RunLabel};
use vetinari_core::domain::devflow::ids::{NodeId, NodeInstance, RunId};
use vetinari_core::domain::devflow::lanes::{Lane, LaneRow, RowKind};
use vetinari_core::domain::devflow::notes::RunNotes;
use vetinari_core::domain::devflow::outcome::Line;
use vetinari_core::domain::devflow::run_view::RunStatus;
use vetinari_core::domain::devflow::step::GateKind;
use vetinari_core::domain::devflow::summary::{Mark, NeedsYou, Phase, PhaseCell, RunSummary};
use vetinari_core::domain::text::NonEmptyText;

const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

pub(crate) fn run() -> RunId {
    RunId::parse("01JB80").expect("run")
}

pub(crate) fn ask_info() -> NodeInstance {
    NodeInstance::parse("ask-info@1").expect("instance")
}

pub(crate) fn item() -> RunItem {
    RunItem {
        summary: RunSummary {
            run: run(),
            status: RunStatus::Active,
            phases: vec![PhaseCell {
                phase: Phase::Pre,
                mark: Mark::NeedsYou,
                round: None,
            }],
            needs_you: Some(NeedsYou::Gate {
                instance: ask_info(),
                gate: GateKind::InfoNeeded,
            }),
        },
        label: Some(RunLabel {
            ticket: NonEmptyText::parse("ENG-398").expect("key"),
            title: NonEmptyText::parse("Paginate audit export").expect("title"),
        }),
        started_at: Some(NOW),
    }
}

pub(crate) fn detail() -> RunDetail {
    let row = |indent, node: &str, kind, mark| DetailRow {
        row: LaneRow {
            indent,
            node: NodeId::parse(node).expect("node"),
            lane: Lane::Alone,
            kind,
            state: None,
        },
        mark,
    };
    RunDetail {
        item: item(),
        rows: vec![
            row(
                0,
                "intake",
                RowKind::Loop {
                    reached: 2,
                    selected: NonZeroU32::new(2).expect("two"),
                    bound: NonZeroU32::new(3).expect("three"),
                },
                Mark::NeedsYou,
            ),
            row(1, "ask-info", RowKind::Node, Mark::NeedsYou),
        ],
        notes: RunNotes {
            questions: vec![Line::parse("Page size?").expect("question")],
            findings: None,
        },
    }
}
