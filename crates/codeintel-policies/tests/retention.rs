//! Data retention against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and a repo
//! node started in-process over a temp git repository:
//!
//! ```text
//! main:     m1 ── m2 ── m3          (tag v1 on m1)
//!                  └── f1 ── f2     (feature)
//! ```
//!
//! with the three built-in policies (branch tips 12 weeks, tags 48 weeks, commits on a branch
//! one week) and uploads placed so that each way of being kept, or not, shows up.

use std::collections::BTreeSet;

use chrono::{TimeDelta, Utc};
use sourcegull_codeintel_policies::expirer::{ExpirerConfig, ExpirerReport, expire_pass};
use sourcegull_codeintel_policies::store::{GitObjectType, PolicyFields, PolicyStore};
use sourcegull_codeintel_policies::{Protection, RepoGit, Retention, overview};
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload, Upload, UploadState};
use sourcegull_db::{Db, migrate};
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;

const NAME: &str = "gitlab.example.com/acme/calc";
const INDEXER: &str = "scip-test";
const TIP_POLICY: i32 = 1;
const TAG_POLICY: i32 = 2;
const COMMIT_POLICY: i32 = 3;

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    db: Db,
    node: RepoNode,
    repo: RepoId,
    m1: String,
    m2: String,
    m3: String,
    f1: String,
    f2: String,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let src = SourceRepo::new();
    let m1 = src.commit("m1", &[("a.txt", "1\n")]);
    src.git(&["tag", "v1"]);
    let m2 = src.commit("m2", &[("a.txt", "2\n")]);
    src.git(&["checkout", "-q", "-b", "feature"]);
    let f1 = src.commit("f1", &[("b.txt", "1\n")]);
    let f2 = src.commit("f2", &[("b.txt", "2\n")]);
    src.git(&["checkout", "-q", "main"]);
    let m3 = src.commit("m3", &[("a.txt", "3\n")]);
    let repo = RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
             VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(NAME)
        .fetch_one(db.pool())
        .await
        .unwrap(),
    );
    let node = RepoNode::start(src.remote()).await;
    node.clone_repo(repo, NAME).await;
    Some(Env {
        _t: t,
        _src: src,
        db,
        node,
        repo,
        m1,
        m2,
        m3,
        f1,
        f2,
    })
}

impl Env {
    /// A completed upload at `commit` uploaded `days` ago.
    async fn upload(&self, commit: &str, root: &str, days: i64) -> i64 {
        let id = CodeIntelStore::new(&self.db)
            .create_upload(&NewUpload {
                repo_id: self.repo,
                commit: commit.into(),
                root: root.into(),
                indexer: INDEXER.into(),
                indexer_version: "1".into(),
                num_parts: 1,
                uploader: None,
                associated_index_id: None,
                ci_job: None,
            })
            .await
            .unwrap();
        sourcegull_db::sqlx::query(
            "UPDATE codeintel_uploads SET state = 'completed', upload_size = 1,
                uploaded_at = now() - make_interval(days => $2)
             WHERE id = $1",
        )
        .bind(id)
        .bind(i32::try_from(days).unwrap())
        .execute(self.db.pool())
        .await
        .unwrap();
        id
    }

    async fn get(&self, id: i64) -> Upload {
        CodeIntelStore::new(&self.db)
            .get_upload(id)
            .await
            .unwrap()
            .unwrap()
    }

    fn git(&self) -> RepoGit {
        RepoGit::new(self.node.clients.clone(), self.repo, NAME)
    }

    async fn protections(&self, id: i64) -> BTreeSet<(Option<i32>, String)> {
        let git = self.git();
        let now = Utc::now();
        let retention = Retention::load(&git, &self.db, now).await.unwrap();
        let upload = self.get(id).await;
        let all = retention.protections(&upload, now, false).await.unwrap();
        let first = retention.protections(&upload, now, true).await.unwrap();
        assert_eq!(first.len(), usize::from(!all.is_empty()));
        assert!(first.iter().all(|p| all.contains(p)));
        all.into_iter()
            .map(|Protection { policy, commit }| (policy, commit))
            .collect()
    }
}

fn set(items: &[(Option<i32>, &str)]) -> BTreeSet<(Option<i32>, String)> {
    items.iter().map(|(p, c)| (*p, (*c).to_owned())).collect()
}

