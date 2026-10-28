//! Repository permissions over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): repository lookups honour permissions, and the explicit
//! permissions API grants, keeps pending and unrestricts.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{BindId, ConfigHandle, PermissionsUserMapping, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

const PUBLIC: &str = "gitlab.example.com/acme/public";
const PRIVATE: &str = "gitlab.example.com/acme/private";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
    checker: Checker,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    private_id: String,
}

/// The code hosts the repositories come from, as `(external_service_type, external_service_id)`.
/// Enforcement does not depend on it; the read-path tests run on each.
const CODE_HOSTS: [(&str, &str); 3] = [
    ("gitlab", "https://gitlab.example.com/"),
    ("github", "https://github.com/"),
    ("gitea", "https://gitea.example.com/"),
];

async fn insert_repo(db: &Db, host: (&str, &str), name: &str, ext: &str, private: bool) -> i32 {
    sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(name)
    .bind(private)
    .bind(ext)
    .bind(host.0)
    .bind(host.1)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn setup() -> Option<Env> {
    setup_on(CODE_HOSTS[0]).await
}

/// [`setup`] with the repositories on `host`.
async fn setup_on(host: (&str, &str)) -> Option<Env> {
    let t = TestDb::new().await?;
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
    let alice = user(&db, "alice").await;
    insert_repo(&db, host, PUBLIC, "1", false).await;
    let private = insert_repo(&db, host, PRIVATE, "2", true).await;
    let checker = Checker::new(db.clone(), config.clone());
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            sourcegull_rpc::Router::default(),
            sourcegull_rpc::ClientConfig::default(),
        )),
        authz: checker.clone(),
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        auth,
        config,
        checker,
        schema,
        admin,
        alice,
        private_id: marshal_id("Repository", i64::from(private)),
    })
}

async fn user(db: &Db, name: &str) -> UserId {
    let new = NewUser {
        username: name.into(),
        ..Default::default()
    };
    db.users().create(&new).await.unwrap().id
}

impl Env {
    async fn run(&self, actor: UserId, query: &str, vars: Value) -> Response {
        let actor = self.auth.actor_for(actor, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, actor: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn code(&self, actor: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(actor, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        let code = err.extensions.as_ref().and_then(|e| e.get("code")).cloned();
        code.and_then(|c| c.into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// The repository names `actor` can list, the total count, and whether lookups of the
    /// private repository by name and by ID find it.
    async fn view(&self, actor: UserId) -> (Vec<String>, i64, bool, bool) {
        let data = self
            .ok(
                actor,
                "query($name: String!, $id: ID!) {
                    repositories { totalCount nodes { name } }
                    repository(name: $name) { name }
                    node(id: $id) { id } }",
                json!({ "name": PRIVATE, "id": self.private_id }),
            )
            .await;
        let names = data["repositories"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_owned())
            .collect();
        (
            names,
            data["repositories"]["totalCount"].as_i64().unwrap(),
            !data["repository"].is_null(),
            !data["node"].is_null(),
        )
    }

    fn set_config(&self, f: impl FnOnce(&mut SiteConfig)) {
        let mut c = self.config.load().value.clone();
        f(&mut c);
        self.config.store(self.config.version() + 1, c);
    }
}

const SET_USERS: &str = "mutation($repo: ID!, $users: [UserPermissionInput!]!) {
    setRepositoryPermissionsForUsers(repository: $repo, userPermissions: $users) { alwaysNil } }";
const UNRESTRICT: &str = "mutation($repos: [ID!]!, $on: Boolean!) {
    setRepositoryPermissionsUnrestricted(repositories: $repos, unrestricted: $on) { alwaysNil } }";
const AUTHORIZED: &str = "query($u: String) {
    authorizedUserRepositories(username: $u, first: 10) { totalCount nodes { name } } }";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_lookups_honour_permissions() {
    for host in CODE_HOSTS {
        let Some(env) = setup_on(host).await else {
            return;
        };
        lookups_honour_permissions(&env).await;
    }
}

