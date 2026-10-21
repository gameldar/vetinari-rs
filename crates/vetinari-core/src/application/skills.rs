//! The skill editor's application service.
//!
//! The editor holds strings; the domain holds a [`Skill`]. [`SkillDraft`] is
//! the boundary between them: it reports every bad field at once so the editor
//! can mark them all, and anything it produces is already valid.

use std::sync::Arc;

use super::ports::{PortResult, SkillStore};
use crate::domain::agent::TaskClass;
use crate::domain::ids::{AgentId, SkillId};
use crate::domain::skill::{PromptTemplate, Skill, SkillLibrary};
use crate::domain::text::NonEmptyText;

/// One thing wrong with a draft.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {message}")]
pub struct DraftProblem {
    /// Which field is wrong.
    pub field: &'static str,
    /// What is wrong with it.
    pub message: String,
}

/// The editable form of a skill, as the TUI editor holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillDraft {
    /// The skill id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// Its prompt template.
    pub template: String,
    /// The class of work it produces.
    pub class: String,
    /// The engine it pins, or blank for routing.
    pub agent: String,
    /// Whitespace-separated engine flags.
    pub flags: String,
    /// What the skill is for.
    pub description: String,
}

impl SkillDraft {
    /// A blank draft for a new skill.
    #[must_use]
    pub fn blank() -> Self {
        Self {
            class: TaskClass::DeepCodeChange.name().to_owned(),
            ..Self::default()
        }
    }

    /// Open an existing skill for editing.
    #[must_use]
    pub fn from_skill(skill: &Skill) -> Self {
        Self {
            id: skill.id().as_str().to_owned(),
            name: skill.name().as_str().to_owned(),
            template: skill.template().as_str().to_owned(),
            class: skill.class().name().to_owned(),
            agent: skill.agent().map(AgentId::as_str).unwrap_or("").to_owned(),
            flags: skill.flags().join(" "),
            description: skill.description().unwrap_or("").to_owned(),
        }
    }

    /// Parse the draft, reporting every problem rather than just the first.
    pub fn parse(&self) -> Result<Skill, Vec<DraftProblem>> {
        let mut problems = Vec::new();

        let id = SkillId::parse(&self.id).map_err(|error| DraftProblem {
            field: "id",
            message: error.to_string(),
        });
        let name = NonEmptyText::parse(&self.name).map_err(|error| DraftProblem {
            field: "name",
            message: error.to_string(),
        });
        let template = PromptTemplate::parse(&self.template).map_err(|error| DraftProblem {
            field: "template",
            message: error.to_string(),
        });
        let class = self
            .class
            .parse::<TaskClass>()
            .map_err(|error| DraftProblem {
                field: "class",
                message: error.to_string(),
            });
        let agent = if self.agent.trim().is_empty() {
            Ok(None)
        } else {
            AgentId::parse(&self.agent)
                .map(Some)
                .map_err(|error| DraftProblem {
                    field: "agent",
                    message: error.to_string(),
                })
        };

        problems.extend(
            [
                id.as_ref().err(),
                name.as_ref().err(),
                template.as_ref().err(),
                class.as_ref().err(),
                agent.as_ref().err(),
            ]
            .into_iter()
            .flatten()
            .cloned(),
        );
        if !problems.is_empty() {
            return Err(problems);
        }

        let mut skill = Skill::new(
            id.expect("checked above"),
            name.expect("checked above"),
            template.expect("checked above"),
            class.expect("checked above"),
        );
        if let Some(agent) = agent.expect("checked above") {
            skill = skill.with_agent(agent);
        }
        for flag in self.flags.split_whitespace() {
            skill = skill.with_flag(flag);
        }
        if !self.description.trim().is_empty() {
            skill = skill.with_description(self.description.trim());
        }
        Ok(skill)
    }
}

/// Loads and persists the skill library.
pub struct SkillService {
    store: Arc<dyn SkillStore>,
}

