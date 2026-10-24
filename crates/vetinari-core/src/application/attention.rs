//! The attention engine: poll every source the active profile wants.
//!
//! Sources are polled concurrently and each answers on its own, failures
//! included, because one rate-limited tracker must not cost the developer
//! sight of a failing build, nor read as everything it reported being
//! resolved.

use std::sync::Arc;

use futures::future::join_all;

use super::ports::{AttentionSource, PortError, PortResult};
use crate::domain::attention::AttentionItem;
use crate::domain::ids::SourceId;
use crate::domain::profile::Profile;

/// A source that could not be polled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProblem {
    /// Which source failed.
    pub source: SourceId,
    /// Why.
    pub error: PortError,
}

/// One source's answer to a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    /// Which source.
    pub source: SourceId,
    /// Everything it reports, or why it could not be asked.
    pub outcome: PortResult<Vec<AttentionItem>>,
}

/// Polls the registered sources.
#[derive(Default)]
pub struct AttentionEngine {
    sources: Vec<Arc<dyn AttentionSource>>,
}

impl AttentionEngine {
    /// Add a source.
    pub fn register(&mut self, source: Arc<dyn AttentionSource>) {
        self.sources.push(source);
    }

    /// Every registered source id.
    #[must_use]
    pub fn source_ids(&self) -> Vec<SourceId> {
        self.sources
            .iter()
            .map(|source| source.id().clone())
            .collect()
    }

    /// Poll the sources the profile wants, concurrently, keeping each
    /// source's answer apart.
    pub async fn poll(&self, profile: &Profile) -> Vec<Polled> {
        let account = profile.account();
        join_all(
            self.sources
                .iter()
                .filter(|source| profile.wants_source(source.id()))
                .map(|source| async {
                    Polled {
                        source: source.id().clone(),
                        outcome: source.fetch(account).await,
                    }
                }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attention::{ItemState, SourceCategory};
    use crate::domain::ids::{ProfileId, Slug};
    use crate::domain::profile::Profile;
    use crate::domain::text::NonEmptyText;
    use crate::domain::workspace::TargetKind;
    use crate::testing::FakeSource;
    use time::OffsetDateTime;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-21 09:00 UTC);

    fn source_id(name: &str) -> SourceId {
        SourceId::parse(name).expect("slug")
    }

    fn item(
        source: &str,
        native: &str,
        category: SourceCategory,
        state: ItemState,
    ) -> AttentionItem {
        AttentionItem::builder(
            &source_id(source),
            native,
            category,
            NonEmptyText::parse(&format!("{source} {native}")).expect("title"),
        )
        .state(state)
        .occurred_at(NOW)
        .build()
    }

    fn profile(sources: &[&str]) -> Profile {
        let mut profile = Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Tmux,
        );
        for source in sources {
            profile = profile.with_source(source_id(source));
        }
        profile
    }

    fn engine(sources: Vec<Arc<dyn AttentionSource>>) -> AttentionEngine {
        let mut engine = AttentionEngine::default();
        for source in sources {
            engine.register(source);
        }
        engine
    }

    #[tokio::test]
    async fn every_source_is_polled_as_the_active_profiles_account() {
        // The profile is what scopes credentials, and it changes while
        // Vetinari is running, so the account has to travel with the call.
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            Vec::new(),
        ));
        let engine = engine(vec![github.clone()]);
        let work = profile(&["github"]).with_account(Slug::parse("work").expect("account"));
        let home = profile(&["github"]).with_account(Slug::parse("home").expect("account"));

        engine.poll(&work).await;
        engine.poll(&home).await;

        let seen: Vec<Option<String>> = github
            .accounts()
            .into_iter()
            .map(|account| account.map(|slug| slug.as_str().to_owned()))
            .collect();
        assert_eq!(
            seen,
            vec![Some("work".to_owned()), Some("home".to_owned())],
            "switching profile must switch which credential is used"
        );
    }

    #[tokio::test]
    async fn a_profile_with_no_account_polls_without_one() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            Vec::new(),
        ));
        let engine = engine(vec![github.clone()]);

        engine.poll(&profile(&["github"])).await;

        assert_eq!(github.accounts(), vec![None]);
    }

    #[tokio::test]
    async fn a_poll_keeps_each_sources_answer_apart() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "142",
                SourceCategory::CodeReview,
                ItemState::CiFailed,
            )],
        ));
        let jira = Arc::new(FakeSource::failing(
            source_id("jira"),
            SourceCategory::IssueTracking,
            PortError::unavailable("jira", "503"),
        ));

        let polled = engine(vec![github, jira]).poll(&profile(&[])).await;

        let answers: Vec<_> = polled
            .iter()
            .map(|polled| {
                (
                    polled.source.as_str().to_owned(),
                    polled.outcome.as_ref().map(Vec::len).ok(),
                )
            })
            .collect();
        assert_eq!(
            answers,
            vec![("github".to_owned(), Some(1)), ("jira".to_owned(), None)]
        );
    }

    #[tokio::test]
    async fn a_profile_only_polls_the_sources_it_wants() {
        let github = Arc::new(FakeSource::new(
            source_id("github"),
            SourceCategory::CodeReview,
            vec![item(
                "github",
                "1",
                SourceCategory::CodeReview,
                ItemState::NeedsReview,
            )],
        ));
        let slack = Arc::new(FakeSource::new(
            source_id("slack"),
            SourceCategory::Messaging,
            vec![item(
                "slack",
                "D1",
                SourceCategory::Messaging,
                ItemState::Unread,
            )],
        ));

        let polled = engine(vec![github.clone(), slack.clone()])
            .poll(&profile(&["github"]))
            .await;

        assert_eq!(polled.len(), 1);
        assert_eq!(github.fetch_count(), 1);
        assert_eq!(slack.fetch_count(), 0);
    }
}
