//! Code Ownership signals and analytics over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and an in-process repo node serving a repository with
//! history by several authors: signal configurations (site admins only), recent
//! contributors ranked by commits after the CODEOWNERS owners, recent viewers from
//! `logEvent(ViewBlob)`, exclusions, and ownership coverage (`instanceOwnershipStats`,
//! `GitTree.ownershipStats`, `ownershipStatsByRepository`) computed by the indexers.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_own::jobs::{Indexer, Outcome, OwnJob};
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const REPO: &str = "gitlab.example.com/acme/web";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    auth: Authenticator,
    schema: SourcegullSchema,
    indexer: Indexer,
    repo: RepoId,
    admin: UserId,
    alice: UserId,
}

/// Commits by `author` (`Name <email>`) changing `files`.
fn commit(src: &SourceRepo, author: &str, files: &[(&str, &str)]) {
    for (p, c) in files {
        src.write(p, c);
    }
    src.git(&["add", "-A"]);
    src.git(&["commit", "-q", "--author", author, "-m", "change"]);
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, SiteConfig::default());
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
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    // Alice commits as alice@example.com.
    sqlx::query(
        "INSERT INTO user_emails (user_id, email, verified_at) \
         VALUES ($1, 'alice@example.com', now())",
    )
    .bind(alice.get())
    .execute(db.pool())
    .await
    .unwrap();

    let src = SourceRepo::new();
    src.commit(
        "initial",
        &[
            ("CODEOWNERS", "*.go @admin\n"),
            ("src/main.go", "package main\n"),
            ("src/util.go", "package main\n"),
            ("docs/intro.md", "intro\n"),
            ("README.md", "readme\n"),
        ],
    );
    let alice_id = "Alice Liddell <alice@example.com>";
    let bob = "Bob Builder <bob@example.com>";
    commit(&src, bob, &[("src/main.go", "package main // 1\n")]);
    commit(&src, bob, &[("src/main.go", "package main // 2\n")]);
    commit(
        &src,
        bob,
        &[
            ("src/main.go", "package main // 3\n"),
            ("src/util.go", "package main // 3\n"),
        ],
    );
    commit(&src, alice_id, &[("src/main.go", "package main // 4\n")]);
    commit(&src, alice_id, &[("docs/intro.md", "intro 2\n")]);

    let node = RepoNode::start(src.remote()).await;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(REPO)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let repo = RepoId(id);
    node.clone_repo(repo, REPO).await;
    let authz = Checker::new(db.clone(), config.clone());
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config,
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz,
        app: None,
    });
    let indexer = Indexer::new(db.clone(), node.clients.clone());
    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        auth,
        schema,
        indexer,
        repo,
        admin,
        alice,
    })
}

impl Env {
    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
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

    async fn error_code(&self, user: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(user, query, vars).await;
        let e = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        e.extensions
            .as_ref()
            .and_then(|x| x.get("code"))
            .map(|c| c.to_string().trim_matches('"').to_owned())
            .unwrap_or_default()
    }

    /// Runs an indexer job a day after the history was written.
    async fn index(&self, job_type: &str) -> Outcome {
        // SourceRepo's clock starts at 1,700,000,000.
        let now = Utc.timestamp_opt(1_700_000_000 + 86_400, 0).unwrap();
        let repo_id = (job_type != "recent-views").then_some(self.repo.get());
        self.indexer
            .run(
                &OwnJob {
                    repo_id,
                    job_type: job_type.into(),
                },
                now,
            )
            .await
            .unwrap()
    }

    async fn configure(&self, configs: Value) -> Value {
        self.ok(
            self.admin,
            "mutation($c: [OwnSignalConfigurationUpdate!]!) {
                updateOwnSignalConfigurations(input: { configs: $c }) {
                    name isEnabled excludedRepoPatterns } }",
            json!({ "c": configs }),
        )
        .await
    }

    /// `(owner, [reason summaries])` of a path at HEAD.
    async fn owners(&self, path: &str, dir: bool) -> Vec<(String, Vec<String>)> {
        let target = if dir { "tree" } else { "blob" };
        let q = format!(
            r#"query($name: String!, $path: String!) {{ repository(name: $name) {{
                commit(rev: "HEAD") {{ {target}(path: $path) {{ ownership {{
                    totalCount totalOwners
                    nodes {{
                        owner {{
                            __typename
                            ... on Person {{ displayName email user {{ username }} }}
                        }}
                        reasons {{
                            __typename
                            ... on RecentContributorOwnershipSignal {{
                                title description contributionCount }}
                            ... on RecentViewOwnershipSignal {{ title viewCount }}
                        }}
                    }}
                }} }} }} }} }}"#
        );
        let d = self
            .ok(self.alice, &q, json!({ "name": REPO, "path": path }))
            .await;
        d["repository"]["commit"][target]["ownership"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                let o = &n["owner"];
                let who = match o["user"]["username"].as_str() {
                    Some(u) => format!("user:{u}"),
                    None => format!(
                        "person:{} <{}>",
                        o["displayName"].as_str().unwrap(),
                        o["email"].as_str().unwrap()
                    ),
                };
                let reasons = n["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| match r["__typename"].as_str().unwrap() {
                        "RecentContributorOwnershipSignal" => {
                            assert_eq!(r["title"], "recent contributor");
                            format!("contributor({})", r["contributionCount"])
                        }
                        "RecentViewOwnershipSignal" => format!("view({})", r["viewCount"]),
                        other => other.to_owned(),
                    })
                    .collect();
                (who, reasons)
            })
            .collect()
    }
}