impl SkillService {
    /// Build the service.
    #[must_use]
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self { store }
    }

    /// Load the library as stored.
    pub async fn load(&self) -> PortResult<SkillLibrary> {
        self.store.load().await
    }

    /// Load the library, falling back to the built-in skills when the store
    /// is empty or unreadable, so the editor is never blank.
    pub async fn load_or_builtin(&self) -> SkillLibrary {
        match self.store.load().await {
            Ok(library) if !library.is_empty() => library,
            _ => SkillLibrary::builtin(),
        }
    }

    /// Add or replace a skill and persist the library.
    pub async fn save(&self, library: &mut SkillLibrary, skill: Skill) -> PortResult<()> {
        library.upsert(skill);
        self.store.save(library).await
    }

    /// Remove a skill and persist the library, reporting whether it existed.
    pub async fn delete(&self, library: &mut SkillLibrary, id: &SkillId) -> PortResult<bool> {
        if !library.remove(id) {
            return Ok(false);
        }
        self.store.save(library).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemorySkillStore;

    fn draft() -> SkillDraft {
        SkillDraft {
            id: "pr-triage".to_owned(),
            name: "PR triage".to_owned(),
            template: "Triage PR {{number}}".to_owned(),
            class: "pr-summary".to_owned(),
            agent: String::new(),
            flags: String::new(),
            description: String::new(),
        }
    }

    #[test]
    fn a_valid_draft_parses_into_a_skill() {
        let skill = draft().parse().expect("valid draft");
        assert_eq!(skill.id().as_str(), "pr-triage");
        assert_eq!(skill.name().as_str(), "PR triage");
        assert_eq!(skill.class(), TaskClass::PrSummary);
        assert!(skill.agent().is_none());
        assert!(skill.flags().is_empty());
    }

    #[test]
    fn a_draft_can_pin_an_engine_and_carry_flags() {
        let parsed = SkillDraft {
            agent: "agy-gemini".to_owned(),
            flags: "--model=gemini-2.5-pro  --yolo".to_owned(),
            description: "Fast triage".to_owned(),
            ..draft()
        }
        .parse()
        .expect("valid draft");
        assert_eq!(parsed.agent().map(AgentId::as_str), Some("agy-gemini"));
        assert_eq!(
            parsed.flags(),
            &["--model=gemini-2.5-pro".to_owned(), "--yolo".to_owned()]
        );
        assert_eq!(parsed.description(), Some("Fast triage"));
    }

    #[test]
    fn a_draft_reports_every_field_that_is_wrong_at_once() {
        let errors = SkillDraft {
            id: "Not A Slug".to_owned(),
            name: "  ".to_owned(),
            template: "unclosed {{".to_owned(),
            class: "sorcery".to_owned(),
            agent: "not a slug".to_owned(),
            ..draft()
        }
        .parse()
        .expect_err("invalid draft");

        let fields: Vec<_> = errors.iter().map(|problem| problem.field).collect();
        assert_eq!(fields, vec!["id", "name", "template", "class", "agent"]);
        assert!(errors.iter().all(|problem| !problem.message.is_empty()));
    }

    #[test]
    fn a_draft_can_be_opened_from_an_existing_skill_and_round_trips() {
        let original = SkillLibrary::builtin()
            .get(&SkillId::parse("pr-review").expect("id"))
            .expect("built-in")
            .clone();
        let reparsed = SkillDraft::from_skill(&original)
            .parse()
            .expect("round trip");
        assert_eq!(reparsed, original);
    }

    #[test]
    fn a_blank_draft_is_offered_with_a_sensible_class() {
        let blank = SkillDraft::blank();
        assert_eq!(blank.class, TaskClass::DeepCodeChange.name());
        assert!(blank.parse().is_err(), "a blank draft is not yet a skill");
    }

    #[tokio::test]
    async fn saving_a_skill_persists_the_whole_library() {
        let store = Arc::new(MemorySkillStore::with_library(SkillLibrary::default()));
        let service = SkillService::new(store.clone());
        let mut library = service.load().await.expect("load");

        service
            .save(&mut library, draft().parse().expect("skill"))
            .await
            .expect("save");

        assert_eq!(library.len(), 1);
        assert_eq!(store.library().len(), 1);
        assert!(
            store
                .library()
                .get(&SkillId::parse("pr-triage").expect("id"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_empty_store_loads_the_builtin_skills_so_the_editor_is_never_blank() {
        let store = Arc::new(MemorySkillStore::with_library(SkillLibrary::default()));
        let service = SkillService::new(store);
        let library = service.load_or_builtin().await;
        assert!(
            library
                .get(&SkillId::parse("pr-review").expect("id"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn deleting_a_skill_persists_the_removal() {
        let store = Arc::new(MemorySkillStore::default());
        let service = SkillService::new(store.clone());
        let mut library = service.load().await.expect("load");
        let id = SkillId::parse("pr-review").expect("id");

        assert!(service.delete(&mut library, &id).await.expect("delete"));

        assert!(library.get(&id).is_none());
        assert!(store.library().get(&id).is_none());
        assert!(!service.delete(&mut library, &id).await.expect("delete"));
    }
}
