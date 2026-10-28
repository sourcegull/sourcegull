//! Bulk changes store tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): specs
//! and resolution, workspaces and fair execution queueing, the execution cache, changeset
//! specs, apply transactions, changeset listings, counts and the computed state, the
//! reconciler and bulk queues, events, and encrypted credentials.

use std::sync::Arc;

use sourcegull_batches::store::{
    ArchivedFilter, BatchChangeState, BatchChangeStore, BatchSpecStore,
    BatchSpecWorkspaceFileStore, BatchSpecWorkspaceStore, BatchesCredentialStore, Changeset,
    ChangesetHookUpdate, ChangesetJobStore, ChangesetJobType, ChangesetSpecStore,
    ChangesetSpecType, ChangesetState, ChangesetStore, ChangesetSync, CheckState, CredentialKind,
    CredentialSecret, ExecutionCacheStore, ExternalState, ListBatchChanges, ListBatchSpecs,
    ListChangesetSpecs, ListChangesets, ListWorkspaces, NewBatchChange, NewBatchSpec,
    NewBatchSpecWorkspaceFile, NewChangeset, NewChangesetEvent, NewChangesetSpec, NewWorkspace,
    PublicationState, Published, ReconcilerState, ReviewState, SshKeypair, UiPublicationState,
    compute_changeset_state,
};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::sqlx;
use sourcegull_db::{Db, DbError, Namespace, NewUser, migrate};
use sourcegull_pgqueue::Queue;
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoAccess, RepoId, UserId};

const GITLAB: &str = "gitlab";
const GITLAB_URL: &str = "https://gitlab.example.com/";

async fn setup() -> Option<(TestDb, Db)> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    Some((t, db))
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

