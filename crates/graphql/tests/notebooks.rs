//! Notebooks over GraphQL against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`):
//! create, read through `node` and `notebook`, list with search and filters, update, stars,
//! delete; who may see and manage private, public and org notebooks; blocks in repositories
//! the viewer may not read are hidden and survive their saves.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, UserId};

const PUBLIC: &str = "gitlab.example.com/acme/public";
const PRIVATE: &str = "gitlab.example.com/acme/private";

const FIELDS: &str = "id title public patternType viewerCanManage viewerHasStarred
    creator { username } updater { username }
    namespace { id ... on User { username } ... on Org { name } }
    stars { totalCount nodes { user { username } } }
    blocks {
        __typename
        ... on MarkdownBlock { id markdownInput }
        ... on QueryBlock { id queryInput }
        ... on FileBlock { id fileInput { repositoryName filePath revision lineRange { startLine endLine } } }
        ... on SymbolBlock { id symbolInput { repositoryName filePath revision lineContext symbolName symbolContainerName symbolKind } }
    }";

fn create_q() -> String {
    format!("mutation($nb: NotebookInput!) {{ createNotebook(notebook: $nb) {{ {FIELDS} }} }}")
}

fn update_q() -> String {
    format!(
        "mutation($id: ID!, $nb: NotebookInput!) {{ updateNotebook(id: $id, notebook: $nb) {{ {FIELDS} }} }}"
    )
}

fn node_q() -> String {
    format!(
        "query($id: ID!) {{ node(id: $id) {{ ... on Notebook {{ {FIELDS} }} }} notebook(id: $id) {{ id title }} }}"
    )
}

const LIST: &str =
    "query($q: String, $creator: ID, $starred: ID, $ns: ID, $order: NotebooksOrderBy,
                         $desc: Boolean, $first: Int, $after: String) {
    notebooks(query: $q, creatorUserID: $creator, starredByUserID: $starred, namespace: $ns,
              orderBy: $order, descending: $desc, first: $first, after: $after) {
        totalCount pageInfo { hasNextPage endCursor } nodes { title }
    }
}";

const DELETE: &str = "mutation($id: ID!) { deleteNotebook(id: $id) { alwaysNil } }";
const STAR: &str =
    "mutation($id: ID!) { createNotebookStar(notebookID: $id) { user { username } createdAt } }";
const UNSTAR: &str = "mutation($id: ID!) { deleteNotebookStar(notebookID: $id) { alwaysNil } }";

struct Env {
    _t: TestDb,
    schema: SourcegullSchema,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
}

