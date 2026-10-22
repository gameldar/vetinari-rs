//! Workspace targets.

pub mod editor;
pub mod herdr;
pub mod tmux;

pub use editor::EditorTarget;
pub use herdr::HerdrSocketTarget;
pub use tmux::TmuxTarget;

/// Quote a command for a shell.
///
/// Both tmux's `send-keys` and Herdr's `pane.send_input` *type* a command at a
/// shell rather than execing an argument vector, so the vector has to be
/// rendered back into something a shell will split the same way.
pub(crate) fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| {
            if !argument.is_empty()
                && argument
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "-_./=:@,+".contains(ch))
            {
                argument.clone()
            } else {
                format!("'{}'", argument.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_command_is_left_alone_and_anything_else_is_quoted() {
        assert_eq!(
            shell_join(&["echo".to_owned(), "hello-world".to_owned()]),
            "echo hello-world"
        );
        assert_eq!(
            shell_join(&["echo".to_owned(), "two words".to_owned()]),
            "echo 'two words'"
        );
        assert_eq!(shell_join(&["echo".to_owned(), String::new()]), "echo ''");
    }

    #[test]
    fn a_single_quote_cannot_escape_the_quoting() {
        assert_eq!(
            shell_join(&["echo".to_owned(), "it's".to_owned()]),
            r"echo 'it'\''s'"
        );
    }
}
