//! Translating crossterm key events into Vetinari's own chords.
//!
//! Terminals report the same keypress in several ways, so this is the one
//! place that has to know about them: a shifted letter arrives as the capital
//! itself, `Ctrl` arrives with the lowercase, and `BackTab` is how a terminal
//! spells shift-tab.

use crossterm::event::{KeyCode as TermCode, KeyEvent, KeyEventKind, KeyModifiers};
use vetinari_core::domain::keys::{KeyChord, KeyCode, Modifiers};

/// Convert a crossterm key event, if it is one Vetinari acts on.
///
/// Key *release* events are ignored: terminals that report them would
/// otherwise run every binding twice.
#[must_use]
pub fn chord_of(event: KeyEvent) -> Option<KeyChord> {
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let code = match event.code {
        TermCode::Char(ch) => KeyCode::Char(ch),
        TermCode::Enter => KeyCode::Enter,
        TermCode::Esc => KeyCode::Esc,
        TermCode::Tab => KeyCode::Tab,
        TermCode::BackTab => KeyCode::Tab,
        TermCode::Backspace => KeyCode::Backspace,
        TermCode::Delete => KeyCode::Delete,
        TermCode::Insert => KeyCode::Insert,
        TermCode::Left => KeyCode::Left,
        TermCode::Right => KeyCode::Right,
        TermCode::Up => KeyCode::Up,
        TermCode::Down => KeyCode::Down,
        TermCode::Home => KeyCode::Home,
        TermCode::End => KeyCode::End,
        TermCode::PageUp => KeyCode::PageUp,
        TermCode::PageDown => KeyCode::PageDown,
        TermCode::F(n) => KeyCode::Function(n),
        _ => return None,
    };

    let shift =
        event.modifiers.contains(KeyModifiers::SHIFT) || matches!(event.code, TermCode::BackTab);
    let modifiers = Modifiers::new(
        event.modifiers.contains(KeyModifiers::CONTROL),
        event.modifiers.contains(KeyModifiers::ALT),
        // A shifted character arrives as the character itself, so carrying
        // the modifier as well would stop `G` matching a binding on `G`.
        shift && !matches!(event.code, TermCode::Char(_)),
    );

    Some(KeyChord::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: TermCode, modifiers: KeyModifiers) -> Option<KeyChord> {
        chord_of(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn a_plain_letter_becomes_a_plain_chord() {
        assert_eq!(
            press(TermCode::Char('j'), KeyModifiers::NONE),
            Some(KeyChord::plain(KeyCode::Char('j')))
        );
    }

    #[test]
    fn a_capital_letter_carries_no_shift_modifier_so_it_matches_its_binding() {
        assert_eq!(
            press(TermCode::Char('G'), KeyModifiers::SHIFT),
            Some(KeyChord::plain(KeyCode::Char('G')))
        );
    }

    #[test]
    fn control_and_alt_are_carried_through() {
        let chord = press(TermCode::Char('d'), KeyModifiers::CONTROL).expect("chord");
        assert!(chord.modifiers().ctrl);
        assert_eq!(chord.to_string(), "C-d");

        let chord = press(TermCode::Char('x'), KeyModifiers::ALT).expect("chord");
        assert!(chord.modifiers().alt);
    }

    #[test]
    fn shift_tab_arrives_as_back_tab_and_becomes_a_shifted_tab() {
        let chord = press(TermCode::BackTab, KeyModifiers::NONE).expect("chord");
        assert_eq!(chord.code(), KeyCode::Tab);
        assert!(chord.modifiers().shift);
        assert_eq!(chord.to_string(), "S-TAB");
    }

    #[test]
    fn the_named_keys_are_all_mapped() {
        for (term, expected) in [
            (TermCode::Enter, KeyCode::Enter),
            (TermCode::Esc, KeyCode::Esc),
            (TermCode::Tab, KeyCode::Tab),
            (TermCode::Backspace, KeyCode::Backspace),
            (TermCode::Delete, KeyCode::Delete),
            (TermCode::Insert, KeyCode::Insert),
            (TermCode::Left, KeyCode::Left),
            (TermCode::Right, KeyCode::Right),
            (TermCode::Up, KeyCode::Up),
            (TermCode::Down, KeyCode::Down),
            (TermCode::Home, KeyCode::Home),
            (TermCode::End, KeyCode::End),
            (TermCode::PageUp, KeyCode::PageUp),
            (TermCode::PageDown, KeyCode::PageDown),
            (TermCode::F(5), KeyCode::Function(5)),
        ] {
            assert_eq!(
                press(term, KeyModifiers::NONE).map(|chord| chord.code()),
                Some(expected)
            );
        }
    }

    #[test]
    fn a_space_is_a_character_so_the_leader_key_works() {
        assert_eq!(
            press(TermCode::Char(' '), KeyModifiers::NONE),
            Some(KeyChord::plain(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn a_key_release_is_ignored_so_bindings_do_not_run_twice() {
        let event = KeyEvent::new(TermCode::Char('j'), KeyModifiers::NONE);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..event
        };
        assert_eq!(chord_of(release), None);
    }

    #[test]
    fn a_key_vetinari_has_no_name_for_is_ignored() {
        assert_eq!(press(TermCode::CapsLock, KeyModifiers::NONE), None);
    }
}
