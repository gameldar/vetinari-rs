//! The dogfooding engine's input: a feedback request plus the state the
//! developer was looking at when they wrote it.
//!
//! Capturing context is the point of the feature - a request typed while
//! staring at a failing CI row means something different from the same words
//! typed on an empty feed - so the captured state is part of the prompt the
//! implementing agent receives.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use super::git::WorktreeSpec;
use super::ids::ProfileId;
use super::keymap::Mode;
use super::skill::Bindings;
use super::text::NonEmptyText;
use super::workspace::SpaceName;

/// What the developer could see when they opened the feedback overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedContext {
    profile: ProfileId,
    mode: Mode,
    selected_item: Option<String>,
    pending_keys: Option<String>,
    spaces: Vec<String>,
    sessions: Vec<String>,
    log_tail: Vec<String>,
    config_path: Option<PathBuf>,
}

impl CapturedContext {
    /// How many log lines are worth sending to an agent.
    pub const MAX_LOG_LINES: usize = 20;

    /// Start a capture from the state that always exists.
    #[must_use]
    pub const fn new(profile: ProfileId, mode: Mode) -> Self {
        Self {
            profile,
            mode,
            selected_item: None,
            pending_keys: None,
            spaces: Vec::new(),
            sessions: Vec::new(),
            log_tail: Vec::new(),
            config_path: None,
        }
    }

    /// Record the feed row that was selected.
    #[must_use]
    pub fn with_selected_item(mut self, row: impl Into<String>) -> Self {
        self.selected_item = Some(row.into());
        self
    }

    /// Record a half-finished key sequence.
    #[must_use]
    pub fn with_pending_keys(mut self, keys: impl Into<String>) -> Self {
        self.pending_keys = Some(keys.into());
        self
    }

    /// Record an open workspace space.
    #[must_use]
    pub fn with_space(mut self, space: impl Into<String>) -> Self {
        self.spaces.push(space.into());
        self
    }

    /// Record a running agent session.
    #[must_use]
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.sessions.push(session.into());
        self
    }

    /// Record a log line, keeping only the most recent ones.
    #[must_use]
    pub fn with_log_line(mut self, line: impl Into<String>) -> Self {
        self.log_tail.push(line.into());
        if self.log_tail.len() > Self::MAX_LOG_LINES {
            let overflow = self.log_tail.len() - Self::MAX_LOG_LINES;
            self.log_tail.drain(..overflow);
        }
        self
    }

    /// Record which config file is in force.
    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// The log lines kept.
    #[must_use]
    pub fn log_tail(&self) -> &[String] {
        &self.log_tail
    }

    /// Render the capture as a markdown block for an agent prompt.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "## Vetinari state at the time of the request");
        let _ = writeln!(out, "- Profile: {}", self.profile);
        let _ = writeln!(out, "- Mode: {}", self.mode.name());
        if let Some(row) = &self.selected_item {
            let _ = writeln!(out, "- Selected queue row: {row}");
        }
        if let Some(keys) = &self.pending_keys {
            let _ = writeln!(out, "- Pending key sequence: {keys}");
        }
        if let Some(path) = &self.config_path {
            let _ = writeln!(out, "- Config file: {}", path.display());
        }
        if !self.spaces.is_empty() {
            let _ = writeln!(out, "\n### Open spaces");
            for space in &self.spaces {
                let _ = writeln!(out, "- {space}");
            }
        }
        if !self.sessions.is_empty() {
            let _ = writeln!(out, "\n### Agent sessions");
            for session in &self.sessions {
                let _ = writeln!(out, "- {session}");
            }
        }
        if !self.log_tail.is_empty() {
            let _ = writeln!(out, "\n### Recent log");
            let _ = writeln!(out, "```");
            for line in &self.log_tail {
                let _ = writeln!(out, "{line}");
            }
            let _ = writeln!(out, "```");
        }
        out
    }
}

/// The project a feedback request improves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackTarget {
    name: NonEmptyText,
    repo: PathBuf,
}

impl FeedbackTarget {
    /// Name a target project and its repository.
    #[must_use]
    pub const fn new(name: NonEmptyText, repo: PathBuf) -> Self {
        Self { name, repo }
    }

    /// The project name.
    #[must_use]
    pub const fn name(&self) -> &NonEmptyText {
        &self.name
    }

    /// The repository to cut a worktree from.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }
}

/// A natural-language request to improve Vetinari or the active project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackRequest {
    request: NonEmptyText,
    target: FeedbackTarget,
    context: CapturedContext,
    submitted_at: OffsetDateTime,
}

impl FeedbackRequest {
    /// Capture a request.
    #[must_use]
    pub const fn new(
        request: NonEmptyText,
        target: FeedbackTarget,
        context: CapturedContext,
        submitted_at: OffsetDateTime,
    ) -> Self {
        Self {
            request,
            target,
            context,
            submitted_at,
        }
    }

