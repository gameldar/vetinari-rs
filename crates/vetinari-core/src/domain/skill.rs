//! Skills: prompt templates with the engine and flags they expect.
//!
//! A template is parsed once, so a skill that cannot render is rejected when
//! it is saved rather than when a workflow tries to run it at the worst
//! possible moment.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::agent::{TaskClass, TaskSpec};
use super::ids::{AgentId, SkillId};
use super::text::NonEmptyText;

/// Why a template could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template held nothing but whitespace.
    #[error("template is empty")]
    Empty,
    /// A `{{` was never closed.
    #[error("template has an unclosed placeholder")]
    UnclosedPlaceholder,
    /// A `{{}}` named nothing.
    #[error("template has an unnamed placeholder")]
    EmptyPlaceholder,
}

/// Why a template could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// A placeholder had no value supplied.
    #[error("no value supplied for {{{{{name}}}}}")]
    MissingBinding {
        /// The placeholder that went unbound.
        name: String,
    },
    /// Rendering produced nothing usable.
    #[error("rendered prompt is empty")]
    Empty,
}

/// Values to substitute into a template.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bindings(BTreeMap<String, String>);

impl Bindings {
    /// An empty set of bindings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a binding.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    /// Look a binding up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Fragment {
    Literal(String),
    Placeholder(String),
}

/// A prompt with `{{placeholder}}` holes, validated at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    raw: String,
    fragments: Vec<Fragment>,
}

impl PromptTemplate {
    /// Parse a template, checking that every placeholder is closed and named.
    pub fn parse(raw: &str) -> Result<Self, TemplateError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TemplateError::Empty);
        }

        let mut fragments = Vec::new();
        let mut rest = trimmed;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                fragments.push(Fragment::Literal(rest[..open].to_owned()));
            }
            let after = &rest[open + 2..];
            let close = after.find("}}").ok_or(TemplateError::UnclosedPlaceholder)?;
            let name = after[..close].trim();
            if name.is_empty() {
                return Err(TemplateError::EmptyPlaceholder);
            }
            fragments.push(Fragment::Placeholder(name.to_owned()));
            rest = &after[close + 2..];
        }
        if !rest.is_empty() {
            fragments.push(Fragment::Literal(rest.to_owned()));
        }

        Ok(Self {
            raw: trimmed.to_owned(),
            fragments,
        })
    }

    /// The template as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The placeholders it needs, sorted and deduplicated.
    #[must_use]
    pub fn placeholders(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .fragments
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Placeholder(name) => Some(name.clone()),
                Fragment::Literal(_) => None,
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Substitute the bindings, yielding a prompt that is known to be usable.
    pub fn render(&self, bindings: &Bindings) -> Result<NonEmptyText, RenderError> {
        let mut out = String::with_capacity(self.raw.len());
        for fragment in &self.fragments {
            match fragment {
                Fragment::Literal(text) => out.push_str(text),
                Fragment::Placeholder(name) => {
                    let value = bindings
                        .get(name)
                        .ok_or_else(|| RenderError::MissingBinding { name: name.clone() })?;
                    out.push_str(value);
                }
            }
        }
        NonEmptyText::parse(&out).map_err(|_| RenderError::Empty)
    }
}

impl fmt::Display for PromptTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for PromptTemplate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for PromptTemplate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A named, reusable prompt bound to a class of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    id: SkillId,
    name: NonEmptyText,
    template: PromptTemplate,
    class: TaskClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl Skill {
    /// Define a skill.
    #[must_use]
    pub const fn new(
        id: SkillId,
        name: NonEmptyText,
        template: PromptTemplate,
        class: TaskClass,
    ) -> Self {
        Self {
            id,
            name,
            template,
            class,
            agent: None,
            flags: Vec::new(),
            description: None,
        }
    }

    /// Pin the skill to one engine, overriding the routing table.
    #[must_use]
    pub fn with_agent(mut self, agent: AgentId) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Add a command-line flag for the engine.
    #[must_use]
    pub fn with_flag(mut self, flag: impl Into<String>) -> Self {
        self.flags.push(flag.into());
        self
    }

    /// Describe what the skill is for.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The skill id.
    #[must_use]
    pub const fn id(&self) -> &SkillId {
        &self.id
    }

    /// Its display name.
    #[must_use]
    pub const fn name(&self) -> &NonEmptyText {
        &self.name
    }

    /// Its template.
    #[must_use]
    pub const fn template(&self) -> &PromptTemplate {
        &self.template
    }

    /// The class of work it produces.
    #[must_use]
    pub const fn class(&self) -> TaskClass {
        self.class
    }

    /// The engine it pins, if any.
    #[must_use]
    pub const fn agent(&self) -> Option<&AgentId> {
        self.agent.as_ref()
    }

    /// Extra engine flags.
    #[must_use]
    pub fn flags(&self) -> &[String] {
        &self.flags
    }

    /// Its description.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Replace the template, e.g. from the skill editor.
    pub fn set_template(&mut self, template: PromptTemplate) {
        self.template = template;
    }

    /// Render the skill into a task ready for an engine.
    pub fn to_task(
        &self,
        bindings: &Bindings,
        working_dir: PathBuf,
    ) -> Result<TaskSpec, RenderError> {
        let prompt = self.template.render(bindings)?;
        Ok(TaskSpec::new(prompt, self.class, working_dir).with_skill(self.id.clone()))
    }
}

