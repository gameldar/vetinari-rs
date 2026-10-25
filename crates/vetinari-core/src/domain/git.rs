//! Git worktrees: the isolation every spawned coding task gets.
//!
//! Branch and directory names are derived here, once, from the thing that
//! prompted the work. Both are sanitised to what git and a filesystem accept,
//! and the directory name includes the repository so two projects can have a
//! worktree for the same-sounding request.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ids::{EmptyId, opaque_id};
use super::workspace::SpaceName;

opaque_id!(
    /// A commit, as `git rev-parse` names it.
    CommitId
);

/// A git branch name that git will accept.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

impl BranchName {
    /// The longest branch name Vetinari will generate.
    pub const MAX_LEN: usize = 100;

    /// Used when the input sanitises away to nothing.
    pub const FALLBACK: &'static str = "vetinari/work";

    /// Sanitise arbitrary text into a branch name.
    #[must_use]
    pub fn sanitized(raw: &str) -> Self {
        let mut out = String::with_capacity(raw.len().min(Self::MAX_LEN));
        for ch in raw.to_ascii_lowercase().chars() {
            if out.len() >= Self::MAX_LEN {
                break;
            }
            let mapped = if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' {
                ch
            } else if ch == '/' {
                '/'
            } else {
                '-'
            };
            match mapped {
                '/' => {
                    while out.ends_with('-') {
                        out.pop();
                    }
                    if !out.is_empty() && !out.ends_with('/') {
                        out.push('/');
                    }
                }
                '-' => {
                    if !out.is_empty() && !out.ends_with('-') && !out.ends_with('/') {
                        out.push('-');
                    }
                }
                kept => out.push(kept),
            }
        }
        while out.ends_with('-') || out.ends_with('/') || out.ends_with('.') {
            out.pop();
        }
        if out.ends_with(".lock") {
            out.truncate(out.len() - ".lock".len());
            out.push_str("-lock");
        }
        if out.is_empty() {
            out.push_str(Self::FALLBACK);
        }
        Self(out)
    }

    /// Accept a branch name only if it is already one Vetinari would have
    /// generated, as when reading back a name it wrote.
    pub fn parse(raw: &str) -> Result<Self, InvalidBranchName> {
        let branch = Self::sanitized(raw);
        if branch.0 == raw {
            Ok(branch)
        } else {
            Err(InvalidBranchName {
                raw: raw.to_owned(),
            })
        }
    }

    /// Borrow the branch name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A branch name was not one Vetinari generates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{raw:?} is not a branch name vetinari generates")]
pub struct InvalidBranchName {
    /// The offending name.
    pub raw: String,
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A request to create a worktree: which repository, which branch, where, and
/// what to base it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSpec {
    repo: PathBuf,
    branch: BranchName,
    path: PathBuf,
    base: Option<String>,
}

impl WorktreeSpec {
    /// Build a spec by hand.
    #[must_use]
    pub const fn new(
        repo: PathBuf,
        branch: BranchName,
        path: PathBuf,
        base: Option<String>,
    ) -> Self {
        Self {
            repo,
            branch,
            path,
            base,
        }
    }

    /// A worktree for reviewing a pull request, based on the PR's own branch.
    #[must_use]
    pub fn for_pull_request(
        repo: PathBuf,
        worktree_root: PathBuf,
        number: u64,
        head_ref: &str,
    ) -> Self {
        let directory =
            SpaceName::sanitized(&format!("{}-pr-{number}-{head_ref}", repo_label(&repo)));
        Self {
            path: worktree_root.join(directory.as_str()),
            repo,
            branch: BranchName::sanitized(&format!("vetinari/review/pr-{number}")),
            base: Some(head_ref.to_owned()),
        }
    }

    /// A worktree for implementing a feedback request, based on the
    /// repository's current head.
    #[must_use]
    pub fn for_feedback(repo: PathBuf, worktree_root: PathBuf, request: &str) -> Self {
        let slug = SpaceName::sanitized(request);
        let directory = SpaceName::sanitized(&format!("{}-feedback-{slug}", repo_label(&repo)));
        Self {
            path: worktree_root.join(directory.as_str()),
            repo,
            branch: BranchName::sanitized(&format!("vetinari/feedback/{slug}")),
            base: None,
        }
    }

    /// The worktree a dev workflow run works in. Every step of the run uses
    /// this one branch and directory, so a rerun adopts the work an earlier
    /// attempt left.
    #[must_use]
    pub fn for_run(
        repo: PathBuf,
        worktree_root: &Path,
        branch: &BranchName,
        base: Option<&BranchName>,
    ) -> Self {
        let name = branch.as_str();
        let name = name.strip_prefix("vetinari/").unwrap_or(name);
        let directory = SpaceName::sanitized(&format!("{}-{name}", repo_label(&repo)));
        Self {
            path: worktree_root.join(directory.as_str()),
            repo,
            branch: branch.clone(),
            base: base.map(ToString::to_string),
        }
    }

