//! Code Ownership in search through `GET /.api/search/stream`, against the test
//! Postgres and a real repo node with a repository holding `.gitlab/CODEOWNERS`:
//! `file:has.owner()` (resolved handles, emails and teams, assigned owners, negation, an
//! ingested file replacing the repository's) indexed and unindexed,
//! `select:file.owners` streaming resolved `person` and `team` results, and
//! `file:has.contributor()` matching the authors of the commits that changed a file.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_own::store::{Assignee, NewTeam, OwnStore};
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tower::ServiceExt;

const REPO: &str = "gitlab.example.com/acme/owned";

const CODEOWNERS: &str = "\
* @nobody-default
*.go @ali-gl
/docs/ docs@example.com @acme/backend
[Frontend]
web/ @acme/frontend
";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    db: Db,
    node: RepoNode,
    id: RepoId,
    router: Router,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let src = SourceRepo::new();
    src.commit(
        "initial",
        &[
            (".gitlab/CODEOWNERS", CODEOWNERS),
            ("src/main.go", "package main\n"),
            ("docs/intro.md", "intro package\n"),
            ("docs/guide.md", "guide\n"),
            ("web/app.js", "app package\n"),
            ("README.md", "readme package\n"),
        ],
    );
    // Bob changes two files.
    src.write("src/main.go", "package main\n\nfunc main() {}\n");
    src.write("docs/guide.md", "guide v2\n");
    src.git(&["add", "-A"]);
    src.git(&[
        "commit",
        "-q",
        "--author=Bob Builder <bob@example.com>",
        "-m",
        "bob",
    ]);
    let node = RepoNode::start(src.remote()).await;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(REPO)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let id = RepoId(id);
    node.clone_repo(id, REPO).await;

    // alice is `ali-gl` on GitLab; carol has the docs address; acme/backend is a team.
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            display_name: Some("Alice A.".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let carol = db
        .users()
        .create(&NewUser {
            username: "carol".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    sqlx::query(
        "INSERT INTO user_external_accounts (user_id, service_type, service_id, account_id, \
         account_data) VALUES ($1, 'gitlab', 'https://gitlab.example.com/', '7', \
         '{\"username\": \"ali-gl\"}')",
    )
    .bind(alice.get())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_emails (user_id, email, verified_at) VALUES ($1, 'docs@example.com', now())",
    )
    .bind(carol.get())
    .execute(db.pool())
    .await
    .unwrap();
    let own = OwnStore::new(&db);
    own.create_team(&NewTeam {
        name: "acme/backend".into(),
        display_name: Some("Backend".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    own.assign(Assignee::User(carol), id, "web", None)
        .await
        .unwrap();

    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_public: true,
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone());
    Some(Env {
        _t: t,
        _src: src,
        db,
        node,
        id,
        router: public_router(state),
    })
}

fn encode(q: &str) -> String {
    let mut out = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The SSE events of a search: (event name, JSON data).
async fn search(router: &Router, q: &str) -> Vec<(String, Value)> {
    let uri = format!("/.api/search/stream?q={}", encode(q));
    let resp = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&body)
        .split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_owned());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            event.map(|e| (e, serde_json::from_str(&data).unwrap_or(Value::Null)))
        })
        .collect()
}

fn matches(events: &[(String, Value)]) -> Vec<Value> {
    events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| d.as_array().cloned().unwrap_or_default())
        .collect()
}

/// The matched paths of a search, sorted.
async fn paths(router: &Router, q: &str) -> Vec<String> {
    let events = search(router, q).await;
    assert!(
        !events.iter().any(|(e, _)| e == "alert" || e == "error"),
        "{q}: {events:?}"
    );
    let mut out: Vec<String> = matches(&events)
        .iter()
        .filter_map(|m| m["path"].as_str().map(str::to_owned))
        .collect();
    out.sort();
    out.dedup();
    out
}

