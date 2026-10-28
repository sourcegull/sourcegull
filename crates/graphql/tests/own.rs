//! Code Ownership over GraphQL against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`)
//! and an in-process repo node serving a repository with `.gitlab/CODEOWNERS`: ranked owners
//! of files, directories and the repository with their reasons; assignments of users and
//! teams (`OWNERSHIP#ASSIGN`, which `USER` lacks by default); ingested CODEOWNERS files that
//! replace the repository's (site admins only); and the teams API.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const REPO: &str = "gitlab.example.com/acme/web";

const CODEOWNERS: &str = "\
# Default owners
* @admin
[Go]
*.go @ali-gl @acme/frontend
";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    auth: Authenticator,
    schema: SourcegullSchema,
    repo: RepoId,
    admin: UserId,
    alice: UserId,
    bob: UserId,
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
    // alice is `ali-gl` on GitLab.
    sourcegull_db::sqlx::query(
        "INSERT INTO user_external_accounts (user_id, service_type, service_id, account_id, \
         account_data) VALUES ($1, 'gitlab', 'https://gitlab.example.com/', '7', \
         '{\"username\": \"ali-gl\"}')",
    )
    .bind(users[0].get())
    .execute(db.pool())
    .await
    .unwrap();
    let src = SourceRepo::new();
    src.commit(
        "initial",
        &[
            (".gitlab/CODEOWNERS", CODEOWNERS),
            ("src/main.go", "package main\n"),
            ("src/README.md", "readme\n"),
        ],
    );
    let node = RepoNode::start(src.remote()).await;
    let id: i32 = sourcegull_db::sqlx::query_scalar(
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
    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        auth,
        schema,
        repo,
        admin,
        alice: users[0],
        bob: users[1],
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

    fn repo_id(&self) -> String {
        marshal_id("Repository", i64::from(self.repo.get()))
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
                            ... on Person {{ name email user {{ username }} }}
                            ... on Team {{ name external }}
                        }}
                        reasons {{
                            __typename
                            ... on CodeownersFileEntry {{
                                ruleLineMatch section pattern
                                codeownersFile {{ __typename path }}
                            }}
                            ... on AssignedOwner {{ isDirectMatch }}
                        }}
                    }}
                }} }} }} }} }}"#
        );
        let d = self
            .ok(self.alice, &q, json!({ "name": REPO, "path": path }))
            .await;
        let conn = &d["repository"]["commit"][target]["ownership"];
        let nodes = conn["nodes"].as_array().unwrap();
        assert_eq!(conn["totalCount"].as_u64().unwrap() as usize, nodes.len());
        nodes
            .iter()
            .map(|n| {
                let o = &n["owner"];
                let who = match o["__typename"].as_str().unwrap() {
                    "Team" if o["external"] == true => format!("external-team:{}", o["name"]),
                    "Team" => format!("team:{}", o["name"].as_str().unwrap()),
                    _ => match o["user"]["username"].as_str() {
                        Some(u) => format!("user:{u}"),
                        None => format!("person:{}{}", o["name"], o["email"]),
                    },
                };
                let reasons = n["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| match r["__typename"].as_str().unwrap() {
                        "CodeownersFileEntry" => format!(
                            "{}:{}@{}#{}",
                            r["codeownersFile"]["__typename"].as_str().unwrap(),
                            r["codeownersFile"]["path"].as_str().unwrap(),
                            r["section"].as_str().unwrap(),
                            r["ruleLineMatch"]
                        ),
                        "AssignedOwner" => format!("assigned(direct={})", r["isDirectMatch"]),
                        other => other.to_owned(),
                    })
                    .collect();
                (who, reasons)
            })
            .collect()
    }
}

const ASSIGN_OWNER: &str = "mutation($o: ID!, $r: ID!, $p: String!) {
    assignOwner(input: { assignedOwnerID: $o, repoID: $r, absolutePath: $p }) { alwaysNil } }";
