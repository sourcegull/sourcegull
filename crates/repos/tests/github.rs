//! GitHub connections end to end: discovery sync and webhook ingestion against the test
//! Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and the mock GitHub server.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use sourcegull_codehost::{ClientOptions, DomainEvent};
use sourcegull_codehost_github::testing::{APP_PRIVATE_KEY, MockGitHub, repo};
use sourcegull_codehost_github::webhooks::signature;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    AccountSpec, Db, LinkAccount, ListPermsSyncJobs, ListReposOptions, NewExternalService, NewUser,
    PermsSyncSubject, migrate,
};
use sourcegull_repos::{Registry, Syncer, WebhookIngest, WebhookTarget};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceId, ExternalServiceKind};
use tokio_util::sync::CancellationToken;

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    github: MockGitHub,
    registry: Registry,
}

async fn setup() -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate up");
    let db = Db::from_pool(tdb.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let github = MockGitHub::start("tok").await;
    github.with(|s| {
        s.repos = vec![repo(1, "acme/api"), repo(2, "acme/web"), repo(3, "other/x")];
    });
    let registry = Registry {
        client_options: ClientOptions {
            retry_backoff: Duration::from_millis(1),
            ..ClientOptions::default()
        },
        ..Registry::default()
    };
    Some(Env {
        _t: tdb,
        db,
        keyring,
        github,
        registry,
    })
}

impl Env {
    async fn service(&self) -> ExternalServiceId {
        let config = format!(
            r#"{{"url": "{}", "token": "tok", "orgs": ["acme"],
                "webhooks": [{{"secret": "hooksecret"}}]}}"#,
            self.github.url
        );
        self.registry
            .validate(ExternalServiceKind::GitHub, &config)
            .expect("valid config");
        self.db
            .external_services(Some(self.keyring.clone()))
            .create(&NewExternalService {
                kind: ExternalServiceKind::GitHub,
                display_name: "GitHub".into(),
                config,
            })
            .await
            .unwrap()
            .id
    }

    fn syncer(&self) -> Syncer {
        Syncer::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            self.registry.clone(),
            ConfigHandle::new(1, SiteConfig::default()),
        )
    }

    async fn live_names(&self) -> Vec<String> {
        self.db
            .repos()
            .list(&ListReposOptions::default())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    async fn queued(&self) -> Vec<(sourcegull_types::RepoId, String)> {
        sourcegull_db::sqlx::query_as(
            "SELECT repo_id, reason FROM repo_update_jobs WHERE state = 'queued' ORDER BY repo_id",
        )
        .fetch_all(self.db.pool())
        .await
        .unwrap()
    }

    async fn clear_queue(&self) {
        sourcegull_db::sqlx::query(
            "UPDATE repo_update_jobs SET state = 'completed', finished_at = now()",
        )
        .execute(self.db.pool())
        .await
        .unwrap();
    }
}

