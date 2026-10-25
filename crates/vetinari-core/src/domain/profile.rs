//! Context profiles: the work/home switch from the PRD.
//!
//! A profile decides which sources are polled, where worktrees are cut, which
//! workspace target is used, and which engine handles which work. The set is
//! non-empty by construction, so there is always an active profile to ask.

use std::path::PathBuf;

use super::agent::TaskClass;
use super::ids::{ProfileId, Slug, SourceId};
use super::routing::RoutingTable;
use super::text::NonEmptyText;
use super::workspace::TargetKind;

/// One context profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    id: ProfileId,
    name: NonEmptyText,
    target: TargetKind,
    sources: Vec<SourceId>,
    repo_roots: Vec<PathBuf>,
    worktree_root: Option<PathBuf>,
    routing: RoutingTable,
    account: Option<Slug>,
}

impl Profile {
    /// Define a profile with default routing and every source enabled.
    #[must_use]
    pub fn new(id: ProfileId, name: NonEmptyText, target: TargetKind) -> Self {
        Self {
            id,
            name,
            target,
            sources: Vec::new(),
            repo_roots: Vec::new(),
            worktree_root: None,
            routing: RoutingTable::defaults(),
            account: None,
        }
    }

    /// Enable a source. Listing none leaves every source enabled.
    #[must_use]
    pub fn with_source(mut self, source: SourceId) -> Self {
        self.sources.push(source);
        self
    }

    /// Add a directory to search for repositories.
    #[must_use]
    pub fn with_repo_root(mut self, root: PathBuf) -> Self {
        self.repo_roots.push(root);
        self
    }

    /// Set where worktrees are created.
    #[must_use]
    pub fn with_worktree_root(mut self, root: PathBuf) -> Self {
        self.worktree_root = Some(root);
        self
    }

    /// Use a specific routing table.
    #[must_use]
    pub fn with_routing(mut self, routing: RoutingTable) -> Self {
        self.routing = routing;
        self
    }

    /// Scope credentials to a named account, so work and home can hold
    /// separate tokens for one provider.
    #[must_use]
    pub fn with_account(mut self, account: Slug) -> Self {
        self.account = Some(account);
        self
    }

    /// The profile id.
    #[must_use]
    pub const fn id(&self) -> &ProfileId {
        &self.id
    }

    /// Its display name.
    #[must_use]
    pub const fn name(&self) -> &NonEmptyText {
        &self.name
    }

    /// The workspace target it opens spaces in.
    #[must_use]
    pub const fn target(&self) -> TargetKind {
        self.target
    }

    /// The sources it enables; empty means all of them.
    #[must_use]
    pub fn sources(&self) -> &[SourceId] {
        &self.sources
    }

    /// Where its repositories live.
    #[must_use]
    pub fn repo_roots(&self) -> &[PathBuf] {
        &self.repo_roots
    }

    /// Where the clone of `owner/name` is: `name` under the first
    /// repository root, or the working directory without one.
    #[must_use]
    pub fn repo_path(&self, slug: &str) -> PathBuf {
        let name = slug.rsplit('/').next().unwrap_or(slug);
        match self.repo_roots.first() {
            Some(root) => root.join(name),
            None => PathBuf::from(name),
        }
    }

    /// Where its worktrees are created.
    #[must_use]
    pub fn worktree_root(&self) -> Option<&PathBuf> {
        self.worktree_root.as_ref()
    }

    /// Its routing table.
    #[must_use]
    pub const fn routing(&self) -> &RoutingTable {
        &self.routing
    }

    /// The credential account it uses.
    #[must_use]
    pub const fn account(&self) -> Option<&Slug> {
        self.account.as_ref()
    }

    /// Whether this profile wants signals from a source.
    #[must_use]
    pub fn wants_source(&self, source: &SourceId) -> bool {
        self.sources.is_empty() || self.sources.iter().any(|enabled| enabled == source)
    }

    /// The engine for a class of work under this profile.
    #[must_use]
    pub fn agent_for(&self, class: TaskClass) -> &super::ids::AgentId {
        self.routing.agent_for(class)
    }
}

/// Two profiles shared an id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileSetError {
    /// A profile id was used twice.
    #[error("profile {id} is defined twice")]
    Duplicate {
        /// The repeated id.
        id: ProfileId,
    },
}

/// Every configured profile, with exactly one of them active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSet {
    profiles: Vec<Profile>,
    active: usize,
}

impl ProfileSet {
    /// Build a set from at least one profile.
    pub fn new(first: Profile, rest: Vec<Profile>) -> Result<Self, ProfileSetError> {
        let mut profiles = Vec::with_capacity(rest.len() + 1);
        profiles.push(first);
        profiles.extend(rest);
        for index in 1..profiles.len() {
            if profiles[..index]
                .iter()
                .any(|earlier| earlier.id() == profiles[index].id())
            {
                return Err(ProfileSetError::Duplicate {
                    id: profiles[index].id().clone(),
                });
            }
        }
        Ok(Self {
            profiles,
            active: 0,
        })
    }

    /// The active profile.
    #[must_use]
    pub fn active(&self) -> &Profile {
        &self.profiles[self.active]
    }

