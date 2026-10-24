//! The command vocabulary a keypress can resolve to.
//!
//! Actions are the only thing the keymap produces, and the only thing the TUI
//! reducer consumes, which keeps key handling and behaviour independent of one
//! another. Each action owns its configuration name and its which-key label.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A configuration file named an action that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown action {name:?}")]
pub struct UnknownAction {
    /// The unrecognised name.
    pub name: String,
}

macro_rules! actions {
    ($( $variant:ident => $name:literal , $desc:literal );+ $(;)?) => {
        /// A command resolved from a key sequence.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Action {
            $(
                #[doc = $desc]
                $variant,
            )+
        }

        impl Action {
            /// Every action, in declaration order.
            pub const ALL: &'static [Action] = &[ $( Action::$variant ),+ ];

            /// The stable name used in configuration files.
            #[must_use]
            pub const fn name(&self) -> &'static str {
                match self {
                    $( Action::$variant => $name, )+
                }
            }

            /// The short label shown in the which-key popup and footer.
            #[must_use]
            pub const fn description(&self) -> &'static str {
                match self {
                    $( Action::$variant => $desc, )+
                }
            }
        }

        impl FromStr for Action {
            type Err = UnknownAction;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                match raw.trim() {
                    $( $name => Ok(Action::$variant), )+
                    other => Err(UnknownAction { name: other.to_owned() }),
                }
            }
        }
    };
}

actions! {
    Quit => "quit", "Quit Vetinari";
    Refresh => "refresh", "Refresh all sources";
    Help => "help", "Describe keybindings";
    CancelPending => "cancel-pending", "Cancel the pending key sequence";

    NavDown => "nav-down", "Move down";
    NavUp => "nav-up", "Move up";
    NavTop => "nav-top", "Jump to first entry";
    NavBottom => "nav-bottom", "Jump to last entry";
    NavPageDown => "nav-page-down", "Scroll down a page";
    NavPageUp => "nav-page-up", "Scroll up a page";
    PaneLeft => "pane-left", "Focus pane to the left";
    PaneRight => "pane-right", "Focus pane to the right";
    PaneNext => "pane-next", "Focus next pane";
    PanePrev => "pane-prev", "Focus previous pane";
    FilterStart => "filter-start", "Filter the queue";
    FilterClear => "filter-clear", "Clear the filter";
    Select => "select", "Open the selected entry";

    QueueTakeNext => "queue-take-next", "Take what needs you next";
    QueueTake => "queue-take", "Work on this";
    QueueFinish => "queue-finish", "Finish";
    QueueSilence => "queue-silence", "Silence";
    QueueParkHour => "queue-park-hour", "Park for an hour";
    QueueParkTomorrow => "queue-park-tomorrow", "Park until tomorrow";
    QueueParkChanged => "queue-park-changed", "Park until it changes";
    QueueWaitAgent => "queue-wait-agent", "Wait on its agent";
    QueueWaitReply => "queue-wait-reply", "Wait on a reply";
    FocusToggle => "focus-toggle", "Toggle focus time";

    FeedbackOpen => "feedback-open", "Feedback / enhancement";
    FeedbackSubmitLast => "feedback-submit-last", "Re-run last feedback task";

    PrReviewSpawn => "pr-review-spawn", "PR review workflow";
    PrApprove => "pr-approve", "Approve pull request";
    PrComment => "pr-comment", "Comment on pull request";

    IssueUpdateStatus => "issue-update-status", "Update issue status";
    IssueAssign => "issue-assign", "Assign issue";
    IssueStandupSummary => "issue-standup-summary", "Standup summary";

    ChatReply => "chat-reply", "Reply to message";
    ChatConvertToTask => "chat-convert-to-task", "Convert to task";

    MeetingActions => "meeting-actions", "Meeting / calendar";
    MeetingPrep => "meeting-prep", "Prepare for meeting";
    MeetingSchedule => "meeting-schedule", "View schedule";

    AgentMenu => "agent-menu", "Agent management";
    AgentUnblock => "agent-unblock", "Unblock agent";
    AgentCancel => "agent-cancel", "Cancel agent task";
    AgentRerun => "agent-rerun", "Re-run agent task";
    AgentLogs => "agent-logs", "Show agent output";

    WorkspaceNavigate => "workspace-navigate", "Workspace targets";
    WorkspaceFocusSpace => "workspace-focus-space", "Focus space";
    WorkspaceCloseSpace => "workspace-close-space", "Close space";
    WorkspaceRunAgent => "workspace-run-agent", "Run the agent in its space";

    ProfileCycle => "profile-cycle", "Switch context profile";
    ProfileMenu => "profile-menu", "Choose context profile";

    SkillEditor => "skill-editor", "Edit agent skills";
    SkillRun => "skill-run", "Run a skill";

    AuthWizard => "auth-wizard", "Credential setup wizard";
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for Action {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_through_their_config_names() {
        for action in Action::ALL {
            let name = action.name();
            assert_eq!(
                name.parse::<Action>().expect("known action"),
                *action,
                "round trip of {name}"
            );
        }
    }

    #[test]
    fn config_names_are_kebab_case_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for action in Action::ALL {
            let name = action.name();
            assert!(seen.insert(name), "duplicate action name {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{name} is not kebab-case"
            );
        }
    }

    #[test]
    fn every_action_carries_a_which_key_description() {
        for action in Action::ALL {
            assert!(
                !action.description().is_empty(),
                "{} has no description",
                action.name()
            );
        }
    }

    #[test]
    fn unknown_action_names_are_rejected_with_the_offending_text() {
        let err = "summon-dragon".parse::<Action>().expect_err("unknown");
        assert_eq!(
            err,
            UnknownAction {
                name: "summon-dragon".to_owned()
            }
        );
    }

    #[test]
    fn the_prd_headline_actions_exist() {
        for name in [
            "feedback-open",
            "pr-review-spawn",
            "profile-cycle",
            "skill-editor",
            "agent-menu",
            "workspace-navigate",
            "meeting-actions",
            "issue-update-status",
            "chat-reply",
            "meeting-prep",
            "workspace-focus-space",
            "agent-unblock",
            "quit",
        ] {
            assert!(name.parse::<Action>().is_ok(), "{name} is missing");
        }
    }
}
