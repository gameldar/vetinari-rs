//! The skill library on disk, as TOML.
//!
//! A malformed file is reported rather than quietly replaced with the
//! built-ins: a developer's prompt templates are their work, and losing them
//! to a parse error would be worse than refusing to start.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use vetinari_core::application::ports::{PortError, PortResult, SkillStore};
use vetinari_core::domain::skill::SkillLibrary;

/// Persists the skill library in a TOML file.
#[derive(Debug, Clone)]
pub struct FileSkillStore {
    path: PathBuf,
}

impl FileSkillStore {
    /// Store the library at a path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the library lives, relative to a config directory.
    #[must_use]
    pub fn default_path(config_dir: &Path) -> PathBuf {
        config_dir
            .join(super::super::config::loader::APP_DIR)
            .join("skills.toml")
    }

    /// The file backing the library.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl SkillStore for FileSkillStore {
    async fn load(&self) -> PortResult<SkillLibrary> {
        let text = match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SkillLibrary::default());
            }
            Err(error) => {
                return Err(PortError::unavailable(
                    "skill library",
                    format!("could not read {}: {error}", self.path.display()),
                ));
            }
        };

        toml::from_str(&text).map_err(|error| {
            PortError::protocol(
                "skill library",
                format!("{} is malformed: {error}", self.path.display()),
            )
        })
    }

    async fn save(&self, library: &SkillLibrary) -> PortResult<()> {
        let text = toml::to_string_pretty(library).map_err(|error| {
            PortError::protocol("skill library", format!("could not encode: {error}"))
        })?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                PortError::unavailable(
                    "skill library",
                    format!("could not create {}: {error}", parent.display()),
                )
            })?;
        }
        tokio::fs::write(&self.path, text).await.map_err(|error| {
            PortError::unavailable(
                "skill library",
                format!("could not write {}: {error}", self.path.display()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetinari_core::domain::agent::TaskClass;
    use vetinari_core::domain::ids::SkillId;
    use vetinari_core::domain::skill::{PromptTemplate, Skill};
    use vetinari_core::domain::text::NonEmptyText;

    fn skill(id: &str) -> Skill {
        Skill::new(
            SkillId::parse(id).expect("id"),
            NonEmptyText::parse(id).expect("name"),
            PromptTemplate::parse("do {{thing}}").expect("template"),
            TaskClass::Enhancement,
        )
    }

    #[tokio::test]
    async fn a_library_survives_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSkillStore::new(dir.path().join("skills.toml"));
        let mut library = SkillLibrary::default();
        library.upsert(skill("pr-triage"));
        library.upsert(
            skill("deep-fix")
                .with_agent(vetinari_core::domain::ids::AgentId::parse("claude-code").expect("id"))
                .with_flag("--verbose")
                .with_description("Reproduce then fix"),
        );

        store.save(&library).await.expect("save");
        let loaded = store.load().await.expect("load");

        assert_eq!(loaded, library);
    }

    #[tokio::test]
    async fn an_absent_file_loads_as_an_empty_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSkillStore::new(dir.path().join("skills.toml"));
        assert!(store.load().await.expect("load").is_empty());
    }

    #[tokio::test]
    async fn saving_creates_the_directory_it_needs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deep/er/skills.toml");
        let store = FileSkillStore::new(path.clone());

        store.save(&SkillLibrary::builtin()).await.expect("save");

        assert!(path.is_file());
    }

    #[tokio::test]
    async fn a_malformed_file_is_reported_rather_than_silently_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("skills.toml");
        std::fs::write(&path, "this is not a skill library").expect("write");
        let store = FileSkillStore::new(path);

        let error = store.load().await.expect_err("malformed");

        assert!(matches!(error, PortError::Protocol { .. }));
    }

    #[tokio::test]
    async fn a_skill_with_a_broken_template_is_rejected_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("skills.toml");
        std::fs::write(
            &path,
            "[broken]\nid = \"broken\"\nname = \"Broken\"\ntemplate = \"unclosed {{\"\nclass = \"enhancement\"\n",
        )
        .expect("write");
        let store = FileSkillStore::new(path);

        assert!(store.load().await.is_err());
    }

    #[tokio::test]
    async fn the_built_in_library_is_written_in_a_form_that_loads_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSkillStore::new(dir.path().join("skills.toml"));
        let builtin = SkillLibrary::builtin();

        store.save(&builtin).await.expect("save");

        assert_eq!(store.load().await.expect("load"), builtin);
    }

    #[test]
    fn the_default_path_sits_beside_the_config() {
        assert_eq!(
            FileSkillStore::default_path(Path::new("/home/dev/.config")),
            PathBuf::from("/home/dev/.config/vetinari/skills.toml")
        );
    }
}
