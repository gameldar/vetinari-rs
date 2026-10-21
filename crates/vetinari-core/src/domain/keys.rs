//! Key chords and key sequences, spelled the way Doom Emacs spells them.
//!
//! The terminal layer (crossterm) converts its own events into [`KeyChord`];
//! configuration files spell the same thing as text. Both end up here so the
//! keymap only ever deals with one representation.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A key, independent of any modifier held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyCode {
    /// A printable character, as the terminal delivers it.
    Char(char),
    /// Return.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Insert.
    Insert,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// A function key, `F1` upwards.
    Function(u8),
}

/// Modifier keys held alongside a [`KeyCode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Modifiers {
    /// Control.
    pub ctrl: bool,
    /// Alt / Meta.
    pub alt: bool,
    /// Shift, only meaningful for non-printable keys.
    pub shift: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };

    /// Build the set from its three flags.
    #[must_use]
    pub const fn new(ctrl: bool, alt: bool, shift: bool) -> Self {
        Self { ctrl, alt, shift }
    }
}

/// Why a chord or sequence could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyParseError {
    /// Nothing but whitespace was supplied.
    #[error("key sequence is empty")]
    Empty,
    /// A `<Name>` or bare-word key was not recognised.
    #[error("unknown key {spelling:?}")]
    UnknownKey {
        /// The unrecognised spelling.
        spelling: String,
    },
    /// A modifier prefix was given with no key after it.
    #[error("modifier {prefix:?} has no key after it")]
    DanglingModifier {
        /// The trailing modifier prefix.
        prefix: String,
    },
}

/// A single keypress: one [`KeyCode`] plus the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyChord {
    code: KeyCode,
    modifiers: Modifiers,
}

impl KeyChord {
    /// Build a chord from a code and its modifiers.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self { code, modifiers }
    }

    /// Build a chord with no modifiers held.
    #[must_use]
    pub const fn plain(code: KeyCode) -> Self {
        Self::new(code, Modifiers::NONE)
    }

    /// The key itself.
    #[must_use]
    pub const fn code(&self) -> KeyCode {
        self.code
    }

    /// The modifiers held with it.
    #[must_use]
    pub const fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// The character this chord would insert in insert mode, if any.
    #[must_use]
    pub const fn as_insertable_char(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(ch) if !self.modifiers.ctrl && !self.modifiers.alt => Some(ch),
            _ => None,
        }
    }

    /// Parse one chord, e.g. `j`, `SPC`, `C-c`, `<F5>`, `C-M-x`.
    pub fn parse(raw: &str) -> Result<Self, KeyParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(KeyParseError::Empty);
        }

        let mut modifiers = Modifiers::NONE;
        let mut rest = trimmed;
        // `C-c` splits as ("C", "c"); a bare `-` key splits as ("", "").
        while let Some((prefix, remainder)) = rest.split_once('-') {
            match prefix {
                "C" | "c" => modifiers.ctrl = true,
                "M" | "m" | "A" | "a" => modifiers.alt = true,
                "S" | "s" => modifiers.shift = true,
                _ => break,
            }
            if remainder.is_empty() {
                return Err(KeyParseError::DanglingModifier {
                    prefix: prefix.to_owned(),
                });
            }
            rest = remainder;
        }

        let code = parse_code(rest)?;
        Ok(Self { code, modifiers })
    }
}

fn parse_code(spelling: &str) -> Result<KeyCode, KeyParseError> {
    let bare = spelling
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(spelling);

    if let Some(digits) = bare
        .strip_prefix('F')
        .or_else(|| bare.strip_prefix('f'))
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
    {
        let n: u8 = digits.parse().map_err(|_| KeyParseError::UnknownKey {
            spelling: spelling.to_owned(),
        })?;
        return Ok(KeyCode::Function(n));
    }

    let mut chars = bare.chars();
    if let (Some(single), None) = (chars.next(), chars.next())
        && bare == spelling
    {
        return Ok(KeyCode::Char(single));
    }

    match bare.to_ascii_lowercase().as_str() {
        "spc" | "space" => Ok(KeyCode::Char(' ')),
        "ret" | "cr" | "enter" | "return" => Ok(KeyCode::Enter),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "bs" | "backspace" => Ok(KeyCode::Backspace),
        "del" | "delete" => Ok(KeyCode::Delete),
        "ins" | "insert" => Ok(KeyCode::Insert),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" | "pgup" => Ok(KeyCode::PageUp),
        "pagedown" | "pgdn" => Ok(KeyCode::PageDown),
        _ => Err(KeyParseError::UnknownKey {
            spelling: spelling.to_owned(),
        }),
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            f.write_str("C-")?;
        }
        if self.modifiers.alt {
            f.write_str("M-")?;
        }
        if self.modifiers.shift {
            f.write_str("S-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("SPC"),
            KeyCode::Char(ch) => write!(f, "{ch}"),
            KeyCode::Enter => f.write_str("RET"),
            KeyCode::Esc => f.write_str("<Esc>"),
            KeyCode::Tab => f.write_str("TAB"),
            KeyCode::Backspace => f.write_str("<Backspace>"),
            KeyCode::Delete => f.write_str("<Delete>"),
            KeyCode::Insert => f.write_str("<Insert>"),
            KeyCode::Left => f.write_str("<Left>"),
            KeyCode::Right => f.write_str("<Right>"),
            KeyCode::Up => f.write_str("<Up>"),
            KeyCode::Down => f.write_str("<Down>"),
            KeyCode::Home => f.write_str("<Home>"),
            KeyCode::End => f.write_str("<End>"),
            KeyCode::PageUp => f.write_str("<PageUp>"),
            KeyCode::PageDown => f.write_str("<PageDown>"),
            KeyCode::Function(n) => write!(f, "<F{n}>"),
        }
    }
}

