//! The agents Vetinari can see but did not start.
//!
//! A workspace target such as Herdr runs agents of the developer's own. They
//! belong in the feed - a blocked agent is wasting time whoever started it -
//! but not in the session registry, which only holds work Vetinari can
//! cancel, answer, and re-run. So they are polled, ranked, and listed here,
//! and nowhere does a type suggest they can be controlled.

use std::sync::Arc;

use super::ports::{ExternalAgentSource, PortError};
use crate::domain::agent::ObservedAgent;
use crate::domain::attention::{AttentionItem, SourceCategory};
use crate::domain::ids::SourceId;
use crate::domain::text::NonEmptyText;
use crate::domain::workspace::TargetKind;

/// What one poll of every observing target found.
#[derive(Debug, Default)]
pub struct Observation {
    /// The agents seen.
    pub agents: Vec<ObservedAgent>,
    /// The targets that could not answer, and why.
    pub problems: Vec<(TargetKind, PortError)>,
}

/// The workspace targets that report their own agents.
#[derive(Default)]
pub struct ObservedAgents {
    sources: Vec<Arc<dyn ExternalAgentSource>>,
}

impl ObservedAgents {
    /// The source id observed agents appear under in the feed.
    pub const SOURCE_NAME: &'static str = "workspace-agents";

    /// Watch a target.
    pub fn register(&mut self, source: Arc<dyn ExternalAgentSource>) {
        self.sources.push(source);
    }

    /// Which targets are being watched.
    #[must_use]
    pub fn targets(&self) -> Vec<TargetKind> {
        self.sources.iter().map(|source| source.target()).collect()
    }

    /// Whether anything is being watched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Ask every target what it can see.
    ///
    /// One target failing is collected rather than fatal, the same way a
    /// failing attention source is: the rest of the monitor still works.
    pub async fn poll(&self) -> Observation {
        let mut observation = Observation::default();
        for source in &self.sources {
            match source.observed_agents().await {
                Ok(agents) => observation.agents.extend(agents),
                Err(error) => observation.problems.push((source.target(), error)),
            }
        }
        observation
    }

    /// The feed source id used for observed agents.
    #[must_use]
    pub fn source_id() -> SourceId {
        SourceId::parse(Self::SOURCE_NAME).expect("built-in source name is valid")
    }

    /// Observed agents, rendered as feed items.
    #[must_use]
    pub fn attention_items(agents: &[ObservedAgent]) -> Vec<AttentionItem> {
        let source = Self::source_id();
        agents
            .iter()
            .map(|agent| Self::as_item(&source, agent))
            .collect()
    }

