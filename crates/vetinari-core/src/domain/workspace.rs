//! Workspace spaces and the names given to them.
//!
//! Herdr, tmux, and editors all accept a name and a path, and all dislike
//! different characters, so names are sanitised once here rather than in each
//! adapter.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::ids::{SessionId, SpaceId};

/// Which backend owns a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// Herdr, over its Unix socket.
    Herdr,
    /// A dedicated tmux session.
    Tmux,
    /// An external editor such as Zed.
    Editor,
}

/// A configuration file named a workspace target that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown workspace target {name:?}")]
pub struct UnknownTarget {
    /// The unrecognised name.
    pub name: String,
}

impl TargetKind {
    /// Every target kind.
    pub const ALL: &'static [TargetKind] =
        &[TargetKind::Herdr, TargetKind::Tmux, TargetKind::Editor];

    /// The name used in configuration files.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            TargetKind::Herdr => "herdr",
            TargetKind::Tmux => "tmux",
            TargetKind::Editor => "editor",
        }
    }

    /// The name shown in the TUI.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            TargetKind::Herdr => "Herdr",
            TargetKind::Tmux => "tmux",
            TargetKind::Editor => "Editor",
        }
    }
}

impl FromStr for TargetKind {
    type Err = UnknownTarget;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        TargetKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.name() == raw.trim())
            .ok_or_else(|| UnknownTarget {
                name: raw.trim().to_owned(),
            })
    }
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A space name every backend will accept: lowercase, hyphen-separated, and
/// bounded in length.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpaceName(String);

impl SpaceName {
    /// The longest name tmux and Herdr handle comfortably.
    pub const MAX_LEN: usize = 60;

    /// Sanitise arbitrary text into a usable name.
    #[must_use]
    pub fn sanitized(raw: &str) -> Self {
        let mut out = String::with_capacity(raw.len().min(Self::MAX_LEN));
        let mut pending_separator = false;
        for ch in raw.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() || lower == '.' {
                if pending_separator && !out.is_empty() {
                    out.push('-');
                }
                pending_separator = false;
                out.push(lower);
                if out.len() >= Self::MAX_LEN {
                    break;
                }
            } else {
                pending_separator = true;
            }
        }
        if out.is_empty() {
            out.push_str("space");
        }
        Self(out)
    }

    /// The name for a pull request review space.
    #[must_use]
    pub fn for_pull_request(number: u64, head_ref: &str) -> Self {
        Self::sanitized(&format!("pr-{number}-{head_ref}"))
    }

    /// The name for a space opened from a feedback request.
    #[must_use]
    pub fn for_feedback(request: &str) -> Self {
        Self::sanitized(&format!("feedback-{request}"))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SpaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A space, window, or editor context owned by a workspace target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    id: SpaceId,
    name: SpaceName,
    path: PathBuf,
    target: TargetKind,
    session: Option<SessionId>,
}

impl Space {
    /// Record a space a target reported or created.
    #[must_use]
    pub const fn new(id: SpaceId, name: SpaceName, path: PathBuf, target: TargetKind) -> Self {
        Self {
            id,
            name,
            path,
            target,
            session: None,
        }
    }

    /// The backend's id for it.
    #[must_use]
    pub const fn id(&self) -> &SpaceId {
        &self.id
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> &SpaceName {
        &self.name
    }

    /// The directory it is rooted at.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which backend owns it.
    #[must_use]
    pub const fn target(&self) -> TargetKind {
        self.target
    }

    /// The agent session running in it, if any.
    #[must_use]
    pub const fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    /// Tie an agent session to the space.
    #[must_use]
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    /// One line for the workspace monitor.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "[{}] {} ({})",
            self.target.name(),
            self.name,
            self.path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_kinds_round_trip_through_their_config_names() {
        for kind in TargetKind::ALL {
            assert_eq!(
                kind.name().parse::<TargetKind>().expect("known kind"),
                *kind
            );
            assert!(!kind.display_name().is_empty());
        }
        assert!("carrier-pigeon".parse::<TargetKind>().is_err());
    }

    #[test]
    fn a_space_name_is_slugified_for_backends_that_dislike_punctuation() {
        let name = SpaceName::sanitized("Fix memory leak in auth/parser!");
        assert_eq!(name.as_str(), "fix-memory-leak-in-auth-parser");
    }

    #[test]
    fn space_names_collapse_runs_and_trim_separators() {
        assert_eq!(SpaceName::sanitized("  a   b  ").as_str(), "a-b");
        assert_eq!(SpaceName::sanitized("--a--").as_str(), "a");
    }

    #[test]
    fn an_unnameable_space_falls_back_to_a_stable_placeholder() {
        assert_eq!(SpaceName::sanitized("***").as_str(), "space");
    }

    #[test]
    fn space_names_are_capped_so_tmux_and_herdr_accept_them() {
        let long = "x".repeat(200);
        let name = SpaceName::sanitized(&long);
        assert_eq!(name.as_str().len(), SpaceName::MAX_LEN);
    }

    #[test]
    fn a_pull_request_space_is_named_after_the_review() {
        let name = SpaceName::for_pull_request(142, "fix/memory-leak");
        assert_eq!(name.as_str(), "pr-142-fix-memory-leak");
    }

    #[test]
    fn a_feedback_space_is_named_after_the_request() {
        let name = SpaceName::for_feedback("Add a which-key popup");
        assert_eq!(name.as_str(), "feedback-add-a-which-key-popup");
    }

    #[test]
    fn a_space_describes_itself_for_the_monitor_pane() {
        let space = Space::new(
            SpaceId::parse("space-3").expect("id"),
            SpaceName::sanitized("pr-142"),
            PathBuf::from("/work/trees/pr-142"),
            TargetKind::Herdr,
        );
        assert_eq!(space.describe(), "[herdr] pr-142 (/work/trees/pr-142)");
        assert!(space.session().is_none());
    }

    #[test]
    fn a_space_remembers_the_agent_session_it_hosts() {
        let space = Space::new(
            SpaceId::parse("space-3").expect("id"),
            SpaceName::sanitized("pr-142"),
            PathBuf::from("/work/trees/pr-142"),
            TargetKind::Tmux,
        )
        .with_session(SessionId::parse("s9").expect("id"));
        assert_eq!(space.session().map(SessionId::as_str), Some("s9"));
    }
}