async fn lookups_honour_permissions(env: &Env) {
    let hidden = (vec![PUBLIC.to_owned()], 1, false, false);
    assert_eq!(env.view(env.alice).await, hidden);
    let all = (vec![PRIVATE.to_owned(), PUBLIC.to_owned()], 2, true, true);
    assert_eq!(env.view(env.admin).await, all);

    // Site admins too, when configured.
    env.set_config(|c| c.authz_enforce_for_site_admins = true);
    assert_eq!(env.view(env.admin).await, hidden);
    env.set_config(|c| c.authz_enforce_for_site_admins = false);

    // Granting through the API needs explicit permissions turned on.
    let vars = json!({ "repo": env.private_id, "users": [{ "bindID": "alice" }] });
    assert_eq!(
        env.code(env.admin, SET_USERS, vars.clone()).await,
        "FORBIDDEN"
    );
    env.set_config(|c| {
        c.permissions_user_mapping = PermissionsUserMapping {
            enabled: true,
            bind_id: BindId::Username,
        }
    });
    assert_eq!(
        env.code(env.alice, SET_USERS, vars.clone()).await,
        "FORBIDDEN"
    );
    let vars = json!({
        "repo": env.private_id,
        "users": [{ "bindID": "alice" }, { "bindID": "Bob" }],
    });
    env.ok(env.admin, SET_USERS, vars).await;
    // In production `NOTIFY authz` does this.
    env.checker.invalidate_all();
    assert_eq!(env.view(env.alice).await, all);

    // Bob does not exist yet: pending until he does.
    let pending = env
        .ok(env.admin, "{ usersWithPendingPermissions }", json!({}))
        .await;
    assert_eq!(pending["usersWithPendingPermissions"], json!(["bob"]));
    let repos = env.ok(env.admin, AUTHORIZED, json!({ "u": "bob" })).await;
    assert_eq!(
        repos["authorizedUserRepositories"],
        json!({ "totalCount": 1, "nodes": [{ "name": PRIVATE }] })
    );
    let bob = user(&env.db, "bob").await;
    assert_eq!(env.view(bob).await, all);
    let info = env
        .ok(
            env.admin,
            "query($name: String!) { repository(name: $name) {
                permissionsInfo { permissions unrestricted updatedAt }
                authorizedUsers(first: 10) { totalCount nodes { username } } } }",
            json!({ "name": PRIVATE }),
        )
        .await;
    let repo = &info["repository"];
    assert_eq!(repo["permissionsInfo"]["permissions"], json!(["READ"]));
    assert_eq!(repo["permissionsInfo"]["unrestricted"], false);
    assert_eq!(
        repo["authorizedUsers"],
        json!({ "totalCount": 2, "nodes": [{ "username": "alice" }, { "username": "bob" }] })
    );
    let as_alice = env
        .code(
            env.alice,
            "query($name: String!) { repository(name: $name) { permissionsInfo { unrestricted } } }",
            json!({ "name": PRIVATE }),
        )
        .await;
    assert_eq!(as_alice, "FORBIDDEN");

    // Replacing the list revokes; unrestricting opens the repository to everyone.
    let vars = json!({ "repo": env.private_id, "users": [] });
    env.ok(env.admin, SET_USERS, vars).await;
    env.checker.invalidate_all();
    assert_eq!(env.view(env.alice).await, hidden);
    let vars = json!({ "repos": [env.private_id], "on": true });
    env.ok(env.admin, UNRESTRICT, vars).await;
    env.checker.invalidate_all();
    assert_eq!(env.view(env.alice).await, all);
    let missing = json!({ "repos": [marshal_id("Repository", 999)], "on": true });
    assert_eq!(env.code(env.admin, UNRESTRICT, missing).await, "NOT_FOUND");
}

