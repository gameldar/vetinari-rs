//! The overlays: everything that takes the foreground and asks a question.
//!
//! Each one carries what to do with its answer, so the reducer never has to
//! remember why a prompt was opened.

use vetinari_core::application::ports::DeviceCodeChallenge;
use vetinari_core::application::skills::SkillDraft;
use vetinari_core::domain::attention::{IssueRef, MessageRef, PullRequestRef};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::ids::{ItemId, ProfileId, SessionId, SkillId, SourceId, SpaceId};
use vetinari_core::domain::skill::Skill;

use crate::text_input::TextInput;

/// What a prompt's answer is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// Filtering the feed, applied as it is typed.
    Filter,
    /// A feedback or enhancement request.
    Feedback,
    /// A comment on a pull request.
    PrComment {
        /// The source that produced the item.
        source: SourceId,
        /// The pull request.
        pr: PullRequestRef,
    },
    /// A reply to a chat message.
    ChatReply {
        /// The source that produced the item.
        source: SourceId,
        /// The message to reply to.
        message: MessageRef,
    },
    /// An answer to a question an agent is blocked on.
    AgentAnswer(SessionId),
    /// A new status for an issue.
    IssueStatus {
        /// The source that produced the item.
        source: SourceId,
        /// The issue.
        issue: IssueRef,
    },
    /// A new assignee for an issue.
    IssueAssignee {
        /// The source that produced the item.
        source: SourceId,
        /// The issue.
        issue: IssueRef,
    },
    /// An API token for a provider.
    ApiToken(Provider),
}

/// A question with a text answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// What the answer is for.
    pub kind: PromptKind,
    /// The overlay's title.
    pub title: String,
    /// The line of guidance under the title.
    pub hint: String,
    /// The text being typed.
    pub input: TextInput,
}

impl Prompt {
    /// Build a single-line prompt.
    #[must_use]
    pub fn line(kind: PromptKind, title: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            hint: hint.into(),
            input: TextInput::single_line(),
        }
    }

    /// Build a multi-line prompt.
    #[must_use]
    pub fn paragraph(kind: PromptKind, title: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            hint: hint.into(),
            input: TextInput::multiline(),
        }
    }
}

/// What choosing a menu item does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuTarget {
    /// Activate a profile.
    Profile(ProfileId),
    /// Act on an agent session.
    Session(SessionId),
    /// Focus a workspace space.
    Space(SpaceId),
    /// Run or edit a skill.
    Skill(SkillId),
    /// Authenticate a provider.
    Provider(Provider),
    /// Start a skill that does not exist yet.
    NewSkill,
    /// Act on a feed item.
    Item(ItemId),
}

/// One row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// The text shown.
    pub label: String,
    /// The dimmer text shown after it.
    pub detail: String,
    /// What choosing it does.
    pub target: MenuTarget,
}

/// Which menu is open, and so what choosing an item means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// Switch context profile.
    Profiles,
    /// Manage agent sessions.
    Agents,
    /// Navigate workspace spaces.
    Spaces,
    /// Run a skill.
    Skills,
    /// Edit a skill.
    SkillsToEdit,
    /// Set up credentials.
    Providers,
    /// Choose a meeting to prepare for.
    Meetings,
}

/// A list to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// Which menu this is.
    pub kind: MenuKind,
    /// Its title.
    pub title: String,
    /// Its rows.
    pub items: Vec<MenuItem>,
    cursor: usize,
}

impl Menu {
    /// Build a menu.
    #[must_use]
    pub fn new(kind: MenuKind, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            kind,
            title: title.into(),
            items,
            cursor: 0,
        }
    }

    /// The highlighted row.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The highlighted item, if the menu has any.
    #[must_use]
    pub fn selected(&self) -> Option<&MenuItem> {
        self.items.get(self.cursor)
    }

    /// Move down, stopping at the end.
    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.cursor = (self.cursor + 1).min(self.items.len() - 1);
        }
    }

    /// Move up, stopping at the start.
    pub fn previous(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Jump to the first row.
    pub fn first(&mut self) {
        self.cursor = 0;
    }

    /// Jump to the last row.
    pub fn last(&mut self) {
        self.cursor = self.items.len().saturating_sub(1);
    }
}

/// What a form is editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    /// A skill; `Some` when editing an existing one.
    Skill(Option<SkillId>),
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    /// The draft key it populates.
    pub key: &'static str,
    /// Its label.
    pub label: &'static str,
    /// Its value.
    pub input: TextInput,
}

