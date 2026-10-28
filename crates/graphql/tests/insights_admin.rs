//! Trends admin API and date grouping end to end against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and an in-process repo node:
//!
//! - `insightAdminBackfillQueue`: states through a backfill (new, queued, completed), queue
//!   positions, sorting, the state and text filters, paging, site admins only;
//! - `moveInsightSeriesBackfillToFrontOfQueue`/`…BackOfQueue` reorder unfinished backfills
//!   and their jobs; `retryInsightSeriesBackfill` restarts finished ones; each refuses the
//!   other kind;
//! - `groupBy: DATE`: a commit search grouped by commit date, backfilled on the node.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_graphql::{Request, Variables};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_gitstore::{Remote, RemoteAuth};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_insights::{BackfillState, Counter, Enqueuer, InsightStore};
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::RepoNode;
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;

const REPO: &str = "gitlab.example.com/acme/web";

struct Source {
    dir: tempfile::TempDir,
}

impl Source {
    fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        s.git(Utc::now(), &["init", "-q", "-b", "main"]);
        s
    }

    fn git(&self, at: DateTime<Utc>, args: &[&str]) {
        let date = format!("{} +0000", at.timestamp());
        let out = Command::new("git")
            .args(["-c", "user.name=Ann", "-c", "user.email=ann@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.dir.path())
            .current_dir(self.dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(&self, at: DateTime<Utc>, message: &str) {
        std::fs::write(self.dir.path().join("a.txt"), message).unwrap();
        self.git(at, &["add", "-A"]);
        self.git(at, &["commit", "-q", "-m", message]);
    }

    fn remote(&self) -> Remote {
        Remote::new(&self.dir.path().display().to_string(), RemoteAuth::None)
    }
}

struct Env {
    schema: SourcegullSchema,
    auth: Authenticator,
}

impl Env {
    async fn run(&self, user: UserId, query: &str, vars: Value) -> async_graphql::Response {
        let actor = self
            .auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, user: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(user, query, vars).await;
        let e = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: expected an error, got {:?}", resp.data));
        let code = e
            .extensions
            .as_ref()
            .and_then(|x| x.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default();
        let mut message = e.message.clone();
        if let Some(m) = e.extensions.as_ref().and_then(|x| x.get("messages")) {
            message.push_str(&format!(" {:?}", m.clone().into_json().unwrap()));
        }
        (code, message)
    }
}

const CREATE_LINE: &str = "mutation($input: LineChartSearchInsightInput!) {
    createLineChartSearchInsight(input: $input) { view { id } } }";

const QUEUE: &str = "query($first: Int, $after: String, $orderBy: BackfillQueueOrderBy,
        $descending: Boolean, $states: [InsightQueueItemState!], $text: String) {
    insightAdminBackfillQueue(first: $first, after: $after, orderBy: $orderBy,
            descending: $descending, states: $states, textSearch: $text) {
        totalCount
        pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
        nodes { id insightViewTitle creator { __typename username } seriesLabel seriesSearchQuery
            backfillQueueStatus { state queuePosition errors cost percentComplete
                createdAt startedAt completedAt runtime } } } }";

const ITEM: &str = "id insightViewTitle backfillQueueStatus { state queuePosition }";

fn line(title: &str, query: &str, group_by: Option<&str>) -> Value {
    let mut series = json!({ "query": query, "options": { "label": format!("{title} series") } });
    if let Some(g) = group_by {
        series["groupBy"] = json!(g);
    }
    json!({ "input": {
        "options": { "title": title },
        "dataSeries": [series],
        "repositoryScope": { "repositories": [REPO] },
        "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
    } })
}

fn nodes(v: &Value) -> &Vec<Value> {
    v["insightAdminBackfillQueue"]["nodes"].as_array().unwrap()
}

/// (title, state, position) of each listed item.
fn summary(v: &Value) -> Vec<(String, String, Option<i64>)> {
    nodes(v)
        .iter()
        .map(|n| {
            let s = &n["backfillQueueStatus"];
            (
                n["insightViewTitle"].as_str().unwrap().to_owned(),
                s["state"].as_str().unwrap().to_owned(),
                s["queuePosition"].as_i64(),
            )
        })
        .collect()
}

fn item_id(v: &Value, title: &str) -> String {
    nodes(v)
        .iter()
        .find(|n| n["insightViewTitle"] == title)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn backfill_queue_admin_api_and_date_grouping() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            insights_maximum_sample_size: 4,
            insights_query_worker_rate_limit: 1000.0,
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    // The first user is the site admin.
    let admin = auth
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;

    // Two fixes 2.5 days ago, one 1.5 days ago (noon-ish, so each falls on one UTC day).
    let now = Utc::now();
    let day = TimeDelta::days(1);
    let (d1, d2) = (now - day * 5 / 2, now - day * 3 / 2);
    let src = Source::new();
    src.commit(d1, "fix one");
    src.commit(d1 + TimeDelta::seconds(1), "fix two");
    src.commit(d2, "fix three");
    let node = RepoNode::start(src.remote()).await;
    let repo: i32 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
         VALUES ($1, false, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(REPO)
    .fetch_one(db.pool())
    .await
    .unwrap();
    node.clone_repo(RepoId(repo), REPO).await;
    sourcegull_db::sqlx::query(
        "INSERT INTO gitserver_repos (repo_id, clone_status, last_changed)
         VALUES ($1, 'cloned', $2)",
    )
    .bind(repo)
    .bind(now - day)
    .execute(db.pool())
    .await
    .unwrap();

    let state = AppState::new(
        db.clone(),
        config.clone(),
        sourcegull_observability::MetricsHandle::from_prometheus(
            sourcegull_observability::build_recorder().unwrap().handle(),
        ),
        auth.clone(),
    )
    .with_repo_clients(node.clients.clone());
    let env = Env {
        schema: build_schema(Services::from_state(&state)),
        auth: auth.clone(),
    };
    let counter = Counter::new(state.search_backend(), Some(config.clone()));
    let enqueuer = Enqueuer::new(counter.clone(), Some(config.clone()));
    let store = InsightStore::new(&db);

    // --- DATE needs a commit or diff search ---
    let (code, msg) = env
        .err(alice, CREATE_LINE, line("bad", "fix", Some("DATE")))
        .await;
    assert_eq!(code, "VALIDATION");
    assert!(msg.contains("Grouping by date"), "{msg}");

    for (title, query, group) in [
        ("Fixes by day", "type:commit fix", Some("DATE")),
        ("Fix count", "fix", None),
        ("Readme", "file:README fix", None),
    ] {
        env.ok(alice, CREATE_LINE, line(title, query, group)).await;
    }

    // --- site admins only ---
    let (code, msg) = env.err(alice, QUEUE, json!({})).await;
    assert_eq!(code, "FORBIDDEN", "{msg}");

    // New: every backfill waits for the enqueuer.
    let v = env
        .ok(admin, QUEUE, json!({ "orderBy": "QUEUE_POSITION" }))
        .await;
    assert_eq!(v["insightAdminBackfillQueue"]["totalCount"], 3);
    let first = nodes(&v)
        .iter()
        .find(|n| n["insightViewTitle"] == "Fixes by day")
        .unwrap();
    assert_eq!(first["creator"]["username"], "alice");
    assert_eq!(first["creator"]["__typename"], "User");
    assert_eq!(first["seriesSearchQuery"], "type:commit fix");
    assert_eq!(first["seriesLabel"], "Fixes by day series");
    let status = &first["backfillQueueStatus"];
    assert_eq!(status["state"], "NEW");
    assert_eq!(status["percentComplete"], 0);
    assert!(status["cost"].is_null() && status["runtime"].is_null());
    assert!(status["createdAt"].is_string() && status["startedAt"].is_null());
    let positions: Vec<i64> = summary(&v).iter().map(|s| s.2.unwrap()).collect();
    assert_eq!(positions, vec![1, 2, 3]);
    let by_day = item_id(&v, "Fixes by day");
    let readme = item_id(&v, "Readme");

    // Moves work before the backfill starts.
    let moved = env
        .ok(
            admin,
            &format!(
                "mutation($id: ID!) {{ moveInsightSeriesBackfillToFrontOfQueue(id: $id) {{ {ITEM} }} }}"
            ),
            json!({ "id": readme }),
        )
        .await;
    let m = &moved["moveInsightSeriesBackfillToFrontOfQueue"];
    assert_eq!(m["id"], readme.as_str());
    assert_eq!(m["backfillQueueStatus"]["queuePosition"], 1);

    // Queued: jobs exist, none started (the workers are not running yet).
    enqueuer.pass(Utc::now()).await.unwrap();
    let v = env
        .ok(admin, QUEUE, json!({ "orderBy": "QUEUE_POSITION" }))
        .await;
    let queued = summary(&v);
    assert_eq!(queued[0], ("Readme".into(), "QUEUED".into(), Some(1)));
    assert!(
        queued[1..]
            .iter()
            .all(|s| s.1 == "QUEUED" && s.2.is_some_and(|p| p > 1)),
        "{queued:?}"
    );
    let status = &nodes(&v)[0]["backfillQueueStatus"];
    // One repository times four points.
    assert_eq!(status["cost"], 4);
    // Moving to the back reorders the series and its jobs; the front moves it back up.
    let back = format!(
        "mutation($id: ID!) {{ moveInsightSeriesBackfillToBackOfQueue(id: $id) {{ {ITEM} }} }}"
    );
    let moved = env.ok(admin, &back, json!({ "id": readme })).await;
    assert_eq!(
        moved["moveInsightSeriesBackfillToBackOfQueue"]["backfillQueueStatus"]["queuePosition"],
        3
    );
    let priorities: Vec<(String, i64)> = sourcegull_db::sqlx::query_as(
        "SELECT s.query, j.priority FROM insight_backfill_jobs j
         JOIN insight_series s ON s.id = j.series_id ORDER BY j.priority DESC",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(priorities.last().unwrap().0, "file:README fix");
    let (code, _) = env.err(alice, &back, json!({ "id": readme })).await;
    assert_eq!(code, "FORBIDDEN");
    // Unfinished backfills cannot be retried.
    let retry =
        format!("mutation($id: ID!) {{ retryInsightSeriesBackfill(id: $id) {{ {ITEM} }} }}");
    let (code, msg) = env.err(admin, &retry, json!({ "id": by_day })).await;
    assert_eq!(code, "CONFLICT");
    assert!(msg.contains("QUEUED"), "{msg}");
    let (code, _) = env.err(admin, &retry, json!({ "id": "bogus" })).await;
    assert_eq!(code, "INVALID_INPUT");

    // --- backfill everything ---
    let cancel = CancellationToken::new();
    let workers = sourcegull_insights::workers(counter.clone(), "test", 1).unwrap();
    let running = tokio::spawn(workers.run(cancel.clone()));
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        enqueuer.pass(Utc::now()).await.unwrap();
        let live = store.list_live().await.unwrap();
        if live
            .iter()
            .all(|s| s.backfill_state == BackfillState::Completed)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backfills did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let v = env.ok(admin, QUEUE, json!({})).await;
    for n in nodes(&v) {
        let s = &n["backfillQueueStatus"];
        assert_eq!(s["state"], "COMPLETED", "{n}");
        assert_eq!(s["percentComplete"], 100);
        assert!(s["queuePosition"].is_null());
        assert_eq!(s["errors"], json!([]));
        assert!(s["startedAt"].is_string() && s["completedAt"].is_string());
        let runtime = s["runtime"].as_str().unwrap();
        assert!(runtime.ends_with('s'), "{runtime}");
    }
    // Finished backfills cannot be moved.
    let (code, msg) = env.err(admin, &back, json!({ "id": by_day })).await;
    assert_eq!(code, "CONFLICT");
    assert!(msg.contains("COMPLETED"), "{msg}");

    // --- the DATE series: one value per commit day, counted cumulatively per point ---
    let view = env
        .ok(
            alice,
            "{ insightViews { nodes { presentation { ... on LineChartInsightViewPresentation {
                title } } dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition {
                groupBy } } dataSeries { label points { value } } } } }",
            json!({}),
        )
        .await;
    let by_day_view = view["insightViews"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["presentation"]["title"] == "Fixes by day")
        .unwrap();
    assert_eq!(by_day_view["dataSeriesDefinitions"][0]["groupBy"], "DATE");
    let day_of = |t: DateTime<Utc>| t.format("%Y-%m-%d").to_string();
    let mut last: Vec<(String, i64)> = by_day_view["dataSeries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let pts = s["points"].as_array().unwrap();
            (
                s["label"].as_str().unwrap().to_owned(),
                pts.last().unwrap()["value"].as_f64().unwrap() as i64,
            )
        })
        .collect();
    last.sort();
    let mut want: Vec<(String, i64)> = Vec::new();
    for t in [d1, d1 + TimeDelta::seconds(1), d2] {
        match want.iter_mut().find(|w| w.0 == day_of(t)) {
            Some(w) => w.1 += 1,
            None => want.push((day_of(t), 1)),
        }
    }
    want.sort();
    assert_eq!(last, want);

    // --- filters, sorting and paging ---
    let v = env
        .ok(
            admin,
            QUEUE,
            json!({ "text": "FIX C", "states": ["COMPLETED"] }),
        )
        .await;
    assert_eq!(
        summary(&v).into_iter().map(|s| s.0).collect::<Vec<_>>(),
        vec!["Fix count".to_owned()]
    );
    // The label matches too.
    let v = env.ok(admin, QUEUE, json!({ "text": "day series" })).await;
    assert_eq!(v["insightAdminBackfillQueue"]["totalCount"], 1);
    let v = env
        .ok(admin, QUEUE, json!({ "states": ["QUEUED", "UNKNOWN"] }))
        .await;
    assert_eq!(v["insightAdminBackfillQueue"]["totalCount"], 0);
    let v = env.ok(admin, QUEUE, json!({ "states": ["UNKNOWN"] })).await;
    assert_eq!(v["insightAdminBackfillQueue"]["totalCount"], 0);
    let p1 = env.ok(admin, QUEUE, json!({ "first": 2 })).await;
    let info = &p1["insightAdminBackfillQueue"]["pageInfo"];
    assert_eq!(info["hasNextPage"], true);
    assert_eq!(info["hasPreviousPage"], false);
    assert_eq!(nodes(&p1).len(), 2);
    let p2 = env
        .ok(
            admin,
            QUEUE,
            json!({ "first": 2, "after": info["endCursor"] }),
        )
        .await;
    assert_eq!(nodes(&p2).len(), 1);
    assert_eq!(
        p2["insightAdminBackfillQueue"]["pageInfo"]["hasNextPage"],
        false
    );
    let mut seen: Vec<String> = nodes(&p1)
        .iter()
        .chain(nodes(&p2))
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3);
    let asc = env.ok(admin, QUEUE, json!({})).await;
    let desc = env.ok(admin, QUEUE, json!({ "descending": true })).await;
    let mut a = summary(&asc);
    a.reverse();
    assert_eq!(a, summary(&desc));

    // --- retry: the finished backfill starts over and completes again ---
    let r = env.ok(admin, &retry, json!({ "id": by_day })).await;
    assert_eq!(
        r["retryInsightSeriesBackfill"]["backfillQueueStatus"]["state"],
        "NEW"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        enqueuer.pass(Utc::now()).await.unwrap();
        let v = env
            .ok(admin, QUEUE, json!({ "states": ["COMPLETED"] }))
            .await;
        if v["insightAdminBackfillQueue"]["totalCount"] == 3 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the retried backfill did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    cancel.cancel();
    running.await.unwrap();
}
