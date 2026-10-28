//! Bulk operation worker and retention janitor tests against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) with a fake `ChangesetSource`: comment, merge and
//! close on the code host with the initiator's (else the site's) credential, publish, detach
//! and re-enqueue through the reconciler's state, failures recorded on the bulk operation,
//! and the purge of changesets past `batchChanges.changesetsRetention`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpecStore, BatchesCredentialStore, ChangesetJob, ChangesetJobStore,
    ChangesetJobType, ChangesetSpecStore, ChangesetSpecType, ChangesetStore, CredentialKind,
    CredentialSecret, ExternalState, NewBatchChange, NewBatchSpec, NewChangeset, NewChangesetSpec,
    PublicationState, Published, ReconcilerState, UiPublicationState,
};
use sourcegull_batches_reconciler::source::repo_meta;
use sourcegull_batches_reconciler::{
    BulkHandler, PreviewsDeleted, Reconciler, SourceProvider, delete_expired_previews, process_job,
    purge_expired,
};
use sourcegull_codehost::{
    Changeset as HostChangeset, ChangesetSource, ChangesetState as HostState, CodeHostError,
    CreatedChangeset, Credential, MergeOptions, NewChangeset as HostNew, PushStrategy, RepoMeta,
};
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::sqlx;
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_pgqueue::{Queue, QueueConfig, Worker, WorkerOptions};
use sourcegull_testutil::TestDb;
use sourcegull_types::{Repo, RepoId, UserId};
use tokio_util::sync::CancellationToken;

const GITLAB_URL: &str = "https://gitlab.example.com/";
const REPO: &str = "gitlab.example.com/a/one";

#[derive(Default)]
struct State {
    /// `op token [detail]` per call (the token is the credential's password, `-` for none).
    calls: Vec<String>,
    mrs: BTreeMap<i64, HostChangeset>,
    /// Merges fail as not mergeable.
    refuse_merge: bool,
}

#[derive(Clone)]
struct FakeSource {
    state: Arc<Mutex<State>>,
    token: Option<String>,
}

impl FakeSource {
    fn record(&self, op: String) {
        let token = self.token.clone().unwrap_or_else(|| "-".into());
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("{op} {token}"));
    }

    fn set_state(
        &self,
        number: i64,
        state: HostState,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        let mut st = self.state.lock().unwrap();
        let mr = st
            .mrs
            .get_mut(&number)
            .ok_or_else(|| CodeHostError::NotFound("merge request".into()))?;
        mr.state = state;
        mr.updated_at = Utc::now();
        Ok(mr.clone())
    }
}

fn unsupported() -> CodeHostError {
    CodeHostError::NotFound("not supported by the fake".into())
}

#[async_trait]
impl ChangesetSource for FakeSource {
    fn with_credential(
        &self,
        cred: &Credential,
    ) -> sourcegull_codehost::Result<Box<dyn ChangesetSource>> {
        let mut s = self.clone();
        s.token = match cred {
            Credential::Basic { password, .. } => Some(password.clone()),
            Credential::None => None,
        };
        Ok(Box::new(s))
    }

    async fn create(
        &self,
        _: &RepoMeta,
        _: &HostNew,
    ) -> sourcegull_codehost::Result<CreatedChangeset> {
        Err(unsupported())
    }

    async fn undraft(&self, _: &RepoMeta, _: i64) -> sourcegull_codehost::Result<HostChangeset> {
        Err(unsupported())
    }

    async fn update(
        &self,
        _: &RepoMeta,
        _: i64,
        _: &HostNew,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        Err(unsupported())
    }

    async fn close(
        &self,
        _repo: &RepoMeta,
        number: i64,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record(format!("close !{number}"));
        self.set_state(number, HostState::Closed)
    }

    async fn reopen(&self, _: &RepoMeta, _: i64) -> sourcegull_codehost::Result<HostChangeset> {
        Err(unsupported())
    }

    async fn merge(
        &self,
        _repo: &RepoMeta,
        number: i64,
        opts: MergeOptions,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record(format!(
            "merge !{number} squash={} remove={}",
            opts.squash, opts.remove_source_branch
        ));
        if self.state.lock().unwrap().refuse_merge {
            return Err(CodeHostError::NotMergeable("pipeline must succeed".into()));
        }
        self.set_state(number, HostState::Merged)
    }

