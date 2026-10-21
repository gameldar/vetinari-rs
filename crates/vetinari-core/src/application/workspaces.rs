//! The workspace directory: every configured target, addressed by kind.
//!
//! Vetinari can drive Herdr, tmux, and an editor at the same time, so spaces
//! are looked up across targets rather than against one chosen backend.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::ports::{PortError, PortResult, WorkspaceTarget};
use crate::domain::ids::SpaceId;
use crate::domain::workspace::{Space, SpaceName, TargetKind};

/// Every space across every target, plus the targets that could not answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpaceInventory {
    /// The spaces found.
    pub spaces: Vec<Space>,
    /// The targets that failed, and why.
    pub problems: Vec<(TargetKind, PortError)>,
}

/// The configured workspace targets.
#[derive(Default)]
pub struct WorkspaceDirectory {
    targets: BTreeMap<TargetKind, Arc<dyn WorkspaceTarget>>,
}

impl WorkspaceDirectory {
    /// Add a target, replacing any other of the same kind.
    pub fn register(&mut self, target: Arc<dyn WorkspaceTarget>) {
        self.targets.insert(target.kind(), target);
    }

    /// The kinds of target configured.
    #[must_use]
    pub fn kinds(&self) -> Vec<TargetKind> {
        self.targets.keys().copied().collect()
    }

    /// Whether a kind of target is configured.
    #[must_use]
    pub fn has(&self, kind: TargetKind) -> bool {
        self.targets.contains_key(&kind)
    }

    /// Look a target up.
    pub fn target(&self, kind: TargetKind) -> PortResult<&Arc<dyn WorkspaceTarget>> {
        self.targets.get(&kind).ok_or_else(|| {
            PortError::not_configured(
                kind.name(),
                "no workspace target of that kind is configured",
            )
        })
    }

    /// Create a space on a named target.
    pub async fn create_space(
        &self,
        kind: TargetKind,
        name: &SpaceName,
        path: &Path,
    ) -> PortResult<SpaceId> {
        self.target(kind)?.create_space(name, path).await
    }

    /// Every space, from every target.
    pub async fn inventory(&self) -> SpaceInventory {
        let mut inventory = SpaceInventory::default();
        for (kind, target) in &self.targets {
            match target.list_spaces().await {
                Ok(spaces) => inventory.spaces.extend(spaces),
                Err(error) => inventory.problems.push((*kind, error)),
            }
        }
        inventory
    }

    /// Bring a space to the foreground, whichever target owns it.
    pub async fn focus_space(&self, id: &SpaceId) -> PortResult<()> {
        self.owner_of(id).await?.focus_space(id).await
    }

    /// Close a space, whichever target owns it.
    pub async fn close_space(&self, id: &SpaceId) -> PortResult<()> {
        self.owner_of(id).await?.close_space(id).await
    }

    /// Run a command in a space, which is how an agent is placed beside the
    /// developer's own shell.
    pub async fn run_in_space(&self, id: &SpaceId, command: &[String]) -> PortResult<()> {
        self.owner_of(id).await?.run_in_space(id, command).await
    }

    async fn owner_of(&self, id: &SpaceId) -> PortResult<&Arc<dyn WorkspaceTarget>> {
        for target in self.targets.values() {
            if let Ok(spaces) = target.list_spaces().await
                && spaces.iter().any(|space| space.id() == id)
            {
                return Ok(target);
            }
        }
        Err(PortError::not_found("space", id.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeWorkspaceTarget;

    fn directory() -> (
        WorkspaceDirectory,
        Arc<FakeWorkspaceTarget>,
        Arc<FakeWorkspaceTarget>,
    ) {
        let herdr = Arc::new(FakeWorkspaceTarget::new(TargetKind::Herdr));
        let tmux = Arc::new(FakeWorkspaceTarget::new(TargetKind::Tmux));
        let mut directory = WorkspaceDirectory::default();
        directory.register(herdr.clone());
        directory.register(tmux.clone());
        (directory, herdr, tmux)
    }

    #[tokio::test]
    async fn a_space_is_created_on_the_target_that_was_asked_for() {
        let (directory, herdr, tmux) = directory();
        let id = directory
            .create_space(
                TargetKind::Herdr,
                &SpaceName::sanitized("pr-142"),
                Path::new("/work/trees/pr-142"),
            )
            .await
            .expect("create");

        assert_eq!(herdr.spaces().len(), 1);
        assert!(tmux.spaces().is_empty());
        assert_eq!(herdr.spaces()[0].id(), &id);
    }

    #[tokio::test]
    async fn asking_for_an_unregistered_target_says_which_one_is_missing() {
        let (directory, _, _) = directory();
        let error = directory
            .create_space(
                TargetKind::Editor,
                &SpaceName::sanitized("x"),
                Path::new("/tmp"),
            )
            .await
            .expect_err("no editor target");
        assert!(matches!(error, PortError::NotConfigured { .. }));
    }

    #[tokio::test]
    async fn spaces_from_every_target_are_listed_together() {
        let (directory, _, _) = directory();
        directory
            .create_space(
                TargetKind::Herdr,
                &SpaceName::sanitized("a"),
                Path::new("/a"),
            )
            .await
            .expect("create");
        directory
            .create_space(
                TargetKind::Tmux,
                &SpaceName::sanitized("b"),
                Path::new("/b"),
            )
            .await
            .expect("create");

        let inventory = directory.inventory().await;

        assert_eq!(inventory.spaces.len(), 2);
        assert!(inventory.problems.is_empty());
        let kinds: Vec<_> = inventory.spaces.iter().map(Space::target).collect();
        assert!(kinds.contains(&TargetKind::Herdr));
        assert!(kinds.contains(&TargetKind::Tmux));
    }

    #[tokio::test]
    async fn focusing_and_closing_find_the_target_that_owns_the_space() {
        let (directory, herdr, _) = directory();
        let id = directory
            .create_space(
                TargetKind::Herdr,
                &SpaceName::sanitized("a"),
                Path::new("/a"),
            )
            .await
            .expect("create");

        directory.focus_space(&id).await.expect("focus");
        assert_eq!(herdr.focused(), Some(id.clone()));

        directory.close_space(&id).await.expect("close");
        assert!(herdr.spaces().is_empty());
    }

    #[tokio::test]
    async fn focusing_a_space_no_target_owns_is_an_error() {
        let (directory, _, _) = directory();
        let error = directory
            .focus_space(&SpaceId::parse("ghost").expect("id"))
            .await
            .expect_err("unknown space");
        assert!(matches!(error, PortError::NotFound { .. }));
    }

    #[tokio::test]
    async fn a_command_can_be_run_in_a_space_so_an_agent_sits_beside_the_work() {
        let (directory, herdr, _) = directory();
        let id = directory
            .create_space(
                TargetKind::Herdr,
                &SpaceName::sanitized("a"),
                Path::new("/a"),
            )
            .await
            .expect("create");

        directory
            .run_in_space(&id, &["claude".to_owned(), "-p".to_owned()])
            .await
            .expect("run");

        assert_eq!(
            herdr.commands(),
            vec![(
                id.as_str().to_owned(),
                vec!["claude".to_owned(), "-p".to_owned()]
            )]
        );
    }

    #[tokio::test]
    async fn the_registered_target_kinds_are_reported_for_the_ui() {
        let (directory, _, _) = directory();
        assert_eq!(directory.kinds(), vec![TargetKind::Herdr, TargetKind::Tmux]);
    }
}