/// Every skill Vetinari knows about, keyed by id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillLibrary {
    skills: BTreeMap<SkillId, Skill>,
}

impl SkillLibrary {
    /// Add or replace a skill.
    pub fn upsert(&mut self, skill: Skill) {
        self.skills.insert(skill.id.clone(), skill);
    }

    /// Look a skill up.
    #[must_use]
    pub fn get(&self, id: &SkillId) -> Option<&Skill> {
        self.skills.get(id)
    }

    /// Borrow a skill for editing.
    pub fn get_mut(&mut self, id: &SkillId) -> Option<&mut Skill> {
        self.skills.get_mut(id)
    }

    /// Remove a skill, reporting whether it existed.
    pub fn remove(&mut self, id: &SkillId) -> bool {
        self.skills.remove(id).is_some()
    }

    /// How many skills are defined.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the library is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Every skill, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// The skills that produce a given class of work.
    #[must_use]
    pub fn for_class(&self, class: TaskClass) -> Vec<&Skill> {
        self.skills
            .values()
            .filter(|skill| skill.class == class)
            .collect()
    }

    /// The skills Vetinari ships with, covering the PRD's workflows.
    ///
    /// # Panics
    ///
    /// Panics if a built-in template is malformed, which would be a bug here
    /// rather than in a user's configuration.
    #[must_use]
    pub fn builtin() -> Self {
        const BUILTINS: &[(&str, &str, TaskClass, &str)] = &[
            (
                "pr-review",
                "PR review",
                TaskClass::PrReview,
                "Review pull request {{number}} on {{repo}} (branch {{head_ref}}). Read the diff \
                 against {{base_ref}}, run the project's tests, and report correctness risks, \
                 missing tests, and anything that would block approval. Do not push changes.",
            ),
            (
                "pr-summary",
                "PR summary",
                TaskClass::PrSummary,
                "Summarise pull request {{number}} on {{repo}} in under 200 words: what changes, \
                 why, and what a reviewer should look at first.",
            ),
            (
                "deep-fix",
                "Deep bug fix",
                TaskClass::DeepCodeChange,
                "Reproduce and fix the following issue in this worktree, starting with a failing \
                 test: {{description}}",
            ),
            (
                "standup-summary",
                "Standup summary",
                TaskClass::StandupSummary,
                "Summarise my open issues and pull requests for standup: what moved yesterday, \
                 what is in flight today, and what is blocked. Issues: {{issues}}",
            ),
            (
                "meeting-prep",
                "Meeting preparation",
                TaskClass::MeetingPrep,
                "I have '{{title}}' at {{starts_at}} with {{attendees}}. Pull together the \
                 context I need: related issues, open pull requests, and recent decisions. \
                 Finish with three questions worth asking.",
            ),
            (
                "enhancement",
                "Implement feedback",
                TaskClass::Enhancement,
                "{{context}}\n\nImplement this change in the current worktree, test first: \
                 {{request}}",
            ),
        ];

        let mut library = Self::default();
        for (id, name, class, template) in BUILTINS {
            library.upsert(Skill::new(
                SkillId::parse(id).expect("built-in skill id is valid"),
                NonEmptyText::parse(name).expect("built-in skill name is non-empty"),
                PromptTemplate::parse(template).expect("built-in template parses"),
                *class,
            ));
        }
        library
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(raw: &str) -> PromptTemplate {
        PromptTemplate::parse(raw).expect("template")
    }

    #[test]
    fn a_template_substitutes_its_placeholders() {
        let rendered = template("Review PR {{number}} on {{repo}}")
            .render(
                &Bindings::new()
                    .with("number", "142")
                    .with("repo", "acme/web"),
            )
            .expect("render");
        assert_eq!(rendered.as_str(), "Review PR 142 on acme/web");
    }

    #[test]
    fn a_template_lists_the_placeholders_it_needs() {
        let placeholders = template("{{a}} and {{b}} and {{a}}").placeholders();
        assert_eq!(placeholders, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn rendering_without_a_binding_names_the_missing_placeholder() {
        let err = template("Review PR {{number}}")
            .render(&Bindings::new())
            .expect_err("missing binding");
        assert_eq!(
            err,
            RenderError::MissingBinding {
                name: "number".to_owned()
            }
        );
    }

    #[test]
    fn surplus_bindings_are_ignored() {
        let rendered = template("Hello {{name}}")
            .render(
                &Bindings::new()
                    .with("name", "Vimes")
                    .with("rank", "Commander"),
            )
            .expect("render");
        assert_eq!(rendered.as_str(), "Hello Vimes");
    }

    #[test]
    fn a_template_with_no_placeholders_renders_verbatim() {
        let rendered = template("Summarise today's standup")
            .render(&Bindings::new())
            .expect("render");
        assert_eq!(rendered.as_str(), "Summarise today's standup");
    }

    #[test]
    fn an_empty_template_is_rejected_at_parse_time() {
        assert!(PromptTemplate::parse("   ").is_err());
    }

    #[test]
    fn an_unclosed_placeholder_is_rejected_at_parse_time() {
        assert_eq!(
            PromptTemplate::parse("Review {{number"),
            Err(TemplateError::UnclosedPlaceholder)
        );
    }

    #[test]
    fn a_placeholder_must_be_named() {
        assert_eq!(
            PromptTemplate::parse("Review {{}}"),
            Err(TemplateError::EmptyPlaceholder)
        );
    }

    #[test]
    fn a_skill_turns_bindings_into_a_task_for_its_class() {
        let skill = Skill::new(
            SkillId::parse("pr-review").expect("id"),
            NonEmptyText::parse("PR review").expect("name"),
            template("Review PR {{number}} and report findings"),
            TaskClass::PrReview,
        );
        let task = skill
            .to_task(
                &Bindings::new().with("number", "142"),
                PathBuf::from("/work/repo"),
            )
            .expect("task");
        assert_eq!(task.prompt().as_str(), "Review PR 142 and report findings");
        assert_eq!(task.class(), TaskClass::PrReview);
        assert_eq!(task.skill().map(SkillId::as_str), Some("pr-review"));
    }

    #[test]
    fn a_skill_may_pin_an_engine_and_extra_flags() {
        let skill = Skill::new(
            SkillId::parse("pr-summary").expect("id"),
            NonEmptyText::parse("PR summary").expect("name"),
            template("Summarise {{number}}"),
            TaskClass::PrSummary,
        )
        .with_agent(AgentId::parse("agy-gemini").expect("id"))
        .with_flag("--model=gemini-2.5-pro");
        assert_eq!(skill.agent().map(AgentId::as_str), Some("agy-gemini"));
        assert_eq!(skill.flags(), &["--model=gemini-2.5-pro".to_owned()]);
    }

    #[test]
    fn a_skill_library_finds_and_replaces_by_id() {
        let mut library = SkillLibrary::default();
        let id = SkillId::parse("pr-review").expect("id");
        library.upsert(Skill::new(
            id.clone(),
            NonEmptyText::parse("PR review").expect("name"),
            template("v1 {{number}}"),
            TaskClass::PrReview,
        ));
        library.upsert(Skill::new(
            id.clone(),
            NonEmptyText::parse("PR review").expect("name"),
            template("v2 {{number}}"),
            TaskClass::PrReview,
        ));
        assert_eq!(library.len(), 1);
        assert_eq!(
            library.get(&id).expect("skill").template().as_str(),
            "v2 {{number}}"
        );
    }

    #[test]
    fn a_skill_library_lists_in_id_order_and_filters_by_class() {
        let mut library = SkillLibrary::default();
        for (id, class) in [
            ("standup", TaskClass::StandupSummary),
            ("pr-review", TaskClass::PrReview),
            ("deep-fix", TaskClass::DeepCodeChange),
        ] {
            library.upsert(Skill::new(
                SkillId::parse(id).expect("id"),
                NonEmptyText::parse(id).expect("name"),
                template("do {{thing}}"),
                class,
            ));
        }
        let ids: Vec<_> = library.iter().map(|s| s.id().as_str().to_owned()).collect();
        assert_eq!(ids, vec!["deep-fix", "pr-review", "standup"]);
        assert_eq!(library.for_class(TaskClass::PrReview).len(), 1);
    }

    #[test]
    fn removing_a_skill_reports_whether_it_was_there() {
        let mut library = SkillLibrary::default();
        let id = SkillId::parse("gone").expect("id");
        library.upsert(Skill::new(
            id.clone(),
            NonEmptyText::parse("Gone").expect("name"),
            template("x"),
            TaskClass::Enhancement,
        ));
        assert!(library.remove(&id));
        assert!(!library.remove(&id));
    }

    #[test]
    fn the_builtin_library_covers_the_prd_workflows() {
        let library = SkillLibrary::builtin();
        for id in [
            "pr-review",
            "pr-summary",
            "deep-fix",
            "standup-summary",
            "meeting-prep",
            "enhancement",
        ] {
            let id = SkillId::parse(id).expect("id");
            assert!(library.get(&id).is_some(), "{id} is missing");
        }
    }

    #[test]
    fn every_builtin_skill_renders_from_its_documented_bindings() {
        for skill in SkillLibrary::builtin().iter() {
            let mut bindings = Bindings::new();
            for name in skill.template().placeholders() {
                bindings = bindings.with(name, "x");
            }
            assert!(
                skill.template().render(&bindings).is_ok(),
                "{} does not render",
                skill.id()
            );
        }
    }
}