#[tokio::test]
async fn retention() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (m1, m2, m3, f1, f2) = (&env.m1, &env.m2, &env.m3, &env.f1, &env.f2);

    // The default branch's tip keeps what it sees, however old.
    let head = env.upload(m3, "", 400).await;
    // Behind the tip's newer upload and older than a week: nothing keeps it.
    let stale = env.upload(m2, "", 10).await;
    // Tagged (48 weeks).
    let tagged = env.upload(m1, "", 100).await;
    // The feature branch's tip (12 weeks).
    let feature_tip = env.upload(f2, "", 2).await;
    // A commit only the feature branch has, within a week.
    let feature_commit = env.upload(f1, "", 3).await;
    // Too old for the commit policy, but the feature tip sees it (no upload of `old/` closer).
    let seen_from_feature = env.upload(f1, "old/", 10).await;
    // The default branch's tip sees it two commits back; the tag names it too.
    let lib = env.upload(m1, "lib/", 200).await;
    // A commit the repository does not have (force-pushed away).
    let gone = env
        .upload("ffffffffffffffffffffffffffffffffffffffff", "", 1)
        .await;

    assert_eq!(env.protections(head).await, set(&[(None, m3)]));
    assert_eq!(env.protections(stale).await, set(&[]));
    assert_eq!(
        env.protections(tagged).await,
        set(&[(Some(TAG_POLICY), m1)])
    );
    assert_eq!(
        env.protections(feature_tip).await,
        set(&[(Some(TIP_POLICY), f2), (Some(COMMIT_POLICY), f2)])
    );
    // (f2 has its own upload, so the feature tip does not see this one.)
    assert_eq!(
        env.protections(feature_commit).await,
        set(&[(Some(COMMIT_POLICY), f1)])
    );
    // The tag sees it too: m1 has no upload of `old/` behind it, so its nearest is a
    // descendant.
    assert_eq!(
        env.protections(seen_from_feature).await,
        set(&[(Some(TIP_POLICY), f2), (Some(TAG_POLICY), m1)])
    );
    assert_eq!(
        env.protections(lib).await,
        set(&[(Some(TAG_POLICY), m1), (None, m3)])
    );

    // A policy on a commit, for repositories matching a pattern, keeps `stale` (30 days).
    let policies = PolicyStore::new(&env.db);
    let custom = policies
        .create(
            None,
            &PolicyFields {
                repository_patterns: Some(vec!["*/acme/*".into()]),
                name: "Pinned".into(),
                object_type: GitObjectType::Commit,
                pattern: m2.clone(),
                retention_enabled: true,
                retention_duration_hours: Some(30 * 24),
                retain_intermediate_commits: false,
                indexing_enabled: false,
                index_commit_max_age_hours: None,
                index_intermediate_commits: false,
                syntactic_indexing_enabled: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(env.protections(stale).await, set(&[(Some(custom.id), m2)]));
    // Not for other repositories' names.
    let mut other = custom.clone();
    other.repository_patterns = Some(vec!["*/other/*".into()]);
    assert!(!other.applies_to(env.repo, NAME));

    // The overview lists the tip first, then every retention policy of the repository.
    let git = env.git();
    let now = Utc::now();
    let retention = Retention::load(&git, &env.db, now).await.unwrap();
    let rows = overview(&retention, &env.get(lib).await, now)
        .await
        .unwrap();
    let summary: Vec<(Option<i32>, bool, Vec<String>)> = rows
        .iter()
        .map(|r| {
            (
                r.policy.as_ref().map(|p| p.id),
                r.matches,
                r.protecting_commits.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (None, true, vec![m3.clone()]),
            (Some(TIP_POLICY), false, vec![]),
            (Some(TAG_POLICY), true, vec![]),
            (Some(COMMIT_POLICY), false, vec![]),
            (Some(custom.id), false, vec![]),
        ]
    );
    policies.delete(custom.id).await.unwrap();

    // The expirer marks `stale` and deletes `gone`.
    let config = ExpirerConfig::default();
    let report = expire_pass(&env.db, &env.node.clients, &config, Utc::now()).await;
    assert_eq!(
        report,
        ExpirerReport {
            repos: 1,
            scanned: 8,
            expired: 1,
            unknown_commits: 1,
        }
    );
    assert!(env.get(stale).await.expired);
    for id in [
        head,
        tagged,
        feature_tip,
        feature_commit,
        seen_from_feature,
        lib,
    ] {
        let u = env.get(id).await;
        assert!(!u.expired, "upload {id}");
        assert_eq!(u.state, UploadState::Completed);
    }
    assert_eq!(env.get(gone).await.state, UploadState::Deleting);
    // The repository waits a day before its next scan.
    let again = expire_pass(&env.db, &env.node.clients, &config, Utc::now()).await;
    assert_eq!(again, ExpirerReport::default());
    // A day later the repository is scanned, but its uploads were checked too recently.
    let later = Utc::now() + TimeDelta::hours(25);
    let config_short = ExpirerConfig {
        upload_process_delay: std::time::Duration::from_secs(48 * 3600),
        ..ExpirerConfig::default()
    };
    let report = expire_pass(&env.db, &env.node.clients, &config_short, later).await;
    assert_eq!((report.repos, report.scanned), (1, 0));

    // The janitor keeps an expired upload another upload depends on.
    let store = CodeIntelStore::new(&env.db);
    for (upload, kind) in [(stale, "export"), (head, "import")] {
        sourcegull_db::sqlx::query(
            "INSERT INTO codeintel_package_monikers (upload_id, kind, scheme, manager, name,
                version) VALUES ($1, $2, 'scip-test', 'cargo', 'calc', '1.0')",
        )
        .bind(upload)
        .bind(kind)
        .execute(env.db.pool())
        .await
        .unwrap();
    }
    assert_eq!(store.mark_expired_unreferenced().await.unwrap(), 0);
    sourcegull_db::sqlx::query("DELETE FROM codeintel_package_monikers WHERE upload_id = $1")
        .bind(head)
        .execute(env.db.pool())
        .await
        .unwrap();
    assert_eq!(store.mark_expired_unreferenced().await.unwrap(), 1);
    assert_eq!(env.get(stale).await.state, UploadState::Deleting);
}

#[tokio::test]
async fn policy_store() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let policies = PolicyStore::new(&env.db);
    let all = policies.list(&Default::default()).await.unwrap();
    assert_eq!(
        all.iter()
            .map(|p| (p.id, p.protected, p.object_type))
            .collect::<Vec<_>>(),
        vec![
            (TIP_POLICY, true, GitObjectType::Branch),
            (TAG_POLICY, true, GitObjectType::Tag),
            (COMMIT_POLICY, true, GitObjectType::Branch),
        ]
    );

    // Protected policies keep their shape but their durations can change.
    let tip = all[0].clone();
    let mut f = PolicyFields {
        repository_patterns: None,
        name: tip.name.clone(),
        object_type: tip.object_type,
        pattern: tip.pattern.clone(),
        retention_enabled: true,
        retention_duration_hours: Some(100),
        retain_intermediate_commits: false,
        indexing_enabled: true,
        index_commit_max_age_hours: None,
        index_intermediate_commits: false,
        syntactic_indexing_enabled: false,
    };
    let updated = policies.update(TIP_POLICY, &f).await.unwrap();
    assert_eq!(updated.retention_duration_hours, Some(100));
    assert!(updated.indexing_enabled);
    f.pattern = "main".into();
    assert!(matches!(
        policies.update(TIP_POLICY, &f).await,
        Err(sourcegull_codeintel_policies::store::PolicyWriteError::ChangeProtected)
    ));
    assert!(matches!(
        policies.delete(TIP_POLICY).await,
        Err(sourcegull_codeintel_policies::store::PolicyWriteError::DeleteProtected)
    ));
    assert!(matches!(
        policies.delete(999).await,
        Err(sourcegull_codeintel_policies::store::PolicyWriteError::NotFound)
    ));

    // One repository's policy, and filters.
    let mine = policies
        .create(
            Some(env.repo),
            &PolicyFields {
                name: "Release branches".into(),
                pattern: "release/*".into(),
                // Ignored for a repository's own policy.
                repository_patterns: Some(vec!["*".into()]),
                ..f.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(mine.repository_patterns, None);
    let listed = |opts: sourcegull_codeintel_policies::store::ListPolicies| {
        let policies = policies.clone();
        async move {
            policies
                .list(&opts)
                .await
                .unwrap()
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        listed(sourcegull_codeintel_policies::store::ListPolicies {
            repo: Some((env.repo, NAME.into())),
            protected: Some(false),
            ..Default::default()
        })
        .await,
        vec![mine.id]
    );
    assert_eq!(
        listed(sourcegull_codeintel_policies::store::ListPolicies {
            repo: Some((RepoId(env.repo.get() + 1), "other".into())),
            protected: Some(false),
            ..Default::default()
        })
        .await,
        Vec::<i32>::new()
    );
    assert_eq!(
        listed(sourcegull_codeintel_policies::store::ListPolicies {
            query: Some("TAG".into()),
            ..Default::default()
        })
        .await,
        vec![TAG_POLICY]
    );
    assert_eq!(
        listed(sourcegull_codeintel_policies::store::ListPolicies {
            for_indexing: Some(true),
            ..Default::default()
        })
        .await,
        vec![TIP_POLICY, mine.id]
    );
    policies.delete(mine.id).await.unwrap();
}
