//! Workflows: the multi-step jobs a single keypress sets off.
//!
//! Each one composes the same three moves in different combinations - cut a
//! worktree, open a space, start an agent - which is why they live together
//! rather than in the TUI. The order matters: nothing is created until the
//! prompt has rendered, and a workspace that will not open is a warning
//! rather than a dead end, because the developer would rather have the agent
//! working than have the whole thing refuse to start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;

use super::agents::AgentRegistry;
use super::ports::{PortError, PortResult, WorktreeManager};
use super::workspaces::WorkspaceDirectory;
use crate::domain::agent::TaskSpec;
use crate::domain::attention::{EventRef, PullRequestRef};
use crate::domain::feedback::FeedbackRequest;
use crate::domain::git::{Worktree, WorktreeSpec};
use crate::domain::ids::{AgentId, SessionId, SkillId, SpaceId};
use crate::domain::profile::Profile;
use crate::domain::skill::{Bindings, Skill, SkillLibrary};
use crate::domain::workspace::{SpaceName, TargetKind};

/// What a workflow produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowOutcome {
    /// The agent session that was started.
    pub session: SessionId,
    /// The engine running it.
    pub agent: AgentId,
    /// The space opened for the work, when one was.
    pub space: Option<SpaceId>,
    /// The worktree cut for the work, when one was.
    pub worktree: Option<Worktree>,
    /// Non-fatal problems: the work started, but not everything worked.
    pub warnings: Vec<PortError>,
}

/// The skills the built-in workflows render.
pub mod skills {
    /// Reviewing a pull request.
    pub const PR_REVIEW: &str = "pr-review";
    /// Implementing feedback about the active project.
    pub const ENHANCEMENT: &str = "enhancement";
    /// Preparing for a meeting.
    pub const MEETING_PREP: &str = "meeting-prep";
}

/// Composes worktrees, spaces, and agents into the PRD's workflows.
pub struct WorkflowEngine {
    worktrees: Arc<dyn WorktreeManager>,
    workspaces: WorkspaceDirectory,
}

impl WorkflowEngine {
    /// The directory worktrees go in when a profile does not say.
    pub const DEFAULT_WORKTREE_DIR: &'static str = ".vetinari-worktrees";

    /// Build a workflow engine.
    #[must_use]
    pub fn new(worktrees: Arc<dyn WorktreeManager>, workspaces: WorkspaceDirectory) -> Self {
        Self {
            worktrees,
            workspaces,
        }
    }

    /// The workspace targets available.
    #[must_use]
    pub const fn workspaces(&self) -> &WorkspaceDirectory {
        &self.workspaces
    }

    /// The dogfooding workflow: capture, isolate, open, spawn.
    pub async fn start_feedback(
        &self,
        request: &FeedbackRequest,
        library: &SkillLibrary,
        profile: &Profile,
        registry: &mut AgentRegistry,
    ) -> PortResult<WorkflowOutcome> {
        let skill = require_skill(library, skills::ENHANCEMENT)?;
        let spec = render(skill, &request.bindings(), request.target().repo())?;
        let worktree_root = self.worktree_root(profile, request.target().repo());
        let worktree_spec = request.worktree_spec(&worktree_root);
        self.run_isolated(
            spec,
            worktree_spec,
            request.space_name(),
            skill,
            profile,
            registry,
        )
        .await
    }

    /// The PR review workflow: worktree on the PR's branch, space, agent.
    pub async fn start_pr_review(
        &self,
        pr: &PullRequestRef,
        library: &SkillLibrary,
        profile: &Profile,
        registry: &mut AgentRegistry,
    ) -> PortResult<WorkflowOutcome> {
        let skill = require_skill(library, skills::PR_REVIEW)?;
        let repo = resolve_repo(profile, &pr.repo);
        let bindings = Bindings::new()
            .with("number", pr.number.to_string())
            .with("repo", pr.repo.clone())
            .with("head_ref", pr.head_ref.clone())
            .with("base_ref", pr.base_ref.clone());
        let spec = render(skill, &bindings, &repo)?;
        let worktree_root = self.worktree_root(profile, &repo);
        let worktree_spec =
            WorktreeSpec::for_pull_request(repo, worktree_root, pr.number, &pr.head_ref);
        self.run_isolated(
            spec,
            worktree_spec,
            SpaceName::for_pull_request(pr.number, &pr.head_ref),
            skill,
            profile,
            registry,
        )
        .await
    }

