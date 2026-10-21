//! The tmux workspace target.
//!
//! Vetinari keeps its own tmux session so that its windows never interleave
//! with the developer's own, as the PRD requires. A space is a window in that
//! session, identified by tmux's own window id rather than its index, because
//! indexes shift when a window is closed.

use std::path::Path;

use async_trait::async_trait;
use tokio::process::Command;
use vetinari_core::application::ports::{PortError, PortResult, WorkspaceTarget};
use vetinari_core::domain::ids::SpaceId;
use vetinari_core::domain::workspace::{Space, SpaceName, TargetKind};

/// Drives a dedicated tmux session.
#[derive(Debug, Clone)]
pub struct TmuxTarget {
    session: String,
    binary: String,
}

impl TmuxTarget {
    /// Target a session by name, creating it on first use.
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            binary: "tmux".to_owned(),
        }
    }

    /// Use a specific tmux binary.
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    /// The session Vetinari owns.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    async fn run(&self, args: &[String]) -> PortResult<String> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .await
            .map_err(|error| {
                PortError::unavailable("tmux", format!("could not run {}: {error}", self.binary))
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(PortError::Refused {
                what: format!("tmux {}", args.first().cloned().unwrap_or_default()),
                detail: if detail.is_empty() {
                    format!("tmux exited with status {}", output.status)
                } else {
                    detail
                },
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn session_exists(&self) -> bool {
        self.run(&strings(&[
            "has-session",
            "-t",
            &format!("{}:", self.session),
        ]))
        .await
        .is_ok()
    }
}

#[async_trait]
impl WorkspaceTarget for TmuxTarget {
    fn kind(&self) -> TargetKind {
        TargetKind::Tmux
    }

    async fn create_space(&self, name: &SpaceName, path: &Path) -> PortResult<SpaceId> {
        let args = if self.session_exists().await {
            new_window_args(&self.session, name, path)
        } else {
            new_session_args(&self.session, name, path)
        };
        let id = self.run(&args).await?;
        SpaceId::parse(&id)
            .map_err(|_| PortError::protocol("tmux", "the new window reported no id".to_owned()))
    }

    async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        self.run(&select_window_args(id)).await?;
        // Only works when a client is attached; a detached session is fine.
        let _ = self.run(&switch_client_args(&self.session)).await;
        Ok(())
    }

    async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        self.run(&kill_window_args(id)).await?;
        Ok(())
    }

    async fn list_spaces(&self) -> PortResult<Vec<Space>> {
        let output = self.run(&list_windows_args(&self.session)).await?;
        Ok(parse_windows(&output))
    }

    async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()> {
        self.run(&send_keys_args(id, command)).await?;
        Ok(())
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn new_session_args(session: &str, name: &SpaceName, path: &Path) -> Vec<String> {
    strings(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-n",
        name.as_str(),
        "-c",
        &path.display().to_string(),
        "-P",
        "-F",
        "#{window_id}",
    ])
}

fn new_window_args(session: &str, name: &SpaceName, path: &Path) -> Vec<String> {
    strings(&[
        "new-window",
        "-t",
        &format!("{session}:"),
        "-n",
        name.as_str(),
        "-c",
        &path.display().to_string(),
        "-P",
        "-F",
        "#{window_id}",
    ])
}

fn select_window_args(id: &SpaceId) -> Vec<String> {
    strings(&["select-window", "-t", id.as_str()])
}

fn switch_client_args(session: &str) -> Vec<String> {
    strings(&["switch-client", "-t", &format!("{session}:")])
}

fn kill_window_args(id: &SpaceId) -> Vec<String> {
    strings(&["kill-window", "-t", id.as_str()])
}

fn list_windows_args(session: &str) -> Vec<String> {
    strings(&[
        "list-windows",
        "-t",
        &format!("{session}:"),
        "-F",
        "#{window_id}\t#{window_name}\t#{pane_current_path}",
    ])
}

fn send_keys_args(id: &SpaceId, command: &[String]) -> Vec<String> {
    vec![
        "send-keys".to_owned(),
        "-t".to_owned(),
        id.as_str().to_owned(),
        shell_join(command),
        "Enter".to_owned(),
    ]
}

/// Quote a command for a shell, since `send-keys` types text rather than
/// execing an argument vector.
fn shell_join(command: &[String]) -> String {
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

fn parse_windows(output: &str) -> Vec<Space> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = SpaceId::parse(fields.next()?).ok()?;
            let name = SpaceName::sanitized(fields.next()?);
            let path = fields.next()?;
            Some(Space::new(
                id,
                name,
                Path::new(path).to_path_buf(),
                TargetKind::Tmux,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> TmuxTarget {
        TmuxTarget::new("vetinari")
    }

    #[test]
    fn the_first_space_creates_the_session_vetinari_owns() {
        assert_eq!(
            new_session_args(
                "vetinari",
                &SpaceName::sanitized("pr-142"),
                Path::new("/trees/pr-142")
            ),
            vec![
                "new-session",
                "-d",
                "-s",
                "vetinari",
                "-n",
                "pr-142",
                "-c",
                "/trees/pr-142",
                "-P",
                "-F",
                "#{window_id}",
            ]
        );
    }

    #[test]
    fn later_spaces_become_windows_in_that_session() {
        assert_eq!(
            new_window_args(
                "vetinari",
                &SpaceName::sanitized("pr-7"),
                Path::new("/trees/pr-7")
            ),
            vec![
                "new-window",
                "-t",
                "vetinari:",
                "-n",
                "pr-7",
                "-c",
                "/trees/pr-7",
                "-P",
                "-F",
                "#{window_id}",
            ]
        );
    }

    #[test]
    fn a_window_is_focused_by_id_and_the_client_follows_it() {
        let id = SpaceId::parse("@4").expect("id");
        assert_eq!(select_window_args(&id), vec!["select-window", "-t", "@4"]);
        assert_eq!(
            switch_client_args("vetinari"),
            vec!["switch-client", "-t", "vetinari:"]
        );
    }

    #[test]
    fn closing_a_space_kills_its_window() {
        let id = SpaceId::parse("@4").expect("id");
        assert_eq!(kill_window_args(&id), vec!["kill-window", "-t", "@4"]);
    }

    #[test]
    fn listing_windows_asks_for_the_fields_a_space_needs() {
        assert_eq!(
            list_windows_args("vetinari"),
            vec![
                "list-windows",
                "-t",
                "vetinari:",
                "-F",
                "#{window_id}\t#{window_name}\t#{pane_current_path}",
            ]
        );
    }

    #[test]
    fn window_listings_are_parsed_into_spaces() {
        let output = "@1\tpr-142\t/trees/pr-142\n@2\tfeedback-x\t/trees/feedback-x\n";
        let spaces = parse_windows(output);

        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].id().as_str(), "@1");
        assert_eq!(spaces[0].name().as_str(), "pr-142");
        assert_eq!(spaces[0].path(), Path::new("/trees/pr-142"));
        assert_eq!(spaces[0].target(), TargetKind::Tmux);
    }

    #[test]
    fn a_malformed_listing_line_is_skipped_rather_than_failing_the_lot() {
        let output = "@1\tpr-142\t/trees/pr-142\nrubbish\n\n@2\tok\t/tmp\n";
        assert_eq!(parse_windows(output).len(), 2);
    }

    #[test]
    fn a_command_sent_to_a_window_is_quoted_for_the_shell() {
        let id = SpaceId::parse("@4").expect("id");
        let command = vec![
            "claude".to_owned(),
            "--print".to_owned(),
            "fix the 'memory' leak".to_owned(),
        ];
        assert_eq!(
            send_keys_args(&id, &command),
            vec![
                "send-keys".to_owned(),
                "-t".to_owned(),
                "@4".to_owned(),
                "claude --print 'fix the '\\''memory'\\'' leak'".to_owned(),
                "Enter".to_owned(),
            ]
        );
    }

    #[test]
    fn an_argument_that_needs_no_quoting_is_left_alone() {
        assert_eq!(
            shell_join(&["echo".to_owned(), "hello-world".to_owned()]),
            "echo hello-world"
        );
    }

    #[test]
    fn an_empty_argument_is_still_passed_through() {
        assert_eq!(shell_join(&["echo".to_owned(), String::new()]), "echo ''");
    }

    #[test]
    fn the_target_reports_which_kind_it_is() {
        assert_eq!(target().kind(), TargetKind::Tmux);
        assert_eq!(target().session(), "vetinari");
    }

    #[tokio::test]
    async fn without_tmux_installed_every_call_says_so_rather_than_hanging() {
        let target = TmuxTarget::new("vetinari").with_binary("tmux-that-does-not-exist");

        let error = target
            .create_space(&SpaceName::sanitized("x"), Path::new("/tmp"))
            .await
            .expect_err("no tmux");

        assert!(error.is_transient(), "got {error:?}");
        assert!(target.list_spaces().await.is_err());
    }
}