    /// The repository the worktree is cut from.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The branch to create.
    #[must_use]
    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// Where the worktree will live.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The ref to base the branch on, or the repository head when absent.
    #[must_use]
    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }
}

fn repo_label(repo: &Path) -> String {
    repo.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_owned())
}

/// A worktree that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    path: PathBuf,
    branch: BranchName,
    repo: PathBuf,
}

impl Worktree {
    /// Record a worktree.
    #[must_use]
    pub const fn new(path: PathBuf, branch: BranchName, repo: PathBuf) -> Self {
        Self { path, branch, repo }
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The branch checked out in it.
    #[must_use]
    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The repository it belongs to.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// One line for the TUI.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} at {}", self.branch, self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_drop_characters_git_refuses() {
        let branch = BranchName::sanitized("review/PR #142: fix~leak");
        assert_eq!(branch.as_str(), "review/pr-142-fix-leak");
    }

    #[test]
    fn branch_names_keep_slashes_but_never_lead_or_trail_with_one() {
        assert_eq!(BranchName::sanitized("/a/b/").as_str(), "a/b");
        assert_eq!(BranchName::sanitized("a//b").as_str(), "a/b");
    }

    #[test]
    fn branch_names_never_end_with_dot_lock() {
        assert_eq!(BranchName::sanitized("wip.lock").as_str(), "wip-lock");
    }

    #[test]
    fn a_generated_branch_name_parses_back_and_anything_else_is_refused() {
        let branch = BranchName::sanitized("vetinari/ENG-412 retries");
        assert_eq!(BranchName::parse(branch.as_str()), Ok(branch));
        for raw in ["", "Main", "a b", "a/", "x.lock"] {
            assert_eq!(
                BranchName::parse(raw),
                Err(InvalidBranchName {
                    raw: raw.to_owned()
                }),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_unusable_branch_name_falls_back_to_a_placeholder() {
        assert_eq!(BranchName::sanitized("~~~").as_str(), "vetinari/work");
    }

    #[test]
    fn a_review_worktree_is_named_after_the_pull_request() {
        let spec = WorktreeSpec::for_pull_request(
            PathBuf::from("/work/repos/web"),
            PathBuf::from("/work/trees"),
            142,
            "fix/memory-leak",
        );
        assert_eq!(spec.branch().as_str(), "vetinari/review/pr-142");
        assert_eq!(
            spec.path(),
            Path::new("/work/trees/web-pr-142-fix-memory-leak")
        );
        assert_eq!(spec.base(), Some("fix/memory-leak"));
        assert_eq!(spec.repo(), Path::new("/work/repos/web"));
    }

    #[test]
    fn a_feedback_worktree_is_named_after_the_request() {
        let spec = WorktreeSpec::for_feedback(
            PathBuf::from("/work/repos/vetinari"),
            PathBuf::from("/work/trees"),
            "Add a which-key popup",
        );
        assert_eq!(
            spec.branch().as_str(),
            "vetinari/feedback/add-a-which-key-popup"
        );
        assert_eq!(
            spec.path(),
            Path::new("/work/trees/vetinari-feedback-add-a-which-key-popup")
        );
        assert_eq!(spec.base(), None);
    }

    #[test]
    fn a_run_worktree_is_named_after_the_repository_and_the_run_branch() {
        let branch = BranchName::sanitized("vetinari/eng-412-rate-limit-retries");
        let base = BranchName::sanitized("main");
        let spec = WorktreeSpec::for_run(
            PathBuf::from("/work/repos/web"),
            Path::new("/work/trees"),
            &branch,
            Some(&base),
        );
        assert_eq!(spec.branch(), &branch);
        assert_eq!(
            spec.path(),
            Path::new("/work/trees/web-eng-412-rate-limit-retries")
        );
        assert_eq!(spec.base(), Some("main"));
        assert_eq!(spec.repo(), Path::new("/work/repos/web"));
        assert_eq!(
            WorktreeSpec::for_run(
                PathBuf::from("/work/repos/web"),
                Path::new("/work/trees"),
                &branch,
                None
            )
            .base(),
            None
        );
    }

    #[test]
    fn a_worktree_path_never_collides_with_another_repos_branch() {
        let first = WorktreeSpec::for_feedback(
            PathBuf::from("/work/repos/alpha"),
            PathBuf::from("/work/trees"),
            "same request",
        );
        let second = WorktreeSpec::for_feedback(
            PathBuf::from("/work/repos/beta"),
            PathBuf::from("/work/trees"),
            "same request",
        );
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn a_created_worktree_reports_where_it_landed() {
        let worktree = Worktree::new(
            PathBuf::from("/work/trees/web-pr-142"),
            BranchName::sanitized("vetinari/review/pr-142"),
            PathBuf::from("/work/repos/web"),
        );
        assert_eq!(
            worktree.describe(),
            "vetinari/review/pr-142 at /work/trees/web-pr-142"
        );
    }
}
