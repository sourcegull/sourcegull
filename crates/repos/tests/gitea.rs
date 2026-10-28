//! Gitea connections end to end: discovery sync and webhook ingestion against the test
//! Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and the mock Gitea server.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use sourcegull_codehost::{ClientOptions, DomainEvent};
use sourcegull_codehost_gitea::testing::{MockGitea, repo};
use sourcegull_codehost_gitea::webhooks::signature;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ListReposOptions, NewExternalService, migrate};
use sourcegull_repos::{Registry, Syncer, WebhookIngest, WebhookTarget};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceId, ExternalServiceKind};
use tokio_util::sync::CancellationToken;

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    gitea: MockGitea,
    registry: Registry,
}

async fn setup() -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate up");
    let db = Db::from_pool(tdb.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let gitea = MockGitea::start("tok").await;
    gitea.with(|s| {
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
        gitea,
        registry,
    })
}

impl Env {
    async fn service(&self) -> ExternalServiceId {
        let config = format!(
            r#"{{"url": "{}", "token": "tok", "orgs": ["acme"],
                "webhooks": [{{"secret": "hooksecret"}}]}}"#,
            self.gitea.url
        );
        self.registry
            .validate(ExternalServiceKind::Gitea, &config)
            .expect("valid config");
        self.db
            .external_services(Some(self.keyring.clone()))
            .create(&NewExternalService {
                kind: ExternalServiceKind::Gitea,
                display_name: "Gitea".into(),
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
    h.insert("x-gitea-event", HeaderValue::from_str(event).unwrap());
    h.insert(
        "x-gitea-signature",
        HeaderValue::from_str(&signature(secret, body)).unwrap(),
    );
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitea_sync_and_webhooks() {
    let Some(env) = setup().await else { return };
    let svc = env.service().await;
    let host = env.gitea.url.trim_start_matches("http://").to_owned();

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
        .get_by_external_id("gitea", &format!("{}/", env.gitea.url), "2")
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

    // A branch deletion (Gitea sends no push for it) also requests a fetch.
    env.clear_queue().await;
    let deleted = br#"{"ref": "feat", "ref_type": "branch", "repository": {"id": 2}}"#;
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("delete", "hooksecret", deleted),
            deleted,
        )
        .await;
    assert_eq!(out.status, 200, "{}", out.message);
    assert_eq!(env.queued().await.len(), 1);

    // A new repository queues a sync of the connection.
    let created = br#"{"action": "created", "repository": {"id": 9}}"#;
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("repository", "hooksecret", created),
            created,
        )
        .await;
    assert_eq!(out.status, 200, "{}", out.message);
    let jobs = env.db.sync_jobs().list(svc, 10, 0).await.unwrap();
    assert!(!jobs.is_empty());
}
