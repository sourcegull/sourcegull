//! Trends end to end against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and
//! an in-process repo node, over a real repository history (two repositories cloned from it,
//! one private):
//!
//! - views: a search series, a capture group series and a grouped series are created over
//!   GraphQL, backfilled by the workers (each history counted incrementally on the node) and
//!   read with points from the aggregate (a viewer who reads every repository) or summed per
//!   repository (one who does not); identical series are shared between views; updates keep
//!   or replace series; `saveInsightAsNewView`; deleting views deletes unused series;
//! - filters (include/exclude regex, a search context), sort and limit, drill-down queries;
//! - dashboards and grants: personal, organization and global, who sees what, and the
//!   permission checks of the mutations;
//! - previews, scope query checks, the backfill status, a recording that carries an
//!   unchanged repository forward;
//! - pie charts' language statistics from the index and from the unindexed counter;
//! - the CSV export at `/.api/insights/export/{id}`, filtered by repository permissions.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_graphql::{Request, Variables};
use axum::body::Body;
use axum::http::{self, StatusCode};
use chrono::{DateTime, TimeDelta, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_gitstore::{Remote, RemoteAuth};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_insights::{Counter, Enqueuer, InsightStore};
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::RepoNode;
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const PUBLIC: &str = "gitlab.example.com/acme/web";
const PRIVATE: &str = "gitlab.example.com/acme/secret";

/// A source repository whose commits carry chosen dates.
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

    fn git(&self, at: DateTime<Utc>, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commit(&self, at: DateTime<Utc>, files: &[(&str, Option<&str>)]) {
        for (path, content) in files {
            let p = self.dir.path().join(path);
            match content {
                Some(c) => {
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(p, c).unwrap();
                }
                None => std::fs::remove_file(p).unwrap(),
            }
        }
        self.git(at, &["add", "-A"]);
        self.git(at, &["commit", "-q", "-m", "change"]);
    }

    fn remote(&self) -> Remote {
        Remote::new(&self.dir.path().display().to_string(), RemoteAuth::None)
    }
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    let id: i32 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
         VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .bind(private)
    .bind(ext)
    .fetch_one(db.pool())
    .await
    .unwrap();
    RepoId(id)
}

/// What the repo nodes would record after a clone or a fetch that changed refs.
async fn set_last_changed(db: &Db, repo: RepoId, at: DateTime<Utc>) {
    sourcegull_db::sqlx::query(
        "INSERT INTO gitserver_repos (repo_id, clone_status, last_changed) VALUES ($1, 'cloned', $2)
         ON CONFLICT (repo_id) DO UPDATE SET last_changed = $2",
    )
    .bind(repo.get())
    .bind(at)
    .execute(db.pool())
    .await
    .unwrap();
}

struct Env {
    schema: SourcegullSchema,
    auth: Authenticator,
    router: axum::Router,
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

    /// The code and message (with any validation messages) of the first error.
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

    /// `GET /.api/insights/export/{id}` with a token of `user` (none: anonymous).
    async fn export(&self, user: Option<UserId>, id: &str) -> (StatusCode, Vec<u8>) {
        let mut req = http::Request::get(format!("/.api/insights/export/{id}"));
        if let Some(u) = user {
            let actor = self
                .auth
                .actor_for(u, AuthVia::Session)
                .await
                .unwrap()
                .unwrap();
            let (_, secret) = self
                .auth
                .create_access_token(&actor, u, &["user:all".into()], "t", None)
                .await
                .unwrap();
            req = req.header("authorization", format!("token {secret}"));
        }
        let resp = self
            .router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (
            status,
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
    }
}

const CREATE_LINE: &str = "mutation($input: LineChartSearchInsightInput!) {
    createLineChartSearchInsight(input: $input) { view { id
        dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition { seriesId } } } } }";

const VIEW: &str = "query($id: ID!, $filters: InsightViewFiltersInput,
        $display: SeriesDisplayOptionsInput) {
    insightViews(id: $id, filters: $filters, seriesDisplayOptions: $display) {
        nodes { id seriesCount
            presentation { __typename
                ... on LineChartInsightViewPresentation { title
                    seriesPresentation { seriesId label color } }
                ... on PieChartInsightViewPresentation { title otherThreshold } }
            defaultFilters { includeRepoRegex excludeRepoRegex searchContexts }
            appliedFilters { includeRepoRegex excludeRepoRegex searchContexts }
            defaultSeriesDisplayOptions { limit sortOptions { mode direction } }
            dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition {
                seriesId query generatedFromCaptureGroups groupBy isCalculated
                repositoryDefinition { __typename
                    ... on InsightRepositoryScope { repositories }
                    ... on RepositorySearchScope { search allRepositories } }
                timeScope { ... on InsightIntervalTimeScope { unit value } } } }
            dataSeries { seriesId label
                points { dateTime value diffQuery pointInTimeQuery }
                status { totalPoints pendingJobs completedJobs failedJobs backfillQueuedAt
                    isLoadingData incompleteDatapoints { __typename time } } }
            dashboards { nodes { title } }
            languageStatistics { name totalLines totalBytes }
        } totalCount } }";

