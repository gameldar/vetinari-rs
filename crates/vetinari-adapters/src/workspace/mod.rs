//! Workspace targets.

pub mod editor;
pub mod herdr;
pub mod tmux;

pub use editor::EditorTarget;
pub use herdr::HerdrSocketTarget;
pub use tmux::TmuxTarget;
