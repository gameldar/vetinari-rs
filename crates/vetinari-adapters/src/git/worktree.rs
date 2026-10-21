//! Worktree management through the `git` CLI.
//!
//! The CLI rather than a library binding, because a developer's worktrees are
//! shared with the git they run themselves: hooks, config, and credential
//! helpers all have to behave identically whether Vetinari or the developer
//! created the tree.

use std::path::{Path, PathBuf};
use std::process::Output;

use async_trait::async_trait;
use tokio::process::Command;
use vetinari_core::application::ports::{PortError, PortResult, WorktreeManager};
use vetinari_core::domain::git::{BranchName, Worktree, WorktreeSpec};

/// Creates and removes git worktrees.
#[derive(Debug, Clone)]
pub struct GitWorktreeManager {
    binary: String,
}

impl Default for GitWorktreeManager {
    fn default() -> Self {
        Self {
            binary: "git".to_owned(),
        }
    }
}

impl GitWorktreeManager {
    /// Use a specific git binary.
    #[must_use]
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    async fn run(&self, repo: &Path, args: &[&str]) -> PortResult<Output> {
        Command::new(&self.binary)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .map_err(|error| {
                PortError::unavailable("git", format!("could not run {}: {error}", self.binary))
            })
    }

    async fn run_ok(&self, repo: &Path, what: &str, args: &[&str]) -> PortResult<String> {
        let output = self.run(repo, args).await?;
        if !output.status.success() {
            return Err(PortError::Refused {
                what: what.to_owned(),
                detail: stderr_of(&output),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn branch_exists(&self, repo: &Path, branch: &BranchName) -> bool {
        self.run(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .is_ok_and(|output| output.status.success())
    }

    async fn is_worktree(&self, path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        self.run(path, &["rev-parse", "--is-inside-work-tree"])
            .await
            .is_ok_and(|output| output.status.success())
    }

    async fn branch_of(&self, path: &Path) -> BranchName {
        match self
            .run_ok(
                path,
                "git rev-parse",
                &["rev-parse", "--abbrev-ref", "HEAD"],
            )
            .await
        {
            Ok(name) => BranchName::sanitized(name.trim()),
            Err(_) => BranchName::sanitized(BranchName::FALLBACK),
        }
    }
}

#[async_trait]
impl WorktreeManager for GitWorktreeManager {
    async fn create(&self, spec: &WorktreeSpec) -> PortResult<Worktree> {
        if self.is_worktree(spec.path()).await {
            return Ok(Worktree::new(
                spec.path().to_path_buf(),
                self.branch_of(spec.path()).await,
                spec.repo().to_path_buf(),
            ));
        }

        if let Some(parent) = spec.path().parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                PortError::unavailable(
                    "worktree root",
                    format!("could not create {}: {error}", parent.display()),
                )
            })?;
        }

        let path = spec.path().to_string_lossy().into_owned();
        let branch = spec.branch().to_string();
        let mut args = vec!["worktree", "add"];
        if self.branch_exists(spec.repo(), spec.branch()).await {
            args.push(&path);
            args.push(&branch);
        } else {
            args.push("-b");
            args.push(&branch);
            args.push(&path);
            if let Some(base) = spec.base() {
                args.push(base);
            }
        }

        self.run_ok(spec.repo(), "git worktree add", &args).await?;

        Ok(Worktree::new(
            spec.path().to_path_buf(),
            spec.branch().clone(),
            spec.repo().to_path_buf(),
        ))
    }

    async fn remove(&self, worktree: &Worktree) -> PortResult<()> {
        let path = worktree.path().to_string_lossy().into_owned();
        self.run_ok(
            worktree.repo(),
            "git worktree remove",
            &["worktree", "remove", "--force", &path],
        )
        .await?;
        Ok(())
    }

    async fn list(&self, repo: &Path) -> PortResult<Vec<Worktree>> {
        let output = self
            .run_ok(
                repo,
                "git worktree list",
                &["worktree", "list", "--porcelain"],
            )
            .await?;
        Ok(parse_porcelain(&output, repo))
    }
}

fn stderr_of(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        format!("git exited with status {}", output.status)
    } else {
        text
    }
}

/// Parse `git worktree list --porcelain`, leaving out the primary worktree:
/// callers want the trees Vetinari added, not the repository itself.
fn parse_porcelain(output: &str, repo: &Path) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<BranchName> = None;

    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<BranchName>| {
        if let Some(found) = path.take() {
            if found != repo {
                let branch = branch
                    .take()
                    .unwrap_or_else(|| BranchName::sanitized(BranchName::FALLBACK));
                worktrees.push(Worktree::new(found, branch, repo.to_path_buf()));
            } else {
                *branch = None;
            }
        }
    };

