//! The `repo` role end to end against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): discovery
//! through the mock GitLab, then the repo node clones the repo from a local bare repository, a new
//! commit plus an IMMEDIATE update job is fetched and recorded as a ref event, the gRPC service
//! answers, and the janitor removes a deleted repo from disk.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    Db, ListReposOptions, NewExternalService, RepoUpdatePriority, RepoUpdateReason, migrate,
};
use sourcegull_repo_node::{RepoNode, RepoNodeConfig};
use sourcegull_repos::{Registry, Syncer};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use sourcegull_testutil::TestDb;
use sourcegull_types::{CloneStatus, ExternalServiceKind, RepoId};
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A bare "origin" and a working copy pushing to it; returns (origin, work, first commit).
fn origin(root: &Path) -> (PathBuf, PathBuf, String) {
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&origin, &["init", "--bare", "-q"]);
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    (origin, work, head)
}

async fn wait_for<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Debug, sqlx::FromRow)]
struct Event {
    #[sqlx(rename = "ref")]
    ref_name: String,
    old_oid: Option<String>,
    new_oid: Option<String>,
    source: String,
}

async fn events(db: &Db, repo: RepoId) -> Vec<Event> {
    sqlx::query_as(
        "SELECT ref, old_oid, new_oid, source FROM repo_ref_events WHERE repo_id = $1 ORDER BY id",
    )
    .bind(repo)
    .fetch_all(db.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_role_clones_fetches_and_records_ref_events() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring: Arc<Keyring> =
        Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());

    let tmp = tempfile::tempdir().unwrap();
    let (origin_dir, work, first) = origin(tmp.path());

    // Discovery: the mock GitLab lists one project whose clone URL is the local origin.
    let gitlab = MockGitLab::start("tok").await;
    let mut p = project(
        1,
        "acme/api",
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    );
    p["http_url_to_repo"] = serde_json::Value::String(origin_dir.to_str().unwrap().to_owned());
    gitlab.with(|s| s.projects = vec![p]);
    let config = format!(
        r#"{{"url": "{}", "token": "tok", "projectQuery": ["projects?membership=true"]}}"#,
        gitlab.url
    );
    let svc = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config,
        })
        .await
        .unwrap()
        .id;
    let site = ConfigHandle::new(1, SiteConfig::default());
    Syncer::new(
        db.clone(),
        Some(keyring.clone()),
        Registry::default(),
        site.clone(),
    )
    .sync(svc, None, &CancellationToken::new())
    .await
    .unwrap();
    let repo = db
        .repos()
        .list(&ListReposOptions::default())
        .await
        .unwrap()
        .pop()
        .expect("discovered repo");
    assert!(repo.name.ends_with("acme/api"), "{}", repo.name);
    assert_eq!(repo.shard, None);

    // Start the repo node.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let me = format!("http://{}", listener.local_addr().unwrap());
    let mut cfg = RepoNodeConfig::new(&me, vec![me.clone()], tmp.path().join("data"));
    cfg.allow_local_remotes = true;
    cfg.poll_interval = Duration::from_millis(200);
    cfg.janitor_initial_delay = Duration::from_secs(3600);
    cfg.deleted_grace = Duration::ZERO;
    cfg.index_debounce = Duration::from_millis(100);
    let node = RepoNode::open(db.clone(), Some(keyring.clone()), site.clone(), cfg).unwrap();
    let cancel = CancellationToken::new();
    let running = tokio::spawn(node.clone().run(listener, cancel.clone()));

    // Clone: the NORMAL job from discovery is picked up; the repo is assigned to this node.
    let gs = wait_for("the clone", || async {
        db.gitserver_repos()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|g| g.clone_status == CloneStatus::Cloned)
    })
    .await;
    assert_eq!(gs.head_oid.as_deref(), Some(first.as_str()));
    assert_eq!(gs.node.as_deref(), Some(me.as_str()));
    assert!(gs.repo_size_bytes.unwrap_or(0) > 0);
    assert!(gs.last_error.is_none());
    assert_eq!(
        db.repos()
            .get(repo.id)
            .await
            .unwrap()
            .unwrap()
            .shard
            .as_deref(),
        Some(me.as_str())
    );
    assert!(node.store().is_cloned(repo.id));
    // A first clone records no ref events, and the next periodic fetch is scheduled.
    assert!(events(&db, repo.id).await.is_empty());
    let schedule = db
        .gitserver_repos()
        .fetch_schedule(repo.id)
        .await
        .unwrap()
        .unwrap();
    assert!(schedule.next_fetch_at.is_some_and(|t| t > Utc::now()));
    assert_eq!(node.schedule_once().await.unwrap(), 0);

    // The clone is indexed at HEAD.
    let head_indexed = |want: String| {
        let node = node.clone();
        let id = repo.id;
        move || {
            let node = node.clone();
            let want = want.clone();
            async move {
                let branches = node.index().unwrap().branches(id)?;
                (branches[0].name == "HEAD" && branches[0].commit == want).then_some(())
            }
        }
    };
    wait_for("the index build", head_indexed(first.clone())).await;

    // The gRPC service answers for the repo through the routed client.
    let clients = RepoClients::new(Router::new([me.clone()]), ClientConfig::default());
    let status = clients
        .for_repo(repo.id)
        .unwrap()
        .repo_status(pb::RepoStatusRequest {
            repo: Some(pb::RepoRef {
                id: repo.id.get(),
                name: repo.name.clone(),
            }),
            include_size: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(status.cloned);
    assert_eq!(status.head_oid, first);

    // A push and an IMMEDIATE update: fetched, with a ref event for the moved branch.
    std::fs::write(work.join("README.md"), "hello again\n").unwrap();
    git(&work, &["commit", "-q", "-am", "second"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let second = git(&work, &["rev-parse", "HEAD"]);
    db.repo_update_jobs()
        .enqueue(
            repo.id,
            RepoUpdatePriority::IMMEDIATE,
            RepoUpdateReason::Webhook,
        )
        .await
        .unwrap();
    let evs = wait_for("the ref event", || async {
        let evs = events(&db, repo.id).await;
        (!evs.is_empty()).then_some(evs)
    })
    .await;
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0].ref_name, "refs/heads/main");
    assert_eq!(evs[0].old_oid.as_deref(), Some(first.as_str()));
    assert_eq!(evs[0].new_oid.as_deref(), Some(second.as_str()));
    assert_eq!(evs[0].source, "fetch");
    let gs = wait_for("the recorded fetch", || async {
        db.gitserver_repos()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|g| g.head_oid.as_deref() == Some(second.as_str()))
    })
    .await;
    assert_eq!(gs.clone_status, CloneStatus::Cloned);
    // The fetch moved HEAD, so the repo is reindexed after the debounce.
    wait_for("the reindex", head_indexed(second.clone())).await;

    // The indexer reports what it holds for the site admin.
    let row = wait_for("the reported index state", || async {
        db.search_index()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|r| r.indexed && r.branches.first().is_some_and(|b| b.commit == second))
    })
    .await;
    assert_eq!(row.node, me);
    assert_eq!(row.branches[0].name, "HEAD");
    assert!(row.docs >= 1 && row.index_bytes > 0 && row.lines >= 1 && row.shards >= 1);
    assert!(row.last_error.is_none());
    let indexed_at = row.last_indexed_at.unwrap();

    // Admin RPCs: disk usage, the live index status and a forced rebuild.
    let r = || {
        Some(pb::RepoRef {
            id: repo.id.get(),
            name: repo.name.clone(),
        })
    };
    let disk = clients
        .for_repo(repo.id)
        .unwrap()
        .disk_info(pb::DiskInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(disk.total_bytes > 0 && disk.free_bytes <= disk.total_bytes);
    assert_eq!(disk.repos_on_disk, 1);
    assert!(disk.index_bytes > 0);
    let mut search = clients.search_for_repo(repo.id).unwrap();
    let live = search
        .index_status(pb::IndexStatusRequest { repo: r() })
        .await
        .unwrap()
        .into_inner();
    assert!(live.indexed);
    assert_eq!(live.branches[0].commit_oid, second);
    assert_eq!(live.shards.len(), 1);
    assert!(live.updated_at > 0);
    search
        .reindex(pb::ReindexRequest { repo: r() })
        .await
        .unwrap();
    wait_for("the forced rebuild", || async {
        db.search_index()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|r| r.last_indexed_at.is_some_and(|t| t > indexed_at))
    })
    .await;

    // A re-clone request replaces the copy on disk (the marker file goes away).
    let marker = node
        .store()
        .layout()
        .repo_dir(repo.id)
        .join("sourcegull-test-marker");
    std::fs::write(&marker, "x").unwrap();
    db.repo_update_jobs()
        .enqueue(
            repo.id,
            RepoUpdatePriority::IMMEDIATE,
            RepoUpdateReason::Reclone,
        )
        .await
        .unwrap();
    wait_for("the re-clone", || async {
        (!marker.exists() && node.store().is_cloned(repo.id)).then_some(())
    })
    .await;

    // The janitor removes a deleted repo from disk (grace period 0 here).
    sqlx::query("UPDATE repo SET deleted_at = now() - interval '1 minute' WHERE id = $1")
        .bind(repo.id)
        .execute(db.pool())
        .await
        .unwrap();
    let report = node.janitor_once().await.unwrap();
    assert_eq!(report.removed, vec![repo.id]);
    assert!(!node.store().is_cloned(repo.id));
    assert_eq!(
        db.gitserver_repos()
            .get(repo.id)
            .await
            .unwrap()
            .unwrap()
            .clone_status,
        CloneStatus::NotCloned
    );
    wait_for("the index removal", || async {
        node.index()
            .unwrap()
            .branches(repo.id)
            .is_none()
            .then_some(())
    })
    .await;
    wait_for("the reported index removal", || async {
        db.search_index()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|r| !r.indexed && r.branches.is_empty())
    })
    .await;

    cancel.cancel();
    running.await.unwrap().unwrap();
}
