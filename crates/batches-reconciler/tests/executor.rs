//! Reconciler tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) with a fake
//! `ChangesetSource` and an in-process repo node pushing to local bare repositories:
//! publish, draft then undraft, update, close and reopen, archived repositories, deleted
//! merge requests, credentials (user, site, missing), retries until `FAILED`, detach and
//! archive, forks, imports, the syncer, and commit-via-API mode (with its fallback to the
//! push and `rejectUnverifiedCommit`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpecStore, BatchesCredentialStore, Changeset, ChangesetSpecStore,
    ChangesetSpecType, ChangesetState, ChangesetStore, CheckState, CredentialKind,
    CredentialSecret, ExternalState, NewBatchChange, NewBatchSpec, NewChangeset, NewChangesetSpec,
    PublicationState, Published, ReconcilerState, ReviewState,
};
use sourcegull_batches_reconciler::source::repo_meta;
use sourcegull_batches_reconciler::{
    FakeClock, Operation, ReconcileError, ReconcileHandler, Reconciler, SYNCER, Scheduler,
    SourceProvider, sync_due,
};
use sourcegull_codehost::{
    Changeset as HostChangeset, ChangesetEvent as HostEvent, ChangesetEventKind, ChangesetSource,
    ChangesetState as HostState, CheckState as HostCheck, CodeHostError, CommitAction, CommitFile,
    CreatedChangeset, Credential, MergeOptions, NewChangeset as HostNew, NewCommit, PushStrategy,
    RepoMeta, ReviewState as HostReview,
};
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::sqlx;
use sourcegull_db::{Db, Fence, Namespace, NewUser, migrate};
use sourcegull_outbound_webhooks::store::OutboundWebhookStore;
use sourcegull_pgqueue::{Queue, QueueConfig, Worker, WorkerOptions};
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{Repo, RepoId, UserId};
use tokio_util::sync::CancellationToken;

const GITLAB_URL: &str = "https://gitlab.example.com/";
const REPO: &str = "gitlab.example.com/a/one";
const DIFF: &str = "diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-hello
+hello world
";
const DIFF2: &str = "diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-hello
+hello again
";

/// How a fake call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    Transient,
    Archived,
    NotFound,
}

#[derive(Default)]
struct State {
    /// `op token` per call (the token is the credential's password, `-` for none).
    calls: Vec<String>,
    mrs: BTreeMap<i64, HostChangeset>,
    next: i64,
    fail_create: Option<Fail>,
    fail_update: Option<Fail>,
    fail_load: Option<Fail>,
    forks: Vec<Option<String>>,
    /// The source can create commits through the API.
    commit_api: bool,
    fail_commit: Option<Fail>,
    /// Commits created through the API, with the repo's external id.
    commits: Vec<(String, NewCommit)>,
}

#[derive(Clone)]
struct FakeSource {
    state: Arc<Mutex<State>>,
    token: Option<String>,
    push_dir: PathBuf,
    fork_dir: PathBuf,
}

impl FakeSource {
    fn record(&self, op: &str) {
        let token = self.token.clone().unwrap_or_else(|| "-".into());
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("{op} {token}"));
    }

    fn fail(f: Fail) -> CodeHostError {
        match f {
            Fail::Transient => CodeHostError::Transport("connection reset".into()),
            Fail::Archived => CodeHostError::Archived("a/one".into()),
            Fail::NotFound => CodeHostError::NotFound("merge request".into()),
        }
    }

    fn get(&self, number: i64) -> sourcegull_codehost::Result<HostChangeset> {
        self.state
            .lock()
            .unwrap()
            .mrs
            .get(&number)
            .cloned()
            .ok_or_else(|| CodeHostError::NotFound("merge request".into()))
    }

    fn modify(
        &self,
        number: i64,
        f: impl FnOnce(&mut HostChangeset),
    ) -> sourcegull_codehost::Result<HostChangeset> {
        let mut st = self.state.lock().unwrap();
        let mr = st
            .mrs
            .get_mut(&number)
            .ok_or_else(|| CodeHostError::NotFound("merge request".into()))?;
        f(mr);
        mr.updated_at = Utc::now();
        Ok(mr.clone())
    }
}

