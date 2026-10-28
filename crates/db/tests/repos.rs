//! Code host connection and repo store tests against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`). See `db.rs` for why tests use a multi-threaded runtime.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use chrono::{Duration, TimeZone, Utc};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    CodeHostRepoHints, Db, DbError, ExternalServiceUpdate, FetchResult, IndexedState,
    ListReposOptions, ListedRepo, NewExternalService, NewWebhookLog, RefEventSource, RefUpdate,
    RepoOrder, RepoUpdatePriority, RepoUpdateReason, SyncOutcome, SyncReason, migrate,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::{
    CloneStatus, ExternalServiceId, ExternalServiceKind, RemoteUnavailable, RepoId, Visibility,
};

async fn setup() -> Option<(TestDb, Db, Arc<Keyring>)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    Some((t, db, keyring))
}

async fn service(db: &Db, keyring: &Arc<Keyring>, name: &str) -> ExternalServiceId {
    db.external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: name.into(),
            config: format!(r#"{{"url": "https://gitlab.example.com", "token": "secret-{name}"}}"#),
        })
        .await
        .unwrap()
        .id
}

fn listed(id: u32, name: &str) -> ListedRepo {
    ListedRepo {
        name: name.into(),
        uri: name.into(),
        url: format!("https://{name}"),
        description: format!("repo {id}"),
        fork: false,
        archived: false,
        private: true,
        visibility: Visibility::Private,
        stars: 1,
        topics: vec!["rust".into()],
        default_branch: Some("main".into()),
        last_activity_at: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        external_id: id.to_string(),
        external_service_type: "gitlab".into(),
        external_service_id: "https://gitlab.example.com/".into(),
        metadata: serde_json::json!({ "id": id }),
        clone_url: format!("https://{name}.git"),
    }
}