#[tokio::test]
async fn permissions_sync_scheduling_and_history() {
    let Some(env) = setup().await else { return };
    let user_id = marshal_id("User", env.alice.get());
    let schedule = "mutation($u: ID!) { scheduleUserPermissionsSync(user: $u) { alwaysNil } }";
    // Site admins only.
    assert_eq!(
        env.code(env.alice, schedule, json!({ "u": user_id })).await,
        "FORBIDDEN"
    );
    // Nothing to sync without a connection that has an authorization block.
    assert_eq!(
        env.code(env.admin, schedule, json!({ "u": user_id })).await,
        "INVALID_INPUT"
    );
    sourcegull_db::sqlx::query(
        "INSERT INTO external_services (kind, display_name, config_enc) VALUES ('GITLAB', 'gl', '\\x00')",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    env.ok(env.admin, schedule, json!({ "u": user_id })).await;
    env.ok(
        env.admin,
        "mutation($r: ID!) { scheduleRepositoryPermissionsSync(repository: $r) { alwaysNil } }",
        json!({ "r": env.private_id }),
    )
    .await;

    let data = env
        .ok(
            env.admin,
            "query($u: ID!) {
                all: permissionsSyncJobs { totalCount nodes { id } }
                mine: permissionsSyncJobs(userID: $u, state: QUEUED) {
                    totalCount
                    nodes { state reason priority triggeredByUser { username }
                            subject { ... on User { username } } }
                }
                user(username: \"alice\") { permissionsInfo { syncedAt } externalAccounts { totalCount } }
            }",
            json!({ "u": user_id }),
        )
        .await;
    assert_eq!(data["all"]["totalCount"], 2);
    let job = &data["mine"]["nodes"][0];
    assert_eq!(data["mine"]["totalCount"], 1);
    assert_eq!(job["state"], "QUEUED");
    assert_eq!(job["reason"], "manual");
    assert_eq!(job["priority"], "MEDIUM");
    assert_eq!(job["triggeredByUser"]["username"], "admin");
    assert_eq!(job["subject"]["username"], "alice");
    assert_eq!(data["user"]["permissionsInfo"]["syncedAt"], Value::Null);
    assert_eq!(data["user"]["externalAccounts"]["totalCount"], 0);

    // Cancel.
    let id = data["all"]["nodes"][0]["id"].clone();
    env.ok(
        env.admin,
        "mutation($j: ID!) { cancelPermissionsSyncJob(job: $j) { alwaysNil } }",
        json!({ "j": id }),
    )
    .await;
}

#[tokio::test]
async fn access_explanation_names_the_code_host() {
    let Some(env) = setup_on(CODE_HOSTS[1]).await else {
        return;
    };
    let id = sourcegull_services::relay::unmarshal_int_id("Repository", &env.private_id).unwrap();
    let private = sourcegull_types::RepoId(i32::try_from(id).unwrap());
    env.db
        .perms()
        .set_repo_perms(
            private,
            sourcegull_db::PermsSource::Sync,
            &[env.alice],
            None,
        )
        .await
        .unwrap();
    let e = env
        .ok(
            env.admin,
            "query($u: ID!, $r: ID!) { repositoryAccessExplanation(user: $u, repository: $r) {
                reasons { kind detail } } }",
            json!({ "u": marshal_id("User", env.alice.get()), "r": env.private_id }),
        )
        .await;
    assert_eq!(
        e["repositoryAccessExplanation"]["reasons"],
        json!([{ "kind": "PERMISSIONS_SYNC", "detail": "Granted by the permissions sync from GitHub." }])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_access_explanation() {
    let Some(env) = setup().await else { return };
    const EXPLAIN: &str =
        "query($u: ID!, $r: ID!) { repositoryAccessExplanation(user: $u, repository: $r) {
        canRead summary reasons { kind detail grantedAt } } }";
    async fn explain(env: &Env, user: UserId, repo: &str) -> Value {
        let vars = json!({ "u": marshal_id("User", user.get()), "r": repo });
        env.ok(env.admin, EXPLAIN, vars).await["repositoryAccessExplanation"].clone()
    }
    let kinds = |v: &Value| -> Vec<String> {
        v["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["kind"].as_str().unwrap().to_owned())
            .collect()
    };

    // Not readable: private and not granted.
    let e = explain(&env, env.alice, &env.private_id).await;
    assert_eq!(e["canRead"], false);
    assert!(kinds(&e).is_empty());
    assert!(
        e["summary"].as_str().unwrap().contains("not granted"),
        "{e}"
    );

    // Site admins read everything; public repositories are readable by everyone.
    let e = explain(&env, env.admin, &env.private_id).await;
    assert_eq!(
        (e["canRead"].clone(), kinds(&e)),
        (json!(true), vec!["SITE_ADMIN".into()])
    );
    let public: i32 = sourcegull_db::sqlx::query_scalar("SELECT id FROM repo WHERE name = $1")
        .bind(PUBLIC)
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    let e = explain(
        &env,
        env.alice,
        &marshal_id("Repository", i64::from(public)),
    )
    .await;
    assert_eq!(
        (e["canRead"].clone(), kinds(&e)),
        (json!(true), vec!["PUBLIC".into()])
    );
    assert_eq!(
        e["reasons"][0]["detail"],
        "The repository is public on GitLab."
    );

    // An explicit grant, then marking the repository unrestricted.
    env.set_config(|c| {
        c.permissions_user_mapping = PermissionsUserMapping {
            enabled: true,
            bind_id: BindId::Username,
        }
    });
    let vars = json!({ "repo": env.private_id, "users": [{ "bindID": "alice" }] });
    env.ok(env.admin, SET_USERS, vars).await;
    env.checker.invalidate_all();
    let e = explain(&env, env.alice, &env.private_id).await;
    assert_eq!(
        (e["canRead"].clone(), kinds(&e)),
        (json!(true), vec!["EXPLICIT_API".into()])
    );
    assert!(e["reasons"][0]["grantedAt"].is_string());
    let vars = json!({ "repos": [env.private_id], "on": true });
    env.ok(env.admin, UNRESTRICT, vars).await;
    env.checker.invalidate_all();
    let e = explain(&env, env.alice, &env.private_id).await;
    assert_eq!(kinds(&e), ["UNRESTRICTED", "EXPLICIT_API"]);

    // Unknown subjects resolve to null; non-admins may not ask.
    assert!(
        explain(&env, env.alice, &marshal_id("Repository", 999))
            .await
            .is_null()
    );
    let vars = json!({ "u": marshal_id("User", env.alice.get()), "r": env.private_id });
    assert_eq!(env.code(env.alice, EXPLAIN, vars).await, "FORBIDDEN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_contexts_hide_private_repositories() {
    let Some(env) = setup().await else { return };
    let public_id = env
        .ok(
            env.admin,
            "{ repository(name: \"gitlab.example.com/acme/public\") { id } }",
            json!({}),
        )
        .await["repository"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let missing_id = marshal_id("Repository", 999);
    let create =
        "mutation($input: SearchContextInput!, $repos: [SearchContextRepositoryRevisionsInput!]!) {
        createSearchContext(searchContext: $input, repositories: $repos) { id } }";
    let input = |name: &str, ns: Option<&str>| json!({ "name": name, "description": "", "public": true, "namespace": ns, "query": "" });
    let repos = |ids: &[&str]| -> Value {
        ids.iter()
            .map(|id| json!({ "repositoryID": id, "revisions": [] }))
            .collect()
    };
    // The code and message of the first error, with the repository ID replaced by `<id>`.
    let error = |resp: Response, id: &str| {
        let err = resp.errors.first().cloned().expect("an error");
        let code = err.extensions.as_ref().and_then(|e| e.get("code")).cloned();
        let code = code.and_then(|c| c.into_json().ok());
        (format!("{code:?}"), err.message.replace(id, "<id>"))
    };

    // A public context of a site admin's listing both repositories: Alice sees only the public
    // one.
    let both = env
        .ok(
            env.admin,
            create,
            json!({ "input": input("both", None), "repos": repos(&[&public_id, &env.private_id]) }),
        )
        .await;
    let both_id = both["createSearchContext"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let read = "{ searchContextBySpec(spec: \"both\") { repositories { repository { name } } } }";
    let names = |d: Value| -> Vec<String> {
        d["searchContextBySpec"]["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["repository"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(env.ok(env.admin, read, json!({})).await),
        [PRIVATE, PUBLIC]
    );
    assert_eq!(names(env.ok(env.alice, read, json!({})).await), [PUBLIC]);
    let node = "query($id: ID!) { node(id: $id) {
        ... on SearchContext { repositories { repository { name } } } } }";
    let d = env.ok(env.alice, node, json!({ "id": both_id })).await;
    assert_eq!(
        d["node"]["repositories"],
        json!([{ "repository": { "name": PUBLIC } }])
    );

    // Creating or updating a context with the private repository fails exactly as with a
    // missing one.
    let alice_ns = marshal_id("User", env.alice.get());
    let with = |id: &str| json!({ "input": input("mine", Some(&alice_ns)), "repos": repos(&[id]) });
    let hidden = error(
        env.run(env.alice, create, with(&env.private_id)).await,
        &env.private_id,
    );
    let missing = error(
        env.run(env.alice, create, with(&missing_id)).await,
        &missing_id,
    );
    assert_eq!(hidden, missing);
    assert!(hidden.1.contains("not found"), "{hidden:?}");
    let mine = env.ok(env.alice, create, with(&public_id)).await;
    let mine_id = mine["createSearchContext"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let update = "mutation($id: ID!, $input: SearchContextEditInput!, $repos: [SearchContextRepositoryRevisionsInput!]!) {
        updateSearchContext(id: $id, searchContext: $input, repositories: $repos) { id } }";
    let edit = |id: &str| {
        json!({ "id": mine_id, "repos": repos(&[id]),
                "input": { "name": "mine", "description": "", "public": true, "query": "" } })
    };
    let hidden = error(
        env.run(env.alice, update, edit(&env.private_id)).await,
        &env.private_id,
    );
    let missing = error(
        env.run(env.alice, update, edit(&missing_id)).await,
        &missing_id,
    );
    assert_eq!(hidden, missing);
    // A site admin may list it.
    env.ok(env.admin, update, edit(&env.private_id)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_metadata_needs_permission_and_read_access() {
    let Some(env) = setup().await else { return };
    const ADD: &str = "mutation($repo: ID!, $key: String!, $value: String) {
        addRepoKeyValuePair(repo: $repo, key: $key, value: $value) { alwaysNil } }";
    const UPDATE: &str = "mutation($repo: ID!, $key: String!, $value: String) {
        updateRepoKeyValuePair(repo: $repo, key: $key, value: $value) { alwaysNil } }";
    const DELETE: &str = "mutation($repo: ID!, $key: String!) {
        deleteRepoKeyValuePair(repo: $repo, key: $key) { alwaysNil } }";
    const KVPS: &str = "query($name: String!) {
        repository(name: $name) { id keyValuePairs { key value } } }";
    let data = env.ok(env.alice, KVPS, json!({ "name": PUBLIC })).await;
    let public = data["repository"]["id"].as_str().unwrap().to_owned();
    let kvps = |data: Value| data["repository"]["keyValuePairs"].clone();

    // The default USER role holds REPO_METADATA#WRITE.
    let vars = json!({ "repo": public, "key": "team", "value": "search" });
    env.ok(env.alice, ADD, vars.clone()).await;
    assert_eq!(env.code(env.alice, ADD, vars).await, "ALREADY_EXISTS");
    env.ok(env.alice, ADD, json!({ "repo": public, "key": "go" }))
        .await;
    env.ok(
        env.alice,
        UPDATE,
        json!({ "repo": public, "key": "team", "value": "web" }),
    )
    .await;
    assert_eq!(
        kvps(env.ok(env.alice, KVPS, json!({ "name": PUBLIC })).await),
        json!([{ "key": "go", "value": null }, { "key": "team", "value": "web" }])
    );
    env.ok(env.alice, DELETE, json!({ "repo": public, "key": "go" }))
        .await;
    assert_eq!(
        env.code(env.alice, DELETE, json!({ "repo": public, "key": "go" }))
            .await,
        "NOT_FOUND"
    );
    assert_eq!(
        env.code(env.alice, UPDATE, json!({ "repo": public, "key": "nope" }))
            .await,
        "NOT_FOUND"
    );
    assert_eq!(
        env.code(env.alice, ADD, json!({ "repo": public, "key": " " }))
            .await,
        "INVALID_INPUT"
    );

    // A repository the viewer cannot read is not found; a site admin can write it.
    let private = json!({ "repo": env.private_id, "key": "team" });
    assert_eq!(env.code(env.alice, ADD, private.clone()).await, "NOT_FOUND");
    env.ok(env.admin, ADD, private).await;

    // Without the permission every mutation is forbidden.
    sourcegull_db::sqlx::query(
        "DELETE FROM role_permissions rp USING roles r, permissions p
         WHERE rp.role_id = r.id AND rp.permission_id = p.id
           AND r.name = 'USER' AND p.namespace = 'REPO_METADATA'",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    env.auth.invalidate_user(env.alice).await;
    for (q, vars) in [
        (ADD, json!({ "repo": public, "key": "x" })),
        (UPDATE, json!({ "repo": public, "key": "team" })),
        (DELETE, json!({ "repo": public, "key": "team" })),
    ] {
        assert_eq!(env.code(env.alice, q, vars).await, "FORBIDDEN");
    }
    assert_eq!(
        kvps(env.ok(env.alice, KVPS, json!({ "name": PUBLIC })).await),
        json!([{ "key": "team", "value": "web" }])
    );
    // Site admins hold it through their role.
    env.ok(env.admin, DELETE, json!({ "repo": public, "key": "team" }))
        .await;
}

