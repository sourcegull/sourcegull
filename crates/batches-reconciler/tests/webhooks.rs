//! Changeset webhooks against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): GitLab `merge_request` and `pipeline` deliveries (fixtures
//! in `crates/codehost-gitlab/tests/fixtures/webhooks/`) go through `WebhookIngest` with
//! [`ChangesetWebhooks`] and update the matching changeset: states, title, labels, timeline
//! events, check state from the newest pipeline, the next sync time; unknown merge requests
//! and repositories are ignored.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue};
use sourcegull_batches::store::{
    Changeset, ChangesetState, ChangesetStore, ChangesetSync, CheckState, ExternalState,
    NewChangeset, PublicationState, ReviewState,
};
use sourcegull_batches_reconciler::syncer::{MIN_SYNC_DELAY, WEBHOOK_MIN_SYNC_DELAY};
use sourcegull_batches_reconciler::{ChangesetWebhooks, DbSourceProvider, SourceProvider};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::sqlx;
use sourcegull_db::{Db, NewExternalService, migrate};
use sourcegull_repos::{Registry, WebhookIngest, WebhookTarget};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, RepoId};

const SERVICE_ID: &str = "https://gitlab.example.com/";

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../codehost-gitlab/tests/fixtures/webhooks/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `fixture` with `f` applied to its JSON.
fn edited(name: &str, f: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(&fixture(name)).unwrap();
    f(&mut v);
    serde_json::to_vec(&v).unwrap()
}

