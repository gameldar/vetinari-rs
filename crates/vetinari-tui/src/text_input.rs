//! A small text buffer for the overlays.
//!
//! Single-line prompts submit on Enter; the feedback overlay is multi-line, so
//! Enter adds a line there and submitting is an explicit chord. Nothing here
//! knows what the text is for.

use vetinari_core::domain::keys::{KeyChord, KeyCode};

/// What a keypress did to the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// The buffer changed.
    Changed,
    /// The text is finished.
    Submitted,
    /// The developer gave up.
    Cancelled,
    /// The key meant nothing here.
    Ignored,
}

/// An editable string with a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    cursor: usize,
    multiline: bool,
}

impl TextInput {
    /// An empty single-line buffer.
    #[must_use]
    pub fn single_line() -> Self {
        Self::default()
    }

    /// An empty multi-line buffer.
    #[must_use]
    pub fn multiline() -> Self {
        Self {
            multiline: true,
            ..Self::default()
        }
    }

    /// A buffer holding existing text, with the cursor at the end.
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self.cursor = self.value.chars().count();
        self
    }

    /// The text.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The cursor position, counted in characters.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether Enter adds a line rather than submitting.
    #[must_use]
    pub const fn is_multiline(&self) -> bool {
        self.multiline
    }

    /// Whether there is nothing to submit.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    /// Feed a keypress to the buffer.
    pub fn press(&mut self, chord: KeyChord) -> InputOutcome {
        let modifiers = chord.modifiers();
        match chord.code() {
            KeyCode::Esc => InputOutcome::Cancelled,
            KeyCode::Enter if modifiers.ctrl || !self.multiline => InputOutcome::Submitted,
            KeyCode::Char('s') if modifiers.ctrl => InputOutcome::Submitted,
            KeyCode::Enter => {
                self.insert('\n');
                InputOutcome::Changed
            }
            KeyCode::Char('u') if modifiers.ctrl => {
                self.value.clear();
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::Char('w') if modifiers.ctrl => {
                self.delete_word();
                InputOutcome::Changed
            }
            KeyCode::Char('a') if modifiers.ctrl => {
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::Char('e') if modifiers.ctrl => {
                self.cursor = self.value.chars().count();
                InputOutcome::Changed
            }
            KeyCode::Char(ch) if !modifiers.ctrl && !modifiers.alt => {
                self.insert(ch);
                InputOutcome::Changed
            }
            KeyCode::Backspace => {
                self.backspace();
                InputOutcome::Changed
            }
            KeyCode::Delete => {
                self.delete();
                InputOutcome::Changed
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                InputOutcome::Changed
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                InputOutcome::Changed
            }
            KeyCode::Home => {
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                InputOutcome::Changed
            }
            _ => InputOutcome::Ignored,
        }
    }

    fn byte_at(&self, cursor: usize) -> usize {
        self.value
            .char_indices()
            .nth(cursor)
            .map_or(self.value.len(), |(index, _)| index)
    }

    fn insert(&mut self, ch: char) {
        let at = self.byte_at(self.cursor);
        self.value.insert(at, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_at(self.cursor - 1);
        self.value.remove(at);
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let at = self.byte_at(self.cursor);
        self.value.remove(at);
    }

    fn delete_word(&mut self) {
        let mut remaining = self.cursor;
        let chars: Vec<char> = self.value.chars().collect();
        while remaining > 0 && chars[remaining - 1].is_whitespace() {
            remaining -= 1;
        }
        while remaining > 0 && !chars[remaining - 1].is_whitespace() {
            remaining -= 1;
        }
        let kept: String = chars[..remaining]
            .iter()
            .chain(chars[self.cursor..].iter())
            .collect();
        self.value = kept;
        self.cursor = remaining;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(spelling: &str) -> KeyChord {
        KeyChord::parse(spelling).expect("chord")
    }

    fn type_text(input: &mut TextInput, text: &str) {
        for ch in text.chars() {
            input.press(KeyChord::plain(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn typing_builds_up_the_value() {
        let mut input = TextInput::single_line();
        type_text(&mut input, "fix the leak");
        assert_eq!(input.value(), "fix the leak");
        assert_eq!(input.cursor(), 12);
    }

    #[test]
    fn a_single_line_buffer_submits_on_enter() {
        let mut input = TextInput::single_line();
        type_text(&mut input, "main");
        assert_eq!(input.press(chord("RET")), InputOutcome::Submitted);
    }

    #[test]
    fn a_multiline_buffer_adds_a_line_on_enter_and_submits_on_control_enter() {
        let mut input = TextInput::multiline();
        type_text(&mut input, "one");
        assert_eq!(input.press(chord("RET")), InputOutcome::Changed);
        type_text(&mut input, "two");
        assert_eq!(input.value(), "one\ntwo");
        assert_eq!(input.press(chord("C-RET")), InputOutcome::Submitted);
    }

    #[test]
    fn control_s_submits_from_either_kind_of_buffer() {
        let mut input = TextInput::multiline();
        type_text(&mut input, "text");
        assert_eq!(input.press(chord("C-s")), InputOutcome::Submitted);
    }

    #[test]
    fn escape_cancels() {
        let mut input = TextInput::single_line();
        assert_eq!(input.press(chord("<Esc>")), InputOutcome::Cancelled);
    }

    #[test]
    fn backspace_and_delete_work_around_the_cursor() {
        let mut input = TextInput::single_line().with_value("abcd");
        input.press(chord("<Left>"));
        input.press(chord("<Backspace>"));
        assert_eq!(input.value(), "abd");
        input.press(chord("<Delete>"));
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut input = TextInput::single_line().with_value("ab");
        input.press(chord("<Home>"));
        input.press(chord("<Backspace>"));
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn delete_at_the_end_does_nothing() {
        let mut input = TextInput::single_line().with_value("ab");
        input.press(chord("<Delete>"));
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut input = TextInput::single_line().with_value("ab");
        for _ in 0..5 {
            input.press(chord("<Left>"));
        }
        assert_eq!(input.cursor(), 0);
        for _ in 0..5 {
            input.press(chord("<Right>"));
        }
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn control_u_clears_the_line() {
        let mut input = TextInput::single_line().with_value("a mistake");
        input.press(chord("C-u"));
        assert_eq!(input.value(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn control_w_deletes_the_word_before_the_cursor() {
        let mut input = TextInput::single_line().with_value("fix the leak");
        input.press(chord("C-w"));
        assert_eq!(input.value(), "fix the ");
        input.press(chord("C-w"));
        assert_eq!(input.value(), "fix ");
    }

    #[test]
    fn control_a_and_control_e_jump_to_the_ends() {
        let mut input = TextInput::single_line().with_value("abc");
        input.press(chord("C-a"));
        assert_eq!(input.cursor(), 0);
        input.press(chord("C-e"));
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn text_can_be_inserted_in_the_middle() {
        let mut input = TextInput::single_line().with_value("ac");
        input.press(chord("<Left>"));
        type_text(&mut input, "b");
        assert_eq!(input.value(), "abc");
    }

    #[test]
    fn multibyte_characters_are_handled_by_character_not_by_byte() {
        let mut input = TextInput::single_line();
        type_text(&mut input, "héllo");
        assert_eq!(input.cursor(), 5);
        input.press(chord("<Backspace>"));
        input.press(chord("<Backspace>"));
        input.press(chord("<Backspace>"));
        input.press(chord("<Backspace>"));
        assert_eq!(input.value(), "h");
    }

    #[test]
    fn a_blank_buffer_is_recognised_so_nothing_empty_is_submitted() {
        let mut input = TextInput::single_line();
        assert!(input.is_blank());
        type_text(&mut input, "   ");
        assert!(input.is_blank());
        type_text(&mut input, "x");
        assert!(!input.is_blank());
    }

    #[test]
    fn an_unhandled_key_is_ignored_rather_than_inserted() {
        let mut input = TextInput::single_line();
        assert_eq!(input.press(chord("<F5>")), InputOutcome::Ignored);
        assert_eq!(input.value(), "");
    }
}