fn owners(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    list.iter()
        .map(|(w, r)| ((*w).to_owned(), r.iter().map(|s| (*s).to_owned()).collect()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent_contributors_and_views() {
    let Some(env) = setup().await else { return };
    const CONFIGS: &str =
        "{ ownSignalConfigurations { name description isEnabled excludedRepoPatterns } }";
    // Site admins only; all off by default.
    assert_eq!(
        env.error_code(env.alice, CONFIGS, json!({})).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, CONFIGS, json!({})).await;
    let names: Vec<&str> = d["ownSignalConfigurations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            assert_eq!(c["isEnabled"], false);
            c["name"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        names,
        vec!["recent-contributors", "recent-views", "analytics"]
    );

    // Disabled: the indexers do nothing and ownership lists only owners.
    assert_eq!(env.index("recent-contributors").await, Outcome::Skipped);
    assert_eq!(
        env.owners("src/main.go", false).await,
        owners(&[("user:admin", &["CodeownersFileEntry"])])
    );

    env.configure(json!([
        { "name": "recent-contributors", "enabled": true },
        { "name": "recent-views", "enabled": true },
    ]))
    .await;
    assert!(matches!(
        env.index("recent-contributors").await,
        Outcome::Contributions(s) if s.added == 6 && s.removed == 0
    ));
    // Contributors rank after the CODEOWNERS owner, most commits first; alice's commit
    // email resolves to her account, Bob stays a person.
    assert_eq!(
        env.owners("src/main.go", false).await,
        owners(&[
            ("user:admin", &["CodeownersFileEntry"]),
            ("person:Bob Builder <bob@example.com>", &["contributor(3)"]),
            ("user:alice", &["contributor(1)"]),
            ("person:Ann Author <ann@example.com>", &["contributor(1)"]),
        ])
    );
    // Directories count commits below them, once per commit.
    assert_eq!(
        env.owners("src", true).await,
        owners(&[
            ("person:Bob Builder <bob@example.com>", &["contributor(3)"]),
            ("user:alice", &["contributor(1)"]),
            ("person:Ann Author <ann@example.com>", &["contributor(1)"]),
        ])
    );
    assert_eq!(
        env.owners("", true).await,
        owners(&[
            ("person:Bob Builder <bob@example.com>", &["contributor(3)"]),
            ("user:alice", &["contributor(2)"]),
            ("person:Ann Author <ann@example.com>", &["contributor(1)"]),
        ])
    );
    // Running again changes nothing.
    assert!(matches!(
        env.index("recent-contributors").await,
        Outcome::Contributions(s) if s.added == 0 && s.removed == 0
    ));

    // File views logged by the file page.
    const LOG: &str = r#"mutation($a: String!) { logEvent(event: "ViewBlob",
        userCookieID: "c", url: "http://x/", source: WEB, argument: $a) { alwaysNil } }"#;
    let arg = json!({ "repoName": REPO, "filePath": "docs/intro.md" }).to_string();
    for user in [env.alice, env.admin, env.alice] {
        env.ok(user, LOG, json!({ "a": arg })).await;
    }
    // A view of a repository alice cannot read (here: none of that name) is dropped.
    let hidden = json!({ "repoName": "gitlab.example.com/acme/hidden", "filePath": "a" });
    env.ok(env.alice, LOG, json!({ "a": hidden.to_string() }))
        .await;
    let views: i64 = sqlx::query_scalar("SELECT count(*) FROM event_logs WHERE name = 'ViewBlob'")
        .fetch_one(env.indexer_pool())
        .await
        .unwrap();
    assert_eq!(views, 3);
    let bad = env.run(env.alice, LOG, json!({ "a": "[1]" })).await;
    assert!(!bad.errors.is_empty());
    env.ok(
        env.alice,
        r#"mutation { logEvents(events: [{ event: "SearchSubmitted", userCookieID: "c",
            url: "http://x/", source: WEB }]) { alwaysNil } }"#,
        json!({}),
    )
    .await;
    assert!(matches!(env.index("recent-views").await, Outcome::Views(_)));
    assert_eq!(
        env.owners("docs/intro.md", false).await,
        owners(&[
            ("user:alice", &["contributor(1)", "view(2)"]),
            ("person:Ann Author <ann@example.com>", &["contributor(1)"]),
            ("user:admin", &["view(1)"]),
        ])
    );

    // `reasons` filters by kind.
    let d = env
        .ok(
            env.alice,
            r#"query($name: String!) { repository(name: $name) { commit(rev: "HEAD") {
                blob(path: "docs/intro.md") {
                    ownership(reasons: [RECENT_VIEW_OWNERSHIP_SIGNAL]) {
                        totalCount totalOwners } } } } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let conn = &d["repository"]["commit"]["blob"]["ownership"];
    assert_eq!(
        (conn["totalCount"].as_i64(), conn["totalOwners"].as_i64()),
        (Some(2), Some(0))
    );

    // Excluding the repository hides the signal, and the next run clears its data.
    let d = env
        .configure(json!([{ "name": "recent-contributors", "enabled": true,
            "excludedRepoPatterns": ["gitlab.example.com/acme/*"] }]))
        .await;
    assert_eq!(
        d["updateOwnSignalConfigurations"][0]["excludedRepoPatterns"],
        json!(["gitlab.example.com/acme/*"])
    );
    assert_eq!(
        env.owners("src/util.go", false).await,
        owners(&[("user:admin", &["CodeownersFileEntry"])])
    );
    assert_eq!(env.index("recent-contributors").await, Outcome::Skipped);
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM own_aggregate_recent_contribution WHERE repo_id = $1",
    )
    .bind(env.repo.get())
    .fetch_one(env.indexer_pool())
    .await
    .unwrap();
    assert_eq!(left, 0);
    // Unknown signals are rejected.
    let code = env
        .run(
            env.admin,
            "mutation { updateOwnSignalConfigurations(input: { configs: [
                { name: \"nope\", enabled: true }] }) { name } }",
            json!({}),
        )
        .await;
    assert!(!code.errors.is_empty());
}