    fn as_item(source: &SourceId, agent: &ObservedAgent) -> AttentionItem {
        let title = NonEmptyText::parse(&match agent.title() {
            Some(title) => format!("{}: {title}", agent.name()),
            None => format!(
                "{} in {}",
                agent.name(),
                agent
                    .workspace()
                    .map_or_else(|| agent.pane().as_str(), |id| id.as_str())
            ),
        })
        .unwrap_or_else(|_| NonEmptyText::parse("agent").expect("the fallback title is non-empty"));

        let mut builder = AttentionItem::builder(
            source,
            agent.pane().as_str(),
            SourceCategory::AgentState,
            title,
        )
        .state(agent.state().as_item_state())
        .author(agent.target().name())
        .detail("Target", agent.target().name())
        .detail("Status", agent.state().label())
        .detail("Pane", agent.pane().as_str())
        .detail("Started by", "someone other than Vetinari");

        if let Some(workspace) = agent.workspace() {
            builder = builder.detail("Space", workspace.as_str());
        }
        if let Some(how) = agent.source() {
            builder = builder.detail("Detected by", how);
        }
        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::ObservedAgentState;
    use crate::domain::attention::ItemState;
    use crate::domain::ids::SpaceId;
    use crate::testing::FakeExternalAgentSource;

    fn agent(pane: &str, workspace: Option<&str>, state: ObservedAgentState) -> ObservedAgent {
        ObservedAgent::new(
            SpaceId::parse(pane).expect("pane"),
            workspace.map(|id| SpaceId::parse(id).expect("workspace")),
            "claude",
            state,
            TargetKind::Herdr,
            None,
        )
    }

    #[tokio::test]
    async fn every_watched_target_is_asked_and_their_agents_are_pooled() {
        let herdr = Arc::new(FakeExternalAgentSource::new(
            TargetKind::Herdr,
            vec![
                agent("w1:p1", Some("w1"), ObservedAgentState::Blocked),
                agent("w2:p1", Some("w2"), ObservedAgentState::Working),
            ],
        ));
        let tmux = Arc::new(FakeExternalAgentSource::new(
            TargetKind::Tmux,
            vec![agent("@3", None, ObservedAgentState::Idle)],
        ));
        let mut observed = ObservedAgents::default();
        observed.register(herdr);
        observed.register(tmux);

        let observation = observed.poll().await;

        assert_eq!(observation.agents.len(), 3);
        assert!(observation.problems.is_empty());
        assert_eq!(
            observed.targets(),
            vec![TargetKind::Herdr, TargetKind::Tmux]
        );
    }

    #[tokio::test]
    async fn a_target_that_cannot_answer_does_not_lose_the_agents_of_the_others() {
        let broken = Arc::new(FakeExternalAgentSource::failing(
            TargetKind::Herdr,
            PortError::unavailable("herdr", "socket is not there"),
        ));
        let working = Arc::new(FakeExternalAgentSource::new(
            TargetKind::Tmux,
            vec![agent("@3", None, ObservedAgentState::Working)],
        ));
        let mut observed = ObservedAgents::default();
        observed.register(broken);
        observed.register(working);

        let observation = observed.poll().await;

        assert_eq!(observation.agents.len(), 1);
        assert_eq!(observation.problems.len(), 1);
        assert_eq!(observation.problems[0].0, TargetKind::Herdr);
    }

    #[test]
    fn a_blocked_observed_agent_becomes_the_most_urgent_kind_of_feed_item() {
        let items = ObservedAgents::attention_items(&[
            agent("w1:p1", Some("w1"), ObservedAgentState::Blocked),
            agent("w2:p1", Some("w2"), ObservedAgentState::Working),
        ]);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].state(), ItemState::AgentWaiting);
        assert_eq!(items[1].state(), ItemState::AgentRunning);
        assert!(items[0].title().as_str().contains("claude in w1"));
        assert_eq!(items[0].category(), SourceCategory::AgentState);
    }

    #[test]
    fn an_agent_the_target_has_titled_is_queued_under_its_title() {
        let titled = agent("w1:p1", Some("w1"), ObservedAgentState::Done)
            .with_title(NonEmptyText::parse("Herdr attention queue").expect("title"));

        let items = ObservedAgents::attention_items(&[titled]);

        assert_eq!(items[0].title().as_str(), "claude: Herdr attention queue");
    }

    #[test]
    fn an_observed_agent_says_that_vetinari_did_not_start_it() {
        let items = ObservedAgents::attention_items(&[ObservedAgent::new(
            SpaceId::parse("w1:p1").expect("pane"),
            Some(SpaceId::parse("w1").expect("workspace")),
            "codex",
            ObservedAgentState::Blocked,
            TargetKind::Herdr,
            Some("integration".to_owned()),
        )]);

        let details = items[0].details();
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Started by" && value == "someone other than Vetinari"),
            "an agent that cannot be cancelled must not look like one that can: {details:?}"
        );
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Detected by" && value == "integration")
        );
        assert!(
            details
                .iter()
                .any(|(key, value)| key == "Space" && value == "w1")
        );
    }

    #[test]
    fn observed_agents_are_filed_under_their_own_feed_source() {
        let items = ObservedAgents::attention_items(&[agent(
            "w1:p1",
            Some("w1"),
            ObservedAgentState::Working,
        )]);

        assert_eq!(items[0].source().as_str(), "workspace-agents");
        assert!(
            items[0].id().as_str().contains("w1:p1"),
            "the pane keeps the item stable across refreshes: {:?}",
            items[0].id()
        );
    }

    #[test]
    fn nothing_watched_is_not_a_problem() {
        let observed = ObservedAgents::default();
        assert!(observed.is_empty());
        assert!(ObservedAgents::attention_items(&[]).is_empty());
    }
}