    /// Prepare for a meeting: no worktree, no space, just a briefing.
    pub async fn start_meeting_prep(
        &self,
        title: &str,
        event: &EventRef,
        starts_at: OffsetDateTime,
        library: &SkillLibrary,
        profile: &Profile,
        registry: &mut AgentRegistry,
    ) -> PortResult<WorkflowOutcome> {
        let bindings = Bindings::new()
            .with("title", title)
            .with("starts_at", starts_at.to_string())
            .with(
                "attendees",
                if event.attendees.is_empty() {
                    "nobody listed".to_owned()
                } else {
                    event.attendees.join(", ")
                },
            );
        self.start_quick_task(
            &SkillId::parse(skills::MEETING_PREP).expect("built-in skill id is valid"),
            bindings,
            library,
            profile,
            registry,
        )
        .await
    }

    /// Run a skill without isolating it, for summaries and briefings.
    pub async fn start_quick_task(
        &self,
        skill_id: &SkillId,
        bindings: Bindings,
        library: &SkillLibrary,
        profile: &Profile,
        registry: &mut AgentRegistry,
    ) -> PortResult<WorkflowOutcome> {
        let skill = library
            .get(skill_id)
            .ok_or_else(|| PortError::not_found("skill", skill_id.as_str()))?;
        let working_dir = profile
            .repo_roots()
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let spec = render(skill, &bindings, &working_dir)?;
        let agent = profile
            .routing()
            .resolve(spec.class(), skill.agent())
            .clone();
        let session = registry
            .spawn(spec, profile.routing(), skill.agent())
            .await?;
        Ok(WorkflowOutcome {
            session,
            agent,
            space: None,
            worktree: None,
            warnings: Vec::new(),
        })
    }

    /// Run an engine's command line inside a space, so the agent sits beside
    /// the developer's own shell rather than being managed by Vetinari.
    pub async fn delegate_to_space(
        &self,
        space: &SpaceId,
        agent: &AgentId,
        registry: &AgentRegistry,
        spec: &TaskSpec,
    ) -> PortResult<Vec<String>> {
        let command = registry.command_line(agent, spec)?;
        self.workspaces.run_in_space(space, &command).await?;
        Ok(command)
    }

    /// Close a space and remove the worktree behind it.
    pub async fn tear_down(&self, space: &SpaceId, worktree: Option<&Worktree>) -> Vec<PortError> {
        let mut problems = Vec::new();
        if let Err(error) = self.workspaces.close_space(space).await {
            problems.push(error);
        }
        if let Some(worktree) = worktree
            && let Err(error) = self.worktrees.remove(worktree).await
        {
            problems.push(error);
        }
        problems
    }

    async fn run_isolated(
        &self,
        spec: TaskSpec,
        worktree_spec: WorktreeSpec,
        space_name: SpaceName,
        skill: &Skill,
        profile: &Profile,
        registry: &mut AgentRegistry,
    ) -> PortResult<WorkflowOutcome> {
        let worktree = self.worktrees.create(&worktree_spec).await?;
        let spec = spec
            .in_directory(worktree.path().to_path_buf())
            .with_env("VETINARI_PROFILE", profile.id().as_str())
            .with_env("VETINARI_WORKTREE", worktree.path().display().to_string());

        let mut warnings = Vec::new();
        let space = match self
            .open_space(profile.target(), &space_name, worktree.path())
            .await
        {
            Ok(space) => Some(space),
            Err(error) => {
                warnings.push(error);
                None
            }
        };

        let agent = profile
            .routing()
            .resolve(spec.class(), skill.agent())
            .clone();
        let session = match registry.spawn(spec, profile.routing(), skill.agent()).await {
            Ok(session) => session,
            Err(error) => {
                if let Some(space) = &space {
                    let _ = self.workspaces.close_space(space).await;
                }
                return Err(error);
            }
        };
        if let Some(space) = &space {
            registry.attach_space(&session, space.clone());
        }

        Ok(WorkflowOutcome {
            session,
            agent,
            space,
            worktree: Some(worktree),
            warnings,
        })
    }