impl Env {
    async fn run(&self, user: Option<UserId>, query: &str, vars: Value) -> Response {
        let actor = match user {
            Some(u) => self.auth.actor_for(u, AuthVia::Session).await.unwrap(),
            None => None,
        };
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(Some(user), query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn code(&self, user: Option<UserId>, query: &str, vars: Value) -> String {
        let resp = self.run(user, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        let code = err.extensions.as_ref().and_then(|e| e.get("code")).cloned();
        code.and_then(|c| c.into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    async fn titles(&self, user: UserId, vars: Value) -> (Vec<String>, i64) {
        let data = self.ok(user, LIST, vars).await;
        let c = &data["notebooks"];
        (
            c["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["title"].as_str().unwrap().to_owned())
                .collect(),
            c["totalCount"].as_i64().unwrap(),
        )
    }
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) {
    sourcegull_db::sqlx::query(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
         VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/')",
    )
    .bind(name)
    .bind(private)
    .bind(ext)
    .execute(db.pool())
    .await
    .unwrap();
}

async fn user(db: &Db, name: &str) -> UserId {
    let new = NewUser {
        username: name.into(),
        ..Default::default()
    };
    db.users().create(&new).await.unwrap().id
}

fn blocks() -> Value {
    json!([
        {"id": "md", "type": "MARKDOWN", "markdownInput": "# Payments\nHow refunds flow."},
        {"id": "q", "type": "QUERY", "queryInput": "repo:acme lang:go refund"},
        {"id": "pub", "type": "FILE", "fileInput": {"repositoryName": PUBLIC,
            "filePath": "/cmd/main.go", "revision": "", "lineRange": {"startLine": 2, "endLine": 9}}},
        {"id": "priv", "type": "FILE", "fileInput": {"repositoryName": PRIVATE,
            "filePath": "secret.go"}},
        {"id": "sym", "type": "SYMBOL", "symbolInput": {"repositoryName": PRIVATE,
            "filePath": "secret.go", "revision": "main", "lineContext": 3, "symbolName": "Refund",
            "symbolContainerName": "", "symbolKind": "FUNCTION"}},
    ])
}

fn ids(nb: &Value) -> Vec<String> {
    nb["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notebooks_api() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
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
    let bob = user(&db, "bob").await;
    insert_repo(&db, PUBLIC, "1", false).await;
    insert_repo(&db, PRIVATE, "2", true).await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    for u in [admin, bob] {
        db.orgs().add_member(org, u, OrgRole::Member).await.unwrap();
    }
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(RepoClients::new(Router::default(), ClientConfig::default())),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    let env = Env {
        _t: t,
        schema,
        auth,
        config,
    };
    let admin_ns = marshal_id("User", admin.get());
    let alice_ns = marshal_id("User", alice.get());
    let org_ns = marshal_id("Org", org.get());
    let nb = |title: &str, ns: &str, public: bool, blocks: Value| json!({"nb": {"title": title, "namespace": ns, "public": public, "blocks": blocks}});

    // Create: the admin may read the private repository, so sees every block.
    let created = env
        .ok(admin, &create_q(), nb("Refunds", &admin_ns, true, blocks()))
        .await;
    let refunds = &created["createNotebook"];
    let refunds_id = refunds["id"].as_str().unwrap().to_owned();
    assert_eq!(ids(refunds), ["md", "q", "pub", "priv", "sym"]);
    assert_eq!(refunds["creator"]["username"], "admin");
    assert_eq!(refunds["namespace"]["username"], "admin");
    assert_eq!(refunds["patternType"], "standard");
    assert_eq!(refunds["viewerCanManage"], true);
    assert_eq!(refunds["viewerHasStarred"], false);
    let file = &refunds["blocks"][2];
    assert_eq!(file["__typename"], "FileBlock");
    assert_eq!(
        file["fileInput"],
        json!({"repositoryName": PUBLIC, "filePath": "cmd/main.go", "revision": null,
               "lineRange": {"startLine": 2, "endLine": 9}})
    );
    assert_eq!(
        refunds["blocks"][4]["symbolInput"]["symbolKind"],
        "FUNCTION"
    );
    assert_eq!(
        refunds["blocks"][0]["markdownInput"],
        "# Payments\nHow refunds flow."
    );

    // Alice sees the public notebook without the blocks in the private repository, and may
    // not manage it.
    let seen = env.ok(alice, &node_q(), json!({"id": refunds_id})).await;
    assert_eq!(ids(&seen["node"]), ["md", "q", "pub"]);
    assert_eq!(seen["node"]["viewerCanManage"], false);
    assert_eq!(seen["notebook"]["title"], "Refunds");
    let edit = nb("Mine now", &admin_ns, true, json!([]));
    let mut vars = edit.clone();
    vars["id"] = json!(refunds_id);
    assert_eq!(env.code(Some(alice), &update_q(), vars).await, "FORBIDDEN");
    assert_eq!(
        env.code(Some(alice), DELETE, json!({"id": refunds_id}))
            .await,
        "FORBIDDEN"
    );

    // Validation and namespace rules on create.
    let dup = json!([{"id": "a", "type": "MARKDOWN", "markdownInput": "x"},
                     {"id": "a", "type": "QUERY", "queryInput": "y"}]);
    assert_eq!(
        env.code(Some(alice), &create_q(), nb("Dup", &alice_ns, false, dup))
            .await,
        "VALIDATION"
    );
    let missing = json!([{"id": "a", "type": "FILE"}]);
    assert_eq!(
        env.code(
            Some(alice),
            &create_q(),
            nb("Missing", &alice_ns, false, missing)
        )
        .await,
        "VALIDATION"
    );
    assert_eq!(
        env.code(
            Some(alice),
            &create_q(),
            nb("Org", &org_ns, false, json!([]))
        )
        .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(
            Some(alice),
            &create_q(),
            nb("Admin's", &admin_ns, false, json!([]))
        )
        .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(None, &create_q(), nb("Anon", &alice_ns, false, json!([])))
            .await,
        "UNAUTHORIZED"
    );

    // A private notebook of alice's and a private org notebook.
    let private = env
        .ok(
            alice,
            &create_q(),
            nb(
                "Alice scratch",
                &alice_ns,
                false,
                json!([{"id": "1", "type": "MARKDOWN", "markdownInput": "kubernetes notes"}]),
            ),
        )
        .await["createNotebook"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let org_nb = env
        .ok(
            admin,
            &create_q(),
            nb("Org runbook", &org_ns, false, blocks()),
        )
        .await["createNotebook"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Private notebooks are invisible to others: site admins included.
    let none = env.ok(admin, &node_q(), json!({"id": private})).await;
    assert_eq!(none["node"], Value::Null);
    assert_eq!(none["notebook"], Value::Null);
    let none = env.ok(alice, &node_q(), json!({"id": org_nb})).await;
    assert_eq!(none["node"], Value::Null);
    assert_eq!(
        env.code(Some(alice), STAR, json!({"id": org_nb})).await,
        "NOT_FOUND"
    );

    // Listing, search and filters.
    let all = json!({"order": "NOTEBOOK_CREATED_AT"});
    assert_eq!(
        env.titles(alice, all.clone()).await,
        (vec!["Refunds".into(), "Alice scratch".into()], 2)
    );
    assert_eq!(
        env.titles(bob, all.clone()).await,
        (vec!["Refunds".into(), "Org runbook".into()], 2)
    );
    assert_eq!(
        env.titles(alice, json!({"q": "refund"})).await.0,
        ["Refunds"]
    );
    assert_eq!(
        env.titles(alice, json!({"q": "kube"})).await.0,
        ["Alice scratch"]
    );
    assert_eq!(env.titles(bob, json!({"q": "kube"})).await.1, 0);
    assert_eq!(
        env.titles(bob, json!({"ns": org_ns})).await.0,
        ["Org runbook"]
    );
    assert_eq!(
        env.titles(alice, json!({"creator": marshal_id("User", admin.get())}))
            .await
            .0,
        ["Refunds"]
    );
    let page = env
        .ok(
            bob,
            LIST,
            json!({"order": "NOTEBOOK_CREATED_AT", "first": 1}),
        )
        .await;
    assert_eq!(page["notebooks"]["pageInfo"]["hasNextPage"], true);
    let after = page["notebooks"]["pageInfo"]["endCursor"].clone();
    assert_eq!(
        env.titles(
            bob,
            json!({"order": "NOTEBOOK_CREATED_AT", "first": 1, "after": after})
        )
        .await
        .0,
        ["Org runbook"]
    );

    // Anonymous readers: only with public access, and only public notebooks.
    assert_eq!(env.code(None, LIST, json!({})).await, "UNAUTHORIZED");
    let mut c = env.config.load().value.clone();
    c.auth_public = true;
    env.config.store(env.config.version() + 1, c);
    let anon = env.run(None, LIST, json!({})).await;
    assert!(anon.errors.is_empty(), "{:?}", anon.errors);
    let anon = anon.data.into_json().unwrap();
    assert_eq!(anon["notebooks"]["totalCount"], 1);
    let anon = env
        .run(
            None,
            &format!("query($id: ID!) {{ notebook(id: $id) {{ {FIELDS} }} }}"),
            json!({"id": refunds_id}),
        )
        .await;
    assert!(anon.errors.is_empty(), "{:?}", anon.errors);
    let anon = anon.data.into_json().unwrap();
    assert_eq!(ids(&anon["notebook"]), ["md", "q", "pub"]);
    assert_eq!(anon["notebook"]["viewerCanManage"], false);

    // Stars.
    let star = env.ok(alice, STAR, json!({"id": refunds_id})).await;
    assert_eq!(star["createNotebookStar"]["user"]["username"], "alice");
    env.ok(alice, STAR, json!({"id": refunds_id})).await;
    env.ok(bob, STAR, json!({"id": org_nb})).await;
    let seen = env.ok(alice, &node_q(), json!({"id": refunds_id})).await;
    assert_eq!(seen["node"]["viewerHasStarred"], true);
    assert_eq!(seen["node"]["stars"]["totalCount"], 1);
    assert_eq!(
        env.titles(alice, json!({"starred": marshal_id("User", alice.get())}))
            .await
            .0,
        ["Refunds"]
    );
    assert_eq!(
        env.titles(bob, json!({"order": "NOTEBOOK_STAR_COUNT", "desc": true}))
            .await
            .0
            .len(),
        2
    );
    env.ok(alice, UNSTAR, json!({"id": refunds_id})).await;
    let seen = env.ok(alice, &node_q(), json!({"id": refunds_id})).await;
    assert_eq!(seen["node"]["viewerHasStarred"], false);

    // Bob manages the org notebook but may not read the private repository: his save keeps
    // the blocks he was not shown, in place, and cannot overwrite them.
    let seen = env.ok(bob, &node_q(), json!({"id": org_nb})).await;
    assert_eq!(ids(&seen["node"]), ["md", "q", "pub"]);
    assert_eq!(seen["node"]["viewerCanManage"], true);
    let mut vars = nb(
        "Org runbook v2",
        &org_ns,
        true,
        json!([
            {"id": "md", "type": "MARKDOWN", "markdownInput": "# Payments v2"},
            {"id": "pub", "type": "FILE", "fileInput": {"repositoryName": PUBLIC, "filePath": "x.go"}},
            {"id": "new", "type": "QUERY", "queryInput": "type:symbol Refund"},
            {"id": "sym", "type": "MARKDOWN", "markdownInput": "overwrite attempt"},
        ]),
    );
    vars["id"] = json!(org_nb);
    let updated = env.ok(bob, &update_q(), vars).await;
    assert_eq!(ids(&updated["updateNotebook"]), ["md", "pub", "new"]);
    assert_eq!(updated["updateNotebook"]["updater"]["username"], "bob");
    assert_eq!(updated["updateNotebook"]["public"], true);
    let full = env.ok(admin, &node_q(), json!({"id": org_nb})).await;
    assert_eq!(ids(&full["node"]), ["md", "pub", "priv", "sym", "new"]);
    assert_eq!(full["node"]["blocks"][3]["__typename"], "SymbolBlock");
    assert_eq!(full["node"]["blocks"][0]["markdownInput"], "# Payments v2");

    // Moving a notebook needs the right to manage both namespaces.
    let mut vars = nb("Moved", &alice_ns, true, json!([]));
    vars["id"] = json!(org_nb);
    assert_eq!(env.code(Some(bob), &update_q(), vars).await, "FORBIDDEN");

    // Delete.
    env.ok(alice, DELETE, json!({"id": private})).await;
    assert_eq!(
        env.code(Some(alice), DELETE, json!({"id": private})).await,
        "NOT_FOUND"
    );
    env.ok(bob, DELETE, json!({"id": org_nb})).await;
    assert_eq!(env.titles(admin, json!({})).await.1, 1);
}