    for line in output.lines() {
        if let Some(found) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(found.trim()));
        } else if let Some(found) = line.strip_prefix("branch ") {
            let name = found.trim().strip_prefix("refs/heads/").unwrap_or(found);
            branch = Some(BranchName::sanitized(name));
        }
    }
    flush(&mut path, &mut branch);
    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use vetinari_core::domain::git::BranchName;

    async fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    async fn repo_with_a_commit() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let output = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(path)
            .output()
            .await
            .expect("git init runs");
        assert!(output.status.success());
        git(path, &["config", "user.email", "dev@example.test"]).await;
        git(path, &["config", "user.name", "Dev"]).await;
        tokio::fs::write(path.join("README.md"), "hello")
            .await
            .expect("write");
        git(path, &["add", "."]).await;
        git(path, &["commit", "-m", "initial"]).await;
        dir
    }

    fn spec(repo: &Path, trees: &Path, branch: &str, base: Option<&str>) -> WorktreeSpec {
        WorktreeSpec::new(
            repo.to_path_buf(),
            BranchName::sanitized(branch),
            trees.join(branch.replace('/', "-")),
            base.map(str::to_owned),
        )
    }

    #[tokio::test]
    async fn creating_a_worktree_makes_the_directory_and_the_branch() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        let spec = spec(repo.path(), trees.path(), "vetinari/review/pr-142", None);

        let worktree = manager.create(&spec).await.expect("create");

        assert!(worktree.path().join("README.md").is_file());
        assert_eq!(worktree.branch(), spec.branch());
        assert_eq!(
            git(worktree.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await,
            "vetinari/review/pr-142"
        );
    }

    #[tokio::test]
    async fn creating_the_same_worktree_twice_adopts_the_existing_one() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        let spec = spec(repo.path(), trees.path(), "vetinari/feedback/again", None);

        let first = manager.create(&spec).await.expect("create");
        tokio::fs::write(first.path().join("scratch.txt"), "work in progress")
            .await
            .expect("write");

        let second = manager.create(&spec).await.expect("adopt");

        assert_eq!(first.path(), second.path());
        assert!(
            second.path().join("scratch.txt").is_file(),
            "adopting must not throw away work in progress"
        );
    }

    #[tokio::test]
    async fn a_worktree_can_be_based_on_another_branch() {
        let repo = repo_with_a_commit().await;
        git(repo.path(), &["branch", "fix/leak"]).await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        let spec = spec(
            repo.path(),
            trees.path(),
            "vetinari/review/pr-7",
            Some("fix/leak"),
        );

        let worktree = manager.create(&spec).await.expect("create");

        assert_eq!(
            git(worktree.path(), &["rev-parse", "HEAD"]).await,
            git(repo.path(), &["rev-parse", "fix/leak"]).await
        );
    }

    #[tokio::test]
    async fn a_branch_that_already_exists_is_checked_out_rather_than_recreated() {
        let repo = repo_with_a_commit().await;
        git(repo.path(), &["branch", "vetinari/feedback/existing"]).await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();

        let worktree = manager
            .create(&spec(
                repo.path(),
                trees.path(),
                "vetinari/feedback/existing",
                None,
            ))
            .await
            .expect("create");

        assert_eq!(
            git(worktree.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).await,
            "vetinari/feedback/existing"
        );
    }

    #[tokio::test]
    async fn the_worktree_root_is_created_if_it_is_missing() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let nested = trees.path().join("deep/er/still");
        let manager = GitWorktreeManager::default();

        manager
            .create(&spec(
                repo.path(),
                &nested,
                "vetinari/feedback/nested",
                None,
            ))
            .await
            .expect("create");

        assert!(nested.is_dir());
    }

    #[tokio::test]
    async fn worktrees_are_listed_without_the_repository_itself() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        manager
            .create(&spec(repo.path(), trees.path(), "vetinari/one", None))
            .await
            .expect("create");
        manager
            .create(&spec(repo.path(), trees.path(), "vetinari/two", None))
            .await
            .expect("create");

        let listed = manager.list(repo.path()).await.expect("list");

        let branches: Vec<_> = listed
            .iter()
            .map(|worktree| worktree.branch().as_str().to_owned())
            .collect();
        assert_eq!(branches.len(), 2, "the main worktree is not listed");
        assert!(branches.contains(&"vetinari/one".to_owned()));
        assert!(branches.contains(&"vetinari/two".to_owned()));
    }

    #[tokio::test]
    async fn removing_a_worktree_deletes_its_directory() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        let worktree = manager
            .create(&spec(repo.path(), trees.path(), "vetinari/gone", None))
            .await
            .expect("create");

        manager.remove(&worktree).await.expect("remove");

        assert!(!worktree.path().exists());
        assert!(manager.list(repo.path()).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_worktree_with_uncommitted_changes_is_still_removed_on_request() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        let worktree = manager
            .create(&spec(repo.path(), trees.path(), "vetinari/dirty", None))
            .await
            .expect("create");
        tokio::fs::write(worktree.path().join("README.md"), "changed")
            .await
            .expect("write");

        manager.remove(&worktree).await.expect("remove");

        assert!(!worktree.path().exists());
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_is_reported_clearly() {
        let not_a_repo = tempfile::tempdir().expect("tempdir");
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();

        let error = manager
            .create(&spec(not_a_repo.path(), trees.path(), "vetinari/x", None))
            .await
            .expect_err("not a repository");

        match error {
            PortError::Refused { what, detail } => {
                assert_eq!(what, "git worktree add");
                assert!(!detail.is_empty());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn listing_a_directory_that_is_not_a_repository_is_reported() {
        let not_a_repo = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::default();
        assert!(manager.list(not_a_repo.path()).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_git_binary_is_reported_as_unavailable() {
        let repo = repo_with_a_commit().await;
        let trees = tempfile::tempdir().expect("tempdir");
        let manager = GitWorktreeManager::with_binary("git-that-does-not-exist");

        let error = manager
            .create(&spec(repo.path(), trees.path(), "vetinari/x", None))
            .await
            .expect_err("no binary");

        assert!(error.is_transient());
    }

    #[test]
    fn porcelain_output_is_parsed_into_worktrees() {
        let output = "\
worktree /work/repos/web
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /work/trees/web-pr-142
HEAD 2222222222222222222222222222222222222222
branch refs/heads/vetinari/review/pr-142

worktree /work/trees/detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let parsed = parse_porcelain(output, Path::new("/work/repos/web"));

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path(), Path::new("/work/trees/web-pr-142"));
        assert_eq!(parsed[0].branch().as_str(), "vetinari/review/pr-142");
        assert_eq!(
            parsed[1].branch().as_str(),
            BranchName::FALLBACK,
            "a detached worktree still has a usable label"
        );
    }
}