fn hook_headers(event: &str, secret: &str, body: &[u8]) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-github-event", HeaderValue::from_str(event).unwrap());
    h.insert(
        "x-hub-signature-256",
        HeaderValue::from_str(&signature(secret, body)).unwrap(),
    );
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_sync_and_webhooks() {
    let Some(env) = setup().await else { return };
    let svc = env.service().await;
    let host = env.github.url.trim_start_matches("http://").to_owned();

    // A full listing of the org; the repos are unrestricted (no permissions sync).
    let stats = env
        .syncer()
        .sync(svc, None, &CancellationToken::new())
        .await
        .unwrap();
    assert!(stats.full);
    assert_eq!((stats.synced, stats.added), (2, 2));
    assert_eq!(
        env.live_names().await,
        [format!("{host}/acme/api"), format!("{host}/acme/web")]
    );
    let repo = env
        .db
        .repos()
        .get_by_external_id("github", &format!("{}/", env.github.url), "2")
        .await
        .unwrap()
        .expect("repo 2");
    assert!(repo.private);
    assert_eq!(env.queued().await.len(), 2);
    env.clear_queue().await;

    let uuid = env
        .db
        .external_services(None)
        .get(svc)
        .await
        .unwrap()
        .unwrap()
        .webhook_uuid;
    let ingest = WebhookIngest::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        env.registry.clone(),
    );
    let push = br#"{"ref": "refs/heads/main", "before": "aaa", "after": "bbb",
        "repository": {"id": 2, "full_name": "acme/web"}}"#;

    // A bad signature is refused.
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("push", "nope", push),
            push,
        )
        .await;
    assert_eq!(out.status, 401);

    // A signed push requests a fetch of the repo.
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("push", "hooksecret", push),
            push,
        )
        .await;
    assert_eq!(out.status, 200, "{}", out.message);
    assert!(matches!(out.events[0], DomainEvent::RefsUpdated { .. }));
    let queued = env.queued().await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, repo.id);

    // The delivery is logged with its event name.
    let (event,): (Option<String>,) =
        sourcegull_db::sqlx::query_as("SELECT event FROM webhook_logs ORDER BY id DESC LIMIT 1")
            .fetch_one(env.db.pool())
            .await
            .unwrap();
    assert_eq!(event.as_deref(), Some("push"));

    // A rename queues a sync of the connection.
    let renamed = br#"{"action": "renamed", "repository": {"id": 2}}"#;
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("repository", "hooksecret", renamed),
            renamed,
        )
        .await;
    assert_eq!(out.status, 200, "{}", out.message);
    let jobs = env.db.sync_jobs().list(svc, 10, 0).await.unwrap();
    assert!(!jobs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_membership_webhooks_queue_permission_syncs() {
    let Some(env) = setup().await else { return };
    let config = format!(
        r#"{{"url": "{}", "token": "tok", "orgs": ["acme"],
            "authorization": {{"identityProvider": {{"type": "oauth"}}}},
            "webhooks": [{{"secret": "hooksecret"}}]}}"#,
        env.github.url
    );
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitHub,
            display_name: "GitHub".into(),
            config,
        })
        .await
        .unwrap();
    env.syncer()
        .sync(svc.id, None, &CancellationToken::new())
        .await
        .unwrap();
    let service_id = format!("{}/", env.github.url);
    let repo = env
        .db
        .repos()
        .get_by_external_id("github", &service_id, "2")
        .await
        .unwrap()
        .expect("repo 2");
    // Discovery queued repo-centric syncs of the new private repos; finish them.
    sourcegull_db::sqlx::query("UPDATE permission_sync_jobs SET state = 'completed'")
        .execute(env.db.pool())
        .await
        .unwrap();

    // Alice signed in with GitHub (account 7).
    let alice = env
        .db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    env.db
        .external_accounts(Some(env.keyring.clone()))
        .link(
            alice,
            &LinkAccount {
                spec: AccountSpec {
                    service_type: "github",
                    service_id: &service_id,
                    account_id: "7",
                },
                client_id: "mock-client",
                account_data: serde_json::json!({"username": "alice"}),
                auth_data: None,
            },
        )
        .await
        .unwrap();

    let ingest = WebhookIngest::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        env.registry.clone(),
    );
    let deliver = |event: &'static str, body: &'static [u8]| {
        let ingest = ingest.clone();
        let uuid = svc.webhook_uuid.clone();
        async move {
            let out = ingest
                .handle(
                    WebhookTarget::Uuid(&uuid),
                    &hook_headers(event, "hooksecret", body),
                    body,
                )
                .await;
            assert_eq!(out.status, 200, "{}", out.message);
            out.events
        }
    };
    let jobs = |subject| {
        let db = env.db.clone();
        async move {
            db.perms_sync_jobs()
                .list(&ListPermsSyncJobs {
                    subject: Some(subject),
                    state: Some("queued".into()),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_iter()
                .map(|j| j.reason)
                .collect::<Vec<_>>()
        }
    };

    // Removed from an organization: a sync of alice's permissions.
    let events = deliver(
        "organization",
        br#"{"action": "member_removed",
            "membership": {"user": {"id": 7, "login": "alice"}}}"#,
    )
    .await;
    assert!(matches!(events[0], DomainEvent::UserPermsChanged { .. }));
    assert_eq!(jobs(PermsSyncSubject::User(alice)).await, ["webhook"]);

    // A collaborator nobody in Sourcegull signed in as is ignored.
    deliver(
        "member",
        br#"{"action": "added", "member": {"id": 99, "login": "stranger"},
            "repository": {"id": 2}}"#,
    )
    .await;

    // A repository added to a team, and one made private: repo-centric syncs.
    deliver(
        "team_add",
        br#"{"team": {"id": 3}, "repository": {"id": 2}}"#,
    )
    .await;
    assert_eq!(jobs(PermsSyncSubject::Repo(repo.id)).await, ["webhook"]);
    let events = deliver(
        "repository",
        br#"{"action": "privatized", "repository": {"id": 2}}"#,
    )
    .await;
    assert!(matches!(events[1], DomainEvent::RepoPermsChanged { .. }));
    assert_eq!(jobs(PermsSyncSubject::Repo(repo.id)).await, ["webhook"]);
}

/// A connection authenticating as a GitHub App: the sync mints one installation token and lists
/// the installation's repositories; the stored key is redacted for display.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_app_sync() {
    let Some(env) = setup().await else { return };
    env.github.enable_app(11, 22);
    env.github.with(|s| s.affiliated = [1, 3].into());
    let config = format!(
        r#"{{"url": "{}", "githubApp": {{"appID": 11, "installationID": 22, "privateKey": {}}},
            "repositoryQuery": ["affiliated"]}}"#,
        env.github.url,
        serde_json::to_string(APP_PRIVATE_KEY).unwrap()
    );
    env.registry
        .validate(ExternalServiceKind::GitHub, &config)
        .expect("valid config");
    let redacted = env
        .registry
        .redact(ExternalServiceKind::GitHub, &config)
        .unwrap();
    assert!(!redacted.contains("PRIVATE KEY"), "{redacted}");
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitHub,
            display_name: "GitHub App".into(),
            config,
        })
        .await
        .unwrap()
        .id;
    let stats = env
        .syncer()
        .sync(svc, None, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((stats.synced, stats.added), (2, 2));
    let host = env.github.url.trim_start_matches("http://").to_owned();
    assert_eq!(
        env.live_names().await,
        [format!("{host}/acme/api"), format!("{host}/other/x")]
    );
    assert_eq!(env.github.installation_tokens_issued(), 1);
    assert!(
        !env.github
            .requests()
            .iter()
            .any(|r| r.starts_with("/api/v3/user"))
    );
}