    /// Move to the next profile, wrapping, and return it.
    pub fn cycle(&mut self) -> &Profile {
        self.active = (self.active + 1) % self.profiles.len();
        self.active()
    }

    /// Make a named profile active. Returns whether it existed.
    pub fn activate(&mut self, id: &ProfileId) -> bool {
        match self.profiles.iter().position(|profile| profile.id() == id) {
            Some(index) => {
                self.active = index;
                true
            }
            None => false,
        }
    }

    /// Look a profile up.
    #[must_use]
    pub fn get(&self, id: &ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id() == id)
    }

    /// How many profiles there are; always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Always false; a set holds at least one profile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Every profile, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.iter()
    }

    /// The work and home profiles from the PRD.
    ///
    /// # Panics
    ///
    /// Panics if the built-in profile ids are invalid, which would be a bug
    /// here rather than in configuration.
    #[must_use]
    pub fn defaults() -> Self {
        let work = Profile::new(
            ProfileId::parse("work").expect("built-in profile id is valid"),
            NonEmptyText::parse("Work").expect("built-in profile name is non-empty"),
            TargetKind::Herdr,
        );
        let home = Profile::new(
            ProfileId::parse("home").expect("built-in profile id is valid"),
            NonEmptyText::parse("Home").expect("built-in profile name is non-empty"),
            TargetKind::Tmux,
        );
        Self::new(work, vec![home]).expect("built-in profiles are distinct")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str) -> Profile {
        Profile::new(
            ProfileId::parse(id).expect("id"),
            NonEmptyText::parse(id).expect("name"),
            TargetKind::Tmux,
        )
    }

    #[test]
    fn a_repository_is_found_by_name_under_the_first_root() {
        let rooted = profile("work")
            .with_repo_root(PathBuf::from("/work/repos"))
            .with_repo_root(PathBuf::from("/other"));
        assert_eq!(
            rooted.repo_path("acme/web"),
            PathBuf::from("/work/repos/web")
        );
        assert_eq!(profile("bare").repo_path("acme/web"), PathBuf::from("web"));
    }

    #[test]
    fn a_profile_carries_its_sources_roots_and_routing() {
        let built = profile("work")
            .with_source(SourceId::parse("github").expect("id"))
            .with_source(SourceId::parse("jira").expect("id"))
            .with_repo_root(PathBuf::from("/work/repos"))
            .with_account(Slug::parse("work").expect("slug"));
        assert_eq!(built.sources().len(), 2);
        assert_eq!(built.repo_roots(), &[PathBuf::from("/work/repos")]);
        assert_eq!(built.account().map(Slug::as_str), Some("work"));
        assert_eq!(built.target(), TargetKind::Tmux);
        assert_eq!(
            built.routing().agent_for(TaskClass::PrSummary).as_str(),
            "agy-gemini"
        );
    }

    #[test]
    fn a_profile_can_say_whether_it_wants_a_source() {
        let built = profile("home").with_source(SourceId::parse("github").expect("id"));
        assert!(built.wants_source(&SourceId::parse("github").expect("id")));
        assert!(!built.wants_source(&SourceId::parse("slack").expect("id")));
    }

    #[test]
    fn a_profile_with_no_sources_listed_wants_them_all() {
        let built = profile("home");
        assert!(built.wants_source(&SourceId::parse("anything").expect("id")));
    }

    #[test]
    fn a_profile_set_always_has_an_active_profile() {
        let set = ProfileSet::new(profile("work"), vec![]).expect("set");
        assert_eq!(set.active().id().as_str(), "work");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn cycling_moves_to_the_next_profile_and_wraps() {
        let mut set = ProfileSet::new(profile("work"), vec![profile("home")]).expect("set");
        assert_eq!(set.cycle().id().as_str(), "home");
        assert_eq!(set.cycle().id().as_str(), "work");
    }

    #[test]
    fn a_profile_can_be_activated_by_id() {
        let mut set = ProfileSet::new(profile("work"), vec![profile("home")]).expect("set");
        assert!(set.activate(&ProfileId::parse("home").expect("id")));
        assert_eq!(set.active().id().as_str(), "home");
        assert!(!set.activate(&ProfileId::parse("mars").expect("id")));
        assert_eq!(set.active().id().as_str(), "home");
    }

    #[test]
    fn duplicate_profile_ids_are_rejected() {
        let err = ProfileSet::new(profile("work"), vec![profile("work")]).expect_err("duplicate");
        assert_eq!(
            err,
            ProfileSetError::Duplicate {
                id: ProfileId::parse("work").expect("id")
            }
        );
    }

    #[test]
    fn a_profile_set_lists_its_profiles_in_declaration_order() {
        let set =
            ProfileSet::new(profile("work"), vec![profile("home"), profile("oss")]).expect("set");
        let ids: Vec<_> = set.iter().map(|p| p.id().as_str().to_owned()).collect();
        assert_eq!(ids, vec!["work", "home", "oss"]);
    }

    #[test]
    fn the_default_profile_set_offers_work_and_home() {
        let set = ProfileSet::defaults();
        assert_eq!(set.active().id().as_str(), "work");
        let ids: Vec<_> = set.iter().map(|p| p.id().as_str().to_owned()).collect();
        assert_eq!(ids, vec!["work", "home"]);
    }
}