fn title(new: &HostNew) -> String {
    if new.draft {
        format!("Draft: {}", new.title)
    } else {
        new.title.clone()
    }
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
        _repo: &RepoMeta,
        new: &HostNew,
    ) -> sourcegull_codehost::Result<CreatedChangeset> {
        self.record("create");
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.fail_create {
            return Err(Self::fail(f));
        }
        st.next += 1;
        let now = Utc::now();
        let mr = HostChangeset {
            number: st.next,
            title: title(new),
            body: new.body.clone(),
            state: if new.draft {
                HostState::Draft
            } else {
                HostState::Open
            },
            review_state: None,
            check_state: None,
            head_ref: new.head_ref.clone(),
            base_ref: new.base_ref.clone(),
            head_repo_id: new.head_repo_id.clone(),
            labels: vec![],
            url: format!(
                "https://gitlab.example.com/a/one/-/merge_requests/{}",
                st.next
            ),
            created_at: now,
            updated_at: now,
            events: vec![],
            metadata: serde_json::json!({
                "remove_source_branch": new.remove_source_branch,
                "source_project_id": new.head_repo_id,
            }),
        };
        st.mrs.insert(mr.number, mr.clone());
        Ok(CreatedChangeset {
            changeset: mr,
            already_existed: false,
        })
    }

    async fn undraft(
        &self,
        _repo: &RepoMeta,
        number: i64,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("undraft");
        self.modify(number, |mr| {
            mr.state = HostState::Open;
            mr.title = mr.title.trim_start_matches("Draft: ").to_owned();
        })
    }

    async fn update(
        &self,
        _repo: &RepoMeta,
        number: i64,
        new: &HostNew,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("update");
        if let Some(f) = self.state.lock().unwrap().fail_update {
            return Err(Self::fail(f));
        }
        self.modify(number, |mr| {
            mr.title = title(new);
            mr.body = new.body.clone();
            mr.base_ref = new.base_ref.clone();
        })
    }

    async fn close(
        &self,
        _repo: &RepoMeta,
        number: i64,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("close");
        self.modify(number, |mr| mr.state = HostState::Closed)
    }

    async fn reopen(
        &self,
        _repo: &RepoMeta,
        number: i64,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("reopen");
        self.modify(number, |mr| mr.state = HostState::Open)
    }

    async fn merge(
        &self,
        _repo: &RepoMeta,
        number: i64,
        _opts: MergeOptions,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("merge");
        self.modify(number, |mr| mr.state = HostState::Merged)
    }

    async fn create_comment(
        &self,
        _repo: &RepoMeta,
        _number: i64,
        _body: &str,
    ) -> sourcegull_codehost::Result<()> {
        self.record("comment");
        Ok(())
    }

    async fn load(
        &self,
        _repo: &RepoMeta,
        number: i64,
    ) -> sourcegull_codehost::Result<HostChangeset> {
        self.record("load");
        if let Some(f) = self.state.lock().unwrap().fail_load {
            return Err(Self::fail(f));
        }
        let mut mr = self.get(number)?;
        mr.review_state = Some(HostReview::Approved);
        mr.check_state = Some(HostCheck::Passed);
        mr.events = vec![HostEvent {
            kind: ChangesetEventKind::Approved,
            key: "note:1".into(),
            timestamp: mr.updated_at,
            metadata: serde_json::json!({"user": "bob"}),
        }];
        Ok(mr)
    }

    async fn get_fork(
        &self,
        repo: &RepoMeta,
        namespace: Option<&str>,
    ) -> sourcegull_codehost::Result<RepoMeta> {
        self.record("fork");
        self.state
            .lock()
            .unwrap()
            .forks
            .push(namespace.map(str::to_owned));
        let ns = namespace.unwrap_or("alice");
        let mut fork = repo.clone();
        fork.external.id = "fork-id".into();
        fork.fork = true;
        fork.metadata = serde_json::json!({"pathWithNamespace": format!("{ns}/a-one")});
        Ok(fork)
    }

    fn push_strategy(
        &self,
        repo: &RepoMeta,
        _cred: &Credential,
    ) -> sourcegull_codehost::Result<PushStrategy> {
        let dir = if repo.fork {
            &self.fork_dir
        } else {
            &self.push_dir
        };
        Ok(PushStrategy {
            push_url: url::Url::from_file_path(dir).unwrap(),
            fork_namespace: repo.fork.then(|| {
                repo.metadata["pathWithNamespace"]
                    .as_str()
                    .unwrap()
                    .split('/')
                    .next()
                    .unwrap()
                    .to_owned()
            }),
        })
    }

    fn supports_commit_api(&self) -> bool {
        self.state.lock().unwrap().commit_api
    }

    async fn create_commit(
        &self,
        repo: &RepoMeta,
        commit: &NewCommit,
    ) -> sourcegull_codehost::Result<String> {
        self.record("commit");
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.fail_commit {
            return Err(Self::fail(f));
        }
        st.commits.push((repo.external.id.clone(), commit.clone()));
        Ok("c".repeat(40))
    }

    async fn validate_credential(&self, _cred: &Credential) -> sourcegull_codehost::Result<()> {
        Ok(())
    }
}

struct FakeProvider {
    source: FakeSource,
    archived: Mutex<bool>,
}

#[async_trait]
impl SourceProvider for FakeProvider {
    async fn source(
        &self,
        repo: &Repo,
    ) -> sourcegull_batches_reconciler::Result<(Arc<dyn ChangesetSource>, RepoMeta)> {
        let mut meta = repo_meta(
            repo,
            "https://gitlab.example.com/a/one.git".into(),
            serde_json::json!({}),
        );
        meta.archived = *self.archived.lock().unwrap();
        Ok((Arc::new(self.source.clone()), meta))
    }
}

struct Env {
    _t: TestDb,
    db: Db,
    _node: RepoNode,
    source_repo: SourceRepo,
    keyring: Arc<sourcegull_crypto::Keyring>,
    state: Arc<Mutex<State>>,
    provider: Arc<FakeProvider>,
    push: tempfile::TempDir,
    fork: tempfile::TempDir,
    repo: RepoId,
    alice: UserId,
    batch_spec: i64,
    batch_change: i64,
    config: SiteConfigHandle,
    reconciler: Reconciler,
}

fn bare() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let ok = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(d.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    d
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
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

    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let repo: RepoId = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '42', 'gitlab', $2) RETURNING id",
    )
    .bind(REPO)
    .bind(GITLAB_URL)
    .fetch_one(db.pool())
    .await
    .unwrap();

    let source_repo = SourceRepo::new();
    source_repo.commit("initial", &[("README.md", "hello\n")]);
    let node = RepoNode::start(source_repo.remote()).await;
    node.clone_repo(repo, REPO).await;

    let batch_spec = BatchSpecStore::new(&db)
        .create(&NewBatchSpec {
            raw_spec: "name: hello\n".into(),
            spec: serde_json::json!({"name": "hello"}),
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
    let push = bare();
    let fork = bare();
    let fake = Arc::new(FakeProvider {
        source: FakeSource {
            state: state.clone(),
            token: None,
            push_dir: push.path().to_owned(),
            fork_dir: fork.path().to_owned(),
        },
        archived: Mutex::new(false),
    });
    let config = SiteConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.example.com/".into()),
            batch_changes_auto_delete_branch: true,
            ..SiteConfig::default()
        },
    );
    let reconciler = Reconciler::new(
        db.clone(),
        Some(keyring.clone()),
        fake.clone(),
        Some(node.clients.clone()),
        config.clone(),
    )
    .with_sync_delay(Duration::ZERO);
    Some(Env {
        _t: t,
        db,
        _node: node,
        source_repo,
        keyring,
        state,
        provider: fake,
        push,
        fork,
        repo,
        alice,
        batch_spec,
        batch_change,
        config,
        reconciler,
    })
}

impl Env {
    async fn user_credential(&self, token: &str) {
        BatchesCredentialStore::new(&self.db, Some(self.keyring.clone()))
            .set_user_credential(
                self.alice,
                "gitlab",
                GITLAB_URL,
                &CredentialSecret {
                    kind: CredentialKind::PersonalAccessToken,
                    token: token.into(),
                    ssh: None,
                },
            )
            .await
            .unwrap();
    }

