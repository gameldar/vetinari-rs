//! Opening a run's pull request on GitHub (design 7.4): push the run's
//! branch, then adopt the pull request already open from it, or open one.
//! Finding before opening is what makes a rerun of `open-pr` harmless.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use vetinari_core::application::auth::AuthService;
use vetinari_core::application::devflow::ports::PullRequestPublisher;
use vetinari_core::application::ports::{PortError, PortResult, WorktreeManager};
use vetinari_core::domain::credentials::Provider;
use vetinari_core::domain::devflow::outcome::{Line, OpenPrOutcome};
use vetinari_core::domain::devflow::step::StepConfig;
use vetinari_core::domain::profile::Profile;

use crate::HttpClient;
use crate::sources::github::{endpoint, send};

/// Pushes a run's branch and opens its pull request on GitHub, with the
/// profile's GitHub credential.
pub struct GitHubPublisher {
    base_url: Option<String>,
    http: HttpClient,
    auth: Arc<AuthService>,
    git: Arc<dyn WorktreeManager>,
}

impl GitHubPublisher {
    /// The remote a run's branch is pushed to.
    pub const REMOTE: &'static str = "origin";

    /// A publisher for github.com, or for an enterprise `base_url`.
    #[must_use]
    pub fn new(
        base_url: Option<String>,
        http: HttpClient,
        auth: Arc<AuthService>,
        git: Arc<dyn WorktreeManager>,
    ) -> Self {
        Self {
            base_url,
            http,
            auth,
            git,
        }
    }
}

impl GitHubPublisher {
    fn url(&self, path: &str) -> String {
        endpoint(self.base_url.as_deref(), path)
    }

    /// The pull request open from `branch`, if there is one.
    async fn open_from(
        &self,
        token: &str,
        repo: &str,
        branch: &str,
    ) -> PortResult<Option<OpenPrOutcome>> {
        let owner = repo.split('/').next().unwrap_or(repo);
        let head = format!("{owner}:{branch}");
        let listed = send(
            self.http
                .inner()
                .get(self.url(&format!("/repos/{repo}/pulls")))
                .query(&[("head", head.as_str()), ("state", "open")])
                .bearer_auth(token),
        )
        .await?;
        listed
            .as_array()
            .and_then(|pulls| pulls.first())
            .map(opened)
            .transpose()
    }

    /// What a new pull request merges into: the run's base, else the
    /// repository's default branch.
    async fn base(&self, token: &str, repo: &str, config: &StepConfig) -> PortResult<String> {
        if let Some(base) = &config.base {
            return Ok(base.to_string());
        }
        let repository = send(
            self.http
                .inner()
                .get(self.url(&format!("/repos/{repo}")))
                .bearer_auth(token),
        )
        .await?;
        repository["default_branch"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| PortError::protocol("github", "a repository with no default branch"))
    }
}

/// A pull request as the run reports it.
fn opened(pull: &Value) -> PortResult<OpenPrOutcome> {
    let number = pull["number"].as_u64();
    let url = pull["html_url"]
        .as_str()
        .and_then(|url| Line::parse(url).ok());
    match (number, url) {
        (Some(number), Some(url)) => Ok(OpenPrOutcome::Opened { number, url }),
        _ => Err(PortError::protocol(
            "github",
            "a pull request with no number or url",
        )),
    }
}

/// What a run's pull request says about itself.
fn description(config: &StepConfig) -> String {
    let ticket = &config.ticket;
    let link = ticket.url.as_deref().unwrap_or(ticket.key.as_str());
    format!(
        "{} {}\n\nTicket: {link}\n\nOpened by vetinari, which ran this ticket through \
         preflight, implementation and review.",
        ticket.key, ticket.title
    )
}

