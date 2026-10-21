//! Task-to-agent routing.
//!
//! The PRD's example - PR summaries to Gemini, deep bug fixes to Claude Code
//! in a worktree - is a mapping from class of work to engine, with a fallback
//! so that every task can always be placed somewhere.

use std::collections::BTreeMap;

use super::agent::TaskClass;
use super::ids::AgentId;

/// One routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRule {
    /// The class of work the rule covers.
    pub class: TaskClass,
    /// The engine that handles it.
    pub agent: AgentId,
}

/// Which engine handles which class of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingTable {
    rules: BTreeMap<TaskClass, AgentId>,
    fallback: AgentId,
}

impl RoutingTable {
    /// A table with no rules; everything goes to the fallback engine.
    #[must_use]
    pub fn new(fallback: AgentId) -> Self {
        Self {
            rules: BTreeMap::new(),
            fallback,
        }
    }

    /// Add or replace the rule for a class.
    #[must_use]
    pub fn with_rule(mut self, class: TaskClass, agent: AgentId) -> Self {
        self.rules.insert(class, agent);
        self
    }

    /// The engine for a class of work.
    #[must_use]
    pub fn agent_for(&self, class: TaskClass) -> &AgentId {
        self.rules.get(&class).unwrap_or(&self.fallback)
    }

    /// The engine for a class, honouring a skill that pins one.
    #[must_use]
    pub fn resolve<'a>(&'a self, class: TaskClass, pinned: Option<&'a AgentId>) -> &'a AgentId {
        pinned.unwrap_or_else(|| self.agent_for(class))
    }

    /// The engine used when no rule matches.
    #[must_use]
    pub const fn fallback(&self) -> &AgentId {
        &self.fallback
    }

    /// Every rule, in class order.
    #[must_use]
    pub fn rules(&self) -> Vec<RoutingRule> {
        self.rules
            .iter()
            .map(|(class, agent)| RoutingRule {
                class: *class,
                agent: agent.clone(),
            })
            .collect()
    }

    /// The default routing from the PRD: Claude Code for codebase work,
    /// Gemini for long-context reading, scripts for cheap summaries.
    ///
    /// # Panics
    ///
    /// Panics if a built-in engine name is not a valid slug, which would be a
    /// bug here rather than in configuration.
    #[must_use]
    pub fn defaults() -> Self {
        let claude = AgentId::parse("claude-code").expect("built-in agent id is valid");
        let gemini = AgentId::parse("agy-gemini").expect("built-in agent id is valid");
        let script = AgentId::parse("script").expect("built-in agent id is valid");
        Self::new(claude.clone())
            .with_rule(TaskClass::DeepCodeChange, claude.clone())
            .with_rule(TaskClass::PrReview, claude.clone())
            .with_rule(TaskClass::Enhancement, claude)
            .with_rule(TaskClass::PrSummary, gemini.clone())
            .with_rule(TaskClass::LongContextQuery, gemini.clone())
            .with_rule(TaskClass::MeetingPrep, gemini)
            .with_rule(TaskClass::IssueSummary, script.clone())
            .with_rule(TaskClass::StandupSummary, script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId::parse(name).expect("id")
    }

    #[test]
    fn a_rule_beats_the_fallback() {
        let table = RoutingTable::new(agent("claude-code"))
            .with_rule(TaskClass::PrSummary, agent("agy-gemini"));
        assert_eq!(table.agent_for(TaskClass::PrSummary), &agent("agy-gemini"));
        assert_eq!(
            table.agent_for(TaskClass::DeepCodeChange),
            &agent("claude-code")
        );
    }

    #[test]
    fn a_later_rule_overrides_an_earlier_one_for_the_same_class() {
        let table = RoutingTable::new(agent("claude-code"))
            .with_rule(TaskClass::PrSummary, agent("agy-gemini"))
            .with_rule(TaskClass::PrSummary, agent("script"));
        assert_eq!(table.agent_for(TaskClass::PrSummary), &agent("script"));
        assert_eq!(table.rules().len(), 1);
    }

    #[test]
    fn a_skill_that_pins_an_engine_overrides_the_table() {
        let table = RoutingTable::new(agent("claude-code"));
        assert_eq!(
            table.resolve(TaskClass::PrSummary, Some(&agent("agy-gemini"))),
            &agent("agy-gemini")
        );
        assert_eq!(
            table.resolve(TaskClass::PrSummary, None),
            &agent("claude-code")
        );
    }

    #[test]
    fn the_default_table_follows_the_prd_routing_examples() {
        let table = RoutingTable::defaults();
        assert_eq!(
            table.agent_for(TaskClass::DeepCodeChange),
            &agent("claude-code")
        );
        assert_eq!(table.agent_for(TaskClass::PrSummary), &agent("agy-gemini"));
        assert_eq!(
            table.agent_for(TaskClass::LongContextQuery),
            &agent("agy-gemini")
        );
        assert_eq!(table.agent_for(TaskClass::StandupSummary), &agent("script"));
    }

    #[test]
    fn every_task_class_resolves_to_some_engine() {
        let table = RoutingTable::defaults();
        for class in TaskClass::ALL {
            assert!(!table.agent_for(*class).as_str().is_empty());
        }
    }

    #[test]
    fn rules_are_listed_in_class_order_for_display() {
        let table = RoutingTable::new(agent("claude-code"))
            .with_rule(TaskClass::StandupSummary, agent("script"))
            .with_rule(TaskClass::PrReview, agent("claude-code"));
        let classes: Vec<_> = table.rules().iter().map(|rule| rule.class).collect();
        assert_eq!(
            classes,
            vec![TaskClass::PrReview, TaskClass::StandupSummary]
        );
    }
}