    async fn site_credential(&self, token: &str) {
        BatchesCredentialStore::new(&self.db, Some(self.keyring.clone()))
            .set_site_credential(
                "gitlab",
                GITLAB_URL,
                &CredentialSecret {
                    kind: CredentialKind::PersonalAccessToken,
                    token: token.into(),
                    ssh: None,
                },
            )
            .await
            .unwrap();
    }

    fn new_spec(&self, published: Option<Published>) -> NewChangesetSpec {
        NewChangesetSpec {
            batch_spec_id: Some(self.batch_spec),
            head_ref: Some("refs/heads/fix".into()),
            base_ref: Some("refs/heads/main".into()),
            base_rev: Some(self.source_repo.head()),
            title: Some("Fix the readme".into()),
            body: Some("It says hello.".into()),
            published,
            diff: Some(DIFF.as_bytes().to_vec()),
            diff_stat_added: Some(1),
            diff_stat_deleted: Some(1),
            commit_message: Some("Fix the readme".into()),
            commit_author_name: Some("Alice".into()),
            commit_author_email: Some("alice@example.com".into()),
            ..NewChangesetSpec::new(self.repo, ChangesetSpecType::Branch)
        }
    }

    async fn spec(&self, s: NewChangesetSpec) -> i64 {
        ChangesetSpecStore::new(&self.db)
            .create(&s)
            .await
            .unwrap()
            .id
    }

    /// A changeset of the bulk change with spec `spec`.
    async fn changeset(&self, spec: i64) -> i64 {
        ChangesetStore::new(&self.db)
            .create(&NewChangeset {
                current_spec_id: Some(spec),
                batch_change_id: Some(self.batch_change),
                owned_by_batch_change_id: Some(self.batch_change),
                ..NewChangeset::new(self.repo, "gitlab")
            })
            .await
            .unwrap()
            .id
    }

    /// Re-applies with a new spec for changeset `id`.
    async fn reapply(&self, id: i64, spec: i64) {
        let mut tx = BatchChangeStore::new(&self.db).begin_apply().await.unwrap();
        tx.set_changeset_spec(id, spec).await.unwrap();
        tx.enqueue(&[id], false).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn get(&self, id: i64) -> Changeset {
        ChangesetStore::new(&self.db)
            .get(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn reconcile(&self, id: i64) -> Result<Vec<Operation>, ReconcileError> {
        let ch = self.get(id).await;
        let ops = self.reconciler.reconcile(&ch).await.map(|p| p.ops)?;
        // Completed, as the queue worker leaves it (a re-apply then moves the spec to
        // `previous_spec_id`).
        sourcegull_db::sqlx::query("UPDATE changesets SET state = 'completed' WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await
            .unwrap();
        Ok(ops)
    }

    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().calls)
    }

    fn mr(&self, number: i64) -> HostChangeset {
        self.state.lock().unwrap().mrs[&number].clone()
    }
}

#[tokio::test]
async fn publish_update_close_reopen() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;

    let ops = env.reconcile(id).await.unwrap();
    assert_eq!(ops, vec![Operation::Push, Operation::Publish]);
    assert_eq!(env.calls(), vec!["create alice-token"]);
    // The commit is on the pushed branch, with the spec's author and message.
    let push = env.push.path();
    assert_eq!(
        git(push, &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello world"
    );
    assert_eq!(
        git(
            push,
            &["log", "-1", "--format=%an <%ae> %s", "refs/heads/fix"]
        )
        .unwrap(),
        "Alice <alice@example.com> Fix the readme"
    );
    let mr = env.mr(1);
    assert_eq!(mr.title, "Fix the readme");
    assert_eq!(
        (mr.head_ref.as_str(), mr.base_ref.as_str()),
        ("fix", "main")
    );
    assert_eq!(
        mr.body,
        "It says hello.\n\n[_Created by Sourcegull bulk change `alice/hello`._]\
         (https://sourcegull.example.com/users/alice/bulk-changes/hello)"
    );
    assert_eq!(mr.metadata["remove_source_branch"], true);
    assert_eq!(mr.head_repo_id, None);

    let ch = env.get(id).await;
    assert_eq!(ch.publication_state, PublicationState::Published);
    assert_eq!(ch.external_id.as_deref(), Some("1"));
    assert_eq!(ch.external_branch.as_deref(), Some("refs/heads/fix"));
    assert_eq!(ch.external_state, Some(ExternalState::Open));
    assert_eq!(
        (ch.diff_stat_added, ch.diff_stat_deleted),
        (Some(1), Some(1))
    );
    assert_eq!(ch.title.as_deref(), Some("Fix the readme"));
    let next = ch.next_sync_at.unwrap();
    assert!(next > Utc::now() && next <= Utc::now() + chrono::Duration::minutes(3));

    // Reconciling again with the same spec does nothing.
    assert_eq!(env.reconcile(id).await.unwrap(), vec![]);
    assert!(env.calls().is_empty());

    // A new title updates the merge request.
    let spec2 = env
        .spec(NewChangesetSpec {
            title: Some("Fix the README".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec2).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Update]);
    assert_eq!(env.calls(), vec!["update alice-token"]);
    assert_eq!(env.get(id).await.title.as_deref(), Some("Fix the README"));

    // A new diff is pushed and synced.
    let spec3 = env
        .spec(NewChangesetSpec {
            title: Some("Fix the README".into()),
            diff: Some(DIFF2.as_bytes().to_vec()),
            diff_stat_added: Some(2),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec3).await;
    assert_eq!(
        env.reconcile(id).await.unwrap(),
        vec![Operation::Push, Operation::Sleep, Operation::Sync]
    );
    assert_eq!(env.calls(), vec!["load alice-token"]);
    assert_eq!(
        git(push, &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello again"
    );
    let ch = env.get(id).await;
    assert_eq!(ch.diff_stat_added, Some(2));
    assert_eq!(ch.external_review_state, Some(ReviewState::Approved));
    assert_eq!(ch.external_check_state, Some(CheckState::Passed));
    let events = ChangesetStore::new(&env.db)
        .events(&[id], &[])
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "APPROVED");

    // Closing.
    ChangesetStore::new(&env.db).close(&[id]).await.unwrap();
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Close]);
    assert_eq!(env.calls(), vec!["close alice-token"]);
    let ch = env.get(id).await;
    assert_eq!(ch.external_state, Some(ExternalState::Closed));
    assert!(!ch.closing);

    // Still attached to its owner after a re-apply: reopened.
    env.reapply(id, spec3).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Reopen]);
    assert_eq!(env.calls(), vec!["reopen alice-token"]);
    assert_eq!(env.get(id).await.external_state, Some(ExternalState::Open));
}