/// A non-empty run of chords, such as `SPC p r`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeySequence(Vec<KeyChord>);

impl KeySequence {
    /// Build a sequence from chords, rejecting an empty run.
    pub fn new(chords: Vec<KeyChord>) -> Result<Self, KeyParseError> {
        if chords.is_empty() {
            return Err(KeyParseError::Empty);
        }
        Ok(Self(chords))
    }

    /// Build a single-chord sequence.
    #[must_use]
    pub fn single(chord: KeyChord) -> Self {
        Self(vec![chord])
    }

    /// Parse a whitespace-separated sequence, e.g. `SPC f f`.
    pub fn parse(raw: &str) -> Result<Self, KeyParseError> {
        let chords = raw
            .split_whitespace()
            .map(KeyChord::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(chords)
    }

    /// The chords, in order.
    #[must_use]
    pub fn chords(&self) -> &[KeyChord] {
        &self.0
    }

    /// How many chords are in the sequence; always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always false; a sequence cannot be empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `prefix` is a leading run of this sequence.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }

    /// Append a chord, yielding the longer sequence.
    #[must_use]
    pub fn push(mut self, chord: KeyChord) -> Self {
        self.0.push(chord);
        self
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, chord) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

impl Serialize for KeySequence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeySequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_characters() {
        assert_eq!(
            KeyChord::parse("j").expect("chord"),
            KeyChord::plain(KeyCode::Char('j'))
        );
        assert_eq!(
            KeyChord::parse("G").expect("chord"),
            KeyChord::plain(KeyCode::Char('G'))
        );
        assert_eq!(
            KeyChord::parse("/").expect("chord"),
            KeyChord::plain(KeyCode::Char('/'))
        );
    }

    #[test]
    fn parses_doom_style_named_keys() {
        assert_eq!(
            KeyChord::parse("SPC").expect("chord"),
            KeyChord::plain(KeyCode::Char(' '))
        );
        assert_eq!(
            KeyChord::parse("<Space>").expect("chord"),
            KeyChord::plain(KeyCode::Char(' '))
        );
        assert_eq!(
            KeyChord::parse("RET").expect("chord"),
            KeyChord::plain(KeyCode::Enter)
        );
        assert_eq!(
            KeyChord::parse("<Esc>").expect("chord"),
            KeyChord::plain(KeyCode::Esc)
        );
        assert_eq!(
            KeyChord::parse("<F5>").expect("chord"),
            KeyChord::plain(KeyCode::Function(5))
        );
    }

    #[test]
    fn parses_modifier_prefixes_in_any_order() {
        let ctrl_alt = KeyChord::parse("C-M-x").expect("chord");
        assert_eq!(ctrl_alt, KeyChord::parse("M-C-x").expect("chord"));
        assert!(ctrl_alt.modifiers().ctrl);
        assert!(ctrl_alt.modifiers().alt);
        assert!(!ctrl_alt.modifiers().shift);
        assert_eq!(ctrl_alt.code(), KeyCode::Char('x'));
    }

    #[test]
    fn parses_shifted_named_keys() {
        let back_tab = KeyChord::parse("S-TAB").expect("chord");
        assert_eq!(back_tab.code(), KeyCode::Tab);
        assert!(back_tab.modifiers().shift);
    }

    #[test]
    fn rejects_unknown_and_empty_chords() {
        assert!(matches!(KeyChord::parse(""), Err(KeyParseError::Empty)));
        assert!(matches!(
            KeyChord::parse("<Nope>"),
            Err(KeyParseError::UnknownKey { .. })
        ));
        assert!(matches!(
            KeyChord::parse("C-"),
            Err(KeyParseError::DanglingModifier { .. })
        ));
    }

    #[test]
    fn renders_chords_back_to_their_canonical_spelling() {
        for spelling in ["SPC", "C-c", "<F5>", "j", "RET", "C-M-x"] {
            let chord = KeyChord::parse(spelling).expect("chord");
            assert_eq!(chord.to_string(), spelling, "round trip of {spelling}");
        }
    }

    #[test]
    fn parses_whitespace_separated_sequences() {
        let seq = KeySequence::parse("SPC f f").expect("sequence");
        assert_eq!(seq.len(), 3);
        assert_eq!(seq.to_string(), "SPC f f");
        assert_eq!(seq.chords()[0], KeyChord::plain(KeyCode::Char(' ')));
    }

    #[test]
    fn sequences_are_never_empty() {
        assert!(matches!(
            KeySequence::parse("   "),
            Err(KeyParseError::Empty)
        ));
    }

    #[test]
    fn sequences_report_their_prefixes() {
        let seq = KeySequence::parse("SPC p r").expect("sequence");
        assert!(seq.starts_with(&KeySequence::parse("SPC p").expect("sequence")));
        assert!(!seq.starts_with(&KeySequence::parse("SPC f").expect("sequence")));
    }
}