    async fn open_space(
        &self,
        kind: TargetKind,
        name: &SpaceName,
        path: &Path,
    ) -> PortResult<SpaceId> {
        self.workspaces.create_space(kind, name, path).await
    }

    fn worktree_root(&self, profile: &Profile, repo: &Path) -> PathBuf {
        if let Some(root) = profile.worktree_root() {
            return root.clone();
        }
        repo.parent()
            .unwrap_or(Path::new("."))
            .join(Self::DEFAULT_WORKTREE_DIR)
    }
}

fn require_skill<'library>(
    library: &'library SkillLibrary,
    id: &str,
) -> PortResult<&'library Skill> {
    let skill_id = SkillId::parse(id).map_err(|error| {
        PortError::not_configured("skill", format!("{id} is not a usable skill id: {error}"))
    })?;
    library
        .get(&skill_id)
        .ok_or_else(|| PortError::not_found("skill", id))
}

fn render(skill: &Skill, bindings: &Bindings, working_dir: &Path) -> PortResult<TaskSpec> {
    skill
        .to_task(bindings, working_dir.to_path_buf())
        .map_err(|error| {
            PortError::not_configured(
                skill.id().as_str(),
                format!("the skill could not be rendered: {error}"),
            )
        })
}

fn resolve_repo(profile: &Profile, repo_slug: &str) -> PathBuf {
    let name = repo_slug.rsplit('/').next().unwrap_or(repo_slug);
    match profile.repo_roots().first() {
        Some(root) => root.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agents::AgentRegistry;
    use crate::domain::attention::{EventRef, PullRequestRef};
    use crate::domain::feedback::{CapturedContext, FeedbackRequest, FeedbackTarget};
    use crate::domain::ids::{AgentId, ProfileId};
    use crate::domain::keymap::Mode;
    use crate::domain::skill::SkillLibrary;
    use crate::domain::text::NonEmptyText;
    use crate::testing::{FakeAgentEngine, FakeWorkspaceTarget, FakeWorktreeManager, FixedClock};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    struct Harness {
        workflows: WorkflowEngine,
        registry: AgentRegistry,
        worktrees: Arc<FakeWorktreeManager>,
        target: Arc<FakeWorkspaceTarget>,
        claude: Arc<FakeAgentEngine>,
        gemini: Arc<FakeAgentEngine>,
        library: SkillLibrary,
        profile: Profile,
    }

    fn harness() -> Harness {
        let clock = Arc::new(FixedClock::new(NOW));
        let worktrees = Arc::new(FakeWorktreeManager::default());
        let target = Arc::new(FakeWorkspaceTarget::new(TargetKind::Herdr));
        let mut workspaces = WorkspaceDirectory::default();
        workspaces.register(target.clone());

        let claude = Arc::new(FakeAgentEngine::new(
            AgentId::parse("claude-code").expect("id"),
        ));
        let gemini = Arc::new(FakeAgentEngine::new(
            AgentId::parse("agy-gemini").expect("id"),
        ));
        let script = Arc::new(FakeAgentEngine::new(AgentId::parse("script").expect("id")));
        let mut registry = AgentRegistry::new(clock.clone());
        registry.register(claude.clone());
        registry.register(gemini.clone());
        registry.register(script);

        let profile = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Herdr,
        )
        .with_worktree_root(PathBuf::from("/work/trees"));

        Harness {
            workflows: WorkflowEngine::new(worktrees.clone(), workspaces),
            registry,
            worktrees,
            target,
            claude,
            gemini,
            library: SkillLibrary::builtin(),
            profile,
        }
    }

    fn feedback_request() -> FeedbackRequest {
        FeedbackRequest::new(
            NonEmptyText::parse("Show group labels in the which-key popup").expect("text"),
            FeedbackTarget::new(
                NonEmptyText::parse("vetinari").expect("name"),
                PathBuf::from("/work/repos/vetinari"),
            ),
            CapturedContext::new(ProfileId::parse("work").expect("id"), Mode::Normal)
                .with_selected_item("[PR] #142 Fix memory leak (CI failed)"),
            NOW,
        )
    }

    fn pull_request() -> PullRequestRef {
        PullRequestRef {
            repo: "acme/web".to_owned(),
            number: 142,
            head_ref: "fix/memory-leak".to_owned(),
            base_ref: "main".to_owned(),
            clone_url: "https://github.test/acme/web.git".to_owned(),
        }
    }

    #[tokio::test]
    async fn feedback_cuts_a_worktree_opens_a_space_and_spawns_an_agent() {
        let mut h = harness();
        let outcome = h
            .workflows
            .start_feedback(&feedback_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");

        let created = h.worktrees.created();
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].branch().as_str(),
            "vetinari/feedback/show-group-labels-in-the-which-key-popup"
        );
        assert_eq!(created[0].repo(), Path::new("/work/repos/vetinari"));

        let spaces = h.target.spaces();
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].path(), created[0].path());
        assert_eq!(outcome.space.as_ref(), Some(spaces[0].id()));

        let spawned = h.claude.spawned();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].working_dir(), created[0].path());
        assert!(
            spawned[0]
                .prompt()
                .as_str()
                .contains("Show group labels in the which-key popup")
        );
        assert!(spawned[0].prompt().as_str().contains("Profile: work"));
        assert!(
            spawned[0]
                .prompt()
                .as_str()
                .contains("[PR] #142 Fix memory leak (CI failed)"),
            "the captured context travels with the request"
        );

        let session = h.registry.session(&outcome.session).expect("tracked");
        assert_eq!(session.space(), outcome.space.as_ref());
        assert!(outcome.warnings.is_empty());
        assert_eq!(
            outcome.worktree.expect("worktree").path(),
            created[0].path()
        );
    }

    #[tokio::test]
    async fn feedback_without_a_worktree_does_not_open_a_space_or_an_agent() {
        let mut h = harness();
        h.worktrees
            .fail_create(PortError::unavailable("git", "not a repository"));

        let error = h
            .workflows
            .start_feedback(&feedback_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect_err("no worktree");

        assert!(error.is_transient());
        assert!(h.target.spaces().is_empty());
        assert!(h.claude.spawned().is_empty());
        assert!(h.registry.sessions().is_empty());
    }

    #[tokio::test]
    async fn a_workspace_that_will_not_open_is_a_warning_not_a_dead_end() {
        let mut h = harness();
        h.target
            .fail_create(PortError::unavailable("herdr", "socket missing"));

        let outcome = h
            .workflows
            .start_feedback(&feedback_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow still runs");

        assert!(outcome.space.is_none());
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(h.claude.spawned().len(), 1, "the work still starts");
    }

    #[tokio::test]
    async fn a_pr_review_worktree_is_based_on_the_pull_requests_own_branch() {
        let mut h = harness();
        let outcome = h
            .workflows
            .start_pr_review(&pull_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");

        let created = h.worktrees.created();
        assert_eq!(created[0].branch().as_str(), "vetinari/review/pr-142");
        assert_eq!(created[0].base(), Some("fix/memory-leak"));
        assert_eq!(
            created[0].path(),
            Path::new("/work/trees/web-pr-142-fix-memory-leak")
        );

        let prompt = h.claude.spawned()[0].prompt().as_str().to_owned();
        assert!(prompt.contains("142"));
        assert!(prompt.contains("acme/web"));
        assert!(prompt.contains("fix/memory-leak"));
        assert!(prompt.contains("main"));
        assert_eq!(outcome.agent.as_str(), "claude-code");
    }

    #[tokio::test]
    async fn a_pr_review_space_is_named_after_the_review() {
        let mut h = harness();
        h.workflows
            .start_pr_review(&pull_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");
        assert_eq!(
            h.target.spaces()[0].name().as_str(),
            "pr-142-fix-memory-leak"
        );
    }

    #[tokio::test]
    async fn a_pr_review_falls_back_to_the_repo_root_when_the_clone_is_local() {
        let mut h = harness();
        h.profile = h
            .profile
            .clone()
            .with_repo_root(PathBuf::from("/work/repos"));
        h.workflows
            .start_pr_review(&pull_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");
        assert_eq!(
            h.worktrees.created()[0].repo(),
            Path::new("/work/repos/web")
        );
    }

    #[tokio::test]
    async fn a_quick_task_runs_without_a_worktree_or_a_space() {
        let mut h = harness();
        let outcome = h
            .workflows
            .start_quick_task(
                &SkillId::parse("standup-summary").expect("id"),
                Bindings::new().with("issues", "DEV-891, DEV-892"),
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect("workflow");

        assert!(h.worktrees.created().is_empty());
        assert!(h.target.spaces().is_empty());
        assert!(outcome.space.is_none());
        assert!(outcome.worktree.is_none());
        assert_eq!(outcome.agent.as_str(), "script");
    }

    #[tokio::test]
    async fn a_quick_task_goes_to_the_engine_its_class_routes_to() {
        let mut h = harness();
        let outcome = h
            .workflows
            .start_quick_task(
                &SkillId::parse("meeting-prep").expect("id"),
                Bindings::new()
                    .with("title", "Sprint planning")
                    .with("starts_at", "09:15")
                    .with("attendees", "@sarah"),
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect("workflow");

        assert_eq!(outcome.agent.as_str(), "agy-gemini");
        assert_eq!(h.gemini.spawned().len(), 1);
        assert!(
            h.gemini.spawned()[0]
                .prompt()
                .as_str()
                .contains("Sprint planning")
        );
    }

    #[tokio::test]
    async fn meeting_prep_briefs_the_agent_on_the_event() {
        let mut h = harness();
        let event = EventRef {
            ends_at: None,
            attendees: vec!["@sarah".to_owned(), "@vimes".to_owned()],
            location: None,
        };

        h.workflows
            .start_meeting_prep(
                "Sprint planning",
                &event,
                NOW,
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect("workflow");

        let prompt = h.gemini.spawned()[0].prompt().as_str().to_owned();
        assert!(prompt.contains("Sprint planning"));
        assert!(prompt.contains("@sarah, @vimes"));
    }

    #[tokio::test]
    async fn meeting_prep_without_attendees_still_renders() {
        let mut h = harness();
        let event = EventRef {
            ends_at: None,
            attendees: Vec::new(),
            location: None,
        };

        h.workflows
            .start_meeting_prep(
                "Focus block",
                &event,
                NOW,
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect("workflow");

        assert!(
            h.gemini.spawned()[0]
                .prompt()
                .as_str()
                .contains("nobody listed")
        );
    }

    #[tokio::test]
    async fn a_workflow_for_a_skill_that_does_not_exist_says_so() {
        let mut h = harness();
        let error = h
            .workflows
            .start_quick_task(
                &SkillId::parse("no-such-skill").expect("id"),
                Bindings::new(),
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect_err("missing skill");
        assert!(matches!(error, PortError::NotFound { .. }));
    }

    #[tokio::test]
    async fn a_skill_missing_a_binding_fails_before_anything_is_created() {
        let mut h = harness();
        let error = h
            .workflows
            .start_quick_task(
                &SkillId::parse("meeting-prep").expect("id"),
                Bindings::new(),
                &h.library,
                &h.profile,
                &mut h.registry,
            )
            .await
            .expect_err("missing binding");

        assert!(matches!(error, PortError::NotConfigured { .. }));
        assert!(h.registry.sessions().is_empty());
    }

    #[tokio::test]
    async fn without_a_configured_worktree_root_one_is_derived_beside_the_repo() {
        let mut h = harness();
        h.profile = Profile::new(
            ProfileId::parse("home").expect("id"),
            NonEmptyText::parse("Home").expect("name"),
            TargetKind::Herdr,
        );

        h.workflows
            .start_feedback(&feedback_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");

        assert_eq!(
            h.worktrees.created()[0].path().parent(),
            Some(Path::new("/work/repos/.vetinari-worktrees"))
        );
    }

    #[tokio::test]
    async fn an_agent_can_be_delegated_to_a_space_instead_of_being_managed() {
        let mut h = harness();
        let outcome = h
            .workflows
            .start_feedback(&feedback_request(), &h.library, &h.profile, &mut h.registry)
            .await
            .expect("workflow");
        let space = outcome.space.expect("space");

        let spec = h
            .registry
            .spec_for(&outcome.session)
            .expect("the spec is kept")
            .clone();
        let command = h
            .workflows
            .delegate_to_space(&space, &outcome.agent, &h.registry, &spec)
            .await
            .expect("delegate");

        assert_eq!(h.target.commands().len(), 1);
        assert_eq!(h.target.commands()[0].0, space.as_str());
        assert_eq!(command, h.target.commands()[0].1);
    }
}