/// Batch steps (the step runner, `gull bulk`) write diffs with `git diff --no-prefix`: those are
/// applied with `-p0` (with `-p1` they fail with "git diff header lacks filename information").
#[tokio::test]
async fn publishes_diffs_without_prefixes() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env
        .spec(NewChangesetSpec {
            diff: Some(
                "diff --git README.md README.md\nindex 3b18e51..95d09f2 100644\n--- README.md\n\
                 +++ README.md\n@@ -1 +1 @@\n-hello\n+hello world\n"
                    .as_bytes()
                    .to_vec(),
            ),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(spec).await;
    assert_eq!(
        env.reconcile(id).await.unwrap(),
        vec![Operation::Push, Operation::Publish]
    );
    assert_eq!(
        git(env.push.path(), &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello world"
    );
}

#[tokio::test]
async fn draft_then_undraft() {
    let Some(env) = setup().await else { return };
    env.site_credential("site-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Draft))).await;
    let id = env.changeset(spec).await;
    assert_eq!(
        env.reconcile(id).await.unwrap(),
        vec![Operation::Push, Operation::PublishDraft]
    );
    // No user credential: the site credential is used.
    assert_eq!(env.calls(), vec!["create site-token"]);
    assert_eq!(env.mr(1).title, "Draft: Fix the readme");
    let ch = env.get(id).await;
    assert_eq!(ch.external_state, Some(ExternalState::Draft));

    let spec2 = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    env.reapply(id, spec2).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Undraft]);
    assert_eq!(env.calls(), vec!["undraft site-token"]);
    let ch = env.get(id).await;
    assert_eq!(ch.external_state, Some(ExternalState::Open));
    assert_eq!(ch.title.as_deref(), Some("Fix the readme"));
}

#[tokio::test]
async fn unpublished_specs_do_nothing() {
    let Some(env) = setup().await else { return };
    let spec = env.spec(env.new_spec(Some(Published::Bool(false)))).await;
    let id = env.changeset(spec).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![]);
    assert!(env.calls().is_empty());
    let ch = env.get(id).await;
    assert_eq!(ch.publication_state, PublicationState::Unpublished);
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/fix"]), None);
}

#[tokio::test]
async fn archived_repositories_are_read_only() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    env.calls();

    // Updating a merge request of an archived project marks it read-only, without failing.
    env.state.lock().unwrap().fail_update = Some(Fail::Archived);
    let spec2 = env
        .spec(NewChangesetSpec {
            title: Some("Other".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec2).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Update]);
    let ch = env.get(id).await;
    assert_eq!(ch.external_state, Some(ExternalState::ReadOnly));
    // Read-only changesets are left alone.
    let spec3 = env
        .spec(NewChangesetSpec {
            title: Some("Third".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec3).await;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![]);

    // Pushing to an archived repository fails without a retry and marks it read-only.
    *env.provider.archived.lock().unwrap() = true;
    let spec4 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/other".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id2 = env.changeset(spec4).await;
    let err = env.reconcile(id2).await.unwrap_err();
    assert!(matches!(err, ReconcileError::ReadOnly(_)), "{err}");
    assert!(!err.is_retryable());
    let ch = env.get(id2).await;
    assert_eq!(ch.external_state, Some(ExternalState::ReadOnly));
    assert_eq!(ch.publication_state, PublicationState::Unpublished);
    assert_eq!(
        git(env.push.path(), &["rev-parse", "refs/heads/other"]),
        None
    );
}