fn series_def_id(created: &Value) -> String {
    created["createLineChartSearchInsight"]["view"]["dataSeriesDefinitions"][0]["seriesId"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn view_id(created: &Value) -> String {
    created["createLineChartSearchInsight"]["view"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn the_view(v: &Value) -> &Value {
    &v["insightViews"]["nodes"][0]
}

/// Label and values of each data series of a view.
fn values(v: &Value) -> Vec<(String, Vec<i64>)> {
    the_view(v)["dataSeries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["label"].as_str().unwrap().to_owned(),
                s["points"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["value"].as_f64().unwrap() as i64)
                    .collect(),
            )
        })
        .collect()
}

fn line(title: &str, series: Value, scope: Value) -> Value {
    json!({ "input": {
        "options": { "title": title },
        "dataSeries": series,
        "repositoryScope": scope,
        "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
    } })
}

fn all_repos() -> Value {
    json!({ "repositories": [] })
}

fn titles(v: &Value) -> Vec<String> {
    v["insightsDashboards"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn insight_views_dashboards_filters_and_export() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            insights_maximum_sample_size: 8,
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
    let admin = auth
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        users.push(
            db.users()
                .create(&NewUser {
                    username: name.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
        );
    }
    let (alice, bob) = (users[0], users[1]);
    let org = db.orgs().create("acme", None).await.unwrap();
    db.orgs()
        .add_member(org.id, bob, sourcegull_types::OrgRole::Member)
        .await
        .unwrap();

    // History: TODO counts 2 (20 days ago), 3 (6.5 days ago), 4 (3.5 days ago); captured
    // values alice 1/2/2 and bob 1/1/0.
    let now = Utc::now();
    let day = TimeDelta::days(1);
    let src = Source::new();
    src.commit(
        now - day * 20,
        &[("a.go", Some("TODO(alice) a\nTODO(bob) b\n"))],
    );
    src.commit(now - day * 13 / 2, &[("b.go", Some("TODO(alice) c\n"))]);
    src.commit(
        now - day * 7 / 2,
        &[
            ("a.go", Some("TODO(alice) a\n")),
            ("docs/x.md", Some("todo todo\n")),
        ],
    );
    let node = RepoNode::start(src.remote()).await;
    let public = insert_repo(&db, PUBLIC, "1", false).await;
    let private = insert_repo(&db, PRIVATE, "2", true).await;
    for (id, name) in [(public, PUBLIC), (private, PRIVATE)] {
        node.clone_repo(id, name).await;
        set_last_changed(&db, id, now - day * 3).await;
    }
    // The public repository is indexed, the private one is not.
    node.index_repo(public, PUBLIC, &[]);

    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle()),
        auth.clone(),
    )
    .with_repo_clients(node.clients.clone());
    let schema = build_schema(Services::from_state(&state));
    let router = sourcegull_http_api::finish(
        sourcegull_http_api::public_routes()
            .merge(sourcegull::features::graphql_routes(&state))
            .merge(sourcegull_insights::http::routes()),
        state.clone(),
    );
    let env = Env {
        schema,
        auth: auth.clone(),
        router,
    };
    let counter = Counter::new(state.search_backend(), Some(config.clone()));
    let enqueuer = Enqueuer::new(counter.clone(), Some(config.clone()));
    let store = InsightStore::new(&db);

    // --- validation ---
    let (code, msg) = env
        .err(
            alice,
            CREATE_LINE,
            line(
                "bad",
                json!([{ "query": "x select:repo", "options": {} }]),
                all_repos(),
            ),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    assert!(msg.contains("select"), "{msg}");
    // The private repository does not exist for alice.
    let (code, msg) = env
        .err(
            alice,
            CREATE_LINE,
            line(
                "bad",
                json!([{ "query": "TODO", "options": {}, "groupBy": "REPO" }]),
                json!({ "repositories": [PUBLIC, PRIVATE] }),
            ),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    assert!(msg.contains(PRIVATE), "{msg}");
    // Grouped series need a repository list.
    let (code, _) = env
        .err(
            alice,
            CREATE_LINE,
            line(
                "bad",
                json!([{ "query": "TODO", "options": {}, "groupBy": "REPO" }]),
                all_repos(),
            ),
        )
        .await;
    assert_eq!(code, "VALIDATION");

    // --- views ---
    let v1 = env
        .ok(
            alice,
            CREATE_LINE,
            line(
                "TODOs",
                json!([{ "query": "TODO", "options": { "label": "todos", "lineColor": "#f00" } }]),
                all_repos(),
            ),
        )
        .await;
    let (v1_id, todo_series) = (view_id(&v1), series_def_id(&v1));
    let v2 = env
        .ok(
            alice,
            CREATE_LINE,
            line(
                "Owners",
                json!([{ "query": "TODO\\((\\w+)\\)", "options": { "label": "owners" },
                    "generatedFromCaptureGroups": true }]),
                all_repos(),
            ),
        )
        .await;
    let v2_id = view_id(&v2);
    let v3 = env
        .ok(
            admin,
            CREATE_LINE,
            line(
                "By repo",
                json!([{ "query": "TODO", "options": { "label": "repos" }, "groupBy": "REPO" }]),
                json!({ "repositories": [PUBLIC, PRIVATE] }),
            ),
        )
        .await;
    let v3_id = view_id(&v3);
    // Bob defines the same series as alice: it is shared.
    let v4 = env
        .ok(
            bob,
            CREATE_LINE,
            line(
                "Mine too",
                json!([{ "query": "TODO", "options": { "label": "t" } }]),
                all_repos(),
            ),
        )
        .await;
    let v4_id = view_id(&v4);
    assert_eq!(series_def_id(&v4), todo_series);
    let pie = env
        .ok(
            alice,
            "mutation($input: PieChartSearchInsightInput!) {
                createPieChartSearchInsight(input: $input) { view { id } } }",
            json!({ "input": { "query": "", "repositoryScope": { "repositories": [PUBLIC] },
                "presentationOptions": { "title": "Languages", "otherThreshold": 0.03 } } }),
        )
        .await;
    let pie_id = pie["createPieChartSearchInsight"]["view"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Backfill: three series (the shared one once), one job per repository each.
    let report = enqueuer.pass(Utc::now()).await.unwrap();
    assert_eq!(report.backfills_started, 3);
    let cancel = CancellationToken::new();
    let workers = sourcegull_insights::workers(counter.clone(), "test", 2).unwrap();
    let running = tokio::spawn(workers.run(cancel.clone()));
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        enqueuer.pass(Utc::now()).await.unwrap();
        let live = store.list_live().await.unwrap();
        if live
            .iter()
            .all(|s| s.backfill_state == sourcegull_insights::BackfillState::Completed)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backfills did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Alice reads the public repository only: per-repository sums.
    let v = env.ok(alice, VIEW, json!({ "id": v1_id })).await;
    let view = the_view(&v);
    assert_eq!(
        values(&v),
        vec![("todos".into(), vec![2, 3, 3, 3, 4, 4, 4, 4])]
    );
    assert_eq!(
        view["presentation"]["seriesPresentation"][0],
        json!({ "seriesId": todo_series, "label": "todos", "color": "#f00" })
    );
    let def = &view["dataSeriesDefinitions"][0];
    assert_eq!(def["repositoryDefinition"]["allRepositories"], true);
    assert_eq!(def["timeScope"], json!({ "unit": "DAY", "value": 1 }));
    assert_eq!(def["generatedFromCaptureGroups"], false);
    let s = &view["dataSeries"][0];
    assert_eq!(s["status"]["completedJobs"], 2);
    assert_eq!(s["status"]["pendingJobs"], 0);
    assert_eq!(s["status"]["failedJobs"], 0);
    assert_eq!(s["status"]["isLoadingData"], false);
    assert!(s["status"]["backfillQueuedAt"].is_string());
    assert!(s["status"]["totalPoints"].as_i64().unwrap() > 0);
    // Drill-down: a diff search bounded to each point's period.
    let p0 = &s["points"][0];
    let p1 = &s["points"][1];
    let q0 = p0["diffQuery"].as_str().unwrap();
    assert!(
        q0.contains("type:diff") && q0.contains("before:") && !q0.contains("after:"),
        "{q0}"
    );
    assert!(q0.ends_with("TODO"), "{q0}");
    let q1 = p1["diffQuery"].as_str().unwrap();
    let after = DateTime::parse_from_rfc3339(p0["dateTime"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert!(q1.contains(&format!("after:{after}")), "{q1}");
    assert!(
        p1["pointInTimeQuery"]
            .as_str()
            .unwrap()
            .contains("rev:at.time(")
    );

    // Capture groups: one series per value, newest value first by default.
    let v = env.ok(alice, VIEW, json!({ "id": v2_id })).await;
    assert_eq!(
        values(&v),
        vec![
            ("alice".into(), vec![1, 2, 2, 2, 2, 2, 2, 2]),
            ("bob".into(), vec![1, 1, 1, 1, 0, 0, 0, 0]),
        ]
    );
    let alice_series = &the_view(&v)["dataSeries"][0];
    assert!(
        alice_series["seriesId"]
            .as_str()
            .unwrap()
            .ends_with("-alice")
    );
    let q = alice_series["points"][1]["diffQuery"].as_str().unwrap();
    assert!(
        q.contains("TODO\\((alice)\\)") && q.contains("patterntype:regexp"),
        "{q}"
    );
    assert_eq!(
        the_view(&v)["dataSeriesDefinitions"][0]["generatedFromCaptureGroups"],
        true
    );
    // Sort and limit.
    let v = env
        .ok(
            alice,
            VIEW,
            json!({ "id": v2_id, "display": { "limit": 1,
                "sortOptions": { "mode": "LEXICOGRAPHICAL", "direction": "DESC" } } }),
        )
        .await;
    assert_eq!(values(&v).len(), 1);
    assert_eq!(values(&v)[0].0, "bob");
    assert_eq!(the_view(&v)["seriesCount"], 2);
    let v = env
        .ok(
            alice,
            VIEW,
            json!({ "id": v2_id, "display": {
                "sortOptions": { "mode": "RESULT_COUNT", "direction": "ASC" }, "numSamples": 3 } }),
        )
        .await;
    assert_eq!(
        values(&v),
        vec![
            ("bob".into(), vec![0, 0, 0]),
            ("alice".into(), vec![2, 2, 2])
        ]
    );

    // Views are visible to their creator only (and through dashboards).
    let v = env.ok(bob, VIEW, json!({ "id": v1_id })).await;
    assert_eq!(v["insightViews"]["nodes"], json!([]));
    let v = env
        .ok(
            bob,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({ "id": v1_id }),
        )
        .await;
    assert!(v["node"].is_null());
    let v = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({ "id": v1_id }),
        )
        .await;
    assert_eq!(v["node"]["id"], v1_id.as_str());
    let (code, _) = env
        .err(
            bob,
            "mutation($id: ID!) { deleteInsightView(id: $id) { alwaysNil } }",
            json!({ "id": v1_id }),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");
    let (status, _) = env.export(Some(bob), &v1_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env.export(None, &v1_id).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v = env
        .ok(
            alice,
            "{ insightViews { nodes { id } totalCount } }",
            json!({}),
        )
        .await;
    assert_eq!(v["insightViews"]["totalCount"], 3);
    let v = env
        .ok(
            alice,
            "{ insightViews(find: \"own\") { nodes { id } } }",
            json!({}),
        )
        .await;
    assert_eq!(v["insightViews"]["nodes"], json!([{ "id": v2_id }]));
    let v = env
        .ok(
            alice,
            "{ insightViews(first: 2) { nodes { id } pageInfo { hasNextPage endCursor } } }",
            json!({}),
        )
        .await;
    assert_eq!(v["insightViews"]["pageInfo"]["hasNextPage"], true);
    let cursor = v["insightViews"]["pageInfo"]["endCursor"].clone();
    let rest = env
        .ok(
            alice,
            "query($after: String) { insightViews(first: 2, after: $after) { nodes { id }
                pageInfo { hasNextPage } } }",
            json!({ "after": cursor }),
        )
        .await;
    assert_eq!(rest["insightViews"]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(rest["insightViews"]["pageInfo"]["hasNextPage"], false);

    // --- dashboards ---
    let create_dashboard = "mutation($input: CreateInsightsDashboardInput!) {
        createInsightsDashboard(input: $input) { dashboard { id title
            grants { users organizations global } } } }";
    let alice_id = sourcegull_services::relay::marshal_id("User", alice.0);
    let bob_id = sourcegull_services::relay::marshal_id("User", bob.0);
    let org_id = sourcegull_services::relay::marshal_id("Org", org.id.0);
    let personal = env
        .ok(
            alice,
            create_dashboard,
            json!({ "input": { "title": "Mine", "grants": { "users": [alice_id] } } }),
        )
        .await;
    let personal_id = personal["createInsightsDashboard"]["dashboard"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Grants only to oneself, one's organizations or everyone; at least one.
    let (code, _) = env
        .err(
            bob,
            create_dashboard,
            json!({ "input": { "title": "x", "grants": { "users": [alice_id] } } }),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    let (code, _) = env
        .err(
            alice,
            create_dashboard,
            json!({ "input": { "title": "x", "grants": { "organizations": [org_id] } } }),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    let (code, _) = env
        .err(
            alice,
            create_dashboard,
            json!({ "input": { "title": "x", "grants": {} } }),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    let org_dash = env
        .ok(
            bob,
            create_dashboard,
            json!({ "input": { "title": "Acme", "grants": { "organizations": [org_id] } } }),
        )
        .await;
    let org_dash_id = org_dash["createInsightsDashboard"]["dashboard"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        org_dash["createInsightsDashboard"]["dashboard"]["grants"],
        json!({ "users": [], "organizations": [org_id], "global": false })
    );
    let global = env
        .ok(
            admin,
            create_dashboard,
            json!({ "input": { "title": "Everyone", "grants": { "global": true } } }),
        )
        .await;
    let global_id = global["createInsightsDashboard"]["dashboard"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let list = "{ insightsDashboards { nodes { title views { nodes { id } totalCount } } } }";
    assert_eq!(
        titles(&env.ok(alice, list, json!({})).await),
        ["Mine", "Everyone"]
    );
    assert_eq!(
        titles(&env.ok(bob, list, json!({})).await),
        ["Acme", "Everyone"]
    );
    assert_eq!(titles(&env.ok(admin, list, json!({})).await), ["Everyone"]);
    // Alice sees neither the org dashboard nor its node.
    let v = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({ "id": org_dash_id }),
        )
        .await;
    assert!(v["node"].is_null());
    let (code, _) = env
        .err(
            alice,
            "mutation($id: ID!) { updateInsightsDashboard(id: $id, input: { title: \"x\" }) {
                dashboard { id } } }",
            json!({ "id": org_dash_id }),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");
    let add = "mutation($view: ID!, $dashboard: ID!) {
        addInsightViewToDashboard(input: { insightViewId: $view, dashboardId: $dashboard }) {
            dashboard { title views { nodes { id } } } } }";
    // Bob cannot add alice's view (he does not see it), alice cannot use the org dashboard.
    let (code, _) = env
        .err(bob, add, json!({ "view": v1_id, "dashboard": org_dash_id }))
        .await;
    assert_eq!(code, "NOT_FOUND");
    let (code, _) = env
        .err(
            alice,
            add,
            json!({ "view": v1_id, "dashboard": org_dash_id }),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");
    let v = env
        .ok(
            alice,
            add,
            json!({ "view": v1_id, "dashboard": personal_id }),
        )
        .await;
    assert_eq!(
        v["addInsightViewToDashboard"]["dashboard"]["views"]["nodes"],
        json!([{ "id": v1_id }])
    );
    env.ok(alice, add, json!({ "view": v1_id, "dashboard": global_id }))
        .await;
    env.ok(admin, add, json!({ "view": v3_id, "dashboard": global_id }))
        .await;
    // Through the global dashboard everyone sees V1 and V3.
    let v = env.ok(admin, VIEW, json!({ "id": v1_id })).await;
    assert_eq!(
        values(&v),
        vec![("todos".into(), vec![4, 6, 6, 6, 8, 8, 8, 8])]
    );
    let v = env.ok(bob, VIEW, json!({ "id": v1_id })).await;
    assert_eq!(
        values(&v),
        vec![("todos".into(), vec![2, 3, 3, 3, 4, 4, 4, 4])]
    );
    let v = env.ok(alice, VIEW, json!({ "id": v1_id })).await;
    let on: Vec<&str> = the_view(&v)["dashboards"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap())
        .collect();
    assert_eq!(on, ["Mine", "Everyone"]);
    // Grouped by repository: alice sees only the public one.
    let v = env.ok(admin, VIEW, json!({ "id": v3_id })).await;
    let mut labels: Vec<String> = values(&v).into_iter().map(|(l, _)| l).collect();
    labels.sort();
    assert_eq!(labels, [PRIVATE, PUBLIC]);
    let v = env.ok(alice, VIEW, json!({ "id": v3_id })).await;
    assert_eq!(
        values(&v),
        vec![(PUBLIC.into(), vec![2, 3, 3, 3, 4, 4, 4, 4])]
    );
    assert!(
        the_view(&v)["dataSeries"][0]["points"][0]["diffQuery"]
            .as_str()
            .unwrap()
            .contains("repo:^(gitlab\\.example\\.com/acme/web)$")
    );
    let remove = "mutation($view: ID!, $dashboard: ID!) {
        removeInsightViewFromDashboard(input: { insightViewId: $view, dashboardId: $dashboard }) {
            dashboard { views { nodes { id } } } } }";
    let v = env
        .ok(
            alice,
            remove,
            json!({ "view": v1_id, "dashboard": personal_id }),
        )
        .await;
    assert_eq!(
        v["removeInsightViewFromDashboard"]["dashboard"]["views"]["nodes"],
        json!([])
    );
    let v = env
        .ok(
            alice,
            "mutation($id: ID!, $users: [ID!]) { updateInsightsDashboard(id: $id,
                input: { title: \"Renamed\", grants: { users: $users } }) {
                dashboard { title grants { users } } } }",
            json!({ "id": personal_id, "users": [alice_id] }),
        )
        .await;
    assert_eq!(
        v["updateInsightsDashboard"]["dashboard"]["title"],
        "Renamed"
    );
    let (code, _) = env
        .err(
            alice,
            "mutation($id: ID!, $users: [ID!]) { updateInsightsDashboard(id: $id,
                input: { grants: { users: $users } }) { dashboard { title } } }",
            json!({ "id": personal_id, "users": [alice_id, bob_id] }),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    let (code, _) = env
        .err(
            alice,
            "mutation($id: ID!) { deleteInsightsDashboard(id: $id) { alwaysNil } }",
            json!({ "id": org_dash_id }),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");
    env.ok(
        bob,
        "mutation($id: ID!) { deleteInsightsDashboard(id: $id) { alwaysNil } }",
        json!({ "id": org_dash_id }),
    )
    .await;
    assert_eq!(titles(&env.ok(bob, list, json!({})).await), ["Everyone"]);

    // --- filters ---
    let v = env
        .ok(
            admin,
            VIEW,
            json!({ "id": v1_id, "filters": { "excludeRepoRegex": "secret" } }),
        )
        .await;
    assert_eq!(values(&v)[0].1, vec![2, 3, 3, 3, 4, 4, 4, 4]);
    assert_eq!(the_view(&v)["appliedFilters"]["excludeRepoRegex"], "secret");
    assert!(the_view(&v)["defaultFilters"]["excludeRepoRegex"].is_null());
    assert!(
        the_view(&v)["dataSeries"][0]["points"][0]["diffQuery"]
            .as_str()
            .unwrap()
            .starts_with("-repo:secret ")
    );
    let v = env
        .ok(
            alice,
            VIEW,
            json!({ "id": v1_id, "filters": { "includeRepoRegex": "secret" } }),
        )
        .await;
    assert_eq!(values(&v)[0].1, vec![0; 8]);
    let (code, _) = env
        .err(
            admin,
            VIEW,
            json!({ "id": v1_id, "filters": { "includeRepoRegex": "(" } }),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    // A query-defined search context.
    env.ok(
        admin,
        "mutation { createSearchContext(searchContext: { name: \"secretonly\",
            description: \"\", public: true, query: \"repo:secret$\" }, repositories: []) { id } }",
        json!({}),
    )
    .await;
    let v = env
        .ok(
            admin,
            VIEW,
            json!({ "id": v1_id, "filters": { "searchContexts": ["secretonly"] } }),
        )
        .await;
    assert_eq!(values(&v)[0].1, vec![2, 3, 3, 3, 4, 4, 4, 4]);
    assert!(
        the_view(&v)["dataSeries"][0]["points"][0]["diffQuery"]
            .as_str()
            .unwrap()
            .contains("context:secretonly")
    );
    let (code, _) = env
        .err(
            admin,
            VIEW,
            json!({ "id": v1_id, "filters": { "searchContexts": ["nope"] } }),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");

    // --- export ---
    let rows = |body: &[u8]| -> Vec<Vec<String>> {
        let (name, csv) = sourcegull_insights::export::unzip(body).expect("a zip");
        assert!(
            name.starts_with("TODOs-") && name.ends_with(".csv"),
            "{name}"
        );
        String::from_utf8(csv)
            .unwrap()
            .lines()
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    };
    let (status, body) = env.export(Some(alice), &v1_id).await;
    assert_eq!(status, StatusCode::OK);
    let r = rows(&body);
    assert_eq!(
        r[0],
        [
            "title",
            "label",
            "query",
            "recording_time",
            "repository",
            "value",
            "capture"
        ]
    );
    assert_eq!(r.len(), 1 + 8);
    assert!(
        r[1..]
            .iter()
            .all(|row| row[4] == PUBLIC && row[0] == "TODOs" && row[1] == "todos")
    );
    assert_eq!(r.last().unwrap()[5], "4");
    let (_, body) = env.export(Some(admin), &v1_id).await;
    let r = rows(&body);
    assert_eq!(r.len(), 1 + 16);
    assert!(r.iter().any(|row| row[4] == PRIVATE));

    // --- updates, sharing, new views ---
    let update = "mutation($id: ID!, $input: UpdateLineChartSearchInsightInput!) {
        updateLineChartSearchInsight(id: $id, input: $input) { view {
            defaultFilters { excludeRepoRegex }
            defaultSeriesDisplayOptions { limit sortOptions { mode direction } }
            dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition { seriesId query } }
            presentation { ... on LineChartInsightViewPresentation { title
                seriesPresentation { label } } } } } }";
    let controls = json!({ "filters": { "excludeRepoRegex": "secret" },
        "seriesDisplayOptions": { "limit": 5,
            "sortOptions": { "mode": "DATE_ADDED", "direction": "ASC" } } });
    // A new label keeps the series.
    let v = env
        .ok(
            bob,
            update,
            json!({ "id": v4_id, "input": {
                "dataSeries": [{ "seriesId": todo_series, "query": "TODO",
                    "options": { "label": "renamed" } }],
                "repositoryScope": all_repos(),
                "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
                "presentationOptions": { "title": "Mine (renamed)" },
                "viewControls": controls } }),
        )
        .await;
    let view = &v["updateLineChartSearchInsight"]["view"];
    assert_eq!(view["dataSeriesDefinitions"][0]["seriesId"], todo_series);
    assert_eq!(view["defaultFilters"]["excludeRepoRegex"], "secret");
    assert_eq!(
        view["defaultSeriesDisplayOptions"],
        json!({ "limit": 5, "sortOptions": { "mode": "DATE_ADDED", "direction": "ASC" } })
    );
    assert_eq!(
        view["presentation"]["seriesPresentation"][0]["label"],
        "renamed"
    );
    // Saved default filters apply when reading.
    let v = env.ok(bob, VIEW, json!({ "id": v4_id })).await;
    assert_eq!(the_view(&v)["appliedFilters"]["excludeRepoRegex"], "secret");
    // A changed query replaces the series; alice's view still holds the old one.
    let v = env
        .ok(
            bob,
            update,
            json!({ "id": v4_id, "input": {
                "dataSeries": [{ "seriesId": todo_series, "query": "TODO lang:go",
                    "options": { "label": "go" } },
                    { "query": "alice", "options": { "label": "names" } }],
                "repositoryScope": all_repos(),
                "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
                "presentationOptions": { "title": "Mine (go)" },
                "viewControls": controls } }),
        )
        .await;
    let defs = v["updateLineChartSearchInsight"]["view"]["dataSeriesDefinitions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(defs.len(), 2);
    assert_ne!(defs[0]["seriesId"], todo_series);
    assert_eq!(defs[0]["query"], "TODO lang:go");
    let todo_internal: i64 =
        sourcegull_db::sqlx::query_scalar("SELECT id FROM insight_series WHERE series_id = $1")
            .bind(&todo_series)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(store.get(todo_internal).await.unwrap().is_some());
    // The same series twice on one view is refused.
    let (code, _) = env
        .err(
            bob,
            update,
            json!({ "id": v4_id, "input": {
                "dataSeries": [{ "query": "x", "options": {} }, { "query": "x", "options": {} }],
                "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
                "presentationOptions": {}, "viewControls": controls } }),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    let saved = env
        .ok(
            alice,
            "mutation($input: SaveInsightAsNewViewInput!) { saveInsightAsNewView(input: $input) {
                view { id dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition {
                    seriesId } }
                    presentation { ... on LineChartInsightViewPresentation { title } } } } }",
            json!({ "input": { "insightViewId": v2_id, "options": { "title": "Owners copy" },
                "dashboard": global_id } }),
        )
        .await;
    let copy = &saved["saveInsightAsNewView"]["view"];
    assert_eq!(copy["presentation"]["title"], "Owners copy");
    let v = env.ok(alice, VIEW, json!({ "id": v2_id })).await;
    assert_eq!(
        copy["dataSeriesDefinitions"][0]["seriesId"],
        the_view(&v)["dataSeriesDefinitions"][0]["seriesId"]
    );

    // --- previews and scope checks ---
    let v = env
        .ok(
            alice,
            "query($input: SearchInsightLivePreviewInput!) { searchInsightLivePreview(input: $input) {
                label points { value diffQuery } } }",
            json!({ "input": { "query": "TODO\\((\\w+)\\)", "label": "x",
                "repositoryScope": { "repositories": [] },
                "timeScope": { "stepInterval": { "unit": "DAY", "value": 1 } },
                "generatedFromCaptureGroups": true } }),
        )
        .await;
    let mut preview: Vec<(String, i64)> = v["searchInsightLivePreview"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["label"].as_str().unwrap().to_owned(),
                s["points"].as_array().unwrap().last().unwrap()["value"]
                    .as_f64()
                    .unwrap() as i64,
            )
        })
        .collect();
    preview.sort();
    // Alice's preview counts only the public repository.
    assert_eq!(preview, [("alice".into(), 2), ("bob".into(), 0)]);
    let v = env
        .ok(
            admin,
            "query($input: SearchInsightPreviewInput!) { searchInsightPreview(input: $input) {
                label points { value } } }",
            json!({ "input": { "repositoryScope": { "repositories": [],
                    "repositoryCriteria": "repo:secret$" },
                "timeScope": { "stepInterval": { "unit": "WEEK", "value": 1 } },
                "series": [{ "query": "TODO", "label": "todo",
                    "generatedFromCaptureGroups": false }] } }),
        )
        .await;
    let s = &v["searchInsightPreview"][0];
    assert_eq!(s["label"], "todo");
    assert_eq!(
        s["points"].as_array().unwrap().last().unwrap()["value"],
        4.0
    );
    let v = env
        .ok(
            alice,
            "{ ok: validateScopedInsightQuery(query: \"repo:acme\") { isValid invalidReason }
               bad: validateScopedInsightQuery(query: \"repo:acme TODO\") { isValid invalidReason }
               n: previewRepositoriesFromQuery(query: \"repo:acme\") { numberOfRepositories } }",
            json!({}),
        )
        .await;
    assert_eq!(v["ok"], json!({ "isValid": true, "invalidReason": null }));
    assert_eq!(v["bad"]["isValid"], false);
    assert!(v["bad"]["invalidReason"].is_string());
    assert_eq!(v["n"]["numberOfRepositories"], 1);
    let v = env
        .ok(
            admin,
            "{ previewRepositoriesFromQuery(query: \"repo:acme\") { numberOfRepositories } }",
            json!({}),
        )
        .await;
    assert_eq!(v["previewRepositoriesFromQuery"]["numberOfRepositories"], 2);

    // --- language statistics ---
    let v = env.ok(alice, VIEW, json!({ "id": pie_id })).await;
    let view = the_view(&v);
    assert_eq!(
        view["presentation"]["__typename"],
        "PieChartInsightViewPresentation"
    );
    assert_eq!(view["presentation"]["otherThreshold"], 0.03);
    assert_eq!(view["dataSeries"], json!([]));
    assert_eq!(
        view["languageStatistics"],
        json!([
            { "name": "Go", "totalLines": 2, "totalBytes": 28.0 },
            { "name": "Markdown", "totalLines": 1, "totalBytes": 10.0 },
        ])
    );
    // The creation page's preview shows the same statistics; unreadable repositories are
    // left out as if they did not exist.
    let v = env
        .ok(
            alice,
            "query($r: [String!]!) { previewLanguageStatistics(repositories: $r) { name totalLines totalBytes } }",
            json!({ "r": [PUBLIC] }),
        )
        .await;
    assert_eq!(v["previewLanguageStatistics"], view["languageStatistics"]);
    let v = env
        .ok(
            alice,
            "query($r: [String!]!) { previewLanguageStatistics(repositories: $r) { name } }",
            json!({ "r": [PRIVATE, "gitlab.example.com/acme/missing"] }),
        )
        .await;
    assert_eq!(v["previewLanguageStatistics"], json!([]));
    // The indexed head is read from the index, the unindexed one counted from its tree, with
    // the same result.
    let indexed = sourcegull_insights::langstats::language_stats(
        &node.clients,
        &[(public.get(), PUBLIC.to_owned())],
    )
    .await
    .unwrap();
    let counted = sourcegull_insights::langstats::language_stats(
        &node.clients,
        &[(private.get(), PRIVATE.to_owned())],
    )
    .await
    .unwrap();
    assert!(indexed.indexed);
    assert!(!counted.indexed);
    assert_eq!(indexed.languages, counted.languages);
    // Alice cannot chart the private repository's languages.
    let (code, _) = env
        .err(
            alice,
            "mutation($id: ID!) { updatePieChartSearchInsight(id: $id, input: { query: \"\",
                repositoryScope: { repositories: [\"gitlab.example.com/acme/secret\"] },
                presentationOptions: { title: \"x\", otherThreshold: 0.1 } }) { view { id } } }",
            json!({ "id": pie_id }),
        )
        .await;
    assert_eq!(code, "VALIDATION");

    // --- recording ---
    // A push to the public repository; the private one does not change.
    src.commit(Utc::now(), &[("b.go", None)]);
    node.fetch_repo(public, PUBLIC).await;
    set_last_changed(&db, public, Utc::now()).await;
    let before = store.repo_states(todo_internal).await.unwrap();
    let report = enqueuer
        .pass(Utc::now() + day + TimeDelta::minutes(5))
        .await
        .unwrap();
    assert!(report.recordings_queued >= 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let v = env.ok(admin, VIEW, json!({ "id": v1_id })).await;
        let vals = values(&v)[0].1.clone();
        if vals.len() == 9 {
            assert_eq!(vals[8], 3 + 4);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "recording did not finish: {v}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let after = store.repo_states(todo_internal).await.unwrap();
    let state_of = |states: &[sourcegull_insights::RepoState], r: RepoId| {
        states
            .iter()
            .find(|s| s.repo_id == r.get())
            .unwrap()
            .clone()
    };
    // The private repository was carried forward without a search.
    assert_eq!(
        state_of(&after, private).checked_at,
        state_of(&before, private).checked_at
    );
    assert_ne!(
        state_of(&after, public).commit,
        state_of(&before, public).commit
    );
    cancel.cancel();
    running.await.unwrap();

    // --- deleting ---
    // Bob sees V1 through the global dashboard, so he may delete it (seeing is editing).
    env.ok(
        bob,
        "mutation($id: ID!) { deleteInsightView(id: $id) { alwaysNil } }",
        json!({ "id": v1_id }),
    )
    .await;
    // No view uses the TODO series any more: it is deleted, and the janitor removes it.
    assert!(store.get(todo_internal).await.unwrap().is_none());
    let v = env.ok(alice, VIEW, json!({ "id": v1_id })).await;
    assert_eq!(v["insightViews"]["nodes"], json!([]));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let removed = sourcegull_insights::janitor::run_once(&store, 30)
        .await
        .unwrap();
    assert!(removed.series >= 1);
    let left: i64 = sourcegull_db::sqlx::query_scalar(
        "SELECT count(*) FROM insight_series_points WHERE series_id = $1",
    )
    .bind(todo_internal)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(left, 0);
}