async fn check_has_owner(env: &Env, extra: &str) {
    let q = |s: &str| format!("repo:owned {extra} {s}");
    let r = &env.router;
    // A GitLab handle resolves to alice; her username finds the same files.
    assert_eq!(
        paths(r, &q("file:has.owner(@alice)")).await,
        vec!["src/main.go"]
    );
    assert_eq!(
        paths(r, &q("file:has.owner(ali-gl)")).await,
        vec!["src/main.go"]
    );
    // carol: through her verified email in CODEOWNERS, and assigned to `web`.
    assert_eq!(
        paths(r, &q("file:has.owner(@carol)")).await,
        vec!["docs/guide.md", "docs/intro.md", "web/app.js"]
    );
    assert_eq!(
        paths(r, &q("file:has.owner(@acme/backend)")).await,
        vec!["docs/guide.md", "docs/intro.md"]
    );
    // Unresolved handles match as written.
    assert_eq!(
        paths(r, &q("file:has.owner(@acme/frontend)")).await,
        vec!["web/app.js"]
    );
    // Last match wins within the default section.
    assert_eq!(
        paths(r, &q("-file:has.owner(@nobody-default)")).await,
        vec!["docs/guide.md", "docs/intro.md", "src/main.go"]
    );
    // With a content pattern.
    assert_eq!(
        paths(r, &q("file:has.owner(@carol) package")).await,
        vec!["docs/intro.md", "web/app.js"]
    );
    assert!(paths(r, &q("file:has.owner(@ghost)")).await.is_empty());
    assert!(paths(r, &q("-file:has.owner()")).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn has_owner_unindexed_and_indexed() {
    let Some(env) = setup().await else { return };
    check_has_owner(&env, "index:no").await;
    env.node.index_repo(env.id, REPO, &[]);
    check_has_owner(&env, "index:only").await;

    // An ingested CODEOWNERS file replaces the repository's.
    let own = OwnStore::new(&env.db);
    own.add_codeowners(env.id, "README.md @alice\n")
        .await
        .unwrap();
    for extra in ["index:no", "index:only"] {
        assert_eq!(
            paths(
                &env.router,
                &format!("repo:owned {extra} file:has.owner(@alice)")
            )
            .await,
            vec!["README.md"]
        );
        // Without CODEOWNERS owners, only README.md and carol's `web` are owned.
        assert_eq!(
            paths(
                &env.router,
                &format!("repo:owned {extra} -file:has.owner()")
            )
            .await,
            vec![
                ".gitlab/CODEOWNERS",
                "docs/guide.md",
                "docs/intro.md",
                "src/main.go"
            ]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_file_owners_streams_people_and_teams() {
    let Some(env) = setup().await else { return };
    for extra in ["index:no", ""] {
        if extra.is_empty() {
            env.node.index_repo(env.id, REPO, &[]);
        }
        let events = search(
            &env.router,
            &format!("repo:owned {extra} select:file.owners"),
        )
        .await;
        assert!(
            !events.iter().any(|(e, _)| e == "alert" || e == "error"),
            "{events:?}"
        );
        let mut owners: Vec<(String, String)> = matches(&events)
            .iter()
            .map(|m| {
                let t = m["type"].as_str().unwrap().to_owned();
                let who = match t.as_str() {
                    "person" => m["user"]["username"]
                        .as_str()
                        .map(|u| format!("user:{u}"))
                        .unwrap_or_else(|| {
                            format!(
                                "{}{}",
                                m["handle"].as_str().unwrap(),
                                m["email"].as_str().unwrap()
                            )
                        }),
                    _ => format!(
                        "{}|{}",
                        m["handle"].as_str().unwrap(),
                        m["displayName"].as_str().unwrap()
                    ),
                };
                (t, who)
            })
            .collect();
        owners.sort();
        // carol is listed once although reached through her email and an assignment.
        assert_eq!(
            owners,
            vec![
                ("person".to_owned(), "nobody-default".to_owned()),
                ("person".to_owned(), "user:alice".to_owned()),
                ("person".to_owned(), "user:carol".to_owned()),
                ("team".to_owned(), "acme/backend|Backend".to_owned()),
                ("team".to_owned(), "acme/frontend|".to_owned()),
            ],
            "{extra}"
        );
        let alice = matches(&events)
            .into_iter()
            .find(|m| m["user"]["username"] == "alice")
            .unwrap();
        assert_eq!(alice["handle"], "ali-gl");
        assert_eq!(alice["user"]["displayName"], "Alice A.");
    }

    // Owners of the files a filter keeps: carol (by email) and acme/backend.
    let events = search(&env.router, "repo:owned file:^docs/ select:file.owners").await;
    assert_eq!(matches(&events).len(), 2, "{events:?}");
}

async fn check_has_contributor(env: &Env, extra: &str) {
    let q = |s: &str| format!("repo:owned {extra} {s}");
    let r = &env.router;
    let all = vec![
        ".gitlab/CODEOWNERS",
        "README.md",
        "docs/guide.md",
        "docs/intro.md",
        "src/main.go",
        "web/app.js",
    ];
    // Names and emails, as case-insensitive regular expressions.
    assert_eq!(
        paths(r, &q("file:has.contributor(bob)")).await,
        vec!["docs/guide.md", "src/main.go"]
    );
    assert_eq!(
        paths(r, &q("file:has.contributor(^bob.builder$)")).await,
        vec!["docs/guide.md", "src/main.go"]
    );
    assert_eq!(
        paths(r, &q(r"file:has.contributor(ann@example\.com)")).await,
        all
    );
    assert_eq!(
        paths(r, &q("-file:has.contributor(Bob)")).await,
        vec![
            ".gitlab/CODEOWNERS",
            "README.md",
            "docs/intro.md",
            "web/app.js"
        ]
    );
    // Terms combine with each other, owner terms and patterns.
    assert_eq!(
        paths(
            r,
            &q("file:has.contributor(ann) file:has.contributor(bob) file:has.owner(@carol)")
        )
        .await,
        vec!["docs/guide.md"]
    );
    assert_eq!(
        paths(r, &q("file:has.contributor(bob) func")).await,
        vec!["src/main.go"]
    );
    assert!(
        paths(r, &q("file:has.contributor(nobody)"))
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn has_contributor_unindexed_and_indexed() {
    let Some(env) = setup().await else { return };
    check_has_contributor(&env, "index:no").await;
    env.node.index_repo(env.id, REPO, &[]);
    check_has_contributor(&env, "index:only").await;

    // An invalid pattern is reported, and commit searches name the predicate in an alert.
    let events = search(&env.router, "repo:owned file:has.contributor(a[)").await;
    assert!(
        events.iter().any(|(e, _)| e == "alert" || e == "error"),
        "{events:?}"
    );
    assert!(matches(&events).is_empty());
    let events = search(
        &env.router,
        "repo:owned type:commit file:has.contributor(bob)",
    )
    .await;
    let alert = events.iter().find(|(e, _)| e == "alert").unwrap();
    assert_eq!(
        alert.1["description"],
        "`file:has.contributor()` in commit and diff searches is not supported yet."
    );
}