async fn repo(db: &Db, name: &str) -> RepoId {
    sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

fn new_spec(ns: Namespace, user: UserId, raw: bool) -> NewBatchSpec {
    NewBatchSpec {
        raw_spec: "name: hello\non:\n  - repository: a\n".into(),
        spec: serde_json::json!({"name": "hello", "on": [{"repository": "a"}]}),
        namespace: ns,
        user_id: Some(user),
        created_from_raw: raw,
        allow_unsupported: false,
        allow_ignored: false,
        no_cache: false,
        batch_change_id: None,
    }
}

fn branch_spec(batch_spec: i64, repo: RepoId, title: &str) -> NewChangesetSpec {
    NewChangesetSpec {
        batch_spec_id: Some(batch_spec),
        head_ref: Some("refs/heads/hello".into()),
        base_ref: Some("refs/heads/main".into()),
        base_rev: Some("a".repeat(40)),
        title: Some(title.into()),
        body: Some("body".into()),
        published: Some(Published::Draft),
        diff: Some(b"diff --git a/x b/x\n".to_vec()),
        diff_stat_added: Some(3),
        diff_stat_deleted: Some(1),
        commit_message: Some("msg".into()),
        ..NewChangesetSpec::new(repo, ChangesetSpecType::Branch)
    }
}

#[tokio::test]
async fn specs_workspaces_and_executions() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    let r1 = repo(&db, "gitlab.example.com/a/one").await;
    let r2 = repo(&db, "gitlab.example.com/a/two").await;
    let specs = BatchSpecStore::new(&db);

    let s1 = specs
        .create(&new_spec(Namespace::User(alice), alice, true))
        .await
        .unwrap();
    let local = specs
        .create(&new_spec(Namespace::Org(org), alice, false))
        .await
        .unwrap();
    assert_eq!(s1.rand_id.len(), 32);
    assert_ne!(s1.rand_id, local.rand_id);
    assert_eq!(
        specs.get_by_rand_id(&s1.rand_id).await.unwrap().unwrap().id,
        s1.id
    );
    assert_eq!(s1.spec["name"], "hello");

    // Listing: newest first, server-side only unless local ones are included, cursor.
    let all = ListBatchSpecs {
        include_local: true,
        ..Default::default()
    };
    let ids: Vec<i64> = specs
        .list(&all)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![local.id, s1.id]);
    assert_eq!(specs.count(&ListBatchSpecs::default()).await.unwrap(), 1);
    let page2 = ListBatchSpecs {
        before_id: Some(local.id),
        ..all.clone()
    };
    assert_eq!(specs.list(&page2).await.unwrap()[0].id, s1.id);
    let in_org = ListBatchSpecs {
        namespace: Some(Namespace::Org(org)),
        ..all.clone()
    };
    assert_eq!(specs.count(&in_org).await.unwrap(), 1);

    // Resolution: one job per spec, reset when queued again.
    let job = specs.enqueue_resolution(s1.id, alice).await.unwrap();
    let queue: Queue<sourcegull_batches::store::ResolutionJob> =
        Queue::new(db.pool().clone(), BatchSpecStore::resolution_queue_config()).unwrap();
    let got = queue.dequeue("w1", 1).await.unwrap();
    assert_eq!(got[0].id, job);
    assert_eq!(got[0].payload.batch_spec_id, s1.id);
    queue.mark_failed(job, "w1", "boom").await.unwrap();
    assert_eq!(specs.enqueue_resolution(s1.id, bob).await.unwrap(), job);
    let row = specs.resolution_job(s1.id).await.unwrap().unwrap();
    assert_eq!(row.state, sourcegull_pgqueue::JobState::Queued);
    assert_eq!(row.payload.initiator_id, bob);
    assert_eq!(row.failure_message, None);

    // Workspaces: skipped and cached ones get no execution job.
    let ws = BatchSpecWorkspaceStore::new(&db);
    let mut skipped = NewWorkspace::new(r2, "refs/heads/main", "b".repeat(40));
    skipped.skipped = true;
    skipped.ignored = true;
    skipped.skipped_reason = Some("ignored by .batchignore".into());
    let mut sub = NewWorkspace::new(r1, "refs/heads/main", "a".repeat(40));
    sub.path = "sub".into();
    sub.file_matches = vec!["sub/go.mod".into()];
    let w_ids = ws
        .create_many(
            s1.id,
            &[
                NewWorkspace::new(r1, "refs/heads/main", "a".repeat(40)),
                sub,
                skipped,
            ],
        )
        .await
        .unwrap();
    assert_eq!(w_ids.len(), 3);
    let w = ws.get(w_ids[1]).await.unwrap().unwrap();
    assert_eq!(w.path, "sub");
    assert_eq!(w.file_matches, vec!["sub/go.mod"]);
    ws.set_cache_results(w_ids[1], true, &serde_json::json!({"0": {"diff": ""}}))
        .await
        .unwrap();

    let list = |state: Option<&str>| ListWorkspaces {
        batch_spec_id: s1.id,
        state: state.map(Into::into),
        ..Default::default()
    };
    assert_eq!(ws.count(&list(None)).await.unwrap(), 3);
    assert_eq!(ws.count(&list(Some("skipped"))).await.unwrap(), 1);
    assert_eq!(ws.count(&list(Some("cached"))).await.unwrap(), 1);
    assert_eq!(ws.count(&list(Some("pending"))).await.unwrap(), 1);
    let searched = ListWorkspaces {
        search: Some("TWO".into()),
        ..list(None)
    };
    assert_eq!(ws.list(&searched).await.unwrap()[0].id, w_ids[2]);

    assert_eq!(ws.enqueue_executions(s1.id, alice).await.unwrap(), 1);
    assert_eq!(
        ws.enqueue_executions(s1.id, alice).await.unwrap(),
        0,
        "idempotent"
    );
    assert_eq!(ws.count(&list(Some("queued"))).await.unwrap(), 1);

    // A second user's spec: dequeueing alternates between users.
    let s2 = specs
        .create(&new_spec(Namespace::User(bob), bob, true))
        .await
        .unwrap();
    let bob_ws = ws
        .create_many(
            s2.id,
            &[
                NewWorkspace::new(r1, "refs/heads/main", "a".repeat(40)),
                NewWorkspace::new(r2, "refs/heads/main", "b".repeat(40)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(ws.enqueue_executions(s2.id, bob).await.unwrap(), 2);
    let jobs = ws.execution_jobs_for_workspaces(&bob_ws).await.unwrap();
    let mut places: Vec<(Option<i64>, Option<i64>)> = jobs
        .iter()
        .map(|j| {
            (
                j.payload.place_in_user_queue,
                j.payload.place_in_global_queue,
            )
        })
        .collect();
    places.sort();
    // Neither user has dequeued yet: alice (lower id) first, then bob, then bob again.
    assert_eq!(places, vec![(Some(1), Some(2)), (Some(2), Some(3))]);

    let exec: Queue<sourcegull_batches::store::ExecutionJob> = Queue::new(
        db.pool().clone(),
        BatchSpecWorkspaceStore::execution_queue_config(
            sourcegull_batches::store::EXECUTION_JOB_VERSION,
        ),
    )
    .unwrap();
    let old: Queue<sourcegull_batches::store::ExecutionJob> = Queue::new(
        db.pool().clone(),
        BatchSpecWorkspaceStore::execution_queue_config(1),
    )
    .unwrap();
    assert!(
        old.dequeue("e0", 1).await.unwrap().is_empty(),
        "version filter"
    );
    let mut order = Vec::new();
    for i in 0..3 {
        let j = exec.dequeue(&format!("e{i}"), 1).await.unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0].payload.place_in_user_queue, None);
        order.push(j[0].payload.user_id);
    }
    assert_eq!(order, vec![alice, bob, bob]);
    assert!(exec.dequeue("e9", 1).await.unwrap().is_empty());

    let alice_job = ws
        .execution_jobs_for_workspaces(&[w_ids[0]])
        .await
        .unwrap()
        .remove(0);
    exec.mark_failed(alice_job.id, &alice_job.worker_hostname, "step 1 failed")
        .await
        .unwrap();
    let stats = ws.stats(s1.id).await.unwrap();
    assert_eq!(
        (
            stats.workspaces,
            stats.skipped_workspaces,
            stats.cached_workspaces
        ),
        (3, 1, 1)
    );
    assert_eq!((stats.executions, stats.failed), (1, 1));
    assert!(stats.finished_at.is_some());

    // Retry drops the produced changeset specs and queues the job again.
    let produced = ChangesetSpecStore::new(&db)
        .create(&branch_spec(s1.id, r1, "t"))
        .await
        .unwrap();
    ws.set_changeset_specs(w_ids[0], &[produced.id])
        .await
        .unwrap();
    assert_eq!(
        ws.get(w_ids[0]).await.unwrap().unwrap().changeset_spec_ids,
        vec![produced.id]
    );
    assert_eq!(ws.retry_executions(s1.id, None, false).await.unwrap(), 1);
    assert!(
        ChangesetSpecStore::new(&db)
            .get(produced.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ws.get(w_ids[0])
            .await
            .unwrap()
            .unwrap()
            .changeset_spec_ids
            .is_empty()
    );
    let retried = ws.execution_job(alice_job.id).await.unwrap().unwrap();
    assert_eq!(retried.state, sourcegull_pgqueue::JobState::Queued);
    assert_eq!(retried.payload.place_in_global_queue, Some(1));

    // Cancel: queued jobs are canceled, running ones flagged.
    assert_eq!(ws.cancel_executions(s2.id).await.unwrap(), 2);
    let stats = ws.stats(s2.id).await.unwrap();
    assert_eq!((stats.processing, stats.canceling), (2, 2));
    assert_eq!(ws.cancel_executions(s1.id).await.unwrap(), 1);
    assert_eq!(ws.stats(s1.id).await.unwrap().canceled, 1);

    // Deleting a spec takes its workspaces and jobs along.
    assert!(specs.delete(s2.id).await.unwrap());
    assert!(ws.get(bob_ws[0]).await.unwrap().is_none());
}

#[tokio::test]
async fn execution_cache_lru() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let cache = ExecutionCacheStore::new(&db);
    cache.set(alice, "k1", &"a".repeat(100), 1).await.unwrap();
    cache.set(alice, "k2", &"b".repeat(100), 1).await.unwrap();
    cache.set(alice, "k3", &"c".repeat(100), 1).await.unwrap();
    // k1 becomes the most recently used.
    sqlx::query(
        "UPDATE batch_spec_execution_cache_entries SET last_used_at = now() - interval '1 hour'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let e = cache.get(alice, "k1").await.unwrap().unwrap();
    assert_eq!(e.value.len(), 100);
    assert!(cache.get(alice, "missing").await.unwrap().is_none());
    cache.set(alice, "k2", "bb", 2).await.unwrap();
    assert_eq!(cache.get(alice, "k2").await.unwrap().unwrap().version, 2);
    sqlx::query("UPDATE batch_spec_execution_cache_entries SET last_used_at = now() - interval '2 hours' WHERE key = 'k3'")
        .execute(db.pool())
        .await
        .unwrap();
    // k2 (2 bytes) and k1 (100) fit; k3, the least recently used, goes.
    assert_eq!(cache.cleanup(150, 0).await.unwrap(), 1);
    let keys: Vec<String> = cache
        .get_many(alice, &["k1".into(), "k2".into(), "k3".into()])
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(!keys.contains(&"k3".to_string()));
    // Old format versions go regardless of size.
    assert_eq!(cache.cleanup(1 << 20, 2).await.unwrap(), 1);
    assert!(cache.get(alice, "k1").await.unwrap().is_none());
    assert_eq!(cache.delete(alice, &["k2".into()]).await.unwrap(), 1);
}

#[tokio::test]
async fn workspace_files() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let spec = BatchSpecStore::new(&db)
        .create(&new_spec(Namespace::User(alice), alice, true))
        .await
        .unwrap();
    let other = BatchSpecStore::new(&db)
        .create(&new_spec(Namespace::User(alice), alice, true))
        .await
        .unwrap();
    let files = BatchSpecWorkspaceFileStore::new(&db);
    let t = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .to_utc();
    let new = |spec: i64, path: &'static str, name: &'static str, content: &'static [u8]| {
        NewBatchSpecWorkspaceFile {
            batch_spec_id: spec,
            path,
            filename: name,
            content,
            modified_at: t,
        }
    };
    let a = files
        .upsert(&new(spec.id, "scripts", "fix.sh", b"one"))
        .await
        .unwrap();
    files
        .upsert(&new(spec.id, "", "z.txt", b"z"))
        .await
        .unwrap();
    files
        .upsert(&new(other.id, "scripts", "fix.sh", b"other"))
        .await
        .unwrap();
    // Same path and name: replaced in place.
    let again = files
        .upsert(&new(spec.id, "scripts", "fix.sh", b"three"))
        .await
        .unwrap();
    assert_eq!(
        (again.id, again.rand_id.as_str(), again.size),
        (a.id, a.rand_id.as_str(), 5)
    );
    assert_eq!(files.content(a.id).await.unwrap().unwrap(), b"three");
    assert_eq!(files.count(spec.id).await.unwrap(), 2);
    let listed: Vec<String> = files
        .list(spec.id, 0, 0)
        .await
        .unwrap()
        .iter()
        .map(|f| f.full_path())
        .collect();
    // By directory, then name: the spec's own directory first.
    assert_eq!(listed, ["z.txt", "scripts/fix.sh"]);
    assert_eq!(
        files.list(spec.id, 1, 1).await.unwrap()[0].filename,
        "fix.sh"
    );
    assert_eq!(
        files
            .get_by_rand_id(spec.id, &a.rand_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        a.id
    );
    // A file is found only through its own spec.
    assert!(
        files
            .get_by_rand_id(other.id, &a.rand_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        files
            .get_by_name(spec.id, "scripts", "fix.sh")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        files
            .get_by_name(spec.id, "", "fix.sh")
            .await
            .unwrap()
            .is_none()
    );
    // Files go with their spec.
    BatchSpecStore::new(&db).delete(spec.id).await.unwrap();
    assert_eq!(files.count(spec.id).await.unwrap(), 0);
    assert_eq!(files.delete_for_spec(other.id).await.unwrap(), 1);
}

struct Applied {
    batch_change: i64,
    spec: i64,
    changesets: Vec<Changeset>,
}

/// Applies a spec with one changeset spec per repo in a transaction.
async fn apply(db: &Db, alice: UserId, repos: &[RepoId]) -> Applied {
    let spec = BatchSpecStore::new(db)
        .create(&new_spec(Namespace::User(alice), alice, false))
        .await
        .unwrap();
    let cs_specs = ChangesetSpecStore::new(db)
        .create_many(
            &repos
                .iter()
                .enumerate()
                .map(|(i, r)| branch_spec(spec.id, *r, &format!("Fix thing {i}")))
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let mut tx = BatchChangeStore::new(db).begin_apply().await.unwrap();
    assert!(
        tx.lock_batch_change(Namespace::User(alice), "hello")
            .await
            .unwrap()
            .is_none()
    );
    let bc = tx
        .create_batch_change(&NewBatchChange {
            name: "hello".into(),
            description: "first".into(),
            namespace: Namespace::User(alice),
            creator_id: Some(alice),
            batch_spec_id: spec.id,
            applied: true,
        })
        .await
        .unwrap();
    let mut changesets = Vec::new();
    for s in &cs_specs {
        changesets.push(
            tx.create_changeset(&NewChangeset {
                batch_change_id: Some(bc.id),
                owned_by_batch_change_id: Some(bc.id),
                current_spec_id: Some(s.id),
                ..NewChangeset::new(s.repo_id, GITLAB)
            })
            .await
            .unwrap(),
        );
    }
    tx.record_apply(bc.id, spec.id, "first", alice)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    Applied {
        batch_change: bc.id,
        spec: spec.id,
        changesets,
    }
}

#[tokio::test]
async fn batch_changes_apply_and_changesets() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let repos = vec![
        repo(&db, "gitlab.example.com/a/one").await,
        repo(&db, "gitlab.example.com/a/two").await,
        repo(&db, "gitlab.example.com/b/three").await,
    ];
    let a = apply(&db, alice, &repos).await;
    let bcs = BatchChangeStore::new(&db);
    let changesets = ChangesetStore::new(&db);

    // Bulk change basics.
    let bc = bcs.get(a.batch_change).await.unwrap().unwrap();
    assert_eq!(bc.state(), BatchChangeState::Open);
    assert_eq!(bc.last_applier_id, Some(alice));
    assert_eq!(
        BatchSpecStore::new(&db)
            .get(a.spec)
            .await
            .unwrap()
            .unwrap()
            .batch_change_id,
        Some(bc.id)
    );
    assert_eq!(
        bcs.get_by_name(Namespace::User(alice), "hello")
            .await
            .unwrap()
            .unwrap()
            .id,
        bc.id
    );
    let dup = bcs
        .create(&NewBatchChange {
            name: "hello".into(),
            description: String::new(),
            namespace: Namespace::User(alice),
            creator_id: Some(alice),
            batch_spec_id: a.spec,
            applied: false,
        })
        .await;
    assert!(matches!(dup, Err(DbError::AlreadyExists { .. })));
    let draft = bcs
        .create(&NewBatchChange {
            name: "other".into(),
            description: String::new(),
            namespace: Namespace::User(alice),
            creator_id: Some(alice),
            batch_spec_id: a.spec,
            applied: false,
        })
        .await
        .unwrap();
    assert_eq!(draft.state(), BatchChangeState::Draft);
    let open = ListBatchChanges {
        states: vec![BatchChangeState::Open],
        ..Default::default()
    };
    assert_eq!(bcs.count(&open).await.unwrap(), 1);
    let in_repo = ListBatchChanges {
        repo: Some(repos[0]),
        ..Default::default()
    };
    assert_eq!(bcs.list(&in_repo).await.unwrap()[0].id, bc.id);

    // Changeset specs listing and diff stats.
    let cs_opts = ListChangesetSpecs {
        batch_spec_id: Some(a.spec),
        ..Default::default()
    };
    let specs = ChangesetSpecStore::new(&db).list(&cs_opts).await.unwrap();
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[0].published, Some(Published::Draft));
    assert_eq!(
        ChangesetSpecStore::new(&db)
            .diff_stat(a.spec)
            .await
            .unwrap(),
        (9, 3)
    );
    let bad = NewChangesetSpec::new(repos[0], ChangesetSpecType::Existing);
    assert!(matches!(
        ChangesetSpecStore::new(&db).create(&bad).await,
        Err(DbError::Invalid(_))
    ));

    // New changesets are queued: computed PROCESSING.
    let c0 = &a.changesets[0];
    assert_eq!(c0.state, ChangesetState::Processing);
    assert_eq!(c0.reconciler_state, ReconcilerState::Queued);
    assert_eq!(c0.batch_change_ids[&bc.id], Default::default());

    // The reconciler publishes c0 and c1; c2 stays unpublished.
    let queue: Queue<Changeset> =
        Queue::new(db.pool().clone(), ChangesetStore::queue_config()).unwrap();
    let got = queue.dequeue("rec", 10).await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].payload.state, ChangesetState::Processing);
    for (i, c) in got.iter().enumerate() {
        if i < 2 {
            changesets
                .update_from_sync(
                    c.id,
                    &ChangesetSync {
                        external_id: Some(format!("{}", 10 + i)),
                        external_branch: Some("refs/heads/hello".into()),
                        external_state: Some(if i == 0 {
                            ExternalState::Open
                        } else {
                            ExternalState::Draft
                        }),
                        external_review_state: Some(ReviewState::Pending),
                        title: Some(format!("Fix thing {i}")),
                        metadata: serde_json::json!({"iid": 10 + i}),
                        publication_state: Some(PublicationState::Published),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        queue.mark_complete(c.id, "rec").await.unwrap();
    }
    let all = |f: fn(&mut ListChangesets)| {
        let mut o = ListChangesets {
            batch_change_id: Some(bc.id),
            ..Default::default()
        };
        f(&mut o);
        o
    };
    let ids = |v: Vec<Changeset>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
    let cs = changesets
        .list(&all(|_| {}), &RepoAccess::All)
        .await
        .unwrap();
    let states: Vec<ChangesetState> = cs.iter().map(|c| c.state).collect();
    assert_eq!(
        states,
        vec![
            ChangesetState::Open,
            ChangesetState::Draft,
            ChangesetState::Unpublished
        ]
    );
    for c in &cs {
        assert_eq!(
            c.state,
            compute_changeset_state(c.reconciler_state, c.publication_state, c.external_state)
        );
    }
    assert_eq!(
        changesets
            .get_by_external_id(repos[1], GITLAB, "11")
            .await
            .unwrap()
            .unwrap()
            .id,
        cs[1].id
    );

    // Filters.
    let open = changesets
        .list(
            &all(|o| o.states = vec![ChangesetState::Open]),
            &RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(ids(open), vec![cs[0].id]);
    let pending = changesets
        .count(
            &all(|o| o.review_state = Some(ReviewState::Pending)),
            &RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(pending, 2);
    let search = changesets
        .list(&all(|o| o.search = Some("three".into())), &RepoAccess::All)
        .await
        .unwrap();
    assert_eq!(ids(search), vec![cs[2].id], "matches the repo name");
    let search = changesets
        .list(
            &all(|o| o.search = Some("\"thing 1\"".into())),
            &RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(ids(search), vec![cs[1].id]);
    let search = changesets
        .count(
            &all(|o| o.search = Some("fix -three".into())),
            &RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(search, 2, "spec title used while unpublished, negation");
    let closable = changesets
        .count(&all(|o| o.only_closable = true), &RepoAccess::All)
        .await
        .unwrap();
    assert_eq!(closable, 2);
    let page = changesets
        .list(
            &all(|o| {
                o.limit = 1;
            }),
            &RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(ids(page), vec![cs[0].id]);
    let mut next = all(|o| o.limit = 5);
    next.after_id = Some(cs[0].id);
    assert_eq!(changesets.count(&next, &RepoAccess::All).await.unwrap(), 3);
    assert_eq!(
        changesets
            .list(&next, &RepoAccess::All)
            .await
            .unwrap()
            .len(),
        2
    );
    let restricted = RepoAccess::Restricted {
        user_id: Some(alice),
        user_mapping: false,
        unrestricted: Arc::new([repos[0].get() as u32].into_iter().collect()),
        granted: Arc::default(),
    };
    assert_eq!(
        changesets.count(&all(|_| {}), &restricted).await.unwrap(),
        1
    );

    // Counts.
    let counts = changesets.counts(bc.id, &RepoAccess::All).await.unwrap();
    assert_eq!(
        (counts.total, counts.open, counts.draft, counts.unpublished),
        (3, 1, 1, 1)
    );

    // Re-apply: drop c2 (archive), keep the others; then the reconciler archives it.
    let mut tx = bcs.begin_apply().await.unwrap();
    let locked = tx
        .lock_batch_change(Namespace::User(alice), "hello")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tx.changesets(locked.id).await.unwrap().len(), 3);
    tx.archive(bc.id, &[cs[2].id], true).await.unwrap();
    tx.set_ui_publication_state(cs[2].id, Some(UiPublicationState::Draft))
        .await
        .unwrap();
    tx.enqueue(&[cs[2].id], false).await.unwrap();
    tx.commit().await.unwrap();
    let c2 = changesets.get(cs[2].id).await.unwrap().unwrap();
    assert!(c2.archived_in(bc.id));
    assert!(!c2.closing, "unpublished changesets are not closed");
    assert_eq!(c2.ui_publication_state, Some(UiPublicationState::Draft));
    assert_eq!(
        changesets
            .count(&all(|_| {}), &RepoAccess::All)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        changesets
            .count(
                &all(|o| o.archived = ArchivedFilter::Only),
                &RepoAccess::All
            )
            .await
            .unwrap(),
        1
    );
    assert!(changesets.mark_archived(cs[2].id, bc.id).await.unwrap());
    let c2 = changesets.get(cs[2].id).await.unwrap().unwrap();
    assert!(c2.batch_change_ids[&bc.id].is_archived && !c2.batch_change_ids[&bc.id].archive);
    let counts = changesets.counts(bc.id, &RepoAccess::All).await.unwrap();
    assert_eq!((counts.total, counts.archived), (2, 1));

    // A failed attempt shows RETRYING until out of retries; enqueue resets it.
    let got = queue.dequeue("rec", 1).await.unwrap();
    assert_eq!(got[0].id, cs[2].id);
    queue
        .mark_errored(cs[2].id, "rec", "push failed")
        .await
        .unwrap();
    let c2 = changesets.get(cs[2].id).await.unwrap().unwrap();
    assert_eq!(c2.state, ChangesetState::Retrying);
    assert_eq!(c2.reconciler_state, ReconcilerState::Errored);
    assert_eq!(
        changesets
            .count(
                &all(|o| {
                    o.reconciler_states = vec![ReconcilerState::Errored];
                    o.archived = ArchivedFilter::Include;
                }),
                &RepoAccess::All
            )
            .await
            .unwrap(),
        1
    );
    changesets.enqueue(&[cs[2].id]).await.unwrap();
    let c2 = changesets.get(cs[2].id).await.unwrap().unwrap();
    assert_eq!((c2.state, c2.num_failures), (ChangesetState::Processing, 0));

    // Scheduled (rollout windows) until released.
    changesets.schedule(&[cs[2].id]).await.unwrap();
    assert_eq!(
        changesets.get(cs[2].id).await.unwrap().unwrap().state,
        ChangesetState::Scheduled
    );
    assert!(queue.dequeue("rec", 1).await.unwrap().is_empty());
    assert_eq!(changesets.scheduled_ids(10).await.unwrap(), vec![cs[2].id]);
    assert_eq!(
        changesets.place_in_schedule(cs[2].id).await.unwrap(),
        Some(0)
    );
    assert_eq!(changesets.place_in_schedule(cs[0].id).await.unwrap(), None);
    assert_eq!(
        changesets
            .release_scheduled(5, &sourcegull_db::Fence::none("test"))
            .await
            .unwrap(),
        vec![cs[2].id]
    );
    assert!(
        changesets
            .get(cs[2].id)
            .await
            .unwrap()
            .unwrap()
            .window_released
    );
    assert_eq!(queue.dequeue("rec", 1).await.unwrap()[0].id, cs[2].id);
    // A re-enqueue clears the release.
    changesets.enqueue(&[cs[2].id]).await.unwrap();
    assert!(
        !changesets
            .get(cs[2].id)
            .await
            .unwrap()
            .unwrap()
            .window_released
    );
    // Released oldest update first; "publish now" releases one regardless.
    changesets.schedule(&[cs[0].id]).await.unwrap();
    changesets.schedule(&[cs[2].id]).await.unwrap();
    sqlx::query("UPDATE changesets SET updated_at = now() - interval '1 hour' WHERE id = $1")
        .bind(cs[2].id)
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        changesets.scheduled_ids(10).await.unwrap(),
        vec![cs[2].id, cs[0].id]
    );
    assert_eq!(
        changesets.place_in_schedule(cs[0].id).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        changesets.release_now(&[cs[0].id, cs[1].id]).await.unwrap(),
        vec![cs[0].id]
    );
    assert!(
        changesets
            .get(cs[0].id)
            .await
            .unwrap()
            .unwrap()
            .window_released
    );
    assert_eq!(
        changesets
            .release_scheduled(5, &sourcegull_db::Fence::none("test"))
            .await
            .unwrap(),
        vec![cs[2].id]
    );
    for c in queue.dequeue("rec", 2).await.unwrap() {
        queue.mark_complete(c.id, "rec").await.unwrap();
    }

    // Detach: request, then carried out.
    assert_eq!(
        changesets.request_detach(bc.id, &[cs[1].id]).await.unwrap(),
        1
    );
    assert!(
        changesets
            .get(cs[1].id)
            .await
            .unwrap()
            .unwrap()
            .batch_change_ids[&bc.id]
            .detach
    );
    assert!(
        changesets
            .remove_from_batch_change(cs[1].id, bc.id)
            .await
            .unwrap()
    );
    let c1 = changesets.get(cs[1].id).await.unwrap().unwrap();
    assert!(c1.batch_change_ids.is_empty());
    assert!(c1.detached_at.is_some());
    // Re-attaching clears it.
    let mut tx = bcs.begin_apply().await.unwrap();
    tx.attach(bc.id, &[cs[1].id]).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        changesets
            .get(cs[1].id)
            .await
            .unwrap()
            .unwrap()
            .detached_at
            .is_none()
    );

    // Events: upserted by (kind, key).
    let ev = |key: &str, v: i64| NewChangesetEvent {
        kind: "gitlab:approved".into(),
        key: key.into(),
        created_at: chrono::Utc::now(),
        metadata: serde_json::json!({"v": v}),
    };
    changesets
        .upsert_events(cs[0].id, &[ev("1", 1), ev("2", 1)])
        .await
        .unwrap();
    changesets
        .upsert_events(cs[0].id, &[ev("2", 2)])
        .await
        .unwrap();
    let events = changesets.events(&[cs[0].id], &[]).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].metadata.0["v"], 2);
    assert!(
        changesets
            .events(&[cs[0].id], &["other".into()])
            .await
            .unwrap()
            .is_empty()
    );

    // Webhook updates: set fields, merge metadata one level deep, keep the rest.
    sqlx::query("UPDATE changesets SET metadata = $2, title = 'old' WHERE id = $1")
        .bind(cs[0].id)
        .bind(serde_json::json!({"mergeRequest": {"title": "old", "labels": ["x"]}, "k": 1}))
        .execute(db.pool())
        .await
        .unwrap();
    let mut patch = serde_json::Map::new();
    patch.insert("mergeRequest".into(), serde_json::json!({"title": "new"}));
    patch.insert("headPipeline".into(), serde_json::json!({"id": 7}));
    let when = chrono::Utc::now();
    assert!(
        changesets
            .update_from_hook(
                cs[0].id,
                &ChangesetHookUpdate {
                    external_state: Some(ExternalState::Merged),
                    external_check_state: Some(CheckState::Passed),
                    title: Some("new".into()),
                    metadata: patch,
                    next_sync_at: Some(when),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    );
    let c0 = changesets.get(cs[0].id).await.unwrap().unwrap();
    assert_eq!(c0.state, ChangesetState::Merged);
    assert_eq!(c0.external_check_state, Some(CheckState::Passed));
    assert_eq!(c0.title.as_deref(), Some("new"));
    assert_eq!(
        c0.metadata,
        serde_json::json!({"mergeRequest": {"title": "new", "labels": ["x"]}, "k": 1,
                           "headPipeline": {"id": 7}})
    );
    assert_eq!(
        c0.next_sync_at.map(|t| t.timestamp_millis()),
        Some(when.timestamp_millis())
    );
    // Changesets by head branch.
    let on_branch = changesets
        .by_branch(repos[0], GITLAB, "refs/heads/hello")
        .await
        .unwrap();
    assert_eq!(
        on_branch.iter().map(|c| c.id).collect::<Vec<_>>(),
        [cs[0].id]
    );
    // Webhook events are dropped by prefix once a sync replaced them.
    changesets
        .upsert_events(cs[0].id, &[ev("hook:approved:1", 1)])
        .await
        .unwrap();
    assert_eq!(
        changesets
            .delete_events_with_prefix(cs[0].id, "hook:")
            .await
            .unwrap(),
        1
    );
    assert_eq!(changesets.events(&[cs[0].id], &[]).await.unwrap().len(), 2);
    // Restore the state for the rest of the test.
    sqlx::query("UPDATE changesets SET external_state = 'OPEN', next_sync_at = NULL WHERE id = $1")
        .bind(cs[0].id)
        .execute(db.pool())
        .await
        .unwrap();

    // Sync scheduling: published changesets are due.
    let due = changesets.due_for_sync(10).await.unwrap();
    assert_eq!(due, vec![cs[0].id, cs[1].id]);

    // Closing a bulk change marks its open changesets for closing.
    let closed = bcs.close(bc.id, true).await.unwrap().unwrap();
    assert_eq!(closed.state(), BatchChangeState::Closed);
    let c0 = changesets.get(cs[0].id).await.unwrap().unwrap();
    assert!(c0.closing);
    assert_eq!(c0.reconciler_state, ReconcilerState::Queued);

    // Deleting the bulk change untracks its changesets.
    sqlx::query("DELETE FROM batch_changes WHERE id = $1")
        .bind(draft.id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(bcs.delete(bc.id).await.unwrap());
    let c0 = changesets.get(cs[0].id).await.unwrap().unwrap();
    assert!(c0.batch_change_ids.is_empty());
    assert_eq!(c0.owned_by_batch_change_id, None);
}

#[tokio::test]
async fn computed_state_matches_sql() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let r = repo(&db, "gitlab.example.com/a/one").await;
    let c = ChangesetStore::new(&db)
        .create(&NewChangeset::new(r, GITLAB))
        .await
        .unwrap();
    let queue_states = [
        ("scheduled", 0),
        ("queued", 0),
        ("queued", 1),
        ("processing", 0),
        ("errored", 1),
        ("completed", 0),
        ("failed", 3),
        ("canceled", 0),
    ];
    let externals = [
        None,
        Some("OPEN"),
        Some("DRAFT"),
        Some("CLOSED"),
        Some("MERGED"),
        Some("READONLY"),
        Some("DELETED"),
    ];
    for (state, failures) in queue_states {
        for publication in ["UNPUBLISHED", "PUBLISHED"] {
            for external in externals {
                sqlx::query(
                    "UPDATE changesets SET state = $2, num_failures = $3, publication_state = $4,
                         external_state = $5 WHERE id = $1",
                )
                .bind(c.id)
                .bind(state)
                .bind(failures)
                .bind(publication)
                .bind(external)
                .execute(db.pool())
                .await
                .unwrap();
                let got = ChangesetStore::new(&db).get(c.id).await.unwrap().unwrap();
                let want = compute_changeset_state(
                    ReconcilerState::from_queue(state, failures),
                    PublicationState::parse(publication).unwrap(),
                    external.and_then(ExternalState::parse),
                );
                assert_eq!(
                    got.state, want,
                    "{state} {failures} {publication} {external:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn bulk_operations() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let repos = vec![
        repo(&db, "gitlab.example.com/a/one").await,
        repo(&db, "gitlab.example.com/a/two").await,
    ];
    let a = apply(&db, alice, &repos).await;
    let jobs = ChangesetJobStore::new(&db);
    let ids: Vec<i64> = a.changesets.iter().map(|c| c.id).collect();
    let group = jobs
        .create_bulk(
            alice,
            a.batch_change,
            ChangesetJobType::Comment,
            &serde_json::json!({"message": "hi"}),
            &ids,
        )
        .await
        .unwrap();
    let op = jobs.bulk_operation(&group).await.unwrap().unwrap();
    assert_eq!(
        (op.state, op.changeset_count, op.progress),
        ("PROCESSING", 2, 0.0)
    );
    assert_eq!(op.job_type, ChangesetJobType::Comment);

    let queue: Queue<sourcegull_batches::store::ChangesetJob> =
        Queue::new(db.pool().clone(), ChangesetJobStore::queue_config()).unwrap();
    let got = queue.dequeue("w", 2).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].payload.payload["message"], "hi");
    queue.mark_complete(got[0].id, "w").await.unwrap();
    let op = jobs.bulk_operation(&group).await.unwrap().unwrap();
    assert_eq!((op.state, op.progress), ("PROCESSING", 0.5));
    queue
        .mark_failed(got[1].id, "w", "no permission")
        .await
        .unwrap();
    let op = jobs.bulk_operation(&group).await.unwrap().unwrap();
    assert_eq!(op.state, "FAILED");
    assert!(op.finished_at.is_some());
    assert_eq!(
        jobs.errors(&group).await.unwrap(),
        vec![(got[1].payload.changeset_id, "no permission".to_string())]
    );

    let g2 = jobs
        .create_bulk(
            alice,
            a.batch_change,
            ChangesetJobType::Close,
            &serde_json::json!({}),
            &ids[..1],
        )
        .await
        .unwrap();
    let ops = jobs
        .bulk_operations(a.batch_change, None, 10)
        .await
        .unwrap();
    assert_eq!(
        ops.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        vec![g2.as_str(), group.as_str()]
    );
    assert_eq!(jobs.count_bulk_operations(a.batch_change).await.unwrap(), 2);
    assert_eq!(jobs.jobs(&g2).await.unwrap().len(), 1);
    let page = jobs
        .bulk_operations(a.batch_change, Some(ops[0].created_at), 10)
        .await
        .unwrap();
    assert!(page.iter().all(|o| o.id != g2));
}

#[tokio::test]
async fn credentials_encrypted_and_resolved() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let secret = |token: &str| CredentialSecret {
        kind: CredentialKind::PersonalAccessToken,
        token: token.into(),
        ssh: None,
    };

    // Without a keyring nothing can be written.
    assert!(matches!(
        BatchesCredentialStore::new(&db, None)
            .set_user_credential(alice, GITLAB, GITLAB_URL, &secret("x"))
            .await,
        Err(DbError::Invalid(_))
    ));

    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(
        db.encryption_keys()
            .load_keyring(provider.clone())
            .await
            .unwrap(),
    );
    let store = BatchesCredentialStore::new(&db, Some(keyring));

    let mut with_ssh = secret("glpat-alice");
    with_ssh.ssh = Some(SshKeypair {
        public_key: "ssh-ed25519 AAAA".into(),
        private_key: "-----BEGIN-----".into(),
        passphrase: String::new(),
    });
    let uc = store
        .set_user_credential(alice, GITLAB, GITLAB_URL, &with_ssh)
        .await
        .unwrap();
    assert_eq!(uc.user_id, Some(alice));
    assert_eq!(uc.ssh_public_key.as_deref(), Some("ssh-ed25519 AAAA"));
    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT credential_enc FROM user_credentials WHERE id = $1")
            .bind(uc.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        !String::from_utf8_lossy(&raw).contains("glpat-alice"),
        "stored encrypted"
    );
    assert_eq!(store.secret(&uc).await.unwrap(), with_ssh);
    assert!(
        !format!("{with_ssh:?}").contains("glpat"),
        "Debug hides the token"
    );

    // Replacing keeps the id.
    let again = store
        .set_user_credential(alice, GITLAB, GITLAB_URL, &secret("glpat-2"))
        .await
        .unwrap();
    assert_eq!(again.id, uc.id);
    assert_eq!(again.ssh_public_key, None);

    let site = store
        .set_site_credential(GITLAB, GITLAB_URL, &secret("glpat-site"))
        .await
        .unwrap();
    assert!(site.is_site_credential());
    assert_eq!(store.site_credentials().await.unwrap().len(), 1);
    assert_eq!(
        store.site_credential_by_id(site.id).await.unwrap().unwrap(),
        site
    );

    // User credential first, then the site's.
    let (c, s) = store
        .resolve(alice, GITLAB, GITLAB_URL)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((c.id, s.token.as_str()), (uc.id, "glpat-2"));
    let (c, s) = store
        .resolve(bob, GITLAB, GITLAB_URL)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((c.id, s.token.as_str()), (site.id, "glpat-site"));
    assert!(
        store
            .resolve(bob, GITLAB, "https://other/")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.user_credentials(alice).await.unwrap().len(), 1);

    // Rotation: re-encrypt with a newer data key.
    let mut rotated = db.encryption_keys().load_keyring(provider).await.unwrap();
    rotated
        .generate_dek(sourcegull_crypto::RecordClass::UserCredential)
        .unwrap();
    let rotated = BatchesCredentialStore::new(&db, Some(Arc::new(rotated)));
    assert_eq!(rotated.reencrypt().await.unwrap(), 1);
    assert_eq!(rotated.reencrypt().await.unwrap(), 0);
    assert_eq!(rotated.secret(&uc).await.unwrap().token, "glpat-2");

    assert!(store.delete_user_credential(uc.id).await.unwrap());
    assert!(!store.delete_user_credential(uc.id).await.unwrap());
    assert!(store.delete_site_credential(site.id).await.unwrap());
    assert!(
        store
            .resolve(alice, GITLAB, GITLAB_URL)
            .await
            .unwrap()
            .is_none()
    );
}
