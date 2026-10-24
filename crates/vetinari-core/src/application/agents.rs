//! The agent registry: which engines exist, what they are running, and how
//! that work shows up in the attention feed.
//!
//! Routing happens here rather than in the TUI so that a keybinding, a
//! workflow, and a scheduled job all place work the same way. The spec of
//! every task is kept alongside its session, which is what makes re-running a
//! failed task possible without asking the developer to retype anything.

use std::collections::BTreeMap;
use std::sync::Arc;

use time::OffsetDateTime;

use super::ports::{AgentEngine, Clock, PortError, PortResult};
use crate::domain::agent::{AgentSession, AgentStatus, TaskSpec};
use crate::domain::attention::{AgentRef, AttentionItem, ItemContext, SourceCategory};
use crate::domain::ids::{AgentId, ItemId, SessionId, SourceId, SpaceId};
use crate::domain::routing::RoutingTable;
use crate::domain::text::NonEmptyText;

#[derive(Debug, Clone)]
struct Tracked {
    session: AgentSession,
    spec: TaskSpec,
}

/// Engines, their sessions, and the specs behind them.
pub struct AgentRegistry {
    engines: BTreeMap<AgentId, Arc<dyn AgentEngine>>,
    tracked: Vec<Tracked>,
    clock: Arc<dyn Clock>,
    source_id: SourceId,
}

impl AgentRegistry {
    /// The source id agent sessions appear under in the feed.
    pub const SOURCE_NAME: &'static str = "agents";

