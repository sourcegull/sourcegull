//! The auto-indexing scheduler against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and a repo
//! node started in-process over a temp git repository: ref events for branches and tags that
//! indexing policies name queue the commit once, policy age limits and intermediate commits apply,
//! the per-repository rate limit holds, the sweep catches repositories, and configuration comes
//! from the UI, `sourcegull.yaml` or inference in that order.

use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_autoindex::scheduler::{SchedulerConfig, handle_events, sweep_pass};
use sourcegull_autoindex::store::AutoIndexStore;
use sourcegull_autoindex::{AutoIndexer, IndexerMap, RepoRef};
use sourcegull_codeintel_policies::store::{GitObjectType, PolicyFields, PolicyStore};
use sourcegull_codeintel_scip::store::{
    CodeIntelStore, IndexJobRow, IndexJobStore, ListIndexJobs, NewUpload,
};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, RefEventSource, RefUpdate, migrate};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoAccess, RepoId};

const NAME: &str = "gitlab.example.com/acme/calc";

struct Env {
    _t: TestDb,
    src: SourceRepo,
    node: RepoNode,
    db: Db,
    config: sourcegull_config::SiteConfigHandle,
    indexer: AutoIndexer,
    repo: RepoId,
}

fn site(f: impl FnOnce(&mut SiteConfig)) -> SiteConfig {
    let mut c = SiteConfig {
        code_intel_auto_indexing_enabled: true,
        ..SiteConfig::default()
    };
    f(&mut c);
    c
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let src = SourceRepo::new();
    src.commit(
        "init",
        &[("go.mod", "module calc\n"), ("calc.go", "package calc\n")],
    );
    let node = RepoNode::start(src.remote()).await;
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
    sourcegull_db::sqlx::query(
        "INSERT INTO gitserver_repos (repo_id, clone_status, node) VALUES ($1, 'cloned', 'n')",
    )
    .bind(repo)
    .execute(db.pool())
    .await
    .unwrap();
    node.clone_repo(repo, NAME).await;
    let config = ConfigHandle::new(1, site(|_| {}));
    let indexer = AutoIndexer::new(db.clone(), node.clients.clone(), config.clone());
    Some(Env {
        _t: t,
        src,
        node,
        db,
        config,
        indexer,
        repo,
    })
}

fn branch_policy(pattern: &str, max_age: Option<i32>, intermediate: bool) -> PolicyFields {
    PolicyFields {
        repository_patterns: None,
        name: format!("index {pattern}"),
        object_type: GitObjectType::Branch,
        pattern: pattern.into(),
        retention_enabled: false,
        retention_duration_hours: None,
        retain_intermediate_commits: false,
        indexing_enabled: true,
        index_commit_max_age_hours: max_age,
        index_intermediate_commits: intermediate,
        syntactic_indexing_enabled: false,
    }
}

impl Env {
    fn repo_ref(&self) -> RepoRef {
        RepoRef {
            id: self.repo,
            name: NAME.into(),
        }
    }

    async fn policy(&self, f: PolicyFields) {
        PolicyStore::new(&self.db)
            .create(Some(self.repo), &f)
            .await
            .unwrap();
    }

    fn set_site(&self, c: SiteConfig) {
        let v = self.config.load().version + 1;
        self.config.store(v, c);
    }

    /// Fetches the source into the node and records the moves as a fetch would.
    async fn push(&self, updates: &[(&str, Option<&str>, &str)]) {
        self.node
            .clients
            .for_repo(self.repo)
            .unwrap()
            .fetch_repo(pb::FetchRepoRequest {
                repo: Some(pb::RepoRef {
                    id: self.repo.get(),
                    name: NAME.into(),
                }),
            })
            .await
            .unwrap();
        let updates: Vec<RefUpdate> = updates
            .iter()
            .map(|(name, old, new)| RefUpdate {
                name: (*name).into(),
                old_oid: old.map(str::to_owned),
                new_oid: Some((*new).to_owned()),
            })
            .collect();
        self.db
            .ref_events()
            .record(self.repo, &updates, RefEventSource::Fetch)
            .await
            .unwrap();
    }

    async fn jobs(&self) -> Vec<IndexJobRow> {
        let (mut rows, _) = IndexJobStore::new(&self.db)
            .list(
                &ListIndexJobs {
                    limit: 100,
                    ..Default::default()
                },
                &RepoAccess::All,
            )
            .await
            .unwrap();
        rows.sort_by_key(|r| r.id);
        rows
    }

    async fn events(&self, now: DateTime<Utc>) -> u64 {
        handle_events(&self.indexer, &SchedulerConfig::default(), now)
            .await
            .unwrap()
            .jobs
    }
}

