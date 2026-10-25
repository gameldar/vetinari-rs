//! Step files on disk, and keeping them out of git.
//!
//! `info/exclude` rather than `.gitignore`: the developer's ignore file is
//! theirs and is committed, while `info/exclude` is local to the clone. For a
//! linked worktree it lives in the repository's common directory, so git is
//! asked where it is rather than the path being guessed.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::process::Command;
use vetinari_core::application::devflow::ports::StepFiles;
use vetinari_core::application::ports::{PortError, PortResult};

/// What `info/exclude` gains.
const EXCLUDE: &str = ".vetinari/";

/// Reads and writes step files with the filesystem, and `git` to find
/// `info/exclude`.
#[derive(Debug, Clone)]
pub struct FsStepFiles {
    git: String,
}

impl Default for FsStepFiles {
    fn default() -> Self {
        Self {
            git: "git".to_owned(),
        }
    }
}

impl FsStepFiles {
    async fn exclude_file(&self, worktree: &Path) -> PortResult<PathBuf> {
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(worktree)
            .args(["rev-parse", "--git-path", "info/exclude"])
            .output()
            .await
            .map_err(|error| {
                PortError::unavailable("git", format!("could not run {}: {error}", self.git))
            })?;
        if !output.status.success() {
            return Err(PortError::Refused {
                what: "git rev-parse".to_owned(),
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(worktree.join(path))
    }
}

fn unavailable(action: &str, path: &Path, error: &std::io::Error) -> PortError {
    PortError::unavailable(
        "step files",
        format!("could not {action} {}: {error}", path.display()),
    )
}

#[async_trait]
impl StepFiles for FsStepFiles {
    async fn prepare(&self, worktree: &Path) -> PortResult<()> {
        let exclude = self.exclude_file(worktree).await?;
        let existing = match tokio::fs::read_to_string(&exclude).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(unavailable("read", &exclude, &error)),
        };
        if existing.lines().any(|line| line.trim() == EXCLUDE) {
            return Ok(());
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(EXCLUDE);
        text.push('\n');
        self.write(&exclude, text.as_bytes()).await
    }

    async fn read(&self, path: &Path) -> PortResult<Option<Vec<u8>>> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(unavailable("read", path, &error)),
        }
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> PortResult<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| unavailable("create", parent, &error))?;
        }
        tokio::fs::write(path, contents)
            .await
            .map_err(|error| unavailable("write", path, &error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    async fn repo_with_a_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&repo)
            .output()
            .await
            .expect("git init");
        assert!(output.status.success());
        git(&repo, &["config", "user.email", "dev@example.test"]).await;
        git(&repo, &["config", "user.name", "Dev"]).await;
        tokio::fs::write(repo.join("README.md"), "hello")
            .await
            .expect("write");
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-m", "initial"]).await;
        let tree = dir.path().join("tree");
        git(
            &repo,
            &["worktree", "add", "-b", "work", &tree.to_string_lossy()],
        )
        .await;
        (dir, tree)
    }

    #[tokio::test]
    async fn a_prepared_worktree_never_shows_its_step_files_to_git() {
        let (_dir, tree) = repo_with_a_worktree().await;
        let files = FsStepFiles::default();
        files.prepare(&tree).await.expect("prepare");
        files
            .write(&tree.join(".vetinari/steps/review@1-1.json"), b"{}")
            .await
            .expect("write");
        assert_eq!(git(&tree, &["status", "--porcelain"]).await, "");
    }

    #[tokio::test]
    async fn preparing_twice_excludes_once() {
        let (_dir, tree) = repo_with_a_worktree().await;
        let files = FsStepFiles::default();
        files.prepare(&tree).await.expect("prepare");
        files.prepare(&tree).await.expect("prepare again");
        let exclude = git(&tree, &["rev-parse", "--git-path", "info/exclude"]).await;
        let exclude = tree.join(exclude);
        let text = tokio::fs::read_to_string(&exclude).await.expect("read");
        assert_eq!(text.lines().filter(|line| *line == ".vetinari/").count(), 1);
    }

    #[tokio::test]
    async fn preparing_something_that_is_not_a_worktree_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(FsStepFiles::default().prepare(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_file_reads_as_none_and_a_written_one_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = FsStepFiles::default();
        let path = dir.path().join(".vetinari/steps/implement@1-2.head");
        assert_eq!(files.read(&path).await.expect("read"), None);
        files.write(&path, b"abc123").await.expect("write");
        assert_eq!(
            files.read(&path).await.expect("read"),
            Some(b"abc123".to_vec())
        );
    }
}
