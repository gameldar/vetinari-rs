//! The modal state machine that turns keypresses into actions.
//!
//! It owns the pending-prefix state that makes `SPC f f` feel like Doom: each
//! press either completes a binding, extends the prefix, or fails loudly
//! enough for the UI to say so.

use crate::domain::action::Action;
use crate::domain::keymap::{Candidate, Keymap, Lookup, Mode};
use crate::domain::keys::{KeyChord, KeyCode, KeySequence};

/// What the engine made of a keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keystroke {
    /// A binding completed; run this action.
    Dispatch(Action),
    /// The sequence so far is a valid prefix.
    Pending {
        /// The chords pressed so far.
        sequence: KeySequence,
        /// The keys that may come next.
        candidates: Vec<Candidate>,
    },
    /// The sequence leads nowhere; the UI should say so and move on.
    Unbound(KeySequence),
    /// In insert mode: a character to add to the buffer.
    Text(char),
    /// A pending sequence was abandoned.
    Cancelled,
    /// Nothing to do.
    Ignored,
}

/// Interprets keypresses against a [`Keymap`], one mode at a time.
#[derive(Debug, Clone)]
pub struct KeymapEngine {
    keymap: Keymap,
    mode: Mode,
    pending: Vec<KeyChord>,
}

impl KeymapEngine {
    /// Build an engine in normal mode.
    #[must_use]
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            mode: Mode::Normal,
            pending: Vec::new(),
        }
    }

    /// The keymap in force.
    #[must_use]
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Replace the keymap, e.g. after a config reload, discarding any prefix.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.pending.clear();
    }

    /// The current mode.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Switch mode, discarding any pending prefix.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.pending.clear();
    }

    /// The chords pressed so far in an incomplete sequence.
    #[must_use]
    pub fn pending(&self) -> &[KeyChord] {
        &self.pending
    }

    /// The pending prefix, spelled for the status line.
    #[must_use]
    pub fn pending_display(&self) -> Option<String> {
        KeySequence::new(self.pending.clone())
            .ok()
            .map(|sequence| sequence.to_string())
    }

    /// Feed one keypress through the machine.
    pub fn press(&mut self, chord: KeyChord) -> Keystroke {
        if chord.code() == KeyCode::Esc && !self.pending.is_empty() {
            self.pending.clear();
            return Keystroke::Cancelled;
        }

        let mut candidate_sequence = self.pending.clone();
        candidate_sequence.push(chord);

        match self.keymap.lookup(self.mode, &candidate_sequence) {
            Lookup::Action(action) => {
                self.pending.clear();
                Keystroke::Dispatch(action)
            }
            Lookup::Prefix(candidates) => {
                self.pending = candidate_sequence.clone();
                let sequence =
                    KeySequence::new(candidate_sequence).expect("a prefix holds at least one key");
                Keystroke::Pending {
                    sequence,
                    candidates,
                }
            }
            Lookup::Unbound => {
                self.pending.clear();
                if self.mode == Mode::Insert {
                    return match chord.as_insertable_char() {
                        Some(ch) => Keystroke::Text(ch),
                        None => Keystroke::Ignored,
                    };
                }
                let sequence = KeySequence::new(candidate_sequence)
                    .expect("a keypress holds at least one key");
                Keystroke::Unbound(sequence)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(raw: &str) -> KeyChord {
        KeyChord::parse(raw).expect("chord")
    }

    fn press(engine: &mut KeymapEngine, spelling: &str) -> Keystroke {
        let mut last = Keystroke::Ignored;
        for part in spelling.split_whitespace() {
            last = engine.press(chord(part));
        }
        last
    }

    fn engine() -> KeymapEngine {
        KeymapEngine::new(Keymap::doom_defaults())
    }

    #[test]
    fn starts_in_normal_mode_with_nothing_pending() {
        let engine = engine();
        assert_eq!(engine.mode(), Mode::Normal);
        assert!(engine.pending().is_empty());
    }

    #[test]
    fn a_single_bound_key_dispatches_immediately() {
        let mut engine = engine();
        assert_eq!(
            press(&mut engine, "j"),
            Keystroke::Dispatch(Action::NavDown)
        );
        assert!(engine.pending().is_empty());
    }

    #[test]
    fn a_leader_sequence_reports_progress_then_dispatches() {
        let mut engine = engine();
        let Keystroke::Pending {
            sequence,
            candidates,
        } = press(&mut engine, "SPC")
        else {
            panic!("expected pending after the leader key");
        };
        assert_eq!(sequence.to_string(), "SPC");
        assert!(candidates.iter().any(|c| c.label == "+feedback"));

        let Keystroke::Pending { sequence, .. } = press(&mut engine, "f") else {
            panic!("expected pending after SPC f");
        };
        assert_eq!(sequence.to_string(), "SPC f");
        assert_eq!(engine.pending().len(), 2);

        assert_eq!(
            press(&mut engine, "f"),
            Keystroke::Dispatch(Action::FeedbackOpen)
        );
        assert!(engine.pending().is_empty());
    }

    #[test]
    fn an_unbound_continuation_reports_the_whole_sequence_and_resets() {
        let mut engine = engine();
        let outcome = press(&mut engine, "SPC z");
        match outcome {
            Keystroke::Unbound(sequence) => assert_eq!(sequence.to_string(), "SPC z"),
            other => panic!("expected unbound, got {other:?}"),
        }
        assert!(engine.pending().is_empty());
    }

    #[test]
    fn escape_cancels_a_pending_sequence_instead_of_running_its_binding() {
        let mut engine = engine();
        let _ = press(&mut engine, "SPC");
        assert_eq!(press(&mut engine, "<Esc>"), Keystroke::Cancelled);
        assert!(engine.pending().is_empty());
    }

    #[test]
    fn escape_with_nothing_pending_runs_its_normal_mode_binding() {
        let mut engine = engine();
        assert_eq!(
            press(&mut engine, "<Esc>"),
            Keystroke::Dispatch(Action::FilterClear)
        );
    }

    #[test]
    fn insert_mode_turns_printable_keys_into_text() {
        let mut engine = engine();
        engine.set_mode(Mode::Insert);
        assert_eq!(press(&mut engine, "j"), Keystroke::Text('j'));
        assert_eq!(press(&mut engine, "SPC"), Keystroke::Text(' '));
        assert_eq!(
            press(&mut engine, "<Esc>"),
            Keystroke::Dispatch(Action::CancelPending)
        );
    }

    #[test]
    fn insert_mode_ignores_unbound_control_keys() {
        let mut engine = engine();
        engine.set_mode(Mode::Insert);
        assert_eq!(press(&mut engine, "<F5>"), Keystroke::Ignored);
    }

    #[test]
    fn changing_mode_discards_a_pending_sequence() {
        let mut engine = engine();
        let _ = press(&mut engine, "SPC");
        engine.set_mode(Mode::Insert);
        assert!(engine.pending().is_empty());
        engine.set_mode(Mode::Normal);
        assert_eq!(
            press(&mut engine, "j"),
            Keystroke::Dispatch(Action::NavDown)
        );
    }

    #[test]
    fn a_replaced_keymap_takes_effect_immediately() {
        let mut engine = engine();
        let mut custom = Keymap::empty();
        custom
            .bind(
                Mode::Normal,
                KeySequence::parse("x").expect("sequence"),
                Action::Quit,
            )
            .expect("bind");
        engine.set_keymap(custom);
        assert_eq!(press(&mut engine, "x"), Keystroke::Dispatch(Action::Quit));
        assert!(matches!(press(&mut engine, "j"), Keystroke::Unbound(_)));
    }

    #[test]
    fn the_pending_sequence_can_be_rendered_for_the_status_line() {
        let mut engine = engine();
        let _ = press(&mut engine, "SPC a");
        assert_eq!(engine.pending_display(), Some("SPC a".to_owned()));
    }
}