#[async_trait]
impl PullRequestPublisher for GitHubPublisher {
    async fn publish(&self, config: &StepConfig, profile: &Profile) -> PortResult<OpenPrOutcome> {
        self.git
            .push(&config.repo.path, Self::REMOTE, &config.branch)
            .await?;
        let token = self
            .auth
            .token(Provider::GitHub, profile.account().cloned())
            .await?;
        let token = token.expose();
        let repo = config.repo.remote.as_str();
        let branch = config.branch.as_str();
        if let Some(open) = self.open_from(token, repo, branch).await? {
            return Ok(open);
        }
        let base = self.base(token, repo, config).await?;
        let created = send(
            self.http
                .inner()
                .post(self.url(&format!("/repos/{repo}/pulls")))
                .bearer_auth(token)
                .json(&json!({
                    "title": format!("{} {}", config.ticket.key, config.ticket.title),
                    "head": branch,
                    "base": base,
                    "body": description(config),
                })),
        )
        .await?;
        opened(&created)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;
    use vetinari_core::application::ports::DeviceCodeFlow;
    use vetinari_core::domain::credentials::{CredentialKey, Secret, StoredCredential};
    use vetinari_core::domain::ids::{ProfileId, Slug};
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::domain::workspace::TargetKind;
    use vetinari_core::testing::{
        FakeDeviceCodeFlow, FakeWorktreeManager, FixedClock, MemoryCredentialStore,
    };

    use super::*;
    use crate::testing::MockServer;

    const URL: &str = "https://github.test/acme/web/pull/7";

    fn config(base: Option<&str>) -> StepConfig {
        let mut config = json!({
            "schema": 1, "step": "open-pr", "template": "feature-from-ticket",
            "ticket": { "source": "jira", "key": "ENG-412", "title": "Rate-limit retries",
                "url": "https://acme.atlassian.net/browse/ENG-412" },
            "repo": { "path": "/work/repos/web", "remote": "acme/web" },
            "profile": "work", "branch": "vetinari/eng-412-rate-limit-retries"
        });
        if let Some(base) = base {
            config["base"] = json!(base);
        }
        StepConfig::parse(&config).expect("a step config")
    }

    fn profile() -> Profile {
        Profile::new(
            ProfileId::parse("work").expect("id"),
            NonEmptyText::parse("Work").expect("name"),
            TargetKind::Tmux,
        )
        .with_account(Slug::parse("work").expect("account"))
    }

    fn auth() -> Arc<AuthService> {
        let now = datetime!(2026-10-25 10:00 UTC);
        let store = Arc::new(MemoryCredentialStore::default());
        store.seed(
            &CredentialKey::new(
                Provider::GitHub,
                Some(Slug::parse("work").expect("account")),
            ),
            StoredCredential::api_token(Secret::new("ghp_work")),
        );
        let flow: Arc<dyn DeviceCodeFlow> = Arc::new(FakeDeviceCodeFlow::new(0, now));
        Arc::new(AuthService::new(
            store,
            flow,
            Arc::new(FixedClock::new(now)),
        ))
    }

    fn publisher(server: &MockServer, git: &Arc<FakeWorktreeManager>) -> GitHubPublisher {
        GitHubPublisher::new(
            Some(server.base_url.clone()),
            HttpClient::new().expect("client"),
            auth(),
            git.clone(),
        )
    }

    fn opened(number: u64) -> OpenPrOutcome {
        OpenPrOutcome::Opened {
            number,
            url: Line::parse(URL).expect("url"),
        }
    }

    #[tokio::test]
    async fn the_branch_is_pushed_and_its_pull_request_opened() {
        let server = MockServer::start(vec![
            (200, "[]".to_owned()),
            (201, json!({ "number": 7, "html_url": URL }).to_string()),
        ])
        .await;
        let git = Arc::new(FakeWorktreeManager::default());

        let outcome = publisher(&server, &git)
            .publish(&config(Some("main")), &profile())
            .await
            .expect("published");

        assert_eq!(outcome, opened(7));
        assert_eq!(
            git.pushed(),
            [(
                PathBuf::from("/work/repos/web"),
                "origin".to_owned(),
                "vetinari/eng-412-rate-limit-retries".to_owned()
            )]
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert!(
            requests[0].target.starts_with("/repos/acme/web/pulls?"),
            "{}",
            requests[0].target
        );
        assert!(
            requests[0]
                .target
                .contains("head=acme%3Avetinari%2Feng-412-rate-limit-retries"),
            "{}",
            requests[0].target
        );
        assert!(requests[0].target.contains("state=open"));
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].target, "/repos/acme/web/pulls");
        assert_eq!(requests[1].header("authorization"), Some("Bearer ghp_work"));
        let body = requests[1].json();
        assert_eq!(body["title"], json!("ENG-412 Rate-limit retries"));
        assert_eq!(body["head"], json!("vetinari/eng-412-rate-limit-retries"));
        assert_eq!(body["base"], json!("main"));
        assert!(
            body["body"]
                .as_str()
                .is_some_and(|text| text.contains("https://acme.atlassian.net/browse/ENG-412")),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_pull_request_an_earlier_attempt_opened_is_adopted() {
        let server = MockServer::start(vec![(
            200,
            json!([{ "number": 5, "html_url": URL }]).to_string(),
        )])
        .await;
        let git = Arc::new(FakeWorktreeManager::default());

        let outcome = publisher(&server, &git)
            .publish(&config(Some("main")), &profile())
            .await
            .expect("adopted");

        assert_eq!(outcome, opened(5));
        assert_eq!(server.requests().len(), 1, "nothing is opened");
        assert_eq!(git.pushed().len(), 1, "the branch is still pushed");
    }

    #[tokio::test]
    async fn without_a_base_the_repositorys_default_branch_is_the_base() {
        let server = MockServer::start(vec![
            (200, "[]".to_owned()),
            (200, json!({ "default_branch": "trunk" }).to_string()),
            (201, json!({ "number": 7, "html_url": URL }).to_string()),
        ])
        .await;
        let git = Arc::new(FakeWorktreeManager::default());

        publisher(&server, &git)
            .publish(&config(None), &profile())
            .await
            .expect("published");

        let requests = server.requests();
        assert_eq!(requests[1].target, "/repos/acme/web");
        assert_eq!(requests[2].json()["base"], json!("trunk"));
    }

    #[tokio::test]
    async fn a_branch_that_will_not_push_opens_nothing() {
        let server = MockServer::start(Vec::new()).await;
        let git = Arc::new(FakeWorktreeManager::default());
        git.fail_push(PortError::unavailable("git push", "no route to host"));

        let error = publisher(&server, &git)
            .publish(&config(Some("main")), &profile())
            .await
            .expect_err("not pushed");

        assert!(matches!(error, PortError::Unavailable { .. }), "{error:?}");
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn a_host_that_is_down_is_worth_trying_again() {
        let server = MockServer::start(vec![(503, "{}".to_owned())]).await;
        let git = Arc::new(FakeWorktreeManager::default());

        let error = publisher(&server, &git)
            .publish(&config(Some("main")), &profile())
            .await
            .expect_err("down");

        assert!(error.is_transient(), "{error:?}");
    }

    #[tokio::test]
    async fn a_reply_without_a_number_or_url_is_a_protocol_error() {
        let server = MockServer::start(vec![(200, json!([{ "number": 5 }]).to_string())]).await;
        let git = Arc::new(FakeWorktreeManager::default());

        let error = publisher(&server, &git)
            .publish(&config(Some("main")), &profile())
            .await
            .expect_err("unreadable");

        assert!(matches!(error, PortError::Protocol { .. }), "{error:?}");
    }
}