/// Just after the source repository's fixed clock (commits are a minute apart from it).
fn soon() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000 + 3600, 0).unwrap()
}

#[tokio::test]
async fn ref_events_queue_matching_commits_once() {
    let Some(env) = setup().await else { return };
    env.policy(branch_policy("main", None, false)).await;
    let head = env.src.head();
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 1);
    let jobs = env.jobs().await;
    assert_eq!(jobs.len(), 1);
    let j = &jobs[0].job;
    assert_eq!(j.commit, head);
    assert_eq!(j.root, "");
    assert_eq!(j.indexer, IndexerMap::default().get("go").unwrap());
    assert_eq!(j.indexer_args, ["scip-go", "--no-animation"]);
    assert!(j.requested_env_vars.contains(&"GOPROXY".to_owned()));
    assert_eq!(jobs[0].state, "queued");

    // The same commit again (another event, the sweep, a second pass) queues nothing.
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 0);
    let swept = sweep_pass(&env.indexer, &SchedulerConfig::default(), soon())
        .await
        .unwrap();
    assert_eq!(swept.repos, 1);
    assert_eq!(swept.jobs, 0);
    assert_eq!(env.jobs().await.len(), 1);
    assert_eq!(
        AutoIndexStore::new(&env.db).cursor().await.unwrap(),
        env.db
            .ref_events()
            .list_after(0, 100)
            .await
            .unwrap()
            .last()
            .unwrap()
            .id
    );

    // Marked for reindexing, the next event queues it anew.
    IndexJobStore::new(&env.db)
        .mark_should_reindex(&[jobs[0].id])
        .await
        .unwrap();
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 1);
    assert_eq!(env.jobs().await.len(), 2);
}

#[tokio::test]
async fn only_policies_that_name_the_ref_and_young_commits() {
    let Some(env) = setup().await else { return };
    env.policy(branch_policy("release/*", None, false)).await;
    let head = env.src.head();
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 0);

    // Too old for the policy's max age.
    env.policy(branch_policy("main", Some(1), false)).await;
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon() + TimeDelta::hours(5)).await, 0);
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 1);

    // Tags, by a tag policy.
    let mut tag = branch_policy("v*", None, false);
    tag.object_type = GitObjectType::Tag;
    env.policy(tag).await;
    let c2 = env.src.commit("two", &[("calc.go", "package calc // 2\n")]);
    env.src.git(&["tag", "v1.0"]);
    env.push(&[("refs/tags/v1.0", None, &c2)]).await;
    assert_eq!(env.events(soon()).await, 1);
    let commits: Vec<String> = env.jobs().await.into_iter().map(|j| j.job.commit).collect();
    assert_eq!(commits, [head, c2]);
}

#[tokio::test]
async fn intermediate_commits_and_rate_limit() {
    let Some(env) = setup().await else { return };
    env.policy(branch_policy("main", None, true)).await;
    let c1 = env.src.head();
    let c2 = env.src.commit("two", &[("a.go", "package calc\n")]);
    let c3 = env.src.commit("three", &[("b.go", "package calc\n")]);
    env.push(&[("refs/heads/main", Some(&c1), &c3)]).await;
    assert_eq!(env.events(soon()).await, 2);
    let mut commits: Vec<String> = env.jobs().await.into_iter().map(|j| j.job.commit).collect();
    commits.sort();
    let mut want = vec![c2, c3.clone()];
    want.sort();
    assert_eq!(commits, want);

    // One commit per hour: the second push in the hour waits for the sweep.
    env.set_site(site(|c| {
        c.code_intel_auto_indexing_repository_commits_per_hour = 1
    }));
    sourcegull_db::sqlx::query("DELETE FROM codeintel_autoindex_repos")
        .execute(env.db.pool())
        .await
        .unwrap();
    let c4 = env.src.commit("four", &[("c.go", "package calc\n")]);
    let c5 = env.src.commit("five", &[("d.go", "package calc\n")]);
    env.push(&[("refs/heads/main", Some(&c3), &c5)]).await;
    let r = handle_events(&env.indexer, &SchedulerConfig::default(), soon())
        .await
        .unwrap();
    assert_eq!((r.jobs, r.rate_limited), (1, 1));
    // The sweep (no rate limit) queues what was held back, and the first commit, which no
    // push brought.
    let r = sweep_pass(&env.indexer, &SchedulerConfig::default(), soon())
        .await
        .unwrap();
    assert_eq!(r.jobs, 2);
    let queued: Vec<String> = env.jobs().await.into_iter().map(|j| j.job.commit).collect();
    assert_eq!(queued.len(), 5);
    assert!(queued.contains(&c1) && queued.contains(&c4) && queued.contains(&c5));
}