    /// Build a registry with no engines.
    ///
    /// # Panics
    ///
    /// Panics if the built-in source name is not a valid slug, which would be
    /// a bug here.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            engines: BTreeMap::new(),
            tracked: Vec::new(),
            clock,
            source_id: SourceId::parse(Self::SOURCE_NAME).expect("built-in source name is valid"),
        }
    }

    /// Add an engine.
    pub fn register(&mut self, engine: Arc<dyn AgentEngine>) {
        self.engines.insert(engine.id().clone(), engine);
    }

    /// Every registered engine id.
    #[must_use]
    pub fn engine_ids(&self) -> Vec<AgentId> {
        self.engines.keys().cloned().collect()
    }

    /// Look an engine up.
    #[must_use]
    pub fn engine(&self, id: &AgentId) -> Option<&Arc<dyn AgentEngine>> {
        self.engines.get(id)
    }

    /// The command line an engine would run for a task.
    pub fn command_line(&self, agent: &AgentId, spec: &TaskSpec) -> PortResult<Vec<String>> {
        self.require_engine(agent)?.command_line(spec)
    }

    /// Start a task on the engine its class routes to, unless a skill pins
    /// one.
    pub async fn spawn(
        &mut self,
        spec: TaskSpec,
        routing: &RoutingTable,
        pinned: Option<&AgentId>,
    ) -> PortResult<SessionId> {
        let agent = routing.resolve(spec.class(), pinned).clone();
        let engine = self.require_engine(&agent)?.clone();
        if !engine.is_available().await {
            return Err(PortError::not_configured(
                agent.as_str(),
                "the engine is not available on this machine",
            ));
        }
        let session = engine.spawn_task(&spec).await?;
        let id = session.id().clone();
        self.tracked.push(Tracked { session, spec });
        Ok(id)
    }

    /// Ask every engine how its unfinished sessions are getting on.
    ///
    /// Returns the sessions that could not be checked, rather than failing
    /// the whole sweep.
    pub async fn poll(&mut self) -> Vec<(SessionId, PortError)> {
        let pending: Vec<(SessionId, AgentId)> = self
            .tracked
            .iter()
            .filter(|tracked| !tracked.session.status().is_terminal())
            .map(|tracked| {
                (
                    tracked.session.id().clone(),
                    tracked.session.agent().clone(),
                )
            })
            .collect();

        let mut problems = Vec::new();
        for (session_id, agent_id) in pending {
            let Some(engine) = self.engines.get(&agent_id).cloned() else {
                problems.push((
                    session_id,
                    PortError::not_configured(agent_id.as_str(), "the engine is not registered"),
                ));
                continue;
            };
            match engine.check_status(&session_id).await {
                Ok(status) => self.apply_status(&session_id, status),
                Err(error) => problems.push((session_id, error)),
            }
        }
        problems
    }

    /// Every tracked session, newest first.
    #[must_use]
    pub fn sessions(&self) -> Vec<&AgentSession> {
        self.tracked
            .iter()
            .rev()
            .map(|tracked| &tracked.session)
            .collect()
    }

    /// Look a session up.
    #[must_use]
    pub fn session(&self, id: &SessionId) -> Option<&AgentSession> {
        self.tracked
            .iter()
            .find(|tracked| tracked.session.id() == id)
            .map(|tracked| &tracked.session)
    }

    /// Stop a task.
    pub async fn cancel(&mut self, id: &SessionId) -> PortResult<()> {
        let engine = self.engine_for(id)?;
        engine.cancel(id).await?;
        self.apply_status(id, AgentStatus::Cancelled);
        Ok(())
    }

    /// Answer the question a task is blocked on.
    pub async fn answer(&mut self, id: &SessionId, answer: &str) -> PortResult<()> {
        let engine = self.engine_for(id)?;
        engine.answer(id, answer).await?;
        self.apply_status(id, AgentStatus::Running);
        Ok(())
    }

    /// The tail of a task's output.
    pub async fn logs(&self, id: &SessionId, max_lines: usize) -> PortResult<Vec<String>> {
        self.engine_for(id)?.output(id, max_lines).await
    }

    /// Run a task again from its original spec.
    pub async fn rerun(&mut self, id: &SessionId) -> PortResult<SessionId> {
        let tracked = self
            .tracked
            .iter()
            .find(|tracked| tracked.session.id() == id)
            .ok_or_else(|| PortError::not_found("session", id.as_str()))?;
        let spec = tracked.spec.clone();
        let agent = tracked.session.agent().clone();
        let engine = self.require_engine(&agent)?.clone();
        let session = engine.spawn_task(&spec).await?;
        let new_id = session.id().clone();
        self.tracked.push(Tracked { session, spec });
        Ok(new_id)
    }

    /// Tie a session to the space it runs in.
    pub fn attach_space(&mut self, id: &SessionId, space: SpaceId) {
        if let Some(tracked) = self
            .tracked
            .iter_mut()
            .find(|tracked| tracked.session.id() == id)
        {
            tracked.session.attach_space(space);
        }
    }

    /// The spec a session was started from.
    #[must_use]
    pub fn spec_for(&self, id: &SessionId) -> Option<&TaskSpec> {
        self.tracked
            .iter()
            .find(|tracked| tracked.session.id() == id)
            .map(|tracked| &tracked.spec)
    }

    /// Agent work, rendered as feed items.
    #[must_use]
    pub fn attention_items(&self) -> Vec<AttentionItem> {
        let now = self.clock.now();
        self.tracked
            .iter()
            .map(|tracked| self.as_item(tracked, now))
            .collect()
    }

    /// The item a session reports as.
    #[must_use]
    pub fn item_id(session: &SessionId) -> ItemId {
        ItemId::from_parts(
            &SourceId::parse(Self::SOURCE_NAME).expect("built-in source name is valid"),
            session.as_str(),
        )
    }

    /// The feed source id used for agent items.
    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Forget finished sessions, so the monitor does not grow without bound.
    pub fn prune_finished(&mut self, keep: usize) {
        let finished: Vec<usize> = self
            .tracked
            .iter()
            .enumerate()
            .filter(|(_, tracked)| tracked.session.status().is_terminal())
            .map(|(index, _)| index)
            .collect();
        if finished.len() <= keep {
            return;
        }
        let drop_count = finished.len() - keep;
        let to_drop: Vec<usize> = finished.into_iter().take(drop_count).collect();
        let mut index = 0;
        self.tracked.retain(|_| {
            let keep_this = !to_drop.contains(&index);
            index += 1;
            keep_this
        });
    }

    fn as_item(&self, tracked: &Tracked, now: OffsetDateTime) -> AttentionItem {
        let session = &tracked.session;
        let title = NonEmptyText::parse(&format!("{}: {}", session.agent(), session.summary()))
            .unwrap_or_else(|_| {
                NonEmptyText::parse("agent task").expect("the fallback title is non-empty")
            });

        let elapsed = session.elapsed(now);
        let mut builder = AttentionItem::builder(
            &self.source_id,
            session.id().as_str(),
            SourceCategory::AgentState,
            title,
        )
        .state(session.status().as_item_state())
        .occurred_at(session.started_at())
        .author(session.agent().as_str())
        .detail("Class", session.class().name())
        .detail(
            "Working directory",
            session.working_dir().display().to_string(),
        )
        .detail("Status", session.status().label())
        .detail(
            "Elapsed",
            format!(
                "{}m {}s",
                elapsed.whole_minutes(),
                elapsed.whole_seconds() % 60
            ),
        )
        .context(ItemContext::Agent(AgentRef {
            session: session.id().clone(),
            space: session.space().cloned(),
        }));

        if let Some(detail) = session.status().detail() {
            let label = match session.status() {
                AgentStatus::WaitingForInput { .. } => "Question",
                _ => "Error",
            };
            builder = builder.detail(label, detail);
        }
        if let Some(space) = session.space() {
            builder = builder.detail("Space", space.as_str());
        }
        builder.build()
    }

    fn apply_status(&mut self, id: &SessionId, status: AgentStatus) {
        let now = self.clock.now();
        if let Some(tracked) = self
            .tracked
            .iter_mut()
            .find(|tracked| tracked.session.id() == id)
        {
            if status.is_terminal() {
                tracked.session.finish(status, now);
            } else {
                tracked.session.set_status(status);
            }
        }
    }

    fn require_engine(&self, agent: &AgentId) -> PortResult<&Arc<dyn AgentEngine>> {
        self.engines.get(agent).ok_or_else(|| {
            PortError::not_configured(
                agent.as_str(),
                "no engine with that id is registered; check routing and agent configuration",
            )
        })
    }

    fn engine_for(&self, id: &SessionId) -> PortResult<Arc<dyn AgentEngine>> {
        let tracked = self
            .tracked
            .iter()
            .find(|tracked| tracked.session.id() == id)
            .ok_or_else(|| PortError::not_found("session", id.as_str()))?;
        self.require_engine(tracked.session.agent()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::agent::TaskClass;
    use crate::domain::attention::{ItemContext, ItemState};
    use crate::domain::routing::RoutingTable;
    use crate::domain::skill::SkillLibrary;
    use crate::domain::text::NonEmptyText;
    use crate::testing::{FakeAgentEngine, FixedClock};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn agent_id(name: &str) -> AgentId {
        AgentId::parse(name).expect("id")
    }

    fn spec(class: TaskClass) -> TaskSpec {
        TaskSpec::new(
            NonEmptyText::parse("do the thing").expect("prompt"),
            class,
            PathBuf::from("/work/repo"),
        )
    }

    fn registry() -> (AgentRegistry, Arc<FakeAgentEngine>, Arc<FakeAgentEngine>) {
        let claude = Arc::new(FakeAgentEngine::new(agent_id("claude-code")));
        let gemini = Arc::new(FakeAgentEngine::new(agent_id("agy-gemini")));
        let mut registry = AgentRegistry::new(Arc::new(FixedClock::new(NOW)));
        registry.register(claude.clone());
        registry.register(gemini.clone());
        (registry, claude, gemini)
    }

    #[tokio::test]
    async fn a_task_goes_to_the_engine_its_class_routes_to() {
        let (mut registry, claude, gemini) = registry();
        let routing = RoutingTable::defaults();

        registry
            .spawn(spec(TaskClass::PrSummary), &routing, None)
            .await
            .expect("spawn");

        assert_eq!(gemini.spawned().len(), 1);
        assert!(claude.spawned().is_empty());
    }

    #[tokio::test]
    async fn a_skill_that_pins_an_engine_wins_over_the_routing_table() {
        let (mut registry, claude, gemini) = registry();
        let routing = RoutingTable::defaults();

        registry
            .spawn(
                spec(TaskClass::PrSummary),
                &routing,
                Some(&agent_id("claude-code")),
            )
            .await
            .expect("spawn");

        assert_eq!(claude.spawned().len(), 1);
        assert!(gemini.spawned().is_empty());
    }

    #[tokio::test]
    async fn routing_to_an_unregistered_engine_says_so_rather_than_guessing() {
        let (mut registry, _, _) = registry();
        let routing = RoutingTable::new(agent_id("script"));

        let error = registry
            .spawn(spec(TaskClass::IssueSummary), &routing, None)
            .await
            .expect_err("no such engine");

        assert!(matches!(error, PortError::NotConfigured { .. }));
        assert!(registry.sessions().is_empty());
    }

    #[tokio::test]
    async fn a_failed_spawn_is_not_tracked_as_a_session() {
        let (mut registry, claude, _) = registry();
        claude.fail_spawn(PortError::unavailable("claude", "cli missing"));

        let error = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect_err("spawn fails");

        assert!(error.is_transient());
        assert!(registry.sessions().is_empty());
    }

    #[tokio::test]
    async fn a_spawned_session_is_tracked_with_its_engine_and_class() {
        let (mut registry, _, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");

        let session = registry.session(&id).expect("tracked");
        assert_eq!(session.agent().as_str(), "claude-code");
        assert_eq!(session.class(), TaskClass::DeepCodeChange);
        assert_eq!(registry.sessions().len(), 1);
    }

    #[tokio::test]
    async fn polling_brings_statuses_up_to_date_and_stops_the_clock() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");

        claude.set_status(
            &id,
            AgentStatus::WaitingForInput {
                question: "which branch?".to_owned(),
            },
        );
        assert!(registry.poll().await.is_empty());
        assert_eq!(
            registry.session(&id).expect("tracked").status().label(),
            "waiting"
        );
        assert!(
            registry
                .session(&id)
                .expect("tracked")
                .finished_at()
                .is_none()
        );

        claude.set_status(&id, AgentStatus::Succeeded);
        assert!(registry.poll().await.is_empty());
        assert_eq!(
            registry.session(&id).expect("tracked").finished_at(),
            Some(NOW)
        );
    }

    #[tokio::test]
    async fn polling_a_finished_session_does_not_ask_the_engine_again() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");
        claude.set_status(&id, AgentStatus::Succeeded);
        registry.poll().await;

        claude.set_status(&id, AgentStatus::Running);
        registry.poll().await;

        assert!(
            registry
                .session(&id)
                .expect("tracked")
                .status()
                .is_terminal(),
            "a finished session stays finished"
        );
    }

    #[tokio::test]
    async fn polling_reports_sessions_the_engine_has_lost_track_of() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");
        claude.forget_status(&id);

        let problems = registry.poll().await;

        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, id);
        assert!(matches!(problems[0].1, PortError::NotFound { .. }));
    }

    #[tokio::test]
    async fn cancelling_tells_the_engine_and_records_the_outcome() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");

        registry.cancel(&id).await.expect("cancel");

        assert_eq!(claude.cancelled(), vec![id.as_str().to_owned()]);
        assert_eq!(
            registry.session(&id).expect("tracked").status(),
            &AgentStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn answering_a_blocked_agent_sends_the_answer_through() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");
        claude.set_status(
            &id,
            AgentStatus::WaitingForInput {
                question: "which branch?".to_owned(),
            },
        );
        registry.poll().await;

        registry.answer(&id, "main").await.expect("answer");

        assert_eq!(
            claude.answers(),
            vec![(id.as_str().to_owned(), "main".to_owned())]
        );
        assert_eq!(
            registry.session(&id).expect("tracked").status(),
            &AgentStatus::Running
        );
    }

    #[tokio::test]
    async fn re_running_a_task_starts_a_fresh_session_from_the_same_spec() {
        let (mut registry, claude, _) = registry();
        let first = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");

        let second = registry.rerun(&first).await.expect("rerun");

        assert_ne!(first, second);
        assert_eq!(claude.spawned().len(), 2);
        assert_eq!(claude.spawned()[1].prompt().as_str(), "do the thing");
        assert_eq!(registry.sessions().len(), 2);
    }

    #[tokio::test]
    async fn re_running_an_unknown_session_is_an_error() {
        let (mut registry, _, _) = registry();
        let error = registry
            .rerun(&SessionId::parse("ghost").expect("id"))
            .await
            .expect_err("unknown session");
        assert!(matches!(error, PortError::NotFound { .. }));
    }

    #[tokio::test]
    async fn logs_come_from_the_engine_that_ran_the_task() {
        let (mut registry, _, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");
        assert_eq!(
            registry.logs(&id, 10).await.expect("logs"),
            vec!["fake output".to_owned()]
        );
    }

    #[tokio::test]
    async fn sessions_appear_in_the_attention_feed_with_their_state() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect("spawn");
        claude.set_status(
            &id,
            AgentStatus::WaitingForInput {
                question: "which branch?".to_owned(),
            },
        );
        registry.poll().await;
        registry.attach_space(&id, SpaceId::parse("space-1").expect("id"));

        let items = registry.attention_items();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item.id(),
            &AgentRegistry::item_id(&id),
            "a workflow can name the item before the agent first reports"
        );
        assert_eq!(item.state(), ItemState::AgentWaiting);
        assert!(item.title().as_str().contains("do the thing"));
        match item.context() {
            ItemContext::Agent(agent) => {
                assert_eq!(agent.session, id);
                assert_eq!(agent.space.as_ref().map(SpaceId::as_str), Some("space-1"));
            }
            other => panic!("expected agent context, got {other:?}"),
        }
        assert!(
            item.details()
                .iter()
                .any(|(key, value)| key == "Question" && value == "which branch?")
        );
    }

    #[tokio::test]
    async fn an_unavailable_engine_is_reported_before_a_task_is_queued() {
        let unavailable = Arc::new(FakeAgentEngine::unavailable(agent_id("claude-code")));
        let mut registry = AgentRegistry::new(Arc::new(FixedClock::new(NOW)));
        registry.register(unavailable);

        let error = registry
            .spawn(
                spec(TaskClass::DeepCodeChange),
                &RoutingTable::defaults(),
                None,
            )
            .await
            .expect_err("unavailable");

        assert!(matches!(error, PortError::NotConfigured { .. }));
    }

    #[tokio::test]
    async fn finished_sessions_are_pruned_so_the_monitor_does_not_grow_without_bound() {
        let (mut registry, claude, _) = registry();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let id = registry
                .spawn(
                    spec(TaskClass::PrSummary),
                    &RoutingTable::new(agent_id("claude-code")),
                    None,
                )
                .await
                .expect("spawn");
            claude.set_status(&id, AgentStatus::Succeeded);
            ids.push(id);
        }
        let running = registry
            .spawn(
                spec(TaskClass::PrSummary),
                &RoutingTable::new(agent_id("claude-code")),
                None,
            )
            .await
            .expect("spawn");
        registry.poll().await;

        registry.prune_finished(2);

        assert_eq!(
            registry.sessions().len(),
            3,
            "two finished plus the running one"
        );
        assert!(
            registry.session(&running).is_some(),
            "work in flight is never pruned"
        );
        assert!(
            registry.session(&ids[0]).is_none(),
            "the oldest finished session goes first"
        );
        assert!(registry.session(&ids[4]).is_some());
    }

    #[tokio::test]
    async fn pruning_with_fewer_finished_sessions_than_the_limit_keeps_them_all() {
        let (mut registry, claude, _) = registry();
        let id = registry
            .spawn(
                spec(TaskClass::PrSummary),
                &RoutingTable::new(agent_id("claude-code")),
                None,
            )
            .await
            .expect("spawn");
        claude.set_status(&id, AgentStatus::Succeeded);
        registry.poll().await;

        registry.prune_finished(5);

        assert_eq!(registry.sessions().len(), 1);
    }

    #[tokio::test]
    async fn the_registry_can_render_the_command_line_for_a_task() {
        let (registry, _, _) = registry();
        let command = registry
            .command_line(&agent_id("claude-code"), &spec(TaskClass::DeepCodeChange))
            .expect("command");
        assert_eq!(
            command,
            vec!["claude-code".to_owned(), "do the thing".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_skill_from_the_library_can_be_run_end_to_end() {
        let (mut registry, _, gemini) = registry();
        let library = SkillLibrary::builtin();
        let skill = library
            .get(&crate::domain::ids::SkillId::parse("pr-summary").expect("id"))
            .expect("built-in skill");
        let task = skill
            .to_task(
                &crate::domain::skill::Bindings::new()
                    .with("number", "142")
                    .with("repo", "acme/web"),
                PathBuf::from("/work/repo"),
            )
            .expect("task");

        registry
            .spawn(task, &RoutingTable::defaults(), skill.agent())
            .await
            .expect("spawn");

        assert_eq!(gemini.spawned().len(), 1);
        assert!(gemini.spawned()[0].prompt().as_str().contains("142"));
    }
}