async fn queued_jobs(db: &Db) -> Vec<(RepoId, i16, String)> {
    sourcegull_db::sqlx::query_as(
        "SELECT repo_id, priority, reason FROM repo_update_jobs WHERE state = 'queued' ORDER BY repo_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyring_is_created_once_and_reloaded() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let keys = db.encryption_keys().list().await.unwrap();
    assert_eq!(keys.len(), sourcegull_crypto::RecordClass::ALL.len());
    // A second process loads the same keys and can decrypt what the first encrypted.
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let again = db.encryption_keys().load_keyring(provider).await.unwrap();
    assert_eq!(db.encryption_keys().list().await.unwrap(), keys);
    let class = sourcegull_crypto::RecordClass::ExternalServiceConfig;
    let blob = keyring.encrypt(class, b"hello").unwrap();
    assert_eq!(&again.decrypt(class, &blob).unwrap()[..], b"hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_service_crud_encrypts_config() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let store = db.external_services(Some(keyring.clone()));
    let id = service(&db, &keyring, "one").await;
    let svc = store.get(id).await.unwrap().unwrap();
    assert_eq!(svc.kind, ExternalServiceKind::GitLab);
    assert_eq!(svc.display_name, "one");
    assert_eq!(svc.webhook_uuid.len(), 36);
    assert_eq!(
        store
            .get_by_webhook_uuid(&svc.webhook_uuid)
            .await
            .unwrap()
            .map(|s| s.id),
        Some(id)
    );

    // Stored encrypted; decrypted on demand.
    let raw: Vec<u8> =
        sourcegull_db::sqlx::query_scalar("SELECT config_enc FROM external_services")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("secret-one"));
    assert!(store.config(id).await.unwrap().contains("secret-one"));
    let without_keys = db.external_services(None);
    assert!(matches!(
        without_keys.config(id).await,
        Err(DbError::Invalid(_))
    ));
    assert_eq!(without_keys.count(None).await.unwrap(), 1);

    // Creating queued the first sync; a second enqueue coalesces.
    let jobs = db.sync_jobs().list(id, 10, 0).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].reason, "config_changed");
    assert!(
        db.sync_jobs()
            .enqueue(id, SyncReason::Manual)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(db.sync_jobs().count(id).await.unwrap(), 1);

    // Canceling a queued run finishes it at once; a finished run cannot be canceled.
    let first = jobs[0].id;
    assert!(db.sync_jobs().cancel(first).await.unwrap());
    let canceled = db.sync_jobs().get(first).await.unwrap().unwrap();
    assert_eq!(canceled.state, "canceled");
    assert!(canceled.cancel && canceled.finished_at.is_some());
    assert!(!db.sync_jobs().cancel(first).await.unwrap());
    let again = db
        .sync_jobs()
        .enqueue(id, SyncReason::Manual)
        .await
        .unwrap()
        .expect("a new run can be queued after a cancel");
    let page = db.sync_jobs().list(id, 1, 1).await.unwrap();
    assert_eq!(page[0].id, first, "newest first, offset skips {again:?}");

    // Recording a successful full sync sets the watermark; a config change resets it.
    let watermark = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
    store
        .record_sync(
            id,
            &SyncOutcome {
                full: true,
                watermark: Some(watermark),
                error: None,
                next_sync_at: Utc::now() + Duration::minutes(1),
            },
        )
        .await
        .unwrap();
    let svc = store.get(id).await.unwrap().unwrap();
    assert_eq!(svc.sync_watermark, Some(watermark));
    assert!(svc.last_full_sync_at.is_some() && svc.last_sync_error.is_none());
    store
        .record_sync(
            id,
            &SyncOutcome {
                error: Some("boom".into()),
                next_sync_at: Utc::now(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let svc = store.get(id).await.unwrap().unwrap();
    assert_eq!(svc.last_sync_error.as_deref(), Some("boom"));
    assert_eq!(svc.sync_watermark, Some(watermark), "kept on failure");

    let svc = store
        .update(
            id,
            &ExternalServiceUpdate {
                display_name: Some("renamed".into()),
                config: Some(r#"{"url": "https://gitlab.example.com", "token": "t2"}"#.into()),
                config_keeps_sync: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(svc.display_name, "renamed");
    assert!(svc.sync_watermark.is_none() && svc.last_full_sync_at.is_none());
    assert!(store.config(id).await.unwrap().contains("t2"));

    let with_config = store
        .list_with_config(ExternalServiceKind::GitLab)
        .await
        .unwrap();
    assert_eq!(with_config.len(), 1);
    assert!(with_config[0].1.contains("t2"));

    store.soft_delete(id).await.unwrap();
    assert!(store.get(id).await.unwrap().is_none());
    assert!(store.soft_delete(id).await.unwrap_err().is_not_found());
    assert_eq!(store.count(None).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_due_skips_services_with_pending_syncs() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let a = service(&db, &keyring, "a").await;
    let b = service(&db, &keyring, "b").await;
    // Both have a queued config_changed sync already.
    assert!(db.sync_jobs().enqueue_due().await.unwrap().is_empty());
    sourcegull_db::sqlx::query(
        "UPDATE external_service_sync_jobs SET state = 'completed', finished_at = now()",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE external_services SET next_sync_at = now() + interval '1 hour' WHERE id = $1",
    )
    .bind(b)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(db.sync_jobs().enqueue_due().await.unwrap(), [a]);
    assert!(db.sync_jobs().enqueue_due().await.unwrap().is_empty());
    let deleted = db
        .sync_jobs()
        .delete_finished_before(Utc::now() + Duration::seconds(1), 0)
        .await
        .unwrap();
    assert_eq!(deleted, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_listed_tracks_adds_renames_activity_and_deletes() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let svc = service(&db, &keyring, "one").await;
    let other = service(&db, &keyring, "two").await;
    let repos = db.repos();

    let first = repos
        .upsert_listed(
            svc,
            &[
                listed(1, "gitlab.example.com/a"),
                listed(2, "gitlab.example.com/b"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(first.added.len(), 2);
    assert_eq!(first.fetches_queued, first.ids);
    let a = first.ids[0];
    let b = first.ids[1];
    assert_eq!(
        queued_jobs(&db).await,
        [(a, 10, "discovered".into()), (b, 10, "discovered".into())]
    );
    let gs = db.gitserver_repos().get(a).await.unwrap().unwrap();
    assert_eq!(gs.clone_status, CloneStatus::NotCloned);

    // Unchanged listing: nothing modified, no new jobs.
    let same = repos
        .upsert_listed(
            svc,
            &[
                listed(1, "gitlab.example.com/a"),
                listed(2, "gitlab.example.com/b"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(same.unmodified, [a, b]);
    assert!(same.fetches_queued.is_empty());

    // Rename (same external id) keeps the id; newer activity raises the queued job to HIGH.
    let mut renamed = listed(1, "gitlab.example.com/a2");
    renamed.last_activity_at = Some(Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap());
    let changed = repos.upsert_listed(svc, &[renamed]).await.unwrap();
    assert_eq!(changed.modified, [a]);
    assert_eq!(changed.fetches_queued, [a]);
    assert!(
        repos
            .get_by_name("gitlab.example.com/a")
            .await
            .unwrap()
            .is_none()
    );
    let got = repos
        .get_by_name("GITLAB.example.com/A2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.id, a);
    assert_eq!(got.visibility, Visibility::Private);
    assert_eq!(got.topics, ["rust"]);
    assert_eq!(queued_jobs(&db).await[0], (a, 20, "activity".into()));

    // A different external id taking b's name displaces b.
    let taker = repos
        .upsert_listed(svc, &[listed(3, "gitlab.example.com/b")])
        .await
        .unwrap();
    assert_eq!(taker.displaced, [b]);
    assert!(repos.get(b).await.unwrap().is_none());

    // b comes back under its own name: same id, revived.
    let back = repos
        .upsert_listed(svc, &[listed(2, "gitlab.example.com/b-again")])
        .await
        .unwrap();
    assert_eq!(back.added, [b]);
    assert!(repos.get(b).await.unwrap().is_some());

    // Another connection lists `a` too; unlisting from the first does not delete it.
    repos
        .upsert_listed(other, &[listed(1, "gitlab.example.com/a2")])
        .await
        .unwrap();
    let all = repos.list(&ListReposOptions::default()).await.unwrap();
    assert_eq!(all.len(), 3);
    let c = taker.ids[0];
    assert_eq!(
        repos.unlink_unlisted(svc, &[c]).await.unwrap(),
        1,
        "b deleted"
    );
    assert!(
        repos.get(a).await.unwrap().is_some(),
        "still listed by other"
    );
    assert!(repos.get(b).await.unwrap().is_none());
    assert_eq!(repos.sources(a).await.unwrap().len(), 1);

    // Deleting the other connection deletes a.
    db.external_services(Some(keyring.clone()))
        .soft_delete(other)
        .await
        .unwrap();
    assert!(repos.get(a).await.unwrap().is_none());
    // Queued fetches of deleted repos are canceled.
    let queued: Vec<RepoId> = queued_jobs(&db).await.into_iter().map(|j| j.0).collect();
    assert_eq!(queued, [c]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_filters_and_order() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let svc = service(&db, &keyring, "one").await;
    let up = db
        .repos()
        .upsert_listed(
            svc,
            &[
                listed(1, "gitlab.example.com/zeta"),
                listed(2, "gitlab.example.com/Alpha_1"),
                listed(3, "gitlab.example.com/beta"),
            ],
        )
        .await
        .unwrap();
    let (zeta, alpha, beta) = (up.ids[0], up.ids[1], up.ids[2]);
    let gs = db.gitserver_repos();
    gs.set_cloning(zeta, "repo-0").await.unwrap();
    gs.record_fetched(
        zeta,
        &FetchResult {
            changed: true,
            head_oid: Some("abc".into()),
            size_bytes: Some(1024),
            node: "repo-0".into(),
            remote_unavailable: None,
        },
    )
    .await
    .unwrap();
    gs.set_cloning(beta, "repo-1").await.unwrap();
    gs.record_error(beta, "repo-1", "auth failed")
        .await
        .unwrap();

    let names =
        |repos: Vec<sourcegull_types::Repo>| repos.into_iter().map(|r| r.id).collect::<Vec<_>>();
    let store = db.repos();
    assert_eq!(
        names(store.list(&ListReposOptions::default()).await.unwrap()),
        [alpha, beta, zeta]
    );
    let opts = ListReposOptions {
        descending: true,
        ..Default::default()
    };
    assert_eq!(names(store.list(&opts).await.unwrap()), [zeta, beta, alpha]);
    let opts = ListReposOptions {
        query: Some("A_1".into()),
        ..Default::default()
    };
    assert_eq!(names(store.list(&opts).await.unwrap()), [alpha]);
    let opts = ListReposOptions {
        clone_status: Some(CloneStatus::Cloned),
        ..Default::default()
    };
    assert_eq!(names(store.list(&opts).await.unwrap()), [zeta]);
    let opts = ListReposOptions {
        clone_status: Some(CloneStatus::NotCloned),
        ..Default::default()
    };
    assert_eq!(store.count(&opts).await.unwrap(), 2);
    let opts = ListReposOptions {
        failed_fetch: true,
        ..Default::default()
    };
    assert_eq!(names(store.list(&opts).await.unwrap()), [beta]);
    let opts = ListReposOptions {
        order: RepoOrder::Size,
        descending: true,
        limit: 1,
        ..Default::default()
    };
    assert_eq!(names(store.list(&opts).await.unwrap()), [zeta]);
    let opts = ListReposOptions {
        external_service_id: Some(svc),
        ids: Some(vec![alpha, zeta]),
        ..Default::default()
    };
    assert_eq!(store.count(&opts).await.unwrap(), 2);

    let z = gs.get(zeta).await.unwrap().unwrap();
    assert_eq!(z.clone_status, CloneStatus::Cloned);
    assert_eq!(z.head_oid.as_deref(), Some("abc"));
    assert!(z.last_fetched.is_some() && z.last_changed.is_some());
    let b = gs.get(beta).await.unwrap().unwrap();
    assert_eq!(b.clone_status, CloneStatus::NotCloned);
    assert_eq!(b.last_error.as_deref(), Some("auth failed"));
    assert_eq!(gs.get_many(&[zeta, beta]).await.unwrap().len(), 2);

    store.set_kvp(alpha, "team", Some("search")).await.unwrap();
    store.set_kvp(alpha, "tag", None).await.unwrap();
    store.set_kvp(alpha, "team", Some("infra")).await.unwrap();
    assert_eq!(
        store.kvps(alpha).await.unwrap(),
        [("tag".into(), None), ("team".into(), Some("infra".into()))]
    );
    assert!(store.delete_kvp(alpha, "tag").await.unwrap());
    assert!(!store.delete_kvp(alpha, "tag").await.unwrap());
    assert!(!store.add_kvp(alpha, "team", None).await.unwrap());
    assert!(store.add_kvp(beta, "go", None).await.unwrap());
    assert!(store.update_kvp(alpha, "team", Some("web")).await.unwrap());
    assert!(!store.update_kvp(alpha, "missing", None).await.unwrap());
    assert_eq!(
        store.all_kvps().await.unwrap(),
        [
            (alpha, "team".into(), Some("web".into())),
            (beta, "go".into(), None)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_update_jobs_coalesce_and_ref_events() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let svc = service(&db, &keyring, "one").await;
    let up = db
        .repos()
        .upsert_listed(svc, &[listed(1, "gitlab.example.com/a")])
        .await
        .unwrap();
    let a = up.ids[0];
    let jobs = db.repo_update_jobs();
    let first = jobs
        .enqueue(a, RepoUpdatePriority::IMMEDIATE, RepoUpdateReason::Webhook)
        .await
        .unwrap();
    let second = jobs
        .enqueue(a, RepoUpdatePriority::LOW, RepoUpdateReason::Scheduled)
        .await
        .unwrap();
    assert_eq!(first, second, "coalesced into the queued row");
    assert_eq!(queued_jobs(&db).await, [(a, 30, "webhook".into())]);
    assert_eq!(jobs.queued_count().await.unwrap(), 1);

    // Once processing, a new request queues a second row.
    let queue = sourcegull_pgqueue::Queue::<sourcegull_db::RepoUpdateJob>::new(
        db.pool().clone(),
        sourcegull_db::RepoUpdateJobStore::queue_config(),
    )
    .unwrap();
    let taken = queue.dequeue("node-0", 1).await.unwrap();
    assert_eq!(taken[0].payload.repo_id, a);
    assert_eq!(taken[0].payload.priority, 30);
    let third = jobs
        .enqueue(a, RepoUpdatePriority::IMMEDIATE, RepoUpdateReason::Webhook)
        .await
        .unwrap();
    assert_ne!(third, first);
    assert_eq!(jobs.list_for_repo(a, 10).await.unwrap().len(), 2);

    let events = db.ref_events();
    let ids = events
        .record(
            a,
            &[RefUpdate {
                name: "refs/heads/main".into(),
                old_oid: Some("aaa".into()),
                new_oid: Some("bbb".into()),
            }],
            RefEventSource::Webhook,
        )
        .await
        .unwrap();
    let listed = events.list_after(0, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ids[0]);
    assert_eq!(listed[0].ref_name, "refs/heads/main");
    assert_eq!(listed[0].source, "webhook");
    assert!(events.list_after(ids[0], 10).await.unwrap().is_empty());
    assert_eq!(
        events
            .delete_before(Utc::now() + Duration::seconds(1))
            .await
            .unwrap(),
        1
    );

    let logs = db.webhook_logs();
    logs.insert(&NewWebhookLog {
        external_service_id: Some(svc),
        event: Some("Push Hook".into()),
        status_code: 200,
        request: serde_json::json!({ "body": {} }),
        error: None,
    })
    .await
    .unwrap();
    assert_eq!(logs.list(Some(svc), 10).await.unwrap().len(), 1);
    assert_eq!(logs.list(None, 10).await.unwrap()[0].status_code, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_host_hints_read_listed_metadata() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let svc = service(&db, &keyring, "one").await;
    let mut empty = listed(2, "gitlab.example.com/empty");
    empty.metadata = serde_json::json!({ "id": 2, "emptyRepo": true });
    let mut disabled = listed(3, "gitlab.example.com/disabled");
    disabled.metadata = serde_json::json!({
        "id": 3, "emptyRepo": false, "repositoryAccessLevel": "disabled", "accessLevel": 10,
    });
    let up = db
        .repos()
        .upsert_listed(
            svc,
            &[listed(1, "gitlab.example.com/full"), empty, disabled],
        )
        .await
        .unwrap();
    let repos = db.repos();
    assert_eq!(
        repos.code_host_hints(up.ids[0]).await.unwrap(),
        CodeHostRepoHints::default()
    );
    assert!(repos.code_host_hints(up.ids[1]).await.unwrap().empty);
    assert_eq!(
        repos.code_host_hints(up.ids[2]).await.unwrap(),
        CodeHostRepoHints {
            empty: false,
            repository_disabled: true,
            access_level: Some(10),
        }
    );
    assert_eq!(
        repos.code_host_hints(RepoId(999_999)).await.unwrap(),
        CodeHostRepoHints::default()
    );

    // An empty clone records why; the first real fetch clears it.
    let gs = db.gitserver_repos();
    let fetched = |head: Option<&str>, why| FetchResult {
        changed: head.is_some(),
        head_oid: head.map(str::to_owned),
        size_bytes: Some(1),
        node: "repo-0".into(),
        remote_unavailable: why,
    };
    gs.record_error(up.ids[1], "repo-0", "git fetch failed")
        .await
        .unwrap();
    gs.record_fetched(up.ids[1], &fetched(None, Some(RemoteUnavailable::Missing)))
        .await
        .unwrap();
    let row = gs.get(up.ids[1]).await.unwrap().unwrap();
    assert_eq!(row.clone_status, CloneStatus::Cloned);
    assert_eq!(row.remote_unavailable, Some(RemoteUnavailable::Missing));
    assert_eq!(row.last_error, None);
    gs.record_fetched(
        up.ids[2],
        &fetched(None, Some(RemoteUnavailable::NoCodeAccess)),
    )
    .await
    .unwrap();
    let row = gs.get(up.ids[2]).await.unwrap().unwrap();
    assert_eq!(
        row.remote_unavailable,
        Some(RemoteUnavailable::NoCodeAccess)
    );
    let stats = db.search_index().repository_stats().await.unwrap();
    assert_eq!(stats.no_code_access, 1);
    assert_eq!(stats.failed_fetch, 0);
    gs.record_fetched(up.ids[1], &fetched(Some("abc"), None))
        .await
        .unwrap();
    let row = gs.get(up.ids[1]).await.unwrap().unwrap();
    assert_eq!(row.remote_unavailable, None);
    assert_eq!(row.head_oid.as_deref(), Some("abc"));

    // A fetch that ends on repo-0 after the repository moved to repo-1 records nothing.
    sqlx::query("UPDATE repo SET shard = 'repo-1' WHERE id = $1")
        .bind(up.ids[1])
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        !gs.record_fetched(up.ids[1], &fetched(Some("def"), None))
            .await
            .unwrap()
    );
    let row = gs.get(up.ids[1]).await.unwrap().unwrap();
    assert_eq!(row.head_oid.as_deref(), Some("abc"));
    assert_eq!(row.node.as_deref(), Some("repo-0"));
    // Nor does a clone that starts there.
    assert!(!gs.set_cloning(up.ids[1], "repo-0").await.unwrap());
    let row = gs.get(up.ids[1]).await.unwrap().unwrap();
    assert_eq!(row.clone_status, CloneStatus::Cloned);
    // Nor do an error or a cancelled clone that end there.
    assert!(!gs.record_error(up.ids[1], "repo-0", "late").await.unwrap());
    assert!(gs.set_cloning(up.ids[1], "repo-1").await.unwrap());
    assert!(!gs.reset_cloning(up.ids[1], "repo-0").await.unwrap());
    let row = gs.get(up.ids[1]).await.unwrap().unwrap();
    assert_eq!(row.clone_status, CloneStatus::Cloning);
    assert_eq!(row.last_error, None);
    assert!(gs.reset_cloning(up.ids[1], "repo-1").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_rows_are_written_only_by_the_assigned_node() {
    let Some((_t, db, keyring)) = setup().await else {
        return;
    };
    let svc = service(&db, &keyring, "one").await;
    let up = db
        .repos()
        .upsert_listed(svc, &[listed(1, "gitlab.example.com/moving")])
        .await
        .unwrap();
    let id = up.ids[0];
    let store = db.search_index();
    let state = |docs| IndexedState {
        docs,
        shards: 1,
        ..IndexedState::default()
    };
    let set_shard = |shard: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query("UPDATE repo SET shard = $2 WHERE id = $1")
                .bind(id)
                .bind(shard)
                .execute(db.pool())
                .await
                .unwrap();
        }
    };

    // Unassigned and assigned repos are written by their node.
    assert!(store.record_indexed(id, "repo-0", &state(1)).await.unwrap());
    set_shard("repo-0").await;
    assert!(store.record_indexed(id, "repo-0", &state(2)).await.unwrap());

    // Once the repo is assigned elsewhere, the old node's writes are skipped.
    set_shard("repo-1").await;
    assert!(!store.record_indexed(id, "repo-0", &state(3)).await.unwrap());
    assert!(!store.record_not_indexed(id, "repo-0").await.unwrap());
    assert!(!store.record_error(id, "repo-0", "boom").await.unwrap());
    let row = store.get(id).await.unwrap().unwrap();
    assert_eq!((row.node.as_str(), row.docs), ("repo-0", 2));
    assert!(row.indexed && row.last_error.is_none());
    assert!(store.record_indexed(id, "repo-1", &state(4)).await.unwrap());

    // A flip that commits while a write waits on it: the write sees the new owner and skips.
    set_shard("repo-0").await;
    let mut flip = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE repo SET shard = 'repo-1' WHERE id = $1")
        .bind(id)
        .execute(&mut *flip)
        .await
        .unwrap();
    let write = tokio::spawn({
        let store = store.clone();
        async move { store.record_indexed(id, "repo-0", &state(5)).await.unwrap() }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!write.is_finished(), "the write waits for the flip");
    sqlx::query("UPDATE search_index_repos SET node = 'repo-1' WHERE repo_id = $1")
        .bind(id)
        .execute(&mut *flip)
        .await
        .unwrap();
    flip.commit().await.unwrap();
    assert!(!write.await.unwrap());
    let row = store.get(id).await.unwrap().unwrap();
    assert_eq!((row.node.as_str(), row.docs), ("repo-1", 4));
}
