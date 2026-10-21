//! The editor workspace target: Zed, or anything else that opens a directory.
//!
//! An editor has no notion of a space Vetinari can query, so the spaces it
//! reports are the ones Vetinari opened in it, remembered here. Closing a
//! space forgets it rather than pretending to close a window, and running a
//! command beside the work is refused outright rather than guessed at.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::Command;
use vetinari_core::application::ports::{PortError, PortResult, WorkspaceTarget};
use vetinari_core::domain::ids::SpaceId;
use vetinari_core::domain::workspace::{Space, SpaceName, TargetKind};

/// Opens worktrees in an external editor.
#[derive(Debug)]
pub struct EditorTarget {
    command: String,
    args: Vec<String>,
    opened: Mutex<Vec<Space>>,
}

impl EditorTarget {
    /// Target an editor by command line.
    #[must_use]
    pub fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
            opened: Mutex::new(Vec::new()),
        }
    }

    /// The command line used to open a path.
    #[must_use]
    pub fn open_command(&self, path: &Path) -> Vec<String> {
        let mut command = vec![self.command.clone()];
        command.extend(self.args.iter().cloned());
        command.push(path.display().to_string());
        command
    }

    async fn open(&self, path: &Path) -> PortResult<()> {
        let output = Command::new(&self.command)
            .args(&self.args)
            .arg(path)
            .output()
            .await
            .map_err(|error| {
                PortError::unavailable("editor", format!("could not run {}: {error}", self.command))
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(PortError::Refused {
                what: self.command.clone(),
                detail: if detail.is_empty() {
                    format!("the editor exited with status {}", output.status)
                } else {
                    detail
                },
            });
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceTarget for EditorTarget {
    fn kind(&self) -> TargetKind {
        TargetKind::Editor
    }

    async fn create_space(&self, name: &SpaceName, path: &Path) -> PortResult<SpaceId> {
        self.open(path).await?;
        // The path is the identity: opening the same worktree twice is the
        // same space, however it was named.
        let id = SpaceId::parse(&path.display().to_string()).map_err(|_| {
            PortError::not_configured("editor target", "a space needs a path to open")
        })?;
        let mut opened = self.opened.lock().expect("opened lock");
        if !opened.iter().any(|space| space.id() == &id) {
            opened.push(Space::new(
                id.clone(),
                name.clone(),
                path.to_path_buf(),
                TargetKind::Editor,
            ));
        }
        Ok(id)
    }

    async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        let path = {
            let opened = self.opened.lock().expect("opened lock");
            opened
                .iter()
                .find(|space| space.id() == id)
                .map(|space| space.path().to_path_buf())
                .ok_or_else(|| PortError::not_found("space", id.as_str()))?
        };
        self.open(&path).await
    }

    async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        let mut opened = self.opened.lock().expect("opened lock");
        let before = opened.len();
        opened.retain(|space| space.id() != id);
        if opened.len() == before {
            return Err(PortError::not_found("space", id.as_str()));
        }
        Ok(())
    }

    async fn list_spaces(&self) -> PortResult<Vec<Space>> {
        Ok(self.opened.lock().expect("opened lock").clone())
    }

    async fn run_in_space(&self, _id: &SpaceId, _command: &[String]) -> PortResult<()> {
        Err(PortError::unsupported(
            "editor target",
            "running a command beside the work; use the tmux or Herdr target for that",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_invoked_with_its_arguments_then_the_path() {
        let target = EditorTarget::new("zed", vec!["--new".to_owned()]);
        assert_eq!(
            target.open_command(Path::new("/trees/pr-142")),
            vec![
                "zed".to_owned(),
                "--new".to_owned(),
                "/trees/pr-142".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn opening_a_space_records_it_so_it_can_be_focused_again() {
        let target = EditorTarget::new("true", Vec::new());

        let id = target
            .create_space(&SpaceName::sanitized("pr-142"), Path::new("/trees/pr-142"))
            .await
            .expect("open");

        let spaces = target.list_spaces().await.expect("list");
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id(), &id);
        assert_eq!(spaces[0].name().as_str(), "pr-142");
        assert_eq!(spaces[0].target(), TargetKind::Editor);

        target.focus_space(&id).await.expect("focus");
    }

    #[tokio::test]
    async fn opening_the_same_path_twice_does_not_duplicate_the_space() {
        let target = EditorTarget::new("true", Vec::new());
        let first = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect("open");
        let second = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect("open");

        assert_eq!(first, second);
        assert_eq!(target.list_spaces().await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn an_editor_that_will_not_start_is_reported() {
        let target = EditorTarget::new("editor-that-does-not-exist", Vec::new());
        let error = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect_err("no editor");
        assert!(error.is_transient());
        assert!(target.list_spaces().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn an_editor_that_exits_badly_is_reported_as_a_refusal() {
        let target = EditorTarget::new("false", Vec::new());
        let error = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect_err("refused");
        assert!(matches!(error, PortError::Refused { .. }));
    }

    #[tokio::test]
    async fn focusing_a_space_the_editor_never_opened_is_an_error() {
        let target = EditorTarget::new("true", Vec::new());
        let error = target
            .focus_space(&SpaceId::parse("/nowhere").expect("id"))
            .await
            .expect_err("unknown space");
        assert!(matches!(error, PortError::NotFound { .. }));
    }

    #[tokio::test]
    async fn closing_a_space_forgets_it_without_pretending_to_close_a_window() {
        let target = EditorTarget::new("true", Vec::new());
        let id = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect("open");

        target.close_space(&id).await.expect("close");

        assert!(target.list_spaces().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn an_editor_cannot_run_a_command_beside_the_work_and_says_so() {
        let target = EditorTarget::new("true", Vec::new());
        let id = target
            .create_space(&SpaceName::sanitized("a"), Path::new("/trees/a"))
            .await
            .expect("open");

        let error = target
            .run_in_space(&id, &["claude".to_owned()])
            .await
            .expect_err("unsupported");

        assert!(matches!(error, PortError::Unsupported { .. }));
    }
}