#[tokio::test]
async fn refused_pushes_and_bad_patches_fail() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;

    // A patch that does not apply fails at once.
    let bad = env
        .spec(NewChangesetSpec {
            diff: Some(DIFF.replace("-hello", "-goodbye").into_bytes()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(bad).await;
    let err = env.reconcile(id).await.unwrap_err();
    assert!(matches!(err, ReconcileError::PatchFailed { .. }), "{err}");
    assert!(!err.is_retryable());
    assert!(env.calls().is_empty());

    // A second changeset on the same branch as a published one is refused.
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let first = env.changeset(spec).await;
    env.reconcile(first).await.unwrap();
    let again = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let second = env.changeset(again).await;
    let err = env.reconcile(second).await.unwrap_err();
    assert!(matches!(err, ReconcileError::SameBranch), "{err}");
}

#[tokio::test]
async fn deleted_merge_requests_and_the_syncer() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    env.calls();
    // The reconciler finished: complete the queue row as the worker does.
    sqlx::query("UPDATE changesets SET state = 'completed', next_sync_at = now() WHERE id = $1")
        .bind(id)
        .execute(env.db.pool())
        .await
        .unwrap();

    // An event a webhook stored earlier; the full sync replaces it.
    ChangesetStore::new(&env.db)
        .upsert_events(
            id,
            &[sourcegull_batches::store::NewChangesetEvent {
                kind: "APPROVED".into(),
                key: "hook:approved:7:1".into(),
                created_at: Utc::now(),
                metadata: serde_json::json!({}),
            }],
        )
        .await
        .unwrap();

    // The syncer loads due changesets.
    let report = sync_due(&env.reconciler, 10, &Fence::none(SYNCER))
        .await
        .unwrap();
    assert_eq!((report.synced, report.failed), (1, 0));
    assert_eq!(env.calls(), vec!["load alice-token"]);
    let keys: Vec<String> = ChangesetStore::new(&env.db)
        .events(&[id], &[])
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(keys, ["note:1"]);
    let ch = env.get(id).await;
    assert_eq!(ch.external_review_state, Some(ReviewState::Approved));
    assert!(ch.next_sync_at.unwrap() > Utc::now());
    // Not due again yet.
    assert_eq!(
        sync_due(&env.reconciler, 10, &Fence::none(SYNCER))
            .await
            .unwrap()
            .synced,
        0
    );

    // A merge request gone from the code host is DELETED and no longer synced.
    env.state.lock().unwrap().fail_load = Some(Fail::NotFound);
    sqlx::query("UPDATE changesets SET next_sync_at = now() WHERE id = $1")
        .bind(id)
        .execute(env.db.pool())
        .await
        .unwrap();
    assert_eq!(
        sync_due(&env.reconciler, 10, &Fence::none(SYNCER))
            .await
            .unwrap()
            .synced,
        1
    );
    let ch = env.get(id).await;
    assert_eq!(ch.external_state, Some(ExternalState::Deleted));
    assert_eq!(ch.state, ChangesetState::Deleted);
    assert!(ch.external_deleted_at.is_some());
    assert!(
        ChangesetStore::new(&env.db)
            .due_for_sync(10)
            .await
            .unwrap()
            .is_empty()
    );

    // Transient sync errors are recorded and retried later.
    let spec2 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/two".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id2 = env.changeset(spec2).await;
    env.reconcile(id2).await.unwrap();
    sqlx::query("UPDATE changesets SET state = 'completed', next_sync_at = now() WHERE id = $1")
        .bind(id2)
        .execute(env.db.pool())
        .await
        .unwrap();
    env.state.lock().unwrap().fail_load = Some(Fail::Transient);
    assert_eq!(
        sync_due(&env.reconciler, 10, &Fence::none(SYNCER))
            .await
            .unwrap()
            .failed,
        1
    );
    let ch = env.get(id2).await;
    assert!(ch.syncer_error.unwrap().contains("connection reset"));
    assert!(ch.next_sync_at.unwrap() > Utc::now() + chrono::Duration::minutes(10));
}

#[tokio::test]
async fn sync_without_credentials_uses_the_connection_token() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    env.calls();
    sqlx::query("DELETE FROM user_credentials")
        .execute(env.db.pool())
        .await
        .unwrap();
    let ch = env.get(id).await;
    env.reconciler
        .sync(&ch, &Fence::none(SYNCER))
        .await
        .unwrap();
    assert_eq!(env.calls(), vec!["load -"]);
}