#[tokio::test]
async fn disabled_global_and_uploaded_commits_are_skipped() {
    let Some(env) = setup().await else { return };
    // A global policy only counts with allowGlobalPolicies.
    PolicyStore::new(&env.db)
        .create(None, &branch_policy("main", None, false))
        .await
        .unwrap();
    let head = env.src.head();
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 0);
    env.set_site(site(|c| c.code_intel_auto_indexing_enabled = false));
    let swept = sweep_pass(&env.indexer, &SchedulerConfig::default(), soon())
        .await
        .unwrap();
    assert_eq!(swept.repos, 0);
    env.push(&[("refs/heads/main", None, &head)]).await;
    // Events read while disabled are consumed.
    assert_eq!(env.events(soon()).await, 0);
    env.set_site(site(|c| {
        c.code_intel_auto_indexing_allow_global_policies = true
    }));
    assert_eq!(env.events(soon()).await, 0);

    // A commit with an upload from CI is indexed already.
    CodeIntelStore::new(&env.db)
        .create_upload(&NewUpload {
            repo_id: env.repo,
            commit: head.clone(),
            root: String::new(),
            indexer: "scip-go".into(),
            indexer_version: "1".into(),
            num_parts: 1,
            uploader: None,
            associated_index_id: None,
            ci_job: None,
        })
        .await
        .unwrap();
    env.push(&[("refs/heads/main", None, &head)]).await;
    assert_eq!(env.events(soon()).await, 0);
    let c2 = env.src.commit("two", &[("x.go", "package calc\n")]);
    env.push(&[("refs/heads/main", Some(&head), &c2)]).await;
    assert_eq!(env.events(soon()).await, 1);
}

#[tokio::test]
async fn configuration_sources_in_order() {
    let Some(env) = setup().await else { return };
    let repo = env.repo_ref();
    let head = env.src.head();
    let (jobs, source) = env
        .indexer
        .jobs_for(&repo, &head, None, false)
        .await
        .unwrap();
    assert_eq!(source, sourcegull_autoindex::JobSource::Inferred);
    assert_eq!(jobs.len(), 1);

    let c2 = env.src.commit(
        "config",
        &[(
            "sourcegull.yaml",
            "index_jobs:\n  - indexer: acme/scip-go\n    root: cmd\n    indexer_args: [scip-go]\n",
        )],
    );
    env.push(&[("refs/heads/main", Some(&head), &c2)]).await;
    let (jobs, source) = env.indexer.jobs_for(&repo, &c2, None, false).await.unwrap();
    assert_eq!(source, sourcegull_autoindex::JobSource::RepositoryFile);
    assert_eq!(jobs[0].indexer, "acme/scip-go");

    AutoIndexStore::new(&env.db)
        .set_index_configuration(env.repo, r#"{"index_jobs": [{"indexer": "acme/db"}]}"#)
        .await
        .unwrap();
    let (jobs, source) = env.indexer.jobs_for(&repo, &c2, None, false).await.unwrap();
    assert_eq!(source, sourcegull_autoindex::JobSource::Database);
    assert_eq!(jobs[0].indexer, "acme/db");

    // A broken explicit configuration yields nothing rather than falling through.
    let (jobs, source) = env
        .indexer
        .jobs_for(&repo, &c2, Some("{nope"), false)
        .await
        .unwrap();
    assert_eq!(source, sourcegull_autoindex::JobSource::Explicit);
    assert!(jobs.is_empty());

    // Manual queueing (force) ignores what is queued; roots are stored with a slash.
    let ids = env
        .indexer
        .queue(
            &repo,
            &c2,
            Some(r#"{"index_jobs": [{"indexer": "x", "root": "a/b"}]}"#),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let again = env
        .indexer
        .queue(
            &repo,
            &c2,
            Some(r#"{"index_jobs": [{"indexer": "x", "root": "a/b"}]}"#),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(again.len(), 1);
    let rows = IndexJobStore::new(&env.db)
        .get_many(&[ids[0], again[0]])
        .await
        .unwrap();
    assert_eq!(rows[0].job.root, "a/b/");

    // The site's inference script applies to inference.
    AutoIndexStore::new(&env.db)
        .set_index_configuration(env.repo, "")
        .await
        .unwrap();
    AutoIndexStore::new(&env.db)
        .set_inference_script(r#"return require("sg.autoindex.config").new({ ["sg.go"] = false })"#)
        .await
        .unwrap();
    let r = env.indexer.infer(&repo, &head, None).await.unwrap();
    assert!(r.jobs.is_empty());
}