impl Env {
    fn indexer_pool(&self) -> &sqlx::PgPool {
        &self._t.pool
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_analytics() {
    let Some(env) = setup().await else { return };
    const STATS: &str = "totalFiles totalCodeownedFiles totalOwnedFiles \
        totalAssignedOwnershipFiles updatedAt";
    let instance = format!("{{ instanceOwnershipStats {{ {STATS} }} }}");
    assert_eq!(
        env.error_code(env.alice, &instance, json!({})).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, &instance, json!({})).await;
    assert_eq!(d["instanceOwnershipStats"]["totalFiles"], 0);
    assert_eq!(d["instanceOwnershipStats"]["updatedAt"], Value::Null);

    // Off: nothing computed.
    assert_eq!(env.index("analytics").await, Outcome::Skipped);
    env.configure(json!([{ "name": "analytics", "enabled": true }]))
        .await;
    // alice owns `docs` by assignment.
    sourcegull_own::store::OwnStore::new(&Db::from_pool(env._t.pool.clone()))
        .assign(
            sourcegull_own::store::Assignee::User(env.alice),
            env.repo,
            "docs",
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        env.index("analytics").await,
        Outcome::Analytics {
            dirs: 3,
            files: 5,
            truncated: false
        }
    );
    let d = env.ok(env.admin, &instance, json!({})).await;
    let s = &d["instanceOwnershipStats"];
    assert_eq!(
        (
            s["totalFiles"].as_i64(),
            s["totalCodeownedFiles"].as_i64(),
            s["totalAssignedOwnershipFiles"].as_i64(),
            s["totalOwnedFiles"].as_i64()
        ),
        (Some(5), Some(2), Some(1), Some(3))
    );
    assert!(s["updatedAt"].is_string());

    let tree = format!(
        r#"query($name: String!, $path: String!) {{ repository(name: $name) {{
            commit(rev: "HEAD") {{ tree(path: $path) {{ ownershipStats {{ {STATS} }} }} }} }} }}"#
    );
    let d = env
        .ok(env.alice, &tree, json!({ "name": REPO, "path": "src" }))
        .await;
    let s = &d["repository"]["commit"]["tree"]["ownershipStats"];
    assert_eq!(
        (s["totalFiles"].as_i64(), s["totalOwnedFiles"].as_i64()),
        (Some(2), Some(2))
    );
    let d = env
        .ok(env.alice, &tree, json!({ "name": REPO, "path": "docs/" }))
        .await;
    let s = &d["repository"]["commit"]["tree"]["ownershipStats"];
    assert_eq!(
        (
            s["totalAssignedOwnershipFiles"].as_i64(),
            s["totalCodeownedFiles"].as_i64()
        ),
        (Some(1), Some(0))
    );

    let by_repo = format!(
        "{{ ownershipStatsByRepository(first: 10) {{ totalCount nodes {{ name
            repository {{ name }} stats {{ {STATS} }} }} }} }}"
    );
    assert_eq!(
        env.error_code(env.alice, &by_repo, json!({})).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, &by_repo, json!({})).await;
    let conn = &d["ownershipStatsByRepository"];
    assert_eq!(conn["totalCount"], 1);
    assert_eq!(conn["nodes"][0]["repository"]["name"], REPO);
    assert_eq!(conn["nodes"][0]["stats"]["totalOwnedFiles"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_queues_enabled_signals() {
    use sourcegull_own::jobs::{SchedulerConfig, schedule_pass};
    let Some(env) = setup().await else { return };
    let db = Db::from_pool(env._t.pool.clone());
    let now = Utc::now();
    let config = SchedulerConfig::default();
    let jobs = || async {
        let rows: Vec<(Option<i32>, String)> = sqlx::query_as(
            "SELECT repo_id, job_type FROM own_background_jobs WHERE state = 'queued' \
             ORDER BY job_type",
        )
        .fetch_all(env.indexer_pool())
        .await
        .unwrap();
        rows
    };
    sqlx::query("INSERT INTO gitserver_repos (repo_id, clone_status) VALUES ($1, 'cloned')")
        .bind(env.repo.get())
        .execute(env.indexer_pool())
        .await
        .unwrap();
    // Nothing is enabled.
    assert_eq!(schedule_pass(&db, &config, now, true).await.unwrap(), 0);

    env.configure(json!([
        { "name": "recent-contributors", "enabled": true },
        { "name": "recent-views", "enabled": true },
        { "name": "analytics", "enabled": true },
    ]))
    .await;
    // First runs of the cloned repository and the views job; queued once.
    assert_eq!(schedule_pass(&db, &config, now, true).await.unwrap(), 3);
    assert_eq!(schedule_pass(&db, &config, now, true).await.unwrap(), 0);
    let r = Some(env.repo.get());
    assert_eq!(
        jobs().await,
        vec![
            (r, "analytics".to_owned()),
            (r, "recent-contributors".to_owned()),
            (None, "recent-views".to_owned()),
        ]
    );

    // Indexed today: queued again only when refs move.
    sqlx::query("DELETE FROM own_background_jobs")
        .execute(env.indexer_pool())
        .await
        .unwrap();
    for t in ["recent-contributors", "analytics"] {
        sqlx::query(
            "INSERT INTO own_repo_index_state (repo_id, job_type, indexed_at) VALUES ($1, $2, $3)",
        )
        .bind(env.repo.get())
        .bind(t)
        .bind(now)
        .execute(env.indexer_pool())
        .await
        .unwrap();
    }
    assert_eq!(schedule_pass(&db, &config, now, false).await.unwrap(), 0);
    db.ref_events()
        .record(
            env.repo,
            &[sourcegull_db::RefUpdate {
                name: "refs/heads/main".into(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
            }],
            sourcegull_db::RefEventSource::Fetch,
        )
        .await
        .unwrap();
    // Excluded from analytics: only contributors are queued.
    env.configure(json!([{ "name": "analytics", "enabled": true,
        "excludedRepoPatterns": ["*acme*"] }]))
        .await;
    assert_eq!(schedule_pass(&db, &config, now, false).await.unwrap(), 1);
    assert_eq!(jobs().await, vec![(r, "recent-contributors".to_owned())]);
    // The day after, the refresh is due again.
    sqlx::query("DELETE FROM own_background_jobs")
        .execute(env.indexer_pool())
        .await
        .unwrap();
    let tomorrow = now + chrono::TimeDelta::hours(25);
    assert_eq!(
        schedule_pass(&db, &config, tomorrow, false).await.unwrap(),
        1
    );
}