/// Runs the reconciler worker over the queue (no retry backoff) until `done` holds for
/// changeset `id`.
async fn run_worker(env: &Env, id: i64, done: impl Fn(&Changeset) -> bool) -> Changeset {
    let config = QueueConfig {
        backoff_base: Duration::ZERO,
        max_failures: 3,
        ..ChangesetStore::queue_config()
    };
    let queue: Queue<Changeset> = Queue::new(env.db.pool().clone(), config).unwrap();
    let mut opts = WorkerOptions::new("test-worker");
    opts.poll_interval = Duration::from_millis(100);
    let worker = Worker::new(queue, ReconcileHandler::new(env.reconciler.clone()), opts);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(worker.run(cancel.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let ch = loop {
        let ch = env.get(id).await;
        if done(&ch) {
            break ch;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {ch:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    cancel.cancel();
    task.await.unwrap();
    ch
}

#[tokio::test]
async fn publish_waits_for_the_rollout_window() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let mut config = env.config.load().value.clone();
    config.batch_changes_rollout_windows = serde_json::from_value(serde_json::json!([
        {"rate": "1/hour"}
    ]))
    .unwrap();
    assert!(env.config.store(2, config));
    assert!(env.reconciler.has_rollout_windows());

    // The worker plans a publish and schedules the changeset instead.
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    let ch = run_worker(&env, id, |c| c.state == ChangesetState::Scheduled).await;
    assert_eq!(ch.reconciler_state, ReconcilerState::Scheduled);
    assert!(!ch.window_released);
    assert!(env.calls().is_empty());
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/fix"]), None);
    // Directly too: the plan says it was scheduled.
    let plan = env.reconciler.reconcile(&env.get(id).await).await.unwrap();
    assert!(plan.scheduled);
    assert_eq!(plan.ops, vec![Operation::Push, Operation::Publish]);
    assert!(env.calls().is_empty());

    // A second changeset waits behind it.
    let spec2 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/two".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id2 = env.changeset(spec2).await;
    run_worker(&env, id2, |c| c.state == ChangesetState::Scheduled).await;

    // The window releases one per hour; the released one is published.
    let clock = FakeClock::new(Utc::now());
    let mut scheduler = Scheduler::new(env.db.clone(), env.config.clone(), Arc::new(clock.clone()));
    assert_eq!(scheduler.tick().await.unwrap().released, vec![id]);
    let ch = run_worker(&env, id, |c| c.state == ChangesetState::Open).await;
    assert!(ch.window_released);
    assert_eq!(env.calls(), vec!["create alice-token"]);
    assert_eq!(env.get(id2).await.state, ChangesetState::Scheduled);
    assert!(scheduler.tick().await.unwrap().released.is_empty());
    clock.advance(chrono::Duration::hours(1));
    assert_eq!(scheduler.tick().await.unwrap().released, vec![id2]);
    run_worker(&env, id2, |c| c.state == ChangesetState::Open).await;
    assert_eq!(env.calls(), vec!["create alice-token"]);

    // Updates of published changesets do not wait.
    let spec3 = env
        .spec(NewChangesetSpec {
            title: Some("Fix the readme, again".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec3).await;
    let plan = env.reconciler.reconcile(&env.get(id).await).await.unwrap();
    assert!(!plan.scheduled);
    assert_eq!(plan.ops, vec![Operation::Update]);
    assert_eq!(env.calls(), vec!["update alice-token"]);

    // "Publish now" skips the window.
    let spec4 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/three".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id4 = env.changeset(spec4).await;
    run_worker(&env, id4, |c| c.state == ChangesetState::Scheduled).await;
    assert_eq!(
        ChangesetStore::new(&env.db)
            .release_now(&[id4])
            .await
            .unwrap(),
        vec![id4]
    );
    run_worker(&env, id4, |c| c.state == ChangesetState::Open).await;
}

#[tokio::test]
async fn worker_retries_then_fails() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    env.state.lock().unwrap().fail_create = Some(Fail::Transient);
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    let ch = run_worker(&env, id, |c| c.reconciler_state == ReconcilerState::Failed).await;
    assert_eq!(ch.state, ChangesetState::Failed);
    // (3 attempts here; the queue allows 60.)
    assert_eq!(ChangesetStore::queue_config().max_failures, 60);
    assert_eq!(ch.num_failures, 3);
    assert!(ch.failure_message.unwrap().contains("connection reset"));
    assert_eq!(env.calls().len(), 3);

    // Once the code host answers again, a re-enqueue publishes it.
    env.state.lock().unwrap().fail_create = None;
    ChangesetStore::new(&env.db).enqueue(&[id]).await.unwrap();
    let ch = run_worker(&env, id, |c| {
        c.reconciler_state == ReconcilerState::Completed
    })
    .await;
    assert_eq!(ch.state, ChangesetState::Open);
    assert_eq!(ch.failure_message, None);
}

#[tokio::test]
async fn missing_credentials_fail_without_retry() {
    let Some(env) = setup().await else { return };
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    let err = env.reconcile(id).await.unwrap_err();
    assert!(
        matches!(err, ReconcileError::MissingCredentials { .. }),
        "{err}"
    );
    assert!(!err.is_retryable());
    assert!(err.to_string().contains(REPO), "{err}");

    let ch = run_worker(&env, id, |c| c.reconciler_state == ReconcilerState::Failed).await;
    assert_eq!(ch.num_failures, 1);
    assert!(ch.failure_message.unwrap().contains("no credential"));
    assert!(env.calls().is_empty());
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/fix"]), None);
}

#[tokio::test]
async fn detach_and_archive() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    env.calls();

    // Archive (closing first, as an apply that drops the changeset does).
    let mut tx = BatchChangeStore::new(&env.db).begin_apply().await.unwrap();
    tx.archive(env.batch_change, &[id], true).await.unwrap();
    tx.enqueue(&[id], false).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        env.reconcile(id).await.unwrap(),
        vec![Operation::Archive, Operation::Close]
    );
    assert_eq!(env.calls(), vec!["close alice-token"]);
    let ch = env.get(id).await;
    assert!(ch.batch_change_ids[&env.batch_change].is_archived);
    assert!(!ch.batch_change_ids[&env.batch_change].archive);
    assert_eq!(ch.external_state, Some(ExternalState::Closed));
    // Archived: nothing more to do.
    assert_eq!(env.reconcile(id).await.unwrap(), vec![]);

    // Detach.
    ChangesetStore::new(&env.db)
        .request_detach(env.batch_change, &[id])
        .await
        .unwrap();
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Detach]);
    assert!(env.calls().is_empty());
    let ch = env.get(id).await;
    assert!(ch.batch_change_ids.is_empty());
    assert!(ch.detached_at.is_some());
}

#[tokio::test]
async fn forks() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    // The template's `fork: true`.
    let spec = env
        .spec(NewChangesetSpec {
            fork_namespace: Some("<user>".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    assert_eq!(env.calls(), vec!["fork alice-token", "create alice-token"]);
    assert_eq!(env.state.lock().unwrap().forks, vec![None]);
    assert_eq!(
        git(env.fork.path(), &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello world"
    );
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/fix"]), None);
    assert_eq!(env.mr(1).head_repo_id.as_deref(), Some("fork-id"));
    let ch = env.get(id).await;
    assert_eq!(ch.external_fork_namespace.as_deref(), Some("alice"));
    assert_eq!(ch.external_fork_name.as_deref(), Some("a-one"));

    // Later pushes go to the same fork.
    let spec2 = env
        .spec(NewChangesetSpec {
            fork_namespace: Some("<user>".into()),
            diff: Some(DIFF2.as_bytes().to_vec()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec2).await;
    env.reconcile(id).await.unwrap();
    assert_eq!(
        env.state.lock().unwrap().forks,
        vec![None, Some("alice".into())]
    );
    assert_eq!(
        git(env.fork.path(), &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello again"
    );

    // `batchChanges.enforceForks` forks changesets whose template does not ask for it.
    let mut cfg = env.config.load().value.clone();
    cfg.batch_changes_enforce_forks = true;
    env.config.store(2, cfg);
    let spec3 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/enforced".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id3 = env.changeset(spec3).await;
    env.reconcile(id3).await.unwrap();
    assert!(git(env.fork.path(), &["rev-parse", "refs/heads/enforced"]).is_some());
    assert_eq!(
        env.get(id3).await.external_fork_namespace.as_deref(),
        Some("alice")
    );
}

#[tokio::test]
async fn imports() {
    let Some(env) = setup().await else { return };
    // An existing merge request on the code host.
    {
        let mut st = env.state.lock().unwrap();
        st.next = 7;
        let now = Utc::now();
        st.mrs.insert(
            7,
            HostChangeset {
                number: 7,
                title: "Existing".into(),
                body: "b".into(),
                state: HostState::Open,
                review_state: None,
                check_state: None,
                head_ref: "feature".into(),
                base_ref: "main".into(),
                head_repo_id: None,
                labels: vec![],
                url: String::new(),
                created_at: now,
                updated_at: now,
                events: vec![],
                metadata: serde_json::json!({"iid": 7}),
            },
        );
    }
    let id = ChangesetStore::new(&env.db)
        .create(&NewChangeset {
            external_id: Some("7".into()),
            batch_change_id: Some(env.batch_change),
            ..NewChangeset::new(env.repo, "gitlab")
        })
        .await
        .unwrap()
        .id;
    assert_eq!(env.reconcile(id).await.unwrap(), vec![Operation::Import]);
    // Reads fall back to the connection's token.
    assert_eq!(env.calls(), vec!["load -"]);
    let ch = env.get(id).await;
    assert_eq!(ch.publication_state, PublicationState::Published);
    assert_eq!(ch.external_state, Some(ExternalState::Open));
    assert_eq!(ch.external_branch.as_deref(), Some("refs/heads/feature"));
    assert_eq!(ch.title.as_deref(), Some("Existing"));
    assert_eq!(ch.metadata["iid"], 7);
    assert_eq!(
        ChangesetStore::new(&env.db)
            .events(&[id], &[])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(env.reconcile(id).await.unwrap(), vec![]);

    // An import of a merge request that does not exist fails without a retry.
    let id2 = ChangesetStore::new(&env.db)
        .create(&NewChangeset {
            external_id: Some("99".into()),
            batch_change_id: Some(env.batch_change),
            ..NewChangeset::new(env.repo, "gitlab")
        })
        .await
        .unwrap()
        .id;
    let err = env.reconcile(id2).await.unwrap_err();
    assert!(!err.is_retryable(), "{err}");
}

impl Env {
    fn configure(&self, f: impl FnOnce(&mut SiteConfig)) {
        let mut cfg = self.config.load().value.clone();
        f(&mut cfg);
        let version = self.config.load().version + 1;
        self.config.store(version, cfg);
    }

    fn commits(&self) -> Vec<(String, NewCommit)> {
        std::mem::take(&mut self.state.lock().unwrap().commits)
    }
}

#[tokio::test]
async fn commit_via_api() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    env.configure(|c| c.batch_changes_commit_via_api = true);
    env.state.lock().unwrap().commit_api = true;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;

    let ops = env.reconcile(id).await.unwrap();
    assert_eq!(ops, vec![Operation::Push, Operation::Publish]);
    assert_eq!(
        env.calls(),
        vec!["commit alice-token", "create alice-token"]
    );
    // Nothing is pushed with git: the code host created the commit.
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/fix"]), None);
    let commits = env.commits();
    assert_eq!(commits.len(), 1);
    let (repo_id, c) = &commits[0];
    assert_eq!(repo_id, "42");
    assert_eq!(
        c,
        &NewCommit {
            branch: "fix".into(),
            message: "Fix the readme".into(),
            author_name: "Alice".into(),
            author_email: "alice@example.com".into(),
            start_sha: env.source_repo.head(),
            start_repo_id: None,
            force: true,
            files: vec![CommitFile {
                action: CommitAction::Update,
                path: "README.md".into(),
                previous_path: None,
                content: Some(b"hello world\n".to_vec()),
                executable: None,
            }],
        }
    );
    let ch = env.get(id).await;
    assert_eq!(ch.external_branch.as_deref(), Some("refs/heads/fix"));
    assert_eq!(ch.diff_stat_added, Some(1));
    assert_eq!(ch.publication_state, PublicationState::Published);

    // A fork: the commit starts from the target project's base.
    let forked = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/forked".into()),
            fork_namespace: Some("<user>".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let fid = env.changeset(forked).await;
    env.reconcile(fid).await.unwrap();
    assert_eq!(
        env.calls(),
        vec![
            "fork alice-token",
            "commit alice-token",
            "create alice-token"
        ]
    );
    let commits = env.commits();
    assert_eq!(commits[0].0, "fork-id");
    assert_eq!(commits[0].1.start_repo_id.as_deref(), Some("42"));
    assert_eq!(
        git(env.fork.path(), &["rev-parse", "refs/heads/forked"]),
        None
    );
    let ch = env.get(fid).await;
    assert_eq!(ch.external_fork_namespace.as_deref(), Some("alice"));
    assert_eq!(ch.external_fork_name.as_deref(), Some("a-one"));

    // A patch that does not apply fails at once, as with the push.
    let bad = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/bad".into()),
            diff: Some(DIFF.replace("-hello", "-goodbye").into_bytes()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let bid = env.changeset(bad).await;
    let err = env.reconcile(bid).await.unwrap_err();
    assert!(matches!(err, ReconcileError::PatchFailed { .. }), "{err}");
    assert!(env.calls().is_empty());

    // An archived project refuses the commit: read-only.
    env.state.lock().unwrap().fail_commit = Some(Fail::Archived);
    let archived = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/archived".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let aid = env.changeset(archived).await;
    let err = env.reconcile(aid).await.unwrap_err();
    assert!(matches!(err, ReconcileError::ReadOnly(_)), "{err}");
    assert_eq!(
        env.get(aid).await.external_state,
        Some(ExternalState::ReadOnly)
    );
}

#[tokio::test]
async fn commit_via_api_falls_back_to_the_push() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    env.state.lock().unwrap().commit_api = true;
    // Larger than the API limit.
    env.configure(|c| {
        c.batch_changes_commit_via_api = true;
        c.batch_changes_commit_via_api_max_bytes = 5;
    });
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    assert_eq!(env.calls(), vec!["create alice-token"]);
    assert!(env.commits().is_empty());
    assert_eq!(
        git(env.push.path(), &["show", "refs/heads/fix:README.md"]).unwrap(),
        "hello world"
    );

    // Too many files.
    env.configure(|c| {
        c.batch_changes_commit_via_api_max_bytes = 1 << 20;
        c.batch_changes_commit_via_api_max_files = 1;
    });
    let two = "diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-hello
+hello two
diff --git a/NEW.md b/NEW.md
new file mode 100644
--- /dev/null
+++ b/NEW.md
@@ -0,0 +1 @@
+new
";
    let spec = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/two".into()),
            diff: Some(two.as_bytes().to_vec()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    assert!(env.commits().is_empty());
    assert_eq!(
        git(env.push.path(), &["show", "refs/heads/two:NEW.md"]).unwrap(),
        "new"
    );

    // A code host without a commit API.
    env.state.lock().unwrap().commit_api = false;
    env.configure(|c| c.batch_changes_commit_via_api_max_files = 1000);
    let spec = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/three".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(spec).await;
    env.calls();
    env.reconcile(id).await.unwrap();
    assert_eq!(env.calls(), vec!["create alice-token"]);
    assert!(git(env.push.path(), &["rev-parse", "refs/heads/three"]).is_some());
}

#[tokio::test]
async fn reject_unverified_commit() {
    let Some(env) = setup().await else { return };
    env.user_credential("alice-token").await;
    // Implies commit-via-API mode.
    env.configure(|c| c.batch_changes_reject_unverified_commit = true);
    env.state.lock().unwrap().commit_api = true;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    assert_eq!(
        env.calls(),
        vec!["commit alice-token", "create alice-token"]
    );
    assert_eq!(env.commits().len(), 1);

    // Too large for the API: fails instead of pushing an unsigned commit.
    env.configure(|c| c.batch_changes_commit_via_api_max_bytes = 5);
    let big = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/big".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let bid = env.changeset(big).await;
    let err = env.reconcile(bid).await.unwrap_err();
    assert!(
        matches!(&err, ReconcileError::UnverifiedCommitRejected { reason, .. }
            if reason.contains("too large")),
        "{err}"
    );
    assert!(err.to_string().contains("rejectUnverifiedCommit"), "{err}");
    assert!(!err.is_retryable());
    assert!(env.calls().is_empty());
    assert_eq!(git(env.push.path(), &["rev-parse", "refs/heads/big"]), None);

    // A code host without a commit API.
    env.configure(|c| c.batch_changes_commit_via_api_max_bytes = 1 << 20);
    env.state.lock().unwrap().commit_api = false;
    let other = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/other".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let oid = env.changeset(other).await;
    let err = env.reconcile(oid).await.unwrap_err();
    assert!(
        matches!(err, ReconcileError::UnverifiedCommitRejected { .. }),
        "{err}"
    );
    assert_eq!(
        git(env.push.path(), &["rev-parse", "refs/heads/other"]),
        None
    );
}

/// The queued outbound webhook events: event type and decrypted payload, oldest first.
async fn webhook_events(env: &Env) -> Vec<(String, serde_json::Value)> {
    let store = OutboundWebhookStore::new(&env.db, Some(env.keyring.clone()));
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM outbound_webhook_jobs ORDER BY id")
        .fetch_all(env.db.pool())
        .await
        .unwrap();
    let mut out = Vec::new();
    for id in ids {
        let job = store.job(id).await.unwrap().unwrap().payload;
        let payload = serde_json::from_slice(&store.payload(&job).unwrap()).unwrap();
        out.push((job.event_type, payload));
    }
    sqlx::query("DELETE FROM outbound_webhook_jobs")
        .execute(env.db.pool())
        .await
        .unwrap();
    out
}

#[tokio::test]
async fn outbound_webhook_events() {
    let Some(env) = setup().await else { return };
    // Without subscribers nothing is queued.
    env.user_credential("alice-token").await;
    let spec = env.spec(env.new_spec(Some(Published::Bool(true)))).await;
    let first = env.changeset(spec).await;
    env.reconcile(first).await.unwrap();
    assert!(webhook_events(&env).await.is_empty());

    let types = sourcegull_outbound_webhooks::EVENT_TYPES
        .iter()
        .map(|t| sourcegull_outbound_webhooks::store::ScopedEventType {
            event_type: t.key.into(),
            scope: None,
        })
        .collect();
    OutboundWebhookStore::new(&env.db, Some(env.keyring.clone()))
        .create(&sourcegull_outbound_webhooks::store::NewOutboundWebhook {
            url: "https://hooks.example.com/".into(),
            secret: "s".into(),
            event_types: types,
            created_by: None,
        })
        .await
        .unwrap();

    // Publish.
    let spec = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/other".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let id = env.changeset(spec).await;
    env.reconcile(id).await.unwrap();
    let events = webhook_events(&env).await;
    assert_eq!(events.len(), 1);
    let (event, payload) = &events[0];
    assert_eq!(event, "changeset:publish");
    let gql = |kind: &str, id: i64| sourcegull_services::relay::marshal_id(kind, id);
    assert_eq!(payload["id"], gql("Changeset", id));
    assert_eq!(payload["external_id"], "2");
    assert_eq!(payload["state"], "OPEN");
    assert_eq!(payload["title"], "Fix the readme");
    assert_eq!(
        payload["repository_id"],
        gql("Repository", i64::from(env.repo.get()))
    );
    assert_eq!(
        payload["batch_change_ids"],
        serde_json::json!([gql("BatchChange", env.batch_change)])
    );
    assert_eq!(
        payload["owning_batch_change_id"],
        gql("BatchChange", env.batch_change)
    );
    assert_eq!(payload["review_state"], "PENDING");
    assert!(payload["error"].is_null());

    // Update.
    let spec2 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/other".into()),
            title: Some("New title".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec2).await;
    env.reconcile(id).await.unwrap();
    let events = webhook_events(&env).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "changeset:update");
    assert_eq!(events[0].1["title"], "New title");

    // A failed update.
    env.state.lock().unwrap().fail_update = Some(Fail::Transient);
    let spec3 = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/other".into()),
            title: Some("Third title".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    env.reapply(id, spec3).await;
    env.reconcile(id).await.unwrap_err();
    env.state.lock().unwrap().fail_update = None;
    let events = webhook_events(&env).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "changeset:update_error");
    assert_eq!(events[0].1["state"], "RETRYING");
    assert!(!events[0].1["error"].as_str().unwrap().is_empty());

    // Close.
    ChangesetStore::new(&env.db).close(&[id]).await.unwrap();
    env.reconcile(id).await.unwrap();
    let events = webhook_events(&env).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "changeset:close");
    assert_eq!(events[0].1["state"], "CLOSED");

    // A failed publish.
    env.state.lock().unwrap().fail_create = Some(Fail::Transient);
    let spec = env
        .spec(NewChangesetSpec {
            head_ref: Some("refs/heads/third".into()),
            ..env.new_spec(Some(Published::Bool(true)))
        })
        .await;
    let failing = env.changeset(spec).await;
    env.reconcile(failing).await.unwrap_err();
    let events = webhook_events(&env).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "changeset:publish_error");
    assert_eq!(events[0].1["id"], gql("Changeset", failing));
    // Unpublished: title from the spec, no review state.
    assert_eq!(events[0].1["title"], "Fix the readme");
    assert!(events[0].1["review_state"].is_null());
    assert_eq!(events[0].1["state"], "RETRYING");
    assert!(events[0].1["error"].is_string());
}