/// A multi-field editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    /// What is being edited.
    pub kind: FormKind,
    /// Its fields, in tab order.
    pub fields: Vec<FormField>,
    /// Problems reported by the last attempt to save.
    pub problems: Vec<String>,
    cursor: usize,
}

impl Form {
    /// The skill editor's fields, in the order they are shown.
    const SKILL_FIELDS: &'static [(&'static str, &'static str, bool)] = &[
        ("id", "Id", false),
        ("name", "Name", false),
        ("class", "Task class", false),
        ("agent", "Engine (blank to route)", false),
        ("flags", "Engine flags", false),
        ("description", "Description", false),
        ("template", "Prompt template", true),
    ];

    /// Open the skill editor on a draft.
    #[must_use]
    pub fn skill(draft: &SkillDraft, editing: Option<SkillId>) -> Self {
        let fields = Self::SKILL_FIELDS
            .iter()
            .map(|(key, label, multiline)| {
                let value = match *key {
                    "id" => draft.id.clone(),
                    "name" => draft.name.clone(),
                    "class" => draft.class.clone(),
                    "agent" => draft.agent.clone(),
                    "flags" => draft.flags.clone(),
                    "description" => draft.description.clone(),
                    _ => draft.template.clone(),
                };
                let input = if *multiline {
                    TextInput::multiline()
                } else {
                    TextInput::single_line()
                };
                FormField {
                    key,
                    label,
                    input: input.with_value(value),
                }
            })
            .collect();
        Self {
            kind: FormKind::Skill(editing),
            fields,
            problems: Vec::new(),
            cursor: 0,
        }
    }

    /// The field being edited.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The field being edited, if the form has any.
    pub fn focused(&mut self) -> Option<&mut FormField> {
        self.fields.get_mut(self.cursor)
    }

    /// Move to the next field, wrapping.
    pub fn next_field(&mut self) {
        if !self.fields.is_empty() {
            self.cursor = (self.cursor + 1) % self.fields.len();
        }
    }

    /// Move to the previous field, wrapping.
    pub fn previous_field(&mut self) {
        if !self.fields.is_empty() {
            self.cursor = (self.cursor + self.fields.len() - 1) % self.fields.len();
        }
    }

    /// Read the form back as a draft.
    #[must_use]
    pub fn draft(&self) -> SkillDraft {
        let value = |key: &str| {
            self.fields
                .iter()
                .find(|field| field.key == key)
                .map(|field| field.input.value().to_owned())
                .unwrap_or_default()
        };
        SkillDraft {
            id: value("id"),
            name: value("name"),
            template: value("template"),
            class: value("class"),
            agent: value("agent"),
            flags: value("flags"),
            description: value("description"),
        }
    }

    /// Parse the form, recording any problems for display.
    pub fn parse(&mut self) -> Option<Skill> {
        match self.draft().parse() {
            Ok(skill) => {
                self.problems.clear();
                Some(skill)
            }
            Err(problems) => {
                self.problems = problems
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect();
                None
            }
        }
    }
}

/// A device-code challenge, shown while Vetinari waits for approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodeView {
    /// Which provider is being authorised.
    pub provider: Provider,
    /// The challenge to display.
    pub challenge: DeviceCodeChallenge,
    /// The latest word on how it is going.
    pub message: String,
}

/// A scrollable block of text, for help and agent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextView {
    /// Its title.
    pub title: String,
    /// Its lines.
    pub lines: Vec<String>,
    /// How far down it is scrolled.
    pub scroll: usize,
}

impl TextView {
    /// Build a text view.
    #[must_use]
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title: title.into(),
            lines,
            scroll: 0,
        }
    }

    /// Scroll down.
    pub fn scroll_down(&mut self, by: usize) {
        self.scroll = (self.scroll + by).min(self.lines.len().saturating_sub(1));
    }

    /// Scroll up.
    pub fn scroll_up(&mut self, by: usize) {
        self.scroll = self.scroll.saturating_sub(by);
    }
}

