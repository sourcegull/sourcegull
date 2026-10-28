//! Discovery sync, webhook ingestion and the sync worker against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and the mock GitLab server.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use http::{HeaderMap, HeaderValue};
use sourcegull_codehost::DomainEvent;
use sourcegull_codehost_gitlab::ClientOptions;
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ListReposOptions, NewExternalService, migrate};
use sourcegull_repos::{Registry, SyncError, Syncer, WebhookIngest, WebhookTarget};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceId, ExternalServiceKind, RepoId};
use tokio_util::sync::CancellationToken;

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    gitlab: MockGitLab,
    registry: Registry,
}

fn t(day: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap()
}

async fn setup() -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate up");
    let db = Db::from_pool(tdb.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let gitlab = MockGitLab::start("tok").await;
    gitlab.with(|s| {
        s.projects = vec![
            project(1, "acme/api", t(1)),
            project(2, "acme/web", t(2)),
            project(3, "acme/docs", t(3)),
        ];
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
        gitlab,
        registry,
    })
}

impl Env {
    async fn service(&self, extra: &str) -> ExternalServiceId {
        let config = format!(
            r#"{{"url": "{}", "token": "tok", "projectQuery": ["projects?membership=true"],
                "webhooks": [{{"secret": "hooksecret"}}] {extra}}}"#,
            self.gitlab.url
        );
        self.registry
            .validate(ExternalServiceKind::GitLab, &config)
            .expect("valid config");
        self.db
            .external_services(Some(self.keyring.clone()))
            .create(&NewExternalService {
                kind: ExternalServiceKind::GitLab,
                display_name: "GitLab".into(),
                config,
            })
            .await
            .unwrap()
            .id
    }

    fn syncer(&self) -> Syncer {
        self.syncer_at(None)
    }

    /// A syncer whose site configuration has `externalURL` set to `external_url`.
    fn syncer_at(&self, external_url: Option<&str>) -> Syncer {
        Syncer::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            self.registry.clone(),
            ConfigHandle::new(
                1,
                SiteConfig {
                    external_url: external_url.map(str::to_owned),
                    ..SiteConfig::default()
                },
            ),
        )
    }

    /// Makes the next sync of every connection a full listing.
    async fn force_full(&self) {
        sourcegull_db::sqlx::query("UPDATE external_services SET last_full_sync_at = NULL")
            .execute(self.db.pool())
            .await
            .unwrap();
    }

    /// Runs a sync of `svc` as sync job, returning the job's warnings.
    async fn sync_job(&self, syncer: &Syncer, svc: ExternalServiceId) -> Vec<String> {
        // The worker would have finished earlier jobs; free the queue slot.
        self.complete_jobs().await;
        let job = self
            .db
            .sync_jobs()
            .enqueue(svc, sourcegull_db::SyncReason::Manual)
            .await
            .unwrap()
            .expect("queued");
        syncer
            .sync(svc, Some(job.get()), &CancellationToken::new())
            .await
            .unwrap();
        let jobs = self.db.sync_jobs().list(svc, 1, 0).await.unwrap();
        jobs[0].warnings.clone()
    }

    async fn complete_jobs(&self) {
        sourcegull_db::sqlx::query("UPDATE external_service_sync_jobs SET state = 'completed'")
            .execute(self.db.pool())
            .await
            .unwrap();
    }

    fn ingest(&self) -> WebhookIngest {
        WebhookIngest::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            self.registry.clone(),
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

    async fn queued(&self) -> Vec<(RepoId, i16, String)> {
        sourcegull_db::sqlx::query_as(
            "SELECT repo_id, priority, reason FROM repo_update_jobs WHERE state = 'queued' ORDER BY repo_id",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_then_incremental_sync_with_activity_and_deletion() {
    let Some(env) = setup().await else { return };
    let svc = env.service("").await;
    let syncer = env.syncer();
    let cancel = CancellationToken::new();
    let host = env.gitlab.url.trim_start_matches("http://").to_owned();

    // First sync: full listing, three repos, clone requests queued.
    let stats = syncer.sync(svc, None, &cancel).await.unwrap();
    assert!(stats.full);
    assert_eq!((stats.synced, stats.added, stats.deleted), (3, 3, 0));
    assert_eq!(
        env.live_names().await,
        [
            format!("{host}/acme/api"),
            format!("{host}/acme/docs"),
            format!("{host}/acme/web")
        ]
    );
    assert_eq!(env.queued().await.len(), 3);
    let s = env
        .db
        .external_services(None)
        .get(svc)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.sync_watermark, Some(t(3)));
    assert!(s.last_full_sync_at.is_some() && s.last_sync_error.is_none());
    assert!(s.next_sync_at.unwrap() > Utc::now() + chrono::Duration::seconds(30));
    env.clear_queue().await;

    // New activity on project 2 and a new project 4; the next sync is incremental.
    env.gitlab.with(|s| {
        s.projects[1]["last_activity_at"] = serde_json::json!(t(10));
        s.projects.push(project(4, "acme/new", t(11)));
        s.requests.clear();
    });
    let stats = syncer.sync(svc, None, &cancel).await.unwrap();
    assert!(!stats.full);
    // Project 3 (active exactly at the watermark) comes back too, through the overlap.
    assert_eq!(
        (stats.synced, stats.added, stats.modified, stats.unmodified),
        (3, 1, 1, 1)
    );
    assert!(
        env.gitlab
            .requests()
            .iter()
            .any(|r| r.contains("last_activity_after=2026-01-02T23%3A55%3A00Z"))
    );
    let queued = env.queued().await;
    assert_eq!(queued.len(), 2);
    assert_eq!((queued[0].1, queued[0].2.as_str()), (20, "activity"));
    assert_eq!((queued[1].1, queued[1].2.as_str()), (10, "discovered"));

    // Project 3 disappears; incremental syncs never delete, a full one does.
    env.gitlab.with(|s| s.projects.retain(|p| p["id"] != 3));
    let stats = syncer.sync(svc, None, &cancel).await.unwrap();
    assert_eq!(stats.deleted, 0);
    assert_eq!(env.live_names().await.len(), 4);
    sourcegull_db::sqlx::query(
        "UPDATE external_services SET last_full_sync_at = now() - interval '2 hours'",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    let stats = syncer.sync(svc, None, &cancel).await.unwrap();
    assert!(stats.full);
    assert_eq!((stats.synced, stats.deleted, stats.unmodified), (3, 1, 3));
    assert!(
        !env.live_names()
            .await
            .contains(&format!("{host}/acme/docs"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_registers_webhooks() {
    let Some(env) = setup().await else { return };
    let svc = env.service(r#", "webhooks.autoRegister": true"#).await;
    let uuid = env
        .db
        .external_services(None)
        .get(svc)
        .await
        .unwrap()
        .unwrap()
        .webhook_uuid;
    env.gitlab.with(|s| {
        s.hooks_forbidden.insert(3);
    });

    // A local externalURL: nothing is registered, the sync warns and succeeds.
    let warnings = env
        .sync_job(&env.syncer_at(Some("http://localhost:3080")), svc)
        .await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("local address"), "{warnings:?}");
    assert!(env.gitlab.with(|s| s.hook_writes.is_empty()));

    // Created on every project; 403 on project 3 is a warning naming it.
    let syncer = env.syncer_at(Some("https://sourcegull.example.com/"));
    env.force_full().await;
    let warnings = env.sync_job(&syncer, svc).await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("acme/docs") && warnings[0].contains("Maintainer"),
        "{warnings:?}"
    );
    let url = env.gitlab.with(|s| {
        assert_eq!(s.hook_writes, ["POST 1", "POST 2"]);
        s.hooks[&1][0]["url"].as_str().unwrap().to_owned()
    });
    assert!(
        url.starts_with(&format!(
            "https://sourcegull.example.com/.api/webhooks/{uuid}?v="
        )),
        "{url}"
    );
    env.gitlab.with(|s| {
        assert_eq!(s.hooks[&1][0]["token"], "hooksecret");
        assert_eq!(s.hooks[&1][0]["push_events"], true);
        assert_eq!(s.hooks[&1][0]["merge_requests_events"], true);
    });

    // A repeated full sync changes nothing; drift (events turned off in GitLab) is repaired.
    env.gitlab.with(|s| {
        s.hook_writes.clear();
        s.hooks.get_mut(&2).unwrap()[0]["tag_push_events"] = serde_json::json!(false);
    });
    env.force_full().await;
    env.sync_job(&syncer, svc).await;
    env.gitlab.with(|s| {
        assert_eq!(s.hook_writes, ["PUT 2"]);
        assert_eq!(s.hooks[&2][0]["tag_push_events"], true);
    });

    // A new secret changes the hook URL: every hook is updated with the new token.
    let store = env.db.external_services(Some(env.keyring.clone()));
    let config = store
        .config(svc)
        .await
        .unwrap()
        .replace("hooksecret", "newsecret");
    store
        .update(
            svc,
            &sourcegull_db::ExternalServiceUpdate {
                display_name: None,
                config: Some(config),
                config_keeps_sync: false,
            },
        )
        .await
        .unwrap();
    env.gitlab.with(|s| s.hook_writes.clear());
    env.sync_job(&syncer, svc).await;
    env.gitlab.with(|s| {
        assert_eq!(s.hook_writes, ["PUT 1", "PUT 2"]);
        assert_eq!(s.hooks[&1][0]["token"], "newsecret");
        assert_ne!(s.hooks[&1][0]["url"].as_str().unwrap(), url);
    });

    // A project that leaves the connection loses its hook.
    env.gitlab.with(|s| {
        s.projects.retain(|p| p["id"] != 2);
        s.hook_writes.clear();
    });
    env.force_full().await;
    env.sync_job(&syncer, svc).await;
    env.gitlab.with(|s| {
        assert_eq!(s.hook_writes, ["DELETE 2"]);
        assert!(s.hooks[&2].is_empty());
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_listing_records_error_and_deletes_nothing() {
    let Some(env) = setup().await else { return };
    let svc = env.service("").await;
    let syncer = env.syncer();
    let cancel = CancellationToken::new();
    syncer.sync(svc, None, &cancel).await.unwrap();

    // Now project 2 is gone and an explicit project is missing: the listing is incomplete.
    let store = env.db.external_services(Some(env.keyring.clone()));
    let config = store.config(svc).await.unwrap().replace(
        r#""token": "tok""#,
        r#""token": "tok", "projects": [{"id": 404}]"#,
    );
    store
        .update(
            svc,
            &sourcegull_db::ExternalServiceUpdate {
                config: Some(config),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.gitlab.with(|s| s.projects.retain(|p| p["id"] != 2));
    let err = syncer.sync(svc, None, &cancel).await.unwrap_err();
    let SyncError::Partial { stats, message } = err else {
        panic!("expected a partial sync, got {err:?}");
    };
    assert!(stats.full);
    assert_eq!(stats.deleted, 0);
    assert!(message.contains("404"), "{message}");
    assert_eq!(env.live_names().await.len(), 3);
    let s = store.get(svc).await.unwrap().unwrap();
    assert!(s.last_sync_error.unwrap().contains("nothing was deleted"));
    assert!(s.last_full_sync_at.is_none());

    // A bad token fails the whole sync.
    env.gitlab.with(|s| s.token = "rotated".into());
    let err = syncer.sync(svc, None, &cancel).await.unwrap_err();
    assert!(err.to_string().contains("unauthorized"), "{err}");
}

fn hook_headers(token: &str, event: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-gitlab-token", HeaderValue::from_str(token).unwrap());
    h.insert("x-gitlab-event", HeaderValue::from_str(event).unwrap());
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_webhooks_record_ref_events_and_request_fetches() {
    let Some(env) = setup().await else { return };
    let svc = env.service(r#", "systemHook": true"#).await;
    env.syncer()
        .sync(svc, None, &CancellationToken::new())
        .await
        .unwrap();
    env.clear_queue().await;
    let uuid = env
        .db
        .external_services(None)
        .get(svc)
        .await
        .unwrap()
        .unwrap()
        .webhook_uuid;
    let ingest = env.ingest();
    let push = br#"{"object_kind": "push", "before": "aaa", "after": "bbb",
        "ref": "refs/heads/main", "project_id": 2, "project": {"id": 2}}"#;

    // Wrong secret, unknown UUID, malformed body.
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("nope", "Push Hook"),
            push,
        )
        .await;
    assert_eq!(out.status, 401);
    let out = ingest
        .handle(
            WebhookTarget::Uuid("00000000-0000-0000-0000-000000000000"),
            &hook_headers("hooksecret", "Push Hook"),
            push,
        )
        .await;
    assert_eq!(out.status, 404);
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("hooksecret", "Push Hook"),
            b"{",
        )
        .await;
    assert_eq!(out.status, 400);
    assert!(env.queued().await.is_empty());

    // A valid push: ref event plus an immediate fetch.
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("hooksecret", "Push Hook"),
            push,
        )
        .await;
    assert_eq!(out.status, 200, "{}", out.message);
    assert_eq!(out.external_service, Some(svc));
    assert!(matches!(out.events[0], DomainEvent::RefsUpdated { .. }));
    let repo = env
        .db
        .repos()
        .get_by_external_id("gitlab", &format!("{}/", env.gitlab.url), "2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.queued().await, [(repo.id, 30, "webhook".into())]);
    let events = env.db.ref_events().list_after(0, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].repo_id, repo.id);
    assert_eq!(events[0].old_oid.as_deref(), Some("aaa"));
    assert_eq!(events[0].new_oid.as_deref(), Some("bbb"));

    // The legacy endpoint finds the connection by its secret; pushes coalesce.
    let out = ingest
        .handle(
            WebhookTarget::GitLab {
                external_service: None,
            },
            &hook_headers("hooksecret", "Push Hook"),
            push,
        )
        .await;
    assert_eq!(out.status, 200);
    assert_eq!(env.queued().await.len(), 1);
    let out = ingest
        .handle(
            WebhookTarget::GitLab {
                external_service: Some(ExternalServiceId(svc.get() + 1)),
            },
            &hook_headers("hooksecret", "Push Hook"),
            push,
        )
        .await;
    assert_eq!(out.status, 401);

    // A push for a repo we do not mirror is accepted and ignored.
    let other =
        br#"{"object_kind": "push", "after": "c", "ref": "refs/heads/x", "project_id": 999}"#;
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("hooksecret", "Push Hook"),
            other,
        )
        .await;
    assert_eq!(out.status, 200);

    // A system hook rename queues a sync of the connection.
    sourcegull_db::sqlx::query("UPDATE external_service_sync_jobs SET state = 'completed'")
        .execute(env.db.pool())
        .await
        .unwrap();
    let rename = br#"{"event_name": "project_rename", "project_id": 2}"#;
    let out = ingest
        .handle(
            WebhookTarget::Uuid(&uuid),
            &hook_headers("hooksecret", "System Hook"),
            rename,
        )
        .await;
    assert_eq!(out.status, 200);
    let jobs = env.db.sync_jobs().list(svc, 1, 0).await.unwrap();
    assert_eq!(
        (jobs[0].state.as_str(), jobs[0].reason.as_str()),
        ("queued", "webhook")
    );

    // Every delivery is logged, without the secret.
    let logs = env.db.webhook_logs().list(None, 100).await.unwrap();
    assert_eq!(logs.len(), 8);
    assert!(
        logs.iter()
            .all(|l| !l.request.to_string().contains("hooksecret"))
    );
    assert_eq!(logs[0].event.as_deref(), Some("System Hook"));
    assert_eq!(logs.iter().filter(|l| l.status_code == 401).count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_worker_processes_queued_syncs() {
    let Some(env) = setup().await else { return };
    let svc = env.service("").await; // creating queues a sync
    let (worker, resetter) =
        sourcegull_repos::worker::sync_workers(&env.db, env.syncer(), "test-worker").unwrap();
    let cancel = CancellationToken::new();
    let w = tokio::spawn(worker.run(cancel.clone()));
    let r = tokio::spawn(resetter.run(cancel.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let jobs = env.db.sync_jobs().list(svc, 1, 0).await.unwrap();
        if jobs[0].state == "completed" {
            assert_eq!(jobs[0].repos_added, 3);
            assert_eq!(jobs[0].full_sync, Some(true));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sync did not complete: {jobs:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(env.live_names().await.len(), 3);

    // The scheduler queues nothing until the interval passes, then queues one sync.
    assert!(env.db.sync_jobs().enqueue_due().await.unwrap().is_empty());
    sourcegull_db::sqlx::query("UPDATE external_services SET next_sync_at = now()")
        .execute(env.db.pool())
        .await
        .unwrap();
    assert_eq!(env.db.sync_jobs().enqueue_due().await.unwrap(), [svc]);
    sourcegull_repos::worker::prune(&env.db).await;
    cancel.cancel();
    w.await.unwrap();
    r.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_system_hooks_queue_permissions_syncs() {
    let Some(env) = setup().await else { return };
    let svc = env
        .service(
            r#", "systemHook": true,
                "authorization": {"identityProvider": {"type": "username"}}"#,
        )
        .await;
    let users = env.db.users();
    let mut ids = Vec::new();
    for name in ["linked", "alice"] {
        let u = users
            .create(&sourcegull_db::NewUser {
                username: name.into(),
                display_name: None,
                avatar_url: None,
                password_hash: None,
            })
            .await
            .unwrap();
        ids.push(u.id);
    }
    let service_id = format!("{}/", env.gitlab.url);
    env.db
        .external_accounts(Some(env.keyring.clone()))
        .link(
            ids[0],
            &sourcegull_db::LinkAccount {
                spec: sourcegull_db::AccountSpec {
                    service_type: "gitlab",
                    service_id: &service_id,
                    account_id: "41",
                },
                client_id: "",
                account_data: serde_json::json!({"username": "linked-on-gitlab"}),
                auth_data: None,
            },
        )
        .await
        .unwrap();
    let ingest = env.ingest();
    let target = WebhookTarget::GitLab {
        external_service: Some(svc),
    };
    let headers = hook_headers("hooksecret", "System Hook");
    let queued = || async {
        let jobs = env
            .db
            .perms_sync_jobs()
            .list(&sourcegull_db::ListPermsSyncJobs {
                state: Some("queued".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut v: Vec<_> = jobs
            .into_iter()
            .map(|j| (j.user_id.unwrap(), j.reason))
            .collect();
        v.sort();
        v
    };

    // By linked account; then by username; an unknown user is ignored.
    let by_account = br#"{"event_name": "user_add_to_group", "user_id": 41, "group_id": 1}"#;
    let out = ingest.handle(target, &headers, by_account).await;
    assert_eq!(out.status, 200, "{}", out.message);
    assert_eq!(queued().await, [(ids[0], "webhook".to_string())]);
    let by_name = br#"{"event_name": "user_add_to_team", "user_id": 99,
        "user_username": "Alice", "project_id": 2}"#;
    let out = ingest.handle(target, &headers, by_name).await;
    assert_eq!(out.status, 200, "{}", out.message);
    let unknown = br#"{"event_name": "user_remove_from_team", "user_id": 100,
        "user_username": "nobody", "project_id": 2}"#;
    let out = ingest.handle(target, &headers, unknown).await;
    assert_eq!(out.status, 200, "{}", out.message);
    assert_eq!(
        queued().await,
        [
            (ids[0], "webhook".to_string()),
            (ids[1], "webhook".to_string())
        ]
    );
}