struct Env {
    _t: TestDb,
    db: Db,
    ingest: WebhookIngest,
    keyring: Arc<sourcegull_crypto::Keyring>,
    repo: RepoId,
    id: i64,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let svc = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: r#"{"url": "https://gitlab.example.com", "token": "t",
                        "webhooks": [{"secret": "s3"}]}"#
                .into(),
        })
        .await
        .unwrap();
    let repo: RepoId = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ('gitlab.example.com/a/one', '42', 'gitlab', $1) RETURNING id",
    )
    .bind(SERVICE_ID)
    .fetch_one(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO external_service_repos (external_service_id, repo_id, clone_url) \
         VALUES ($1, $2, 'https://gitlab.example.com/a/one.git')",
    )
    .bind(svc.id)
    .bind(repo)
    .execute(db.pool())
    .await
    .unwrap();

    // A published changeset as the reconciler left it: open merge request !1 on `fix`,
    // last synced with pipeline 3100 at its head.
    let store = ChangesetStore::new(&db);
    let ch = store
        .create(&NewChangeset {
            external_id: Some("1".into()),
            publication_state: PublicationState::Published,
            ..NewChangeset::new(repo, "gitlab")
        })
        .await
        .unwrap();
    sqlx::query("UPDATE changesets SET state = 'completed' WHERE id = $1")
        .bind(ch.id)
        .execute(db.pool())
        .await
        .unwrap();
    store
        .update_from_sync(
            ch.id,
            &ChangesetSync {
                external_id: Some("1".into()),
                external_branch: Some("refs/heads/fix".into()),
                external_state: Some(ExternalState::Open),
                external_review_state: Some(ReviewState::Pending),
                external_check_state: Some(CheckState::Pending),
                title: Some("Fix the readme".into()),
                body: Some("It says hello.".into()),
                metadata: serde_json::json!({
                    "mergeRequest": {"iid": 1, "title": "Fix the readme", "labels": []},
                    "headPipeline": {"id": 3100, "status": "running"},
                }),
                next_sync_at: Some(Utc::now() + chrono::Duration::hours(8)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ingest = WebhookIngest::new(db.clone(), Some(keyring.clone()), Registry::default())
        .with_changeset_hooks(Some(Arc::new(ChangesetWebhooks::new(db.clone()))));
    Some(Env {
        _t: t,
        db,
        ingest,
        keyring,
        repo,
        id: ch.id,
    })
}

impl Env {
    async fn deliver(&self, event: &str, body: &[u8]) {
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", HeaderValue::from_static("s3"));
        headers.insert("x-gitlab-event", HeaderValue::from_str(event).unwrap());
        let outcome = self
            .ingest
            .handle(
                WebhookTarget::GitLab {
                    external_service: None,
                },
                &headers,
                body,
            )
            .await;
        assert_eq!(outcome.status, 200, "{}", outcome.message);
    }

    async fn mr(&self, name: &str) {
        self.deliver("Merge Request Hook", &fixture(name)).await;
    }

    async fn pipeline(&self, name: &str) {
        self.deliver("Pipeline Hook", &fixture(name)).await;
    }

    async fn get(&self) -> Changeset {
        ChangesetStore::new(&self.db)
            .get(self.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// `(kind, key)` of the changeset's events, oldest first.
    async fn events(&self) -> Vec<(String, String)> {
        ChangesetStore::new(&self.db)
            .events(&[self.id], &[])
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.key))
            .collect()
    }
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The next sync is about `delay` from now.
fn assert_next_sync(c: &Changeset, delay: std::time::Duration) {
    let next = c.next_sync_at.unwrap();
    let want = Utc::now() + chrono::Duration::from_std(delay).unwrap();
    assert!(
        (next - want).num_seconds().abs() < 60,
        "next sync {next}, want about {want}"
    );
}

#[tokio::test]
async fn merge_request_events_update_the_changeset() {
    let Some(env) = setup().await else { return };

    env.mr("merge_request_open").await;
    let c = env.get().await;
    assert_eq!(c.state, ChangesetState::Open);
    assert_eq!(c.external_updated_at, Some(at("2026-10-19T09:00:00Z")));
    assert_eq!(c.metadata["mergeRequest"]["iid"], 1, "merged, not replaced");
    assert_eq!(
        c.metadata["mergeRequest"]["labels"][0]["name"],
        "batch-change"
    );
    assert_eq!(c.metadata["headPipeline"]["id"], 3100);
    // Webhooks arrive for this connection: the next sync is an hour out.
    assert_next_sync(&c, WEBHOOK_MIN_SYNC_DELAY);
    assert!(env.events().await.is_empty());

    env.mr("merge_request_update_draft").await;
    let c = env.get().await;
    assert_eq!(c.state, ChangesetState::Draft);
    assert_eq!(c.title.as_deref(), Some("Draft: Fix the readme"));
    assert_eq!(env.events().await[0].0, "MARKED_DRAFT");

    env.mr("merge_request_approved").await;
    let c = env.get().await;
    // The draft flag is false again in this payload.
    assert_eq!(c.state, ChangesetState::Open);
    assert_eq!(c.external_review_state, Some(ReviewState::Approved));
    env.mr("merge_request_unapproved").await;
    assert_eq!(
        env.get().await.external_review_state,
        Some(ReviewState::Pending)
    );

    // New commits: the payload cannot say everything, sync now.
    env.mr("merge_request_update_commits").await;
    assert_next_sync(&env.get().await, std::time::Duration::ZERO);

    env.mr("merge_request_close").await;
    let c = env.get().await;
    assert_eq!(c.state, ChangesetState::Closed);
    assert_eq!(c.external_state, Some(ExternalState::Closed));
    env.mr("merge_request_reopen").await;
    assert_eq!(env.get().await.state, ChangesetState::Open);
    env.mr("merge_request_merge").await;
    let c = env.get().await;
    assert_eq!(c.state, ChangesetState::Merged);
    assert_eq!(c.external_updated_at, Some(at("2026-10-19T12:00:00Z")));

    let kinds: Vec<String> = env.events().await.into_iter().map(|e| e.0).collect();
    assert_eq!(
        kinds,
        [
            "MARKED_DRAFT",
            "APPROVED",
            "UNAPPROVED",
            "CLOSED",
            "REOPENED",
            "MERGED"
        ]
    );
    // A redelivery upserts the same events.
    env.mr("merge_request_merge").await;
    assert_eq!(env.events().await.len(), 6);
    assert!(
        env.events()
            .await
            .iter()
            .all(|(_, k)| k.starts_with("hook:"))
    );

    // Every delivery is logged.
    let logged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_logs WHERE status_code = 200")
            .fetch_one(env.db.pool())
            .await
            .unwrap();
    assert_eq!(logged, 9);
}

#[tokio::test]
async fn pipeline_events_update_the_check_state() {
    let Some(env) = setup().await else { return };

    env.pipeline("pipeline_success").await;
    let c = env.get().await;
    assert_eq!(c.external_check_state, Some(CheckState::Passed));
    assert_eq!(c.metadata["headPipeline"]["id"], 3101);
    assert_eq!(c.metadata["mergeRequest"]["iid"], 1);
    assert_eq!(
        env.events().await,
        [("CHECK".to_owned(), "pipeline:3101".to_owned())]
    );
    assert_next_sync(&c, WEBHOOK_MIN_SYNC_DELAY);

    env.pipeline("pipeline_failed").await;
    assert_eq!(
        env.get().await.external_check_state,
        Some(CheckState::Failed)
    );

    // A late delivery for an older pipeline only updates its event.
    env.pipeline("pipeline_success").await;
    let c = env.get().await;
    assert_eq!(c.external_check_state, Some(CheckState::Failed));
    assert_eq!(c.metadata["headPipeline"]["id"], 3102);
    assert_eq!(env.events().await.len(), 2);

    // A pipeline without a merge request: found by its branch.
    env.pipeline("pipeline_branch_running").await;
    let c = env.get().await;
    assert_eq!(c.external_check_state, Some(CheckState::Pending));
    assert_eq!(c.metadata["headPipeline"]["id"], 3103);
    // A tag pipeline does not match a branch.
    env.deliver(
        "Pipeline Hook",
        &edited("pipeline_branch_running", |v| {
            v["object_attributes"]["id"] = 3104.into();
            v["object_attributes"]["tag"] = true.into();
            v["object_attributes"]["status"] = "success".into();
        }),
    )
    .await;
    assert_eq!(
        env.get().await.external_check_state,
        Some(CheckState::Pending)
    );
}

#[tokio::test]
async fn unknown_changesets_and_repos_are_ignored() {
    let Some(env) = setup().await else { return };
    let before = env.get().await;

    // Another merge request in the repository.
    env.deliver(
        "Merge Request Hook",
        &edited("merge_request_close", |v| {
            v["object_attributes"]["iid"] = 99.into();
        }),
    )
    .await;
    // A repository that is not mirrored.
    env.deliver(
        "Merge Request Hook",
        &edited("merge_request_close", |v| v["project"]["id"] = 7.into()),
    )
    .await;
    env.deliver(
        "Pipeline Hook",
        &edited("pipeline_failed", |v| v["merge_request"]["iid"] = 99.into()),
    )
    .await;
    env.deliver(
        "Pipeline Hook",
        &edited("pipeline_branch_running", |v| {
            v["object_attributes"]["ref"] = "other".into();
        }),
    )
    .await;
    let after = env.get().await;
    assert_eq!(after.state, before.state);
    assert_eq!(after.external_check_state, before.external_check_state);
    assert_eq!(after.updated_at, before.updated_at);
    assert!(env.events().await.is_empty());

    // Without the changeset hooks, deliveries are accepted and only logged.
    let plain = WebhookIngest::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        Registry::default(),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-gitlab-token", HeaderValue::from_static("s3"));
    let outcome = plain
        .handle(
            WebhookTarget::GitLab {
                external_service: None,
            },
            &headers,
            &fixture("merge_request_merge"),
        )
        .await;
    assert_eq!(outcome.status, 200);
    assert_eq!(env.get().await.state, ChangesetState::Open);
}

#[tokio::test]
async fn connections_with_webhooks_sync_less_often() {
    let Some(env) = setup().await else { return };
    #[allow(clippy::disallowed_methods)]
    let repo = env.db.repos().get(env.repo).await.unwrap().unwrap();
    let provider = DbSourceProvider::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        Registry::default(),
    );
    assert!(provider.has_webhooks(&repo).await);
    // Without webhook secrets: the default intervals.
    sqlx::query("DELETE FROM external_service_repos")
        .execute(env.db.pool())
        .await
        .unwrap();
    assert!(!provider.has_webhooks(&repo).await);
    let now = Utc::now();
    assert_eq!(
        sourcegull_batches_reconciler::next_sync(now, Some(now), false),
        now + chrono::Duration::from_std(MIN_SYNC_DELAY).unwrap()
    );
}