/// Whatever currently has the foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// A question with a text answer.
    Prompt(Prompt),
    /// A list to choose from.
    Menu(Menu),
    /// A multi-field editor.
    Form(Form),
    /// A block of text.
    Text(TextView),
    /// The details of the selected row, drawn from the queue as it stands.
    Details,
    /// A device-code challenge.
    DeviceCode(DeviceCodeView),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        Menu::new(
            MenuKind::Profiles,
            "Profiles",
            vec![
                MenuItem {
                    label: "Work".to_owned(),
                    detail: "herdr".to_owned(),
                    target: MenuTarget::Profile(ProfileId::parse("work").expect("id")),
                },
                MenuItem {
                    label: "Home".to_owned(),
                    detail: "tmux".to_owned(),
                    target: MenuTarget::Profile(ProfileId::parse("home").expect("id")),
                },
            ],
        )
    }

    #[test]
    fn a_menu_starts_on_its_first_row() {
        let menu = menu();
        assert_eq!(menu.cursor(), 0);
        assert_eq!(menu.selected().expect("item").label, "Work");
    }

    #[test]
    fn menu_navigation_stops_at_both_ends() {
        let mut menu = menu();
        menu.next();
        assert_eq!(menu.cursor(), 1);
        menu.next();
        assert_eq!(menu.cursor(), 1);
        menu.previous();
        menu.previous();
        assert_eq!(menu.cursor(), 0);
        menu.last();
        assert_eq!(menu.cursor(), 1);
        menu.first();
        assert_eq!(menu.cursor(), 0);
    }

    #[test]
    fn an_empty_menu_has_nothing_selected_and_does_not_panic() {
        let mut menu = Menu::new(MenuKind::Agents, "Agents", Vec::new());
        menu.next();
        menu.last();
        assert!(menu.selected().is_none());
        assert_eq!(menu.cursor(), 0);
    }

    #[test]
    fn the_skill_editor_shows_every_field_with_the_template_last() {
        let form = Form::skill(&SkillDraft::blank(), None);
        let keys: Vec<_> = form.fields.iter().map(|field| field.key).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "name",
                "class",
                "agent",
                "flags",
                "description",
                "template"
            ]
        );
        assert!(
            form.fields.last().expect("a field").input.is_multiline(),
            "a prompt template needs more than one line"
        );
    }

    #[test]
    fn a_form_round_trips_an_existing_skill() {
        let library = vetinari_core::domain::skill::SkillLibrary::builtin();
        let original = library
            .get(&SkillId::parse("pr-review").expect("id"))
            .expect("built-in");
        let mut form = Form::skill(
            &SkillDraft::from_skill(original),
            Some(original.id().clone()),
        );

        assert_eq!(form.parse().as_ref(), Some(original));
    }

    #[test]
    fn a_form_reports_its_problems_rather_than_losing_them() {
        let mut form = Form::skill(&SkillDraft::blank(), None);

        assert!(form.parse().is_none());

        assert!(!form.problems.is_empty());
        assert!(
            form.problems
                .iter()
                .any(|problem| problem.starts_with("id:"))
        );
    }

    #[test]
    fn fixing_a_form_clears_the_problems_it_reported() {
        let mut form = Form::skill(&SkillDraft::blank(), None);
        form.parse();
        let fixed = SkillDraft {
            id: "triage".to_owned(),
            name: "Triage".to_owned(),
            template: "Triage {{number}}".to_owned(),
            class: "pr-summary".to_owned(),
            ..SkillDraft::blank()
        };
        form = Form::skill(&fixed, None);

        assert!(form.parse().is_some());
        assert!(form.problems.is_empty());
    }

    #[test]
    fn form_fields_cycle_in_both_directions() {
        let mut form = Form::skill(&SkillDraft::blank(), None);
        let last = form.fields.len() - 1;
        form.previous_field();
        assert_eq!(form.cursor(), last);
        form.next_field();
        assert_eq!(form.cursor(), 0);
    }

    #[test]
    fn a_text_view_scrolls_within_its_lines() {
        let mut view = TextView::new(
            "Output",
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        );
        view.scroll_down(2);
        assert_eq!(view.scroll, 2);
        view.scroll_down(10);
        assert_eq!(view.scroll, 2);
        view.scroll_up(1);
        assert_eq!(view.scroll, 1);
        view.scroll_up(10);
        assert_eq!(view.scroll, 0);
    }

    #[test]
    fn a_prompt_knows_whether_it_takes_a_paragraph() {
        let line = Prompt::line(PromptKind::Filter, "Filter", "type to narrow");
        assert!(!line.input.is_multiline());
        let paragraph = Prompt::paragraph(PromptKind::Feedback, "Feedback", "what should change?");
        assert!(paragraph.input.is_multiline());
    }
}