    /// What the developer asked for.
    #[must_use]
    pub const fn request(&self) -> &NonEmptyText {
        &self.request
    }

    /// The project to change.
    #[must_use]
    pub const fn target(&self) -> &FeedbackTarget {
        &self.target
    }

    /// The captured state.
    #[must_use]
    pub const fn context(&self) -> &CapturedContext {
        &self.context
    }

    /// When it was submitted.
    #[must_use]
    pub const fn submitted_at(&self) -> OffsetDateTime {
        self.submitted_at
    }

    /// Bindings for the `enhancement` skill.
    #[must_use]
    pub fn bindings(&self) -> Bindings {
        Bindings::new()
            .with("request", self.request.as_str())
            .with("context", self.context.render())
    }

    /// The name for the space this work opens in.
    #[must_use]
    pub fn space_name(&self) -> SpaceName {
        SpaceName::for_feedback(&self.request.single_line())
    }

    /// The worktree to cut for the work.
    #[must_use]
    pub fn worktree_spec(&self, worktree_root: &Path) -> WorktreeSpec {
        WorktreeSpec::for_feedback(
            self.target.repo.clone(),
            worktree_root.to_path_buf(),
            &self.request.single_line(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn context() -> CapturedContext {
        CapturedContext::new(ProfileId::parse("work").expect("id"), Mode::Normal)
            .with_selected_item("[PR] #142 Fix memory leak (CI failed)")
            .with_pending_keys("SPC f")
            .with_space("[herdr] pr-142 (/work/trees/pr-142)")
            .with_session("claude-code s1: writing tests (running)")
            .with_log_line("WARN source github: rate limited, retrying in 30s")
            .with_config_path(PathBuf::from("/home/dev/.config/vetinari/config.toml"))
    }

    #[test]
    fn captured_context_renders_what_the_developer_was_looking_at() {
        let rendered = context().render();
        assert!(rendered.contains("Profile: work"));
        assert!(rendered.contains("Mode: normal"));
        assert!(rendered.contains("[PR] #142 Fix memory leak (CI failed)"));
        assert!(rendered.contains("SPC f"));
        assert!(rendered.contains("pr-142"));
        assert!(rendered.contains("rate limited"));
        assert!(rendered.contains("config.toml"));
    }

    #[test]
    fn captured_context_omits_sections_it_has_nothing_for() {
        let rendered =
            CapturedContext::new(ProfileId::parse("home").expect("id"), Mode::Insert).render();
        assert!(rendered.contains("Profile: home"));
        assert!(!rendered.contains("Selected"));
        assert!(!rendered.contains("Recent log"));
    }

    #[test]
    fn captured_context_keeps_only_the_last_few_log_lines() {
        let mut captured =
            CapturedContext::new(ProfileId::parse("work").expect("id"), Mode::Normal);
        for index in 0..100 {
            captured = captured.with_log_line(format!("line {index}"));
        }
        let rendered = captured.render();
        assert!(rendered.contains("line 99"));
        assert!(!rendered.contains("line 0\n"));
        assert_eq!(captured.log_tail().len(), CapturedContext::MAX_LOG_LINES);
    }

    #[test]
    fn a_feedback_request_names_the_project_it_improves() {
        let request = FeedbackRequest::new(
            NonEmptyText::parse("The which-key popup should show group labels").expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            context(),
            NOW,
        );
        assert_eq!(request.target().name().as_str(), "vetinari");
        assert_eq!(request.target().repo(), Path::new("/work/repos/vetinari"));
        assert_eq!(request.submitted_at(), NOW);
    }

    #[test]
    fn a_feedback_request_binds_its_text_and_context_for_the_enhancement_skill() {
        let request = FeedbackRequest::new(
            NonEmptyText::parse("Add group labels").expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            context(),
            NOW,
        );
        let bindings = request.bindings();
        assert_eq!(bindings.get("request"), Some("Add group labels"));
        assert!(
            bindings
                .get("context")
                .expect("context binding")
                .contains("Profile: work")
        );
    }

    #[test]
    fn a_feedback_request_names_its_worktree_and_space_after_itself() {
        let request = FeedbackRequest::new(
            NonEmptyText::parse("Add group labels to the which-key popup").expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            context(),
            NOW,
        );
        assert_eq!(
            request.space_name().as_str(),
            "feedback-add-group-labels-to-the-which-key-popup"
        );
        let spec = request.worktree_spec(&PathBuf::from("/work/trees"));
        assert_eq!(
            spec.branch().as_str(),
            "vetinari/feedback/add-group-labels-to-the-which-key-popup"
        );
    }

    #[test]
    fn a_long_feedback_request_still_produces_a_usable_space_name() {
        let long = "improve ".repeat(40);
        let request = FeedbackRequest::new(
            NonEmptyText::parse(&long).expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/repo"),
            ),
            context(),
            NOW,
        );
        assert!(request.space_name().as_str().len() <= SpaceName::MAX_LEN);
    }
}
