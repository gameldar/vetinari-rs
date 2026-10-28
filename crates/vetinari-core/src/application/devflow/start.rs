//! Turning a caller's start into a workflow: the profile it names must be
//! one vetinari has, and a clone given by path alone is named after its
//! `origin`.

use super::ports::RepoOrigins;
use crate::application::ports::PortError;
use crate::domain::devflow::input::{RepoInput, WorkflowInput};
use crate::domain::devflow::template::{DevWorkflow, NotARepository, RepoTarget};
use crate::domain::ids::ProfileId;
use crate::domain::profile::ProfileSet;

/// Why a start could not become a workflow.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// No profile by that id.
    #[error("no profile {0}")]
    UnknownProfile(ProfileId),
    /// The clone's origin is not a repository on a host.
    #[error("the clone's origin: {0}")]
    NotARepository(#[from] NotARepository),
    /// The clone's origin could not be read.
    #[error(transparent)]
    Port(#[from] PortError),
}

/// The workflow `input` asks for, against `profiles`.
pub async fn workflow(
    input: WorkflowInput,
    profiles: &ProfileSet,
    origins: &dyn RepoOrigins,
) -> Result<DevWorkflow, StartError> {
    match input {
        WorkflowInput::FeatureFromTicket(feature) => {
            if profiles.get(&feature.profile).is_none() {
                return Err(StartError::UnknownProfile(feature.profile));
            }
            let repo = match feature.repo.clone() {
                RepoInput::Named(repo) => repo,
                RepoInput::Clone(path) => {
                    let origin = origins.origin(&path).await?;
                    RepoTarget::cloned(path, &origin)?
                }
            };
            Ok(DevWorkflow::FeatureFromTicket(feature.in_repo(repo)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::application::ports::PortResult;
    use crate::domain::devflow::template::{TemplateKind, sample_feature};
    use crate::domain::profile::Profile;
    use crate::domain::text::NonEmptyText;
    use crate::domain::workspace::TargetKind;

    struct Origins(&'static str);

    #[async_trait]
    impl RepoOrigins for Origins {
        async fn origin(&self, clone: &Path) -> PortResult<String> {
            match self.0 {
                "" => Err(PortError::not_found("origin", clone.display().to_string())),
                url => Ok(url.to_owned()),
            }
        }
    }

    fn profiles() -> ProfileSet {
        let work = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Tmux,
        );
        ProfileSet::new(work, Vec::new()).expect("profiles")
    }

    fn clone_input(profile: &str) -> WorkflowInput {
        TemplateKind::FeatureFromTicket
            .parse_input(&json!({
                "ticket": { "source": "jira", "key": "ENG-412", "title": "Rate-limit retries" },
                "repo": { "path": "/src/web" },
                "profile": profile,
            }))
            .expect("input")
    }

    #[tokio::test]
    async fn a_named_repository_is_started_as_given() {
        let named = WorkflowInput::from(DevWorkflow::FeatureFromTicket(sample_feature()));
        assert_eq!(
            workflow(named, &profiles(), &Origins(""))
                .await
                .expect("workflow"),
            DevWorkflow::FeatureFromTicket(sample_feature())
        );
    }

    #[tokio::test]
    async fn a_clone_is_named_after_its_origin() {
        let DevWorkflow::FeatureFromTicket(started) = workflow(
            clone_input("work"),
            &profiles(),
            &Origins("git@github.com:acme/web.git"),
        )
        .await
        .expect("workflow");
        assert_eq!(
            started.repo,
            RepoTarget::at(PathBuf::from("/src/web"), "acme/web").expect("repo")
        );
    }

    #[tokio::test]
    async fn an_unknown_profile_or_an_origin_that_is_no_repository_is_refused() {
        let origins = Origins("git@github.com:acme/web.git");
        assert!(matches!(
            workflow(clone_input("home"), &profiles(), &origins).await,
            Err(StartError::UnknownProfile(profile)) if profile.as_str() == "home"
        ));
        assert!(matches!(
            workflow(clone_input("work"), &profiles(), &Origins("/srv/git/web")).await,
            Err(StartError::NotARepository(_))
        ));
        assert!(matches!(
            workflow(clone_input("work"), &profiles(), &Origins("")).await,
            Err(StartError::Port(_))
        ));
    }
}