    async fn create_comment(
        &self,
        _repo: &RepoMeta,
        number: i64,
        body: &str,
    ) -> sourcegull_codehost::Result<()> {
        self.record(format!("comment !{number} {body:?}"));
        Ok(())
    }

    async fn load(&self, _: &RepoMeta, _: i64) -> sourcegull_codehost::Result<HostChangeset> {
        Err(unsupported())
    }

    async fn get_fork(
        &self,
        _: &RepoMeta,
        _: Option<&str>,
    ) -> sourcegull_codehost::Result<RepoMeta> {
        Err(unsupported())
    }

    fn push_strategy(
        &self,
        _: &RepoMeta,
        _: &Credential,
    ) -> sourcegull_codehost::Result<PushStrategy> {
        Err(unsupported())
    }

    async fn validate_credential(&self, _cred: &Credential) -> sourcegull_codehost::Result<()> {
        Ok(())
    }
}

struct FakeProvider {
    source: FakeSource,
}

#[async_trait]
impl SourceProvider for FakeProvider {
    async fn source(
        &self,
        repo: &Repo,
    ) -> sourcegull_batches_reconciler::Result<(Arc<dyn ChangesetSource>, RepoMeta)> {
        let meta = repo_meta(
            repo,
            "https://gitlab.example.com/a/one.git".into(),
            json!({}),
        );
        Ok((Arc::new(self.source.clone()), meta))
    }
}

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<sourcegull_crypto::Keyring>,
    state: Arc<Mutex<State>>,
    repo: RepoId,
    alice: UserId,
    bob: UserId,
    batch_spec: i64,
    batch_change: i64,
    reconciler: Reconciler,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let user = |name: &str| NewUser {
        username: name.into(),
        ..Default::default()
    };
    let alice = db.users().create(&user("alice")).await.unwrap().id;
    let bob = db.users().create(&user("bob")).await.unwrap().id;
    let repo: RepoId = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '42', 'gitlab', $2) RETURNING id",
    )
    .bind(REPO)
    .bind(GITLAB_URL)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let batch_spec = BatchSpecStore::new(&db)
        .create(&NewBatchSpec {
            raw_spec: "name: hello\n".into(),
            spec: json!({"name": "hello"}),
            namespace: Namespace::User(alice),
            user_id: Some(alice),
            created_from_raw: true,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap()
        .id;
    let batch_change = BatchChangeStore::new(&db)
        .create(&NewBatchChange {
            name: "hello".into(),
            description: String::new(),
            namespace: Namespace::User(alice),
            creator_id: Some(alice),
            batch_spec_id: batch_spec,
            applied: true,
        })
        .await
        .unwrap()
        .id;
    let state = Arc::new(Mutex::new(State::default()));
    let fake = Arc::new(FakeProvider {
        source: FakeSource {
            state: state.clone(),
            token: None,
        },
    });
    let config = SiteConfigHandle::new(
        1,
        SiteConfig {
            batch_changes_auto_delete_branch: true,
            ..SiteConfig::default()
        },
    );
    let reconciler = Reconciler::new(db.clone(), Some(keyring.clone()), fake, None, config);
    Some(Env {
        _t: t,
        db,
        keyring,
        state,
        repo,
        alice,
        bob,
        batch_spec,
        batch_change,
        reconciler,
    })
}

impl Env {
    fn secret(token: &str) -> CredentialSecret {
        CredentialSecret {
            kind: CredentialKind::PersonalAccessToken,
            token: token.into(),
            ssh: None,
        }
    }

    async fn user_credential(&self, user: UserId, token: &str) {
        BatchesCredentialStore::new(&self.db, Some(self.keyring.clone()))
            .set_user_credential(user, "gitlab", GITLAB_URL, &Self::secret(token))
            .await
            .unwrap();
    }

    async fn site_credential(&self, token: &str) {
        BatchesCredentialStore::new(&self.db, Some(self.keyring.clone()))
            .set_site_credential("gitlab", GITLAB_URL, &Self::secret(token))
            .await
            .unwrap();
    }

    /// A changeset of the bulk change with a spec (`published` as given), published as
    /// merge request `number` (open) when `number` is set, in reconciler state `completed`.
    async fn changeset(&self, number: Option<i64>, published: Option<Published>) -> i64 {
        let spec = ChangesetSpecStore::new(&self.db)
            .create(&NewChangesetSpec {
                batch_spec_id: Some(self.batch_spec),
                head_ref: Some(format!("refs/heads/fix-{}", number.unwrap_or(0))),
                base_ref: Some("refs/heads/main".into()),
                title: Some("Fix".into()),
                published,
                ..NewChangesetSpec::new(self.repo, ChangesetSpecType::Branch)
            })
            .await
            .unwrap()
            .id;
        let id = ChangesetStore::new(&self.db)
            .create(&NewChangeset {
                current_spec_id: Some(spec),
                batch_change_id: Some(self.batch_change),
                owned_by_batch_change_id: Some(self.batch_change),
                ..NewChangeset::new(self.repo, "gitlab")
            })
            .await
            .unwrap()
            .id;
        sqlx::query("UPDATE changesets SET state = 'completed' WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await
            .unwrap();
        if let Some(n) = number {
            sqlx::query(
                "UPDATE changesets SET publication_state = 'PUBLISHED', external_id = $2,
                     external_state = 'OPEN' WHERE id = $1",
            )
            .bind(id)
            .bind(n.to_string())
            .execute(self.db.pool())
            .await
            .unwrap();
            let now = Utc::now();
            self.state.lock().unwrap().mrs.insert(
                n,
                HostChangeset {
                    number: n,
                    title: "Fix".into(),
                    body: String::new(),
                    state: HostState::Open,
                    review_state: None,
                    check_state: None,
                    head_ref: format!("fix-{n}"),
                    base_ref: "main".into(),
                    head_repo_id: None,
                    labels: vec![],
                    url: format!("https://gitlab.example.com/a/one/-/merge_requests/{n}"),
                    created_at: now,
                    updated_at: now,
                    events: vec![],
                    metadata: json!({}),
                },
            );
        }
        id
    }

    fn job(&self, user: UserId, changeset: i64, job_type: ChangesetJobType) -> ChangesetJob {
        ChangesetJob {
            bulk_group: "test".into(),
            user_id: user,
            batch_change_id: self.batch_change,
            changeset_id: changeset,
            job_type,
            payload: json!({}),
        }
    }

    async fn run(&self, job: &ChangesetJob) -> sourcegull_batches_reconciler::Result<()> {
        process_job(&self.reconciler, job).await
    }

    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().calls)
    }

    async fn get(&self, id: i64) -> sourcegull_batches::store::Changeset {
        ChangesetStore::new(&self.db)
            .get(id)
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn code_host_operations_use_the_initiators_credential() {
    let Some(env) = setup().await else { return };
    let a = env.changeset(Some(1), None).await;
    let b = env.changeset(Some(2), None).await;

    // No credential at all: the job fails without calling the code host.
    let mut job = env.job(env.bob, a, ChangesetJobType::Comment);
    job.payload = json!({"message": "Please review"});
    let err = env.run(&job).await.unwrap_err();
    assert!(!err.is_retryable());
    assert!(
        err.to_string()
            .contains("the user who started the bulk operation has no gitlab credential"),
        "{err}"
    );
    assert!(env.calls().is_empty());

    // The initiator's credential, not the last applier's; else the site's.
    env.user_credential(env.alice, "alice-token").await;
    env.user_credential(env.bob, "bob-token").await;
    env.site_credential("site-token").await;
    env.run(&job).await.unwrap();
    assert_eq!(env.calls(), [r#"comment !1 "Please review" bob-token"#]);
    let carol = env
        .db
        .users()
        .create(&NewUser {
            username: "carol".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let mut merge = env.job(carol, a, ChangesetJobType::Merge);
    merge.payload = json!({"squash": true});
    env.run(&merge).await.unwrap();
    assert_eq!(env.calls(), ["merge !1 squash=true remove=true site-token"]);
    // What the code host returned is stored.
    let ch = env.get(a).await;
    assert_eq!(ch.external_state, Some(ExternalState::Merged));
    assert!(ch.next_sync_at.is_some());

    env.run(&env.job(env.bob, b, ChangesetJobType::Close))
        .await
        .unwrap();
    assert_eq!(env.calls(), ["close !2 bob-token"]);
    assert_eq!(env.get(b).await.external_state, Some(ExternalState::Closed));
    // Closing a closed changeset does nothing.
    env.run(&env.job(env.bob, b, ChangesetJobType::Close))
        .await
        .unwrap();
    assert!(env.calls().is_empty());

    // Refused merges and empty comments fail for good.
    let c = env.changeset(Some(3), None).await;
    env.state.lock().unwrap().refuse_merge = true;
    let err = env
        .run(&env.job(env.bob, c, ChangesetJobType::Merge))
        .await
        .unwrap_err();
    assert!(!err.is_retryable(), "{err}");
    assert_eq!(env.get(c).await.external_state, Some(ExternalState::Open));
    let mut empty = env.job(env.bob, c, ChangesetJobType::Comment);
    empty.payload = json!({"message": "  "});
    assert!(env.run(&empty).await.is_err());
    // An unpublished changeset has no merge request to comment on.
    let unpublished = env.changeset(None, None).await;
    let mut job = env.job(env.bob, unpublished, ChangesetJobType::Comment);
    job.payload = json!({"message": "hi"});
    assert!(env.run(&job).await.is_err());
}

#[tokio::test]
async fn publish_detach_and_reenqueue_go_through_the_reconciler() {
    let Some(env) = setup().await else { return };

    // Publish: sets the UI publication state and queues the changeset.
    let a = env.changeset(None, None).await;
    let mut job = env.job(env.alice, a, ChangesetJobType::Publish);
    job.payload = json!({"draft": true});
    env.run(&job).await.unwrap();
    let ch = env.get(a).await;
    assert_eq!(ch.ui_publication_state, Some(UiPublicationState::Draft));
    assert_eq!(ch.reconciler_state, ReconcilerState::Queued);
    let b = env.changeset(None, None).await;
    env.run(&env.job(env.alice, b, ChangesetJobType::Publish))
        .await
        .unwrap();
    assert_eq!(
        env.get(b).await.ui_publication_state,
        Some(UiPublicationState::Published)
    );
    // Not when the spec sets `published`, nor for imported changesets.
    let c = env.changeset(None, Some(Published::Bool(false))).await;
    let err = env
        .run(&env.job(env.alice, c, ChangesetJobType::Publish))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("published value set"), "{err}");
    assert_eq!(env.get(c).await.ui_publication_state, None);
    sqlx::query("UPDATE changesets SET current_spec_id = NULL WHERE id = $1")
        .bind(c)
        .execute(env.db.pool())
        .await
        .unwrap();
    let err = env
        .run(&env.job(env.alice, c, ChangesetJobType::Publish))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("imported"), "{err}");

    // Detach: requested for the bulk change, then carried out by the reconciler.
    let d = env.changeset(Some(4), None).await;
    ChangesetStore::new(&env.db)
        .mark_archived(d, env.batch_change)
        .await
        .unwrap();
    env.run(&env.job(env.alice, d, ChangesetJobType::Detach))
        .await
        .unwrap();
    let ch = env.get(d).await;
    assert!(ch.batch_change_ids[&env.batch_change].detach);
    assert_eq!(ch.reconciler_state, ReconcilerState::Queued);
    env.reconciler.reconcile(&ch).await.unwrap();
    let ch = env.get(d).await;
    assert!(ch.batch_change_ids.is_empty());
    assert!(ch.detached_at.is_some());
    // Detaching again: no longer in the bulk change.
    assert!(
        env.run(&env.job(env.alice, d, ChangesetJobType::Detach))
            .await
            .is_err()
    );

    // Re-enqueue: a failed changeset is queued again with its failures reset.
    let e = env.changeset(Some(5), None).await;
    sqlx::query(
        "UPDATE changesets SET state = 'failed', num_failures = 5, failure_message = 'boom'
         WHERE id = $1",
    )
    .bind(e)
    .execute(env.db.pool())
    .await
    .unwrap();
    env.run(&env.job(env.alice, e, ChangesetJobType::Reenqueue))
        .await
        .unwrap();
    let ch = env.get(e).await;
    assert_eq!(ch.reconciler_state, ReconcilerState::Queued);
    assert_eq!((ch.num_failures, ch.failure_message), (0, None));
    assert_eq!(ch.publication_state, PublicationState::Published);
}

#[tokio::test]
async fn worker_completes_bulk_operations_and_records_failures() {
    let Some(env) = setup().await else { return };
    env.user_credential(env.bob, "bob-token").await;
    let a = env.changeset(Some(1), None).await;
    let b = env.changeset(Some(2), None).await;
    let unpublished = env.changeset(None, None).await;
    let jobs = ChangesetJobStore::new(&env.db);
    let group = jobs
        .create_bulk(
            env.bob,
            env.batch_change,
            ChangesetJobType::Comment,
            &json!({"message": "LGTM?"}),
            &[a, b, unpublished],
        )
        .await
        .unwrap();
    let config = QueueConfig {
        backoff_base: Duration::ZERO,
        ..ChangesetJobStore::queue_config()
    };
    let queue: Queue<ChangesetJob> = Queue::new(env.db.pool().clone(), config).unwrap();
    let mut opts = WorkerOptions::new("test-worker");
    opts.poll_interval = Duration::from_millis(100);
    let worker = Worker::new(queue, BulkHandler::new(env.reconciler.clone()), opts);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(worker.run(cancel.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let op = loop {
        let op = jobs.bulk_operation(&group).await.unwrap().unwrap();
        if op.state != "PROCESSING" {
            break op;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {op:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    cancel.cancel();
    task.await.unwrap();
    assert_eq!(op.state, "FAILED");
    assert!((op.progress - 1.0).abs() < f64::EPSILON);
    assert!(op.finished_at.is_some());
    let mut calls = env.calls();
    calls.sort();
    assert_eq!(
        calls,
        [
            r#"comment !1 "LGTM?" bob-token"#,
            r#"comment !2 "LGTM?" bob-token"#
        ]
    );
    let errors = jobs.errors(&group).await.unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, unpublished);
    assert!(errors[0].1.contains("no merge request"), "{}", errors[0].1);
}

#[tokio::test]
async fn retention_purges_unused_changesets() {
    let Some(env) = setup().await else { return };
    let bc = env.batch_change;
    let active = env.changeset(Some(1), None).await;
    let archived_old = env.changeset(Some(2), None).await;
    let archived_new = env.changeset(Some(3), None).await;
    let detached_old = env.changeset(Some(4), None).await;
    let detached_new = env.changeset(Some(5), None).await;
    let never_detached = env.changeset(Some(6), None).await;
    let store = ChangesetStore::new(&env.db);
    for id in [archived_old, archived_new] {
        store.mark_archived(id, bc).await.unwrap();
    }
    for id in [detached_old, detached_new] {
        store.remove_from_batch_change(id, bc).await.unwrap();
    }
    // A changeset no bulk change tracks but never detached (its bulk change was deleted).
    sqlx::query("UPDATE changesets SET batch_change_ids = '{}' WHERE id = $1")
        .bind(never_detached)
        .execute(env.db.pool())
        .await
        .unwrap();
    // Age the old ones by ten days (archive time and detach time).
    sqlx::query(
        "UPDATE changesets SET batch_change_ids = jsonb_set(batch_change_ids,
             ARRAY[$2::text, 'archivedAt'], to_jsonb(now() - interval '10 days'))
         WHERE id = $1",
    )
    .bind(archived_old)
    .bind(bc.to_string())
    .execute(env.db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE changesets SET detached_at = now() - interval '10 days' WHERE id = $1")
        .bind(detached_old)
        .execute(env.db.pool())
        .await
        .unwrap();
    // An event, which goes with its changeset.
    store
        .upsert_events(
            archived_old,
            &[sourcegull_batches::store::NewChangesetEvent {
                kind: "MERGED".into(),
                key: "state:1".into(),
                created_at: Utc::now(),
                metadata: json!({}),
            }],
        )
        .await
        .unwrap();

    // Ten days old, retention a week: purged; newer ones and used ones stay.
    let week = Duration::from_secs(7 * 86_400);
    assert_eq!(purge_expired(&env.db, week).await.unwrap(), 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM changesets ORDER BY id")
        .fetch_all(env.db.pool())
        .await
        .unwrap();
    assert_eq!(left, [active, archived_new, detached_new, never_detached]);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM changeset_events")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(events, 0);
    // A changeset archived in one bulk change but still used in another stays.
    let other = BatchChangeStore::new(&env.db)
        .create(&NewBatchChange {
            name: "other".into(),
            description: String::new(),
            namespace: Namespace::User(env.alice),
            creator_id: Some(env.alice),
            batch_spec_id: env.batch_spec,
            applied: true,
        })
        .await
        .unwrap()
        .id;
    sqlx::query(
        "UPDATE changesets SET batch_change_ids = batch_change_ids
             || jsonb_build_object($2::text, '{}'::jsonb) WHERE id = $1",
    )
    .bind(archived_new)
    .bind(other.to_string())
    .execute(env.db.pool())
    .await
    .unwrap();
    // A zero retention purges everything archived or detached that is unused.
    assert_eq!(purge_expired(&env.db, Duration::ZERO).await.unwrap(), 1);
    assert!(store.get(archived_new).await.unwrap().is_some());
    assert!(store.get(detached_new).await.unwrap().is_none());
    assert!(store.get(active).await.unwrap().is_some());
}

#[tokio::test]
async fn previews_janitor_deletes_unapplied_specs() {
    let Some(env) = setup().await else { return };
    let batch_specs = BatchSpecStore::new(&env.db);
    let changeset_specs = ChangesetSpecStore::new(&env.db);
    let new_batch_spec = |batch_change_id| NewBatchSpec {
        raw_spec: "name: preview\n".into(),
        spec: json!({"name": "preview"}),
        namespace: Namespace::User(env.alice),
        user_id: Some(env.alice),
        created_from_raw: true,
        allow_unsupported: false,
        allow_ignored: false,
        no_cache: false,
        batch_change_id,
    };
    let unapplied_old = batch_specs.create(&new_batch_spec(None)).await.unwrap().id;
    let unapplied_new = batch_specs.create(&new_batch_spec(None)).await.unwrap().id;
    // A draft of an existing bulk change, which stays with it.
    let draft_old = batch_specs
        .create(&new_batch_spec(Some(env.batch_change)))
        .await
        .unwrap()
        .id;
    let changeset_spec = |batch_spec_id| NewChangesetSpec {
        batch_spec_id,
        head_ref: Some("refs/heads/fix".into()),
        base_ref: Some("refs/heads/main".into()),
        ..NewChangesetSpec::new(env.repo, ChangesetSpecType::Branch)
    };
    let attached_to_old = changeset_specs
        .create(&changeset_spec(Some(unapplied_old)))
        .await
        .unwrap()
        .id;
    let unattached_old = changeset_specs
        .create(&changeset_spec(None))
        .await
        .unwrap()
        .id;
    let unattached_new = changeset_specs
        .create(&changeset_spec(None))
        .await
        .unwrap()
        .id;
    let used_old = changeset_specs
        .create(&changeset_spec(None))
        .await
        .unwrap()
        .id;
    ChangesetStore::new(&env.db)
        .create(&NewChangeset {
            current_spec_id: Some(used_old),
            batch_change_id: Some(env.batch_change),
            ..NewChangeset::new(env.repo, "gitlab")
        })
        .await
        .unwrap();
    // Ten days old: the applied spec, the unapplied one and the draft; three days old: the old
    // changeset specs; the rest a day old.
    sqlx::query(
        "UPDATE batch_specs SET created_at = now() - interval '10 days' WHERE id = ANY($1)",
    )
    .bind(vec![env.batch_spec, unapplied_old, draft_old])
    .execute(env.db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE batch_specs SET created_at = now() - interval '1 day' WHERE id = $1")
        .bind(unapplied_new)
        .execute(env.db.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE changeset_specs SET created_at = now() - interval '3 days' WHERE id = ANY($1)",
    )
    .bind(vec![unattached_old, used_old, attached_to_old])
    .execute(env.db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE changeset_specs SET created_at = now() - interval '1 day' WHERE id = $1")
        .bind(unattached_new)
        .execute(env.db.pool())
        .await
        .unwrap();

    let deleted = delete_expired_previews(
        &env.db,
        sourcegull_batches::store::BATCH_SPEC_TTL,
        sourcegull_batches::store::CHANGESET_SPEC_TTL,
    )
    .await
    .unwrap();
    assert_eq!(
        deleted,
        PreviewsDeleted {
            batch_specs: 1,
            changeset_specs: 1,
        }
    );
    assert!(batch_specs.get(unapplied_old).await.unwrap().is_none());
    assert!(
        changeset_specs
            .get(attached_to_old)
            .await
            .unwrap()
            .is_none()
    );
    assert!(changeset_specs.get(unattached_old).await.unwrap().is_none());
    for id in [env.batch_spec, unapplied_new, draft_old] {
        assert!(
            batch_specs.get(id).await.unwrap().is_some(),
            "batch spec {id}"
        );
    }
    for id in [unattached_new, used_old] {
        assert!(
            changeset_specs.get(id).await.unwrap().is_some(),
            "changeset spec {id}"
        );
    }
    // A second run finds nothing.
    let again = delete_expired_previews(
        &env.db,
        sourcegull_batches::store::BATCH_SPEC_TTL,
        sourcegull_batches::store::CHANGESET_SPEC_TTL,
    )
    .await
    .unwrap();
    assert_eq!(again, PreviewsDeleted::default());
}