#[tokio::test]
async fn code_host_sign_ins_needed() {
    let Some(env) = setup().await else { return };
    const QUERY: &str = "{ currentUser { codeHostSignInsNeeded {
        serviceType serviceID displayName reason authenticationURL } } }";
    let needed = |data: &Value| data["currentUser"]["codeHostSignInsNeeded"].clone();
    assert_eq!(
        needed(&env.ok(env.alice, QUERY, json!({})).await),
        json!([])
    );

    // A GitHub account whose token was rejected, and a Gitea sync without the scope.
    sourcegull_db::sqlx::query(
        "INSERT INTO user_external_accounts (user_id, service_type, service_id, account_id, expired_at)
         VALUES ($1, 'github', 'https://github.com/', '7', now()),
                ($1, 'gitea', 'https://gitea.example.com/', '8', NULL)",
    )
    .bind(env.alice)
    .execute(env.db.pool())
    .await
    .unwrap();
    let states = json!([
        { "externalServiceID": 1, "serviceType": "gitea", "serviceID": "https://gitea.example.com/",
          "status": "error", "message": "the token lacks the scope", "signIn": "missingScope" },
        { "externalServiceID": 2, "serviceType": "gitlab", "serviceID": "https://gitlab.example.com/",
          "status": "ok", "message": "" },
    ]);
    sourcegull_db::sqlx::query(
        "INSERT INTO permission_sync_jobs (state, user_id, reason, finished_at, code_host_states)
         VALUES ('errored', $1, 'scheduled', now() + interval '1 minute', $2)",
    )
    .bind(env.alice)
    .bind(&states)
    .execute(env.db.pool())
    .await
    .unwrap();
    env.set_config(|c| {
        c.auth_providers.push(
            serde_json::from_value(json!({
                "type": "github", "url": "https://github.com", "clientID": "gh id",
                "clientSecret": "s", "displayName": "GitHub.com"
            }))
            .unwrap(),
        );
    });
    let data = env.ok(env.alice, QUERY, json!({})).await;
    assert_eq!(
        needed(&data),
        json!([
            { "serviceType": "github", "serviceID": "https://github.com/", "displayName": "GitHub.com",
              "reason": "EXPIRED", "authenticationURL": "/.auth/github/login?pc=gh+id" },
            { "serviceType": "gitea", "serviceID": "https://gitea.example.com/", "displayName": "Gitea",
              "reason": "MISSING_SCOPE", "authenticationURL": null },
        ])
    );

    // The jobs list names each state's code host.
    let jobs = env
        .ok(
            env.admin,
            "{ permissionsSyncJobs { nodes { codeHostStates { providerType status } } } }",
            json!({}),
        )
        .await;
    assert_eq!(
        jobs["permissionsSyncJobs"]["nodes"][0]["codeHostStates"],
        json!([
            { "providerType": "gitea", "status": "ERROR" },
            { "providerType": "gitlab", "status": "SUCCESS" },
        ])
    );

    // Signing in again (the account is updated after the job) clears the prompt; others
    // may not read it.
    sourcegull_db::sqlx::query(
        "UPDATE user_external_accounts SET updated_at = now() + interval '2 minutes',
             expired_at = NULL WHERE user_id = $1",
    )
    .bind(env.alice)
    .execute(env.db.pool())
    .await
    .unwrap();
    assert_eq!(
        needed(&env.ok(env.alice, QUERY, json!({})).await),
        json!([])
    );
    let bob = user(&env.db, "bob").await;
    let other = "query { user(username: \"alice\") { codeHostSignInsNeeded { reason } } }";
    assert_eq!(env.code(bob, other, json!({})).await, "FORBIDDEN");
}
