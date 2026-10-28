//! Repo node rebalancing end to end against the test Postgres: one node clones a repo; the cluster
//! grows to two nodes so the repo's hash owner becomes the new node; the planner waits until both
//! nodes run with the same list, the new node copies the repo from the old one over gRPC, indexes
//! it and flips the assignment; the web routing table follows; the old node keeps its copy for the
//! grace period and then removes it; the new owner fetches later pushes.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_config::{ConfigHandle, SiteConfig, SiteConfigHandle};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    Db, ListReposOptions, NewExternalService, RepoUpdatePriority, RepoUpdateReason, migrate,
};
use sourcegull_repo_node::{RepoNode, RepoNodeConfig};
use sourcegull_repos::{Registry, Syncer};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, RepoId};
use tokio::net::TcpListener;
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

/// A bare "origin" with a tag and two branches, and a working copy pushing to it.
fn origin(root: &Path) -> (PathBuf, PathBuf, String) {
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&origin, &["init", "--bare", "-q"]);
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "hello rebalancing\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(&work, &["tag", "-a", "v1", "-m", "v1"]);
    git(&work, &["branch", "topic"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main", "topic", "v1"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    (origin, work, head)
}

async fn wait_for<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
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

fn config(me: &str, nodes: &[String], data: PathBuf) -> RepoNodeConfig {
    let mut cfg = RepoNodeConfig::new(me, nodes.to_vec(), data);
    cfg.allow_local_remotes = true;
    cfg.poll_interval = Duration::from_millis(200);
    cfg.janitor_initial_delay = Duration::from_secs(3600);
    cfg.index_debounce = Duration::from_millis(100);
    cfg.rebalance_interval = Duration::from_millis(200);
    cfg.rebalance_plan_interval = Duration::from_millis(200);
    cfg.move_gc_grace = Duration::from_secs(2);
    cfg
}

fn addr(l: &TcpListener) -> String {
    format!("http://{}", l.local_addr().unwrap())
}

async fn shard(db: &Db, repo: RepoId) -> Option<String> {
    db.repos().get(repo).await.unwrap().unwrap().shard
}

async fn move_state(db: &Db, repo: RepoId) -> Option<String> {
    sqlx::query_scalar("SELECT state FROM repo_moves WHERE repo_id = $1")
        .bind(repo)
        .fetch_optional(db.pool())
        .await
        .unwrap()
}

fn refs(dir: &Path) -> String {
    git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"])
}

/// A discovered repo whose clone URL is a local origin.
struct Setup {
    db: Db,
    keyring: Arc<Keyring>,
    site: SiteConfigHandle,
    tmp: tempfile::TempDir,
    repo: sourcegull_types::Repo,
    work: PathBuf,
    first: String,
    _gitlab: MockGitLab,
}

async fn setup(t: &TestDb) -> Setup {
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring: Arc<Keyring> =
        Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let (origin_dir, work, first) = origin(tmp.path());

    let gitlab = MockGitLab::start("tok").await;
    let mut p = project(
        1,
        "acme/moving",
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    );
    p["http_url_to_repo"] = serde_json::Value::String(origin_dir.to_str().unwrap().to_owned());
    gitlab.with(|s| s.projects = vec![p]);
    let svc = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: format!(
                r#"{{"url": "{}", "token": "tok", "projectQuery": ["projects?membership=true"]}}"#,
                gitlab.url
            ),
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
    Setup {
        db,
        keyring,
        site,
        tmp,
        repo,
        work,
        first,
        _gitlab: gitlab,
    }
}

/// Runs a single node over `data` until it has cloned the repo, then stops it.
async fn clone_on_single_node(s: &Setup, data: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = addr(&listener);
    let node = RepoNode::open(
        s.db.clone(),
        Some(s.keyring.clone()),
        s.site.clone(),
        config(&a, std::slice::from_ref(&a), data),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let running = tokio::spawn(node.clone().run(listener, cancel.clone()));
    wait_for("the clone on the first node", || async {
        node.store().is_cloned(s.repo.id).then_some(())
    })
    .await;
    assert_eq!(shard(&s.db, s.repo.id).await.as_deref(), Some(a.as_str()));
    cancel.cancel();
    running.await.unwrap().unwrap();
    a
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scale_out_copies_flips_and_cleans_up() {
    let Some(t) = TestDb::new().await else { return };
    let s = setup(&t).await;
    let (db, keyring, site, tmp, repo, work, first) = (
        s.db.clone(),
        s.keyring.clone(),
        s.site.clone(),
        s.tmp.path(),
        s.repo.clone(),
        s.work.clone(),
        s.first.clone(),
    );

    // One node: it clones and indexes the repo.
    let data_a = tmp.join("data-a");
    let a = clone_on_single_node(&s, data_a.clone()).await;
    let original_refs = refs(&sourcegull_gitstore::Layout::new(&data_a).repo_dir(repo.id));

    // A second node whose address makes it the repo's hash owner.
    let (listener_b, b) = loop {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = addr(&l);
        if Router::new([a.clone(), b.clone()]).hashed_node_for(repo.id) == Some(b.as_str()) {
            break (l, b);
        }
    };
    let nodes = vec![a.clone(), b.clone()];

    // The first node restarts with the new list; the second is not up yet: nothing is planned.
    let listener_a = TcpListener::bind(a.trim_start_matches("http://"))
        .await
        .unwrap();
    let node_a = RepoNode::open(
        db.clone(),
        Some(keyring.clone()),
        site.clone(),
        config(&a, &nodes, data_a.clone()),
    )
    .unwrap();
    node_a.rebalance_heartbeat().await.unwrap();
    let report = node_a.plan_rebalance().await.unwrap();
    assert_eq!(report.waiting_for, vec![b.clone()]);
    assert_eq!(move_state(&db, repo.id).await, None);
    let cancel = CancellationToken::new();
    let running_a = tokio::spawn(node_a.clone().run(listener_a, cancel.clone()));

    // The web routing table: the repo stays on the old node until the flip.
    let clients = RepoClients::new(Router::new(nodes.clone()), ClientConfig::default());
    sourcegull::routing::reload(&db, &clients, &nodes)
        .await
        .unwrap();
    assert_eq!(clients.router().node_for(repo.id), Some(a.as_str()));
    let status = |clients: RepoClients| {
        let r = pb::RepoRef {
            id: repo.id.get(),
            name: repo.name.clone(),
        };
        async move {
            clients
                .for_repo(repo.id)
                .unwrap()
                .repo_status(pb::RepoStatusRequest {
                    repo: Some(r),
                    include_size: false,
                })
                .await
                .unwrap()
                .into_inner()
        }
    };
    assert!(status(clients.clone()).await.cloned);

    // The second node starts: the move is planned, copied, verified and flipped.
    let data_b = tmp.join("data-b");
    let node_b = RepoNode::open(
        db.clone(),
        Some(keyring.clone()),
        site.clone(),
        config(&b, &nodes, data_b),
    )
    .unwrap();
    let running_b = tokio::spawn(node_b.clone().run(listener_b, cancel.clone()));
    wait_for("the flip", || async {
        (shard(&db, repo.id).await.as_deref() == Some(b.as_str())).then_some(())
    })
    .await;
    let row: (String, String, String, i64) = sqlx::query_as(
        "SELECT from_node, to_node, kind, bytes_copied FROM repo_moves WHERE repo_id = $1",
    )
    .bind(repo.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2.as_str()),
        (a.as_str(), b.as_str(), "copy")
    );
    assert!(row.3 > 0, "pack bytes recorded");

    // The new owner has the same refs and an index at HEAD before it took over.
    assert!(node_b.store().is_cloned(repo.id));
    assert_eq!(refs(&node_b.store().repo_dir(repo.id)), original_refs);
    let branches = node_b.index().unwrap().branches(repo.id).unwrap();
    assert_eq!(branches[0].name, "HEAD");
    assert_eq!(branches[0].commit, first);
    let gs = db.gitserver_repos().get(repo.id).await.unwrap().unwrap();
    assert_eq!(gs.node.as_deref(), Some(b.as_str()));

    // Routing follows; the old node still has its copy during the grace period.
    sourcegull::routing::reload(&db, &clients, &nodes)
        .await
        .unwrap();
    assert_eq!(clients.router().node_for(repo.id), Some(b.as_str()));
    let st = status(clients.clone()).await;
    assert!(st.cloned);
    assert_eq!(st.head_oid, first);
    wait_for("the index report of the new owner", || async {
        db.search_index()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|r| r.indexed && r.node == b)
    })
    .await;

    // After the grace period the old copy and its index are removed and the move is done.
    wait_for("the old copy's removal", || async {
        (!node_a.store().is_cloned(repo.id)
            && move_state(&db, repo.id).await.as_deref() == Some("done"))
        .then_some(())
    })
    .await;
    wait_for("the old index's removal", || async {
        node_a
            .index()
            .unwrap()
            .branches(repo.id)
            .is_none()
            .then_some(())
    })
    .await;
    // The index row still belongs to the new owner.
    let row = db.search_index().get(repo.id).await.unwrap().unwrap();
    assert!(row.indexed && row.node == b);

    // Later pushes are fetched by the new owner.
    std::fs::write(work.join("README.md"), "moved\n").unwrap();
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
    wait_for("the fetch on the new owner", || async {
        db.gitserver_repos()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|g| {
                g.head_oid.as_deref() == Some(second.as_str())
                    && g.node.as_deref() == Some(b.as_str())
            })
    })
    .await;
    assert!(!node_a.store().is_cloned(repo.id));

    // Nothing more to plan.
    let report = node_a.plan_rebalance().await.unwrap();
    assert!(report.waiting_for.is_empty());
    assert_eq!(report.queued, 0);

    cancel.cancel();
    running_a.await.unwrap().unwrap();
    running_b.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_of_a_removed_node_that_is_gone_are_recloned() {
    let Some(t) = TestDb::new().await else { return };
    let s = setup(&t).await;
    let (db, repo) = (s.db.clone(), s.repo.clone());
    let a = clone_on_single_node(&s, s.tmp.path().join("data-a")).await;
    // The node was removed and shut down long ago.
    sqlx::query("UPDATE repo_nodes SET seen_at = now() - interval '1 hour' WHERE addr = $1")
        .bind(&a)
        .execute(db.pool())
        .await
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b = addr(&listener);
    let node_b = RepoNode::open(
        db.clone(),
        Some(s.keyring.clone()),
        s.site.clone(),
        config(&b, std::slice::from_ref(&b), s.tmp.path().join("data-b")),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let running = tokio::spawn(node_b.clone().run(listener, cancel.clone()));
    wait_for("the clone on the remaining node", || async {
        (node_b.store().is_cloned(repo.id)
            && shard(&db, repo.id).await.as_deref() == Some(b.as_str()))
        .then_some(())
    })
    .await;
    let (kind, state): (String, String) =
        sqlx::query_as("SELECT kind, state FROM repo_moves WHERE repo_id = $1")
            .bind(repo.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!((kind.as_str(), state.as_str()), ("reclone", "done"));
    let gs = wait_for("the recorded clone", || async {
        db.gitserver_repos()
            .get(repo.id)
            .await
            .unwrap()
            .filter(|g| g.node.as_deref() == Some(b.as_str()))
    })
    .await;
    assert_eq!(gs.head_oid.as_deref(), Some(s.first.as_str()));
    cancel.cancel();
    running.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_repo_moves_to_its_node_and_back_when_unpinned() {
    let Some(t) = TestDb::new().await else { return };
    let s = setup(&t).await;
    let (db, keyring, site, tmp, repo) = (
        s.db.clone(),
        s.keyring.clone(),
        s.site.clone(),
        s.tmp.path(),
        s.repo.clone(),
    );
    let data_a = tmp.join("data-a");
    let a = clone_on_single_node(&s, data_a.clone()).await;

    // A second node that does not own the repo by hash: nothing moves until it is pinned.
    let (listener_b, b) = loop {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = addr(&l);
        if Router::new([a.clone(), b.clone()]).hashed_node_for(repo.id) == Some(a.as_str()) {
            break (l, b);
        }
    };
    let nodes = vec![a.clone(), b.clone()];
    let listener_a = TcpListener::bind(a.trim_start_matches("http://"))
        .await
        .unwrap();
    let node_a = RepoNode::open(
        db.clone(),
        Some(keyring.clone()),
        site.clone(),
        config(&a, &nodes, data_a),
    )
    .unwrap();
    let node_b = RepoNode::open(
        db.clone(),
        Some(keyring.clone()),
        site.clone(),
        config(&b, &nodes, tmp.join("data-b")),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let running_a = tokio::spawn(node_a.clone().run(listener_a, cancel.clone()));
    let running_b = tokio::spawn(node_b.clone().run(listener_b, cancel.clone()));
    wait_for("both nodes settled", || async {
        node_a
            .plan_rebalance()
            .await
            .ok()
            .filter(|r| r.waiting_for.is_empty())
    })
    .await;
    assert_eq!(move_state(&db, repo.id).await, None);
    assert_eq!(shard(&db, repo.id).await.as_deref(), Some(a.as_str()));

    // Pin it to the second node (written without the scheme).
    let pin = |node: Option<&str>| {
        let mut cfg = SiteConfig::default();
        if let Some(node) = node {
            cfg.repo_nodes_pinned_repos
                .insert(repo.name.clone(), node.to_owned());
        }
        cfg
    };
    assert!(site.store(2, pin(Some(b.trim_start_matches("http://")))));
    wait_for("the move to the pinned node", || async {
        (shard(&db, repo.id).await.as_deref() == Some(b.as_str())
            && move_state(&db, repo.id).await.as_deref() == Some("done")
            && node_b.store().is_cloned(repo.id)
            && !node_a.store().is_cloned(repo.id))
        .then_some(())
    })
    .await;
    let clients = RepoClients::new(Router::new(nodes.clone()), ClientConfig::default());
    sourcegull::routing::reload(&db, &clients, &nodes)
        .await
        .unwrap();
    assert_eq!(clients.router().node_for(repo.id), Some(b.as_str()));

    // Unpinned: back to the hash owner.
    assert!(site.store(3, pin(None)));
    wait_for("the move back to the hash owner", || async {
        (shard(&db, repo.id).await.as_deref() == Some(a.as_str())
            && move_state(&db, repo.id).await.as_deref() == Some("done")
            && node_a.store().is_cloned(repo.id)
            && !node_b.store().is_cloned(repo.id))
        .then_some(())
    })
    .await;
    sourcegull::routing::reload(&db, &clients, &nodes)
        .await
        .unwrap();
    assert_eq!(clients.router().node_for(repo.id), Some(a.as_str()));

    cancel.cancel();
    running_a.await.unwrap().unwrap();
    running_b.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_stays_live_while_cleanup_is_slower_than_the_live_window() {
    let Some(t) = TestDb::new().await else { return };
    let s = setup(&t).await;
    let (db, repo) = (s.db.clone(), s.repo.clone());
    let data = s.tmp.path().join("data-a");
    let a = clone_on_single_node(&s, data.clone()).await;

    // A move away from the node whose cleanup is due; holding its row locked makes the
    // cleanup (marking the move done) wait.
    sqlx::query(
        "INSERT INTO repo_moves (repo_id, from_node, to_node, kind, state, flipped_at)
         VALUES ($1, $2, 'http://gone.invalid:1', 'copy', 'flipped', now() - interval '1 hour')",
    )
    .bind(repo.id)
    .bind(&a)
    .execute(db.pool())
    .await
    .unwrap();
    let mut lock = db.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM repo_moves WHERE repo_id = $1 FOR UPDATE")
        .bind(repo.id)
        .execute(&mut *lock)
        .await
        .unwrap();

    let listener = TcpListener::bind(a.trim_start_matches("http://"))
        .await
        .unwrap();
    let cfg = config(&a, std::slice::from_ref(&a), data);
    let live_window = (cfg.rebalance_interval * 6).max(Duration::from_secs(5));
    let node = RepoNode::open(db.clone(), Some(s.keyring.clone()), s.site.clone(), cfg).unwrap();
    let cancel = CancellationToken::new();
    let running = tokio::spawn(node.clone().run(listener, cancel.clone()));

    let heartbeat_age = || async {
        let secs: f64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM now() - seen_at)::float8 FROM repo_nodes WHERE addr = $1",
        )
        .bind(&a)
        .fetch_one(db.pool())
        .await
        .unwrap();
        Duration::from_secs_f64(secs.max(0.0))
    };
    let until = tokio::time::Instant::now() + live_window + Duration::from_secs(2);
    while tokio::time::Instant::now() < until {
        let age = heartbeat_age().await;
        assert!(
            age < live_window / 2,
            "heartbeat is {age:?} old while cleanup is blocked"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(move_state(&db, repo.id).await.as_deref(), Some("flipped"));

    // The cleanup finishes once the lock is gone.
    lock.commit().await.unwrap();
    wait_for("the cleanup", || async {
        (move_state(&db, repo.id).await.as_deref() == Some("done")).then_some(())
    })
    .await;
    cancel.cancel();
    running.await.unwrap().unwrap();
}

async fn insert_repo(db: &Db, name: &str) -> RepoId {
    sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routing_reloads_only_the_repos_a_notification_names() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let (a, b) = ("http://a.invalid:1", "http://b.invalid:1");
    let nodes = vec![a.to_owned(), b.to_owned()];
    let named = insert_repo(&db, "gitlab.example.com/acme/named").await;
    let silent = insert_repo(&db, "gitlab.example.com/acme/silent").await;
    // `silent` starts on the node that is not its hash owner, so the router has it there
    // only once the refresher's first load ran.
    let (a, b) = if Router::new(nodes.clone()).hashed_node_for(silent) == Some(a) {
        (b, a)
    } else {
        (a, b)
    };
    db.repos()
        .assign_shards(&[(silent, a.to_owned())])
        .await
        .unwrap();

    let clients = sourcegull::routing::repo_clients(&nodes);
    let cancel = CancellationToken::new();
    let refresher = tokio::spawn(sourcegull::routing::run_refresher(
        db.clone(),
        clients.clone(),
        nodes.clone(),
        cancel.clone(),
    ));
    let routed = |repo: RepoId, node: &'static str| {
        let clients = clients.clone();
        async move { (clients.router().node_for(repo) == Some(node)).then_some(()) }
    };
    wait_for("the initial load", || routed(silent, a)).await;

    // A change nobody announces stays invisible until a full reload.
    sqlx::query("UPDATE repo SET shard = $2 WHERE id = $1")
        .bind(silent)
        .bind(b)
        .execute(db.pool())
        .await
        .unwrap();
    // An assignment announced with its repo id reloads that repo only.
    db.repos()
        .assign_shards(&[(named, b.to_owned())])
        .await
        .unwrap();
    wait_for("the announced repo", || routed(named, b)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(clients.router().node_for(silent), Some(a));

    // A notification without ids reloads every repo.
    sourcegull_db::notify::notify(db.pool(), sourcegull_db::notify::channels::REPO_SHARDS, "")
        .await
        .unwrap();
    wait_for("the full reload", || routed(silent, b)).await;
    assert_eq!(clients.router().node_for(named), Some(b));

    cancel.cancel();
    refresher.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_that_end_after_the_flip_leave_the_new_owner_alone() {
    use sourcegull_rpc::repo::v1::repo_service_server::RepoService as _;
    use sourcegull_rpc::tonic::{Code, Request};

    let Some(t) = TestDb::new().await else { return };
    let s = setup(&t).await;
    let (db, repo, work) = (s.db.clone(), s.repo.clone(), s.work.clone());
    let data_a = s.tmp.path().join("data-a");
    let a = clone_on_single_node(&s, data_a.clone()).await;
    let node_a = RepoNode::open(
        db.clone(),
        Some(s.keyring.clone()),
        s.site.clone(),
        config(&a, std::slice::from_ref(&a), data_a),
    )
    .unwrap();
    std::fs::write(work.join("README.md"), "pushed during the move\n").unwrap();
    git(&work, &["commit", "-q", "-am", "second"]);
    git(&work, &["push", "-q", "origin", "main"]);

    // The repo flips to another node while the first node's fetch runs (the update starts
    // after the flip here, past the job's ownership check).
    let other = "http://repo-elsewhere:3178";
    sqlx::query("UPDATE repo SET shard = $2 WHERE id = $1")
        .bind(repo.id)
        .bind(other)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE gitserver_repos SET node = $2 WHERE repo_id = $1")
        .bind(repo.id)
        .bind(other)
        .execute(db.pool())
        .await
        .unwrap();
    let events = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repo_ref_events WHERE repo_id = $1")
            .bind(repo.id)
            .fetch_one(db.pool())
            .await
            .unwrap()
    };
    let before = db.gitserver_repos().get(repo.id).await.unwrap().unwrap();
    let events_before = events().await;
    node_a
        .update_repo(&repo, CancellationToken::new(), false)
        .await
        .unwrap();
    let after = db.gitserver_repos().get(repo.id).await.unwrap().unwrap();
    assert_eq!(after.node.as_deref(), Some(other));
    assert_eq!(after.head_oid, before.head_oid);
    assert_eq!(after.last_fetched, before.last_fetched);
    assert_eq!(events().await, events_before);

    // Its index report is skipped, and the new owner's row stays.
    sqlx::query("INSERT INTO search_index_repos (repo_id, node) VALUES ($1, $2) ON CONFLICT (repo_id) DO UPDATE SET node = $2")
        .bind(repo.id)
        .bind(other)
        .execute(db.pool())
        .await
        .unwrap();
    let row_before = db.search_index().get(repo.id).await.unwrap().unwrap();
    let state = sourcegull_db::IndexedState {
        docs: 1,
        ..Default::default()
    };
    assert!(
        !db.search_index()
            .record_indexed(repo.id, &a, &state)
            .await
            .unwrap()
    );
    let row = db.search_index().get(repo.id).await.unwrap().unwrap();
    assert_eq!(row, row_before);
    assert_eq!(row.node, other);

    // A first clone of a repo assigned elsewhere leaves no copy.
    assert!(node_a.store().delete(repo.id).await.unwrap());
    node_a
        .update_repo(&repo, CancellationToken::new(), false)
        .await
        .unwrap();
    assert!(!node_a.store().is_cloned(repo.id));
    let row = db.gitserver_repos().get(repo.id).await.unwrap().unwrap();
    assert_eq!(row.node.as_deref(), Some(other));

    // The node's gRPC writes are refused; reads are served.
    let svc = sourcegull_gitstore::RepoNodeService::new(
        node_a.store().clone(),
        node_a.resolver().clone(),
        sourcegull_gitstore::ServiceConfig {
            allow_local_push: true,
            ..Default::default()
        },
    );
    let r = || {
        Some(pb::RepoRef {
            id: repo.id.get(),
            name: repo.name.clone(),
        })
    };
    let not_assigned = |status: sourcegull_rpc::tonic::Status| {
        assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
        assert_eq!(
            sourcegull_rpc::ErrorKind::of(&status),
            Some(sourcegull_rpc::ErrorKind::NotAssigned)
        );
    };
    not_assigned(
        svc.fetch_repo(Request::new(pb::FetchRepoRequest { repo: r() }))
            .await
            .unwrap_err(),
    );
    not_assigned(
        svc.clone_repo(Request::new(pb::CloneRepoRequest { repo: r() }))
            .await
            .unwrap_err(),
    );
    not_assigned(
        svc.ensure_repo(Request::new(pb::EnsureRepoRequest {
            repo: r(),
            fetch_if_cloned: true,
        }))
        .await
        .unwrap_err(),
    );
    not_assigned(
        svc.create_commit_from_patch(Request::new(pb::CreateCommitFromPatchRequest {
            repo: r(),
            ..Default::default()
        }))
        .await
        .unwrap_err(),
    );
    not_assigned(
        svc.push(Request::new(pb::PushRequest {
            repo: r(),
            source: "main".into(),
            destination_ref: "refs/heads/topic".into(),
            target: None,
        }))
        .await
        .unwrap_err(),
    );
    assert!(!node_a.store().is_cloned(repo.id));
    let st = svc
        .repo_status(Request::new(pb::RepoStatusRequest {
            repo: r(),
            include_size: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!st.cloned);

    // Assigned back: the same write goes through.
    sqlx::query("UPDATE repo SET shard = $2 WHERE id = $1")
        .bind(repo.id)
        .bind(&a)
        .execute(db.pool())
        .await
        .unwrap();
    svc.clone_repo(Request::new(pb::CloneRepoRequest { repo: r() }))
        .await
        .unwrap();
    assert!(node_a.store().is_cloned(repo.id));
}
