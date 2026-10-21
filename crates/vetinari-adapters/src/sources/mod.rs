//! Attention sources.

pub mod calendar;
pub mod github;
pub mod jira;
pub mod slack;

pub use calendar::{GoogleCalendarSettings, GoogleCalendarSource};
pub use github::{GitHubSettings, GitHubSource};
pub use jira::{JiraSettings, JiraSource};
pub use slack::{SlackSettings, SlackSource};