const REMOVE_OWNER: &str = "mutation($o: ID!, $r: ID!, $p: String!) {
    removeAssignedOwner(input: { assignedOwnerID: $o, repoID: $r, absolutePath: $p }) { alwaysNil } }";
const ASSIGN_TEAM: &str = "mutation($o: ID!, $r: ID!, $p: String!) {
    assignTeam(input: { assignedOwnerID: $o, repoID: $r, absolutePath: $p }) { alwaysNil } }";
const REMOVE_TEAM: &str = "mutation($o: ID!, $r: ID!, $p: String!) {
    removeAssignedTeam(input: { assignedOwnerID: $o, repoID: $r, absolutePath: $p }) { alwaysNil } }";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_from_codeowners_and_assignments() {
    let Some(env) = setup().await else { return };
    let blob = ".gitlab/CODEOWNERS";
    let entry = |section: &str, line: u32| format!("GitBlob:{blob}@{section}#{line}");

    // CODEOWNERS only: sections combine; the GitLab handle resolves to alice; the unknown
    // group stays an external team.
    assert_eq!(
        env.owners("src/main.go", false).await,
        vec![
            ("user:admin".to_owned(), vec![entry("codeowners", 2)]),
            ("user:alice".to_owned(), vec![entry("Go", 4)]),
            (
                "external-team:\"acme/frontend\"".to_owned(),
                vec![entry("Go", 4)]
            ),
        ]
    );
    assert_eq!(
        env.owners("src", true).await,
        vec![("user:admin".to_owned(), vec![entry("codeowners", 2)])]
    );

    // Assigning needs OWNERSHIP#ASSIGN, which USER lacks.
    let bob_id = marshal_id("User", env.bob.get());
    let vars = json!({ "o": bob_id, "r": env.repo_id(), "p": "src" });
    assert_eq!(
        env.error_code(env.bob, ASSIGN_OWNER, vars.clone()).await,
        "FORBIDDEN"
    );
    env.ok(env.admin, ASSIGN_OWNER, vars.clone()).await;
    // Idempotent.
    env.ok(env.admin, ASSIGN_OWNER, vars.clone()).await;

    // A team, created by alice, assigned to the file itself.
    let d = env
        .ok(
            env.alice,
            r#"mutation { createTeam(name: "acme/backend", displayName: "Backend") {
                id name displayName viewerCanAdminister
                members { totalCount nodes { ... on User { username } } } } }"#,
            json!({}),
        )
        .await;
    let team = &d["createTeam"];
    assert_eq!(team["viewerCanAdminister"], true);
    assert_eq!(team["members"]["nodes"][0]["username"], "alice");
    let team_id = team["id"].as_str().unwrap().to_owned();
    let tvars = json!({ "o": team_id, "r": env.repo_id(), "p": "src/main.go" });
    env.ok(env.admin, ASSIGN_TEAM, tvars.clone()).await;

    let owners = env.owners("src/main.go", false).await;
    let names: Vec<_> = owners.iter().map(|(o, _)| o.as_str()).collect();
    // The direct assignment ranks first, the inherited one after CODEOWNERS entries.
    assert_eq!(
        names,
        vec![
            "team:acme/backend",
            "user:admin",
            "user:alice",
            "external-team:\"acme/frontend\"",
            "user:bob"
        ]
    );
    assert_eq!(owners[0].1, vec!["assigned(direct=true)"]);
    assert_eq!(owners[4].1, vec!["assigned(direct=false)"]);
    let dir = env.owners("src", true).await;
    assert_eq!(
        dir[0],
        (
            "user:bob".to_owned(),
            vec!["assigned(direct=true)".to_owned()]
        )
    );

    // `reasons` filters; paging.
    let d = env
        .ok(
            env.alice,
            r#"query($name: String!) { repository(name: $name) { commit(rev: "HEAD") {
                blob(path: "src/main.go") {
                    assigned: ownership(reasons: [ASSIGNED_OWNER]) { totalCount }
                    page: ownership(first: 2) { totalCount totalOwners pageInfo { hasNextPage endCursor } nodes { reasons { __typename } } }
                }
                ownership { totalCount nodes { owner { ... on Person { name } } } }
            } } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let c = &d["repository"]["commit"];
    assert_eq!(c["blob"]["assigned"]["totalCount"], 2);
    assert_eq!(c["blob"]["page"]["totalCount"], 5);
    assert_eq!(c["blob"]["page"]["totalOwners"], 5);
    assert_eq!(c["blob"]["page"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(c["blob"]["page"]["pageInfo"]["hasNextPage"], true);
    // The repository: `*` (admin).
    assert_eq!(c["ownership"]["nodes"][0]["owner"]["name"], "admin");

    // Removing.
    env.ok(env.admin, REMOVE_OWNER, vars.clone()).await;
    assert_eq!(
        env.error_code(env.admin, REMOVE_OWNER, vars).await,
        "NOT_FOUND"
    );
    env.ok(env.admin, REMOVE_TEAM, tvars).await;
    assert_eq!(env.owners("src/main.go", false).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingested_codeowners_files() {
    let Some(env) = setup().await else { return };
    let add = "mutation($r: ID, $n: String, $c: String!) {
        addCodeownersFile(input: { repoID: $r, repoName: $n, fileContents: $c }) {
            id contents repository { name } } }";
    let vars = json!({ "r": env.repo_id(), "c": "*.go @bob\n" });
    assert_eq!(
        env.error_code(env.bob, add, vars.clone()).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, add, vars.clone()).await;
    assert_eq!(d["addCodeownersFile"]["repository"]["name"], REPO);
    assert_eq!(env.error_code(env.admin, add, vars).await, "ALREADY_EXISTS");
    // The ingested file replaces the repository's.
    assert_eq!(
        env.owners("src/main.go", false).await,
        vec![(
            "user:bob".to_owned(),
            vec!["VirtualFile:CODEOWNERS@codeowners#1".to_owned()]
        )]
    );
    assert!(env.owners("src/README.md", false).await.is_empty());

    let update = "mutation($n: String, $c: String!) {
        updateCodeownersFile(input: { repoName: $n, fileContents: $c }) { contents } }";
    let d = env
        .ok(env.admin, update, json!({ "n": REPO, "c": "* @alice\n" }))
        .await;
    assert_eq!(d["updateCodeownersFile"]["contents"], "* @alice\n");
    let d = env
        .ok(
            env.admin,
            r#"query($name: String!) {
                codeownersIngestedFiles { totalCount nodes { id contents } }
                repository(name: $name) { ingestedCodeowners { contents } } }"#,
            json!({ "name": REPO }),
        )
        .await;
    assert_eq!(d["codeownersIngestedFiles"]["totalCount"], 1);
    assert_eq!(
        d["repository"]["ingestedCodeowners"]["contents"],
        "* @alice\n"
    );
    let file_id = d["codeownersIngestedFiles"]["nodes"][0]["id"].clone();
    let d = env
        .ok(
            env.admin,
            "query($id: ID!) { node(id: $id) { ... on CodeownersIngestedFile { contents } } }",
            json!({ "id": file_id }),
        )
        .await;
    assert_eq!(d["node"]["contents"], "* @alice\n");
    assert_eq!(
        env.error_code(
            env.bob,
            "{ codeownersIngestedFiles { totalCount } }",
            json!({})
        )
        .await,
        "FORBIDDEN"
    );
    env.ok(
        env.admin,
        "mutation($r: ID) { deleteCodeownersFiles(repositories: [{ repoID: $r }]) { alwaysNil } }",
        json!({ "r": env.repo_id() }),
    )
    .await;
    assert_eq!(env.owners("src/main.go", false).await.len(), 3);
    assert_eq!(
        env.error_code(env.admin, update, json!({ "n": REPO, "c": "x" }))
            .await,
        "NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_api() {
    let Some(env) = setup().await else { return };
    let d = env
        .ok(
            env.alice,
            r#"mutation { createTeam(name: "search") { id name } }"#,
            json!({}),
        )
        .await;
    let id = d["createTeam"]["id"].as_str().unwrap().to_owned();
    // Names are shared with users.
    assert_eq!(
        env.error_code(
            env.alice,
            r#"mutation { createTeam(name: "bob") { id } }"#,
            json!({})
        )
        .await,
        "ALREADY_EXISTS"
    );
    // Read-only teams are for site admins.
    assert_eq!(
        env.error_code(
            env.alice,
            r#"mutation { createTeam(name: "ro", readonly: true) { id } }"#,
            json!({})
        )
        .await,
        "FORBIDDEN"
    );
    let d = env
        .ok(
            env.alice,
            r#"mutation { createTeam(name: "search/core", parentTeamName: "search") {
                parentTeam { name } } }"#,
            json!({}),
        )
        .await;
    assert_eq!(d["createTeam"]["parentTeam"]["name"], "search");

    // bob is not a member: he may not change the team.
    let add = r#"mutation($t: ID, $m: [TeamMemberInput!]!) {
        addTeamMembers(team: $t, members: $m) { members { totalCount } } }"#;
    let members = json!([{ "username": "bob" }]);
    assert_eq!(
        env.error_code(env.bob, add, json!({ "t": id, "m": members }))
            .await,
        "FORBIDDEN"
    );
    let d = env
        .ok(env.alice, add, json!({ "t": id, "m": members }))
        .await;
    assert_eq!(d["addTeamMembers"]["members"]["totalCount"], 2);
    // Unmatched members fail unless skipped.
    let ghost = json!([{ "username": "ghost" }]);
    assert_eq!(
        env.error_code(env.alice, add, json!({ "t": id, "m": ghost }))
            .await,
        "NOT_FOUND"
    );
    let d = env
        .ok(
            env.alice,
            r#"mutation($t: ID) { setTeamMembers(team: $t, members: [{ username: "bob" }, { username: "ghost" }], skipUnmatchedMembers: true) {
                members { nodes { ... on User { username } } } } }"#,
            json!({ "t": id }),
        )
        .await;
    assert_eq!(
        d["setTeamMembers"]["members"]["nodes"],
        json!([{ "username": "bob" }])
    );

    let d = env
        .ok(
            env.bob,
            r#"query($id: ID!) {
                teams { totalCount nodes { name childTeams { nodes { name } } } }
                all: teams(includeChildTeams: true, search: "core") { nodes { name } }
                team(name: "SEARCH") { name viewerCanAdminister }
                currentUser { teams { nodes { name } } }
                node(id: $id) { ... on Team { name } } }"#,
            json!({ "id": id }),
        )
        .await;
    assert_eq!(d["teams"]["totalCount"], 1);
    assert_eq!(
        d["teams"]["nodes"][0]["childTeams"]["nodes"][0]["name"],
        "search/core"
    );
    assert_eq!(d["all"]["nodes"], json!([{ "name": "search/core" }]));
    assert_eq!(d["team"]["viewerCanAdminister"], true);
    assert_eq!(
        d["currentUser"]["teams"]["nodes"],
        json!([{ "name": "search" }])
    );
    assert_eq!(d["node"]["name"], "search");

    // alice created search/core and is its member.
    let d = env
        .ok(
            env.alice,
            r#"mutation { updateTeam(name: "search/core", displayName: "Core", makeRoot: true) {
                displayName parentTeam { name } } }"#,
            json!({}),
        )
        .await;
    assert!(d["updateTeam"]["parentTeam"].is_null());
    assert_eq!(d["updateTeam"]["displayName"], "Core");
    // After setTeamMembers bob is search's only member.
    assert_eq!(
        env.error_code(
            env.alice,
            r#"mutation { deleteTeam(name: "search") { alwaysNil } }"#,
            json!({})
        )
        .await,
        "FORBIDDEN"
    );
    env.ok(
        env.bob,
        r#"mutation { deleteTeam(name: "search") { alwaysNil } }"#,
        json!({}),
    )
    .await;
    let d = env
        .ok(env.admin, r#"{ team(name: "search") { name } }"#, json!({}))
        .await;
    assert!(d["team"].is_null());
}
