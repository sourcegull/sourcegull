//! Bulk changes over GraphQL against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): changeset and
//! bulk change spec uploads with validation errors and namespaces, server-side specs, the command-line
//! client operations, RBAC and site controls, hidden repositories in changeset and spec listings,
//! close/delete, and credentials checked against a `wiremock` GitLab.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{
    Actor, AuthVia, Authenticator, MemorySessionStore, PasswordParams, Permission, PermissionSet,
};
use sourcegull_authz::Checker;
use sourcegull_batches::store::{
    BatchChange, BatchChangeStore, BatchSpecStore, BatchesCredentialStore, ChangesetSpecStore,
    ChangesetStore, NewBatchChange, NewBatchSpec, NewChangeset, NewChangesetEvent,
    NewChangesetSpec,
};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, OrgRole, RepoId, UserId};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PUBLIC: &str = "gitlab.example.com/acme/public";
const OTHER: &str = "gitlab.example.com/acme/other";
const PRIVATE: &str = "gitlab.example.com/acme/private";

const SPEC: &str = "version: 2
name: hello-world
description: Add Hello World to READMEs
on:
  - repositoriesMatchingQuery: file:README.md
steps:
  - run: echo Hello >> README.md
    container: alpine:3
changesetTemplate:
  title: Hello World
  body: My first bulk change!
  branch: hello-world
  commit:
    message: Append Hello World to all README.md files
  published: false
";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
    keyring: Arc<Keyring>,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    bob: UserId,
    org: sourcegull_types::OrgId,
    public: RepoId,
    other: RepoId,
    private: RepoId,
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, private, external_id, external_service_type,
                 external_service_id, url)
             VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/', $4) RETURNING id",
        )
        .bind(name)
        .bind(private)
        .bind(ext)
        .bind(format!("https://{name}"))
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
}

async fn user(db: &Db, name: &str) -> UserId {
    let new = NewUser {
        username: name.into(),
        ..Default::default()
    };
    db.users().create(&new).await.unwrap().id
}

async fn setup() -> Option<Env> {
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
    let bob = user(&db, "bob").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    let public = insert_repo(&db, PUBLIC, "1", false).await;
    let other = insert_repo(&db, OTHER, "2", false).await;
    let private = insert_repo(&db, PRIVATE, "3", true).await;
    let provider = Arc::new(StaticKeyProvider::new("k1", [9u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: Some(keyring.clone()),
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        auth,
        config,
        keyring,
        schema,
        admin,
        alice,
        bob,
        org,
        public,
        other,
        private,
    })
}

fn repo_id(r: RepoId) -> String {
    marshal_id("Repository", i64::from(r.get()))
}

fn ns(u: UserId) -> String {
    marshal_id("User", u.get())
}

/// A branch changeset spec of `repo` in the JSON form command-line clients upload.
fn changeset_spec(repo: RepoId, branch: &str) -> String {
    json!({
        "baseRepository": repo_id(repo),
        "baseRef": "refs/heads/main",
        "baseRev": "0123456789abcdef0123456789abcdef01234567",
        "headRepository": repo_id(repo),
        "headRef": format!("refs/heads/{branch}"),
        "title": "Hello World",
        "body": "My first bulk change!",
        "commits": [{
            "message": "Append Hello World\n\nTo all README.md files.",
            "diff": "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Title\n+Hello World\n",
            "authorName": "Alice",
            "authorEmail": "alice@example.com"
        }],
        "published": false
    })
    .to_string()
}

impl Env {
    async fn actor(&self, user: UserId) -> Actor {
        self.auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap()
    }

    async fn run_as(&self, actor: Actor, query: &str, vars: Value) -> Response {
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
        let actor = self.actor(user).await;
        self.run_as(actor, query, vars).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, user: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(user, query, vars).await;
        first_error(query, &resp)
    }
}

fn first_error(query: &str, resp: &Response) -> (String, String) {
    let err = resp
        .errors
        .first()
        .unwrap_or_else(|| panic!("{query}: no error"));
    let code = err
        .extensions
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.clone().into_json().ok())
        .and_then(|c| c.as_str().map(str::to_owned))
        .unwrap_or_default();
    (code, err.message.clone())
}

const CREATE_CHANGESET_SPEC: &str = "mutation($spec: String!) {
    createChangesetSpec(changesetSpec: $spec) {
        __typename id type
        ... on VisibleChangesetSpec {
            expiresAt
            description {
                __typename
                ... on GitBranchChangesetDescription {
                    baseRepository { name } baseRef headRef title published
                    diffStat { added deleted }
                    commits { subject body author { name email } }
                }
            }
        }
    }
}";

const CREATE_BATCH_SPEC: &str = "mutation($namespace: ID!, $spec: String!, $ids: [ID!]!) {
    createBatchSpec(namespace: $namespace, batchSpec: $spec, changesetSpecs: $ids) {
        id applyURL state source
        description { name description }
        namespace { id url }
        creator { username }
        viewerCanAdminister
        diffStat { added deleted }
        changesetSpecs { totalCount nodes { __typename id } }
    }
}";

const FROM_RAW: &str = "mutation($spec: String!, $namespace: ID!, $bc: ID!) {
    createBatchSpecFromRaw(batchSpec: $spec, namespace: $namespace, batchChange: $bc,
                           allowIgnored: true) {
        id state source applyURL originalInput allowIgnored allowUnsupported noCache
        parsedInput
        workspaceResolution { state workspaces { totalCount stats { queued ignored } } }
    }
}";

const EMPTY: &str = "mutation($namespace: ID!, $name: String!) {
    createEmptyBatchChange(namespace: $namespace, name: $name) {
        id name state url creator { username } currentSpec { id originalInput }
        viewerCanAdminister
    }
}";

const LIST: &str = "query { batchChanges { totalCount nodes { name state } } }";

#[tokio::test]
async fn specs_namespaces_and_validation() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (alice, bob, admin) = (env.alice, env.bob, env.admin);

    // Changeset specs: validated, stored with diff stats, only in readable repositories.
    let data = env
        .ok(
            alice,
            CREATE_CHANGESET_SPEC,
            json!({"spec": changeset_spec(env.public, "hello")}),
        )
        .await;
    let cs = &data["createChangesetSpec"];
    assert_eq!(cs["__typename"], "VisibleChangesetSpec");
    assert_eq!(cs["type"], "BRANCH");
    assert!(cs["expiresAt"].is_string());
    let desc = &cs["description"];
    assert_eq!(desc["__typename"], "GitBranchChangesetDescription");
    assert_eq!(desc["baseRepository"]["name"], PUBLIC);
    assert_eq!(desc["baseRef"], "main");
    assert_eq!(desc["headRef"], "hello");
    assert_eq!(desc["published"], false);
    assert_eq!(desc["diffStat"], json!({"added": 1, "deleted": 0}));
    assert_eq!(desc["commits"][0]["subject"], "Append Hello World");
    assert_eq!(desc["commits"][0]["body"], "To all README.md files.");
    assert_eq!(desc["commits"][0]["author"]["email"], "alice@example.com");
    let alice_cs = cs["id"].as_str().unwrap().to_owned();

    let (code, _) = env
        .err(
            alice,
            CREATE_CHANGESET_SPEC,
            json!({"spec": changeset_spec(env.private, "x")}),
        )
        .await;
    assert_eq!(code, "NOT_FOUND", "a hidden repository is not found");
    let mut bad = serde_json::from_str::<Value>(&changeset_spec(env.public, "x")).unwrap();
    bad["headRef"] = json!("not-a-ref");
    let (code, msg) = env
        .err(
            alice,
            CREATE_CHANGESET_SPEC,
            json!({"spec": bad.to_string()}),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    assert!(msg.contains("headRef"), "{msg}");

    // createChangesetSpecs: all or nothing.
    let data = env
        .ok(
            bob,
            "mutation($specs: [String!]!) { createChangesetSpecs(changesetSpecs: $specs) { id } }",
            json!({"specs": [changeset_spec(env.public, "b1"), changeset_spec(env.other, "b2")]}),
        )
        .await;
    let bob_cs = data["createChangesetSpecs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = ChangesetSpecStore::new(&env.db)
        .count(&Default::default())
        .await
        .unwrap();
    let resp = env
        .run(
            bob,
            "mutation($specs: [String!]!) { createChangesetSpecs(changesetSpecs: $specs) { id } }",
            json!({"specs": [changeset_spec(env.public, "b3"), "{}"]}),
        )
        .await;
    assert!(!resp.errors.is_empty());
    assert_eq!(
        ChangesetSpecStore::new(&env.db)
            .count(&Default::default())
            .await
            .unwrap(),
        before
    );

    // Bulk change specs: errors carry positions.
    let invalid = "name: hello\non:\n  - repository: a\n    branch: [1]\nbogus: true\n";
    let resp = env
        .run(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": ns(alice), "spec": invalid, "ids": []}),
        )
        .await;
    let (code, msg) = first_error(CREATE_BATCH_SPEC, &resp);
    assert_eq!(code, "VALIDATION");
    assert!(msg.contains("line "), "{msg}");
    let ext = resp.errors[0].extensions.as_ref().unwrap();
    let issues = ext.get("issues").unwrap().clone().into_json().unwrap();
    assert!(
        issues
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["line"].is_number() && i["column"].is_number()),
        "{issues}"
    );

    // Namespaces: one's own, an org one belongs to; not someone else's.
    let (code, _) = env
        .err(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": ns(bob), "spec": SPEC, "ids": []}),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    let org_ns = marshal_id("Org", env.org.get());
    let (code, _) = env
        .err(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": org_ns, "spec": SPEC, "ids": []}),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    env.db
        .orgs()
        .add_member(env.org, alice, OrgRole::Member)
        .await
        .unwrap();
    let data = env
        .ok(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": org_ns, "spec": SPEC, "ids": []}),
        )
        .await;
    assert_eq!(
        data["createBatchSpec"]["namespace"]["url"],
        "/organizations/acme"
    );

    // Someone else's changeset spec is "not found".
    let (code, _) = env
        .err(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": ns(alice), "spec": SPEC, "ids": [bob_cs]}),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");

    let data = env
        .ok(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": ns(alice), "spec": SPEC, "ids": [alice_cs]}),
        )
        .await;
    let spec = &data["createBatchSpec"];
    let spec_id = spec["id"].as_str().unwrap().to_owned();
    assert_eq!(spec["state"], "COMPLETED");
    assert_eq!(spec["source"], "LOCAL");
    assert_eq!(
        spec["applyURL"],
        format!("/users/alice/bulk-changes/apply/{spec_id}")
    );
    assert_eq!(spec["description"]["name"], "hello-world");
    assert_eq!(spec["creator"]["username"], "alice");
    assert_eq!(spec["viewerCanAdminister"], true);
    assert_eq!(spec["diffStat"], json!({"added": 1, "deleted": 0}));
    assert_eq!(spec["changesetSpecs"]["totalCount"], 1);
    // A spec cannot be attached twice.
    let (code, _) = env
        .err(
            alice,
            CREATE_BATCH_SPEC,
            json!({"namespace": ns(alice), "spec": SPEC, "ids": [alice_cs]}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");

    // Anyone may look a spec up by its ID (preview links); only the namespace administers it.
    let data = env
        .ok(
            bob,
            "query($id: ID!) { node(id: $id) { ... on BatchSpec {
                originalInput viewerCanAdminister parsedInput } } }",
            json!({"id": spec_id}),
        )
        .await;
    assert_eq!(data["node"]["originalInput"], SPEC);
    assert_eq!(data["node"]["viewerCanAdminister"], false);
    assert_eq!(data["node"]["parsedInput"]["name"], "hello-world");

    // Empty bulk changes (server-side flow).
    let data = env
        .ok(
            alice,
            EMPTY,
            json!({"namespace": ns(alice), "name": "hello-world"}),
        )
        .await;
    let bc = &data["createEmptyBatchChange"];
    assert_eq!(bc["state"], "DRAFT");
    assert_eq!(bc["url"], "/users/alice/bulk-changes/hello-world");
    assert_eq!(bc["currentSpec"]["originalInput"], "name: hello-world\n");
    let bc_id = bc["id"].as_str().unwrap().to_owned();
    let (code, _) = env
        .err(
            alice,
            EMPTY,
            json!({"namespace": ns(alice), "name": "hello-world"}),
        )
        .await;
    assert_eq!(code, "ALREADY_EXISTS");
    let (code, _) = env
        .err(
            alice,
            EMPTY,
            json!({"namespace": ns(alice), "name": "not valid!"}),
        )
        .await;
    assert_eq!(code, "ErrBatchChangeInvalidName");

    // Drafts are listed to their creator and site admins only.
    let names = |v: &Value| v["batchChanges"]["totalCount"].as_i64().unwrap();
    assert_eq!(names(&env.ok(alice, LIST, json!({})).await), 1);
    assert_eq!(names(&env.ok(admin, LIST, json!({})).await), 1);
    assert_eq!(names(&env.ok(bob, LIST, json!({})).await), 0);
    let data = env
        .ok(
            alice,
            "query($ns: ID!) { batchChange(namespace: $ns, name: \"hello-world\") { id } }",
            json!({"ns": ns(alice)}),
        )
        .await;
    assert_eq!(data["batchChange"]["id"], bc_id.as_str());

    // A server-side spec: validated, stored and queued for resolution.
    let data = env
        .ok(
            alice,
            FROM_RAW,
            json!({"spec": SPEC, "namespace": ns(alice), "bc": bc_id}),
        )
        .await;
    let raw = &data["createBatchSpecFromRaw"];
    assert_eq!(raw["state"], "PENDING");
    assert_eq!(raw["source"], "REMOTE");
    assert_eq!(raw["applyURL"], Value::Null);
    assert_eq!(raw["allowIgnored"], true);
    assert_eq!(raw["allowUnsupported"], false);
    assert_eq!(raw["parsedInput"]["steps"][0]["container"], "alpine:3");
    assert_eq!(raw["workspaceResolution"]["state"], "QUEUED");
    assert_eq!(raw["workspaceResolution"]["workspaces"]["totalCount"], 0);
    let raw_id = raw["id"].as_str().unwrap().to_owned();
    let (code, _) = env
        .err(
            bob,
            FROM_RAW,
            json!({"spec": SPEC, "namespace": ns(bob), "bc": bc_id}),
        )
        .await;
    assert_eq!(
        code, "FORBIDDEN",
        "bob may not add specs to alice's bulk change"
    );

    // Replacing the input keeps the ID and queues the resolution again.
    let replaced = SPEC.replace("Hello World to READMEs", "a greeting");
    let data = env
        .ok(
            alice,
            "mutation($id: ID!, $spec: String!) {
                replaceBatchSpecInput(previousSpec: $id, batchSpec: $spec, noCache: true) {
                    id originalInput noCache description { description } } }",
            json!({"id": raw_id, "spec": replaced}),
        )
        .await;
    let r = &data["replaceBatchSpecInput"];
    assert_eq!(r["id"], raw_id.as_str());
    assert_eq!(r["noCache"], true);
    assert_eq!(r["description"]["description"], "Add a greeting");
    let (code, _) = env
        .err(
            alice,
            "mutation($id: ID!) { replaceBatchSpecInput(previousSpec: $id, batchSpec: \"name: [\") { id } }",
            json!({"id": raw_id}),
        )
        .await;
    assert_eq!(code, "VALIDATION");

    // The bulk change's server-side specs.
    let data = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { ... on BatchChange {
                batchSpecs { totalCount nodes { id } }
                all: batchSpecs(includeLocallyExecutedSpecs: true, excludeEmptySpecs: true) { totalCount } } } }",
            json!({"id": bc_id}),
        )
        .await;
    assert_eq!(data["node"]["batchSpecs"]["totalCount"], 1);
    assert_eq!(data["node"]["all"]["totalCount"], 1);

    // deleteBatchSpec: site admins, and not a bulk change's current spec.
    const DELETE_SPEC: &str =
        "mutation($id: ID!) { deleteBatchSpec(batchSpec: $id) { alwaysNil } }";
    let (code, _) = env.err(alice, DELETE_SPEC, json!({"id": spec_id})).await;
    assert_eq!(code, "FORBIDDEN");
    env.ok(admin, DELETE_SPEC, json!({"id": spec_id})).await;
    let data = env
        .ok(
            admin,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({"id": spec_id}),
        )
        .await;
    assert_eq!(data["node"], Value::Null);
    let current = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { ... on BatchChange { currentSpec { id } } } }",
            json!({"id": bc_id}),
        )
        .await;
    let (code, _) = env
        .err(
            admin,
            DELETE_SPEC,
            json!({"id": current["node"]["currentSpec"]["id"]}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
}

/// The operations a command-line client sends, verbatim.
#[tokio::test]
async fn cli_operations() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let alice = env.alice;
    let data = env
        .ok(
            alice,
            "query InstanceInfo {
                site { productVersion }
                maxUnlicensedChangesets
                batchChanges(first: 1) { nodes { id } }
            }",
            json!({}),
        )
        .await;
    assert_eq!(data["site"]["productVersion"], env!("CARGO_PKG_VERSION"));
    let data = env
        .ok(
            alice,
            "query GetCurrentUserID { currentUser { id url } }",
            json!({}),
        )
        .await;
    let namespace = data["currentUser"]["id"].as_str().unwrap().to_owned();
    assert_eq!(data["currentUser"]["url"], "/users/alice");
    let data = env
        .ok(
            alice,
            "query NamespaceQuery($name: String!) {
                user(username: $name) { id url }
                organization(name: $name) { id url }
            }",
            json!({"name": "alice"}),
        )
        .await;
    assert_eq!(data["user"]["id"], namespace.as_str());

    let data = env
        .ok(
            alice,
            "mutation CreateChangesetSpec($spec: String!) {
                createChangesetSpec(changesetSpec: $spec) {
                    ... on HiddenChangesetSpec { id }
                    ... on VisibleChangesetSpec { id }
                }
            }",
            json!({"spec": changeset_spec(env.public, "src")}),
        )
        .await;
    let cs = data["createChangesetSpec"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let data = env
        .ok(
            alice,
            "mutation CreateBatchSpec($namespace: ID!, $spec: String!, $changesetSpecs: [ID!]!) {
                createBatchSpec(namespace: $namespace, batchSpec: $spec,
                                changesetSpecs: $changesetSpecs) {
                    id
                    applyURL
                }
            }",
            json!({"namespace": namespace, "spec": SPEC, "changesetSpecs": [cs]}),
        )
        .await;
    assert!(
        data["createBatchSpec"]["applyURL"]
            .as_str()
            .unwrap()
            .starts_with("/users/alice/bulk-changes/apply/")
    );

    // Remote execution: upsert the bulk change, then create the spec from raw.
    const UPSERT: &str = "mutation UpsertEmptyBatchChange($name: String!, $namespace: ID!) {
        upsertEmptyBatchChange(name: $name, namespace: $namespace) { id name }
    }";
    let first = env
        .ok(
            alice,
            UPSERT,
            json!({"name": "remote", "namespace": namespace}),
        )
        .await;
    let again = env
        .ok(
            alice,
            UPSERT,
            json!({"name": "remote", "namespace": namespace}),
        )
        .await;
    assert_eq!(
        first["upsertEmptyBatchChange"]["id"],
        again["upsertEmptyBatchChange"]["id"]
    );
    let data = env
        .ok(
            alice,
            "mutation CreateBatchSpecFromRaw($batchSpec: String!, $namespace: ID!,
                $allowIgnored: Boolean!, $allowUnsupported: Boolean!, $noCache: Boolean!,
                $batchChange: ID!) {
                createBatchSpecFromRaw(batchSpec: $batchSpec, namespace: $namespace,
                    allowIgnored: $allowIgnored, allowUnsupported: $allowUnsupported,
                    noCache: $noCache, batchChange: $batchChange) { id }
            }",
            json!({
                "batchSpec": SPEC.replace("hello-world", "remote"),
                "namespace": namespace,
                "allowIgnored": false,
                "allowUnsupported": true,
                "noCache": false,
                "batchChange": first["upsertEmptyBatchChange"]["id"],
            }),
        )
        .await;
    assert!(data["createBatchSpecFromRaw"]["id"].is_string());

    // Nullable variables in defaulted positions, as web clients send them.
    env.ok(
        alice,
        "query($first: Int, $after: String) { batchChanges(first: $first, after: $after) { totalCount } }",
        json!({"first": null, "after": null}),
    )
    .await;
}

#[tokio::test]
async fn rbac_and_site_controls() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let alice = env.actor(env.alice).await;
    assert!(alice.has(Permission::BatchChangesRead) && alice.has(Permission::BatchChangesWrite));
    let with = |perms: &[Permission]| Actor {
        permissions: perms.iter().copied().collect::<PermissionSet>(),
        ..alice.clone()
    };
    let create = |a: Actor| {
        env.run_as(
            a,
            EMPTY,
            json!({"namespace": ns(env.alice), "name": "rbac"}),
        )
    };

    // Without BATCH_CHANGES#READ nothing is readable; with READ only, nothing is writable.
    let resp = env.run_as(with(&[]), LIST, json!({})).await;
    assert_eq!(first_error(LIST, &resp).0, "FORBIDDEN");
    let resp = create(with(&[Permission::BatchChangesRead])).await;
    let (code, msg) = first_error(EMPTY, &resp);
    assert_eq!(code, "FORBIDDEN");
    assert!(msg.contains("BATCH_CHANGES#WRITE"), "{msg}");
    let resp = env
        .run_as(with(&[Permission::BatchChangesRead]), LIST, json!({}))
        .await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let resp = create(alice.clone()).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);

    // restrictToAdmins, then bulk changes off.
    env.config.store(
        2,
        SiteConfig {
            batch_changes_restrict_to_admins: true,
            ..SiteConfig::default()
        },
    );
    let (code, _) = env.err(env.alice, LIST, json!({})).await;
    assert_eq!(code, "ErrBatchChangesDisabledForUser");
    env.ok(env.admin, LIST, json!({})).await;
    env.config.store(
        3,
        SiteConfig {
            batch_changes_enabled: false,
            ..SiteConfig::default()
        },
    );
    let (code, _) = env.err(env.admin, LIST, json!({})).await;
    assert_eq!(code, "ErrBatchChangesDisabled");
    let (code, _) = env
        .err(
            env.alice,
            "query { currentUser { batchChangesCodeHosts { totalCount } } }",
            json!({}),
        )
        .await;
    assert_eq!(code, "ErrBatchChangesDisabled");
    env.config.store(4, SiteConfig::default());
    env.ok(env.alice, LIST, json!({})).await;

    // Anonymous viewers get nothing.
    let req = Request::new(LIST);
    let resp = sourcegull_graphql::execute(&env.schema, req, None).await;
    assert_eq!(first_error(LIST, &resp).0, "UNAUTHORIZED");
}

/// An applied bulk change of alice's with changesets in a public, another public and a
/// private repository.
async fn fixture(env: &Env) -> (BatchChange, Vec<i64>) {
    let db = &env.db;
    let spec = BatchSpecStore::new(db)
        .create(&NewBatchSpec {
            raw_spec: SPEC.into(),
            spec: json!({"name": "fixture"}),
            namespace: sourcegull_db::Namespace::User(env.alice),
            user_id: Some(env.alice),
            created_from_raw: false,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap();
    let bc = BatchChangeStore::new(db)
        .create(&NewBatchChange {
            name: "fixture".into(),
            description: "Fixture".into(),
            namespace: sourcegull_db::Namespace::User(env.alice),
            creator_id: Some(env.alice),
            batch_spec_id: spec.id,
            applied: true,
        })
        .await
        .unwrap();
    // (repo, title, external state, review state, external id)
    let rows = [
        (env.public, "Fix the README", "OPEN", "APPROVED", "11"),
        (env.public, "Fix the docs", "MERGED", "APPROVED", "12"),
        (env.other, "Bump deps", "OPEN", "PENDING", "13"),
        (env.private, "Fix secrets", "OPEN", "APPROVED", "14"),
    ];
    let mut ids = Vec::new();
    for (repo, title, state, review, ext) in rows {
        let mut s =
            NewChangesetSpec::new(repo, sourcegull_batches::store::ChangesetSpecType::Branch);
        s.batch_spec_id = Some(spec.id);
        s.user_id = Some(env.alice);
        s.head_ref = Some("refs/heads/fix".into());
        s.base_ref = Some("refs/heads/main".into());
        s.title = Some(title.into());
        s.diff_stat_added = Some(3);
        s.diff_stat_deleted = Some(1);
        let cs = ChangesetSpecStore::new(db).create(&s).await.unwrap();
        let mut new = NewChangeset::new(repo, "gitlab");
        new.batch_change_id = Some(bc.id);
        new.owned_by_batch_change_id = Some(bc.id);
        new.current_spec_id = Some(cs.id);
        let c = ChangesetStore::new(db).create(&new).await.unwrap();
        sourcegull_db::sqlx::query(
            "UPDATE changesets SET state = 'completed', publication_state = 'PUBLISHED',
                 external_state = $2, external_review_state = $3, external_check_state = 'PASSED',
                 external_id = $4, title = $5, diff_stat_added = 3, diff_stat_deleted = 1,
                 metadata = $6
             WHERE id = $1",
        )
        .bind(c.id)
        .bind(state)
        .bind(review)
        .bind(ext)
        .bind(title)
        .bind(json!({"mergeRequest": {
            "web_url": format!("https://gitlab.example.com/mr/{ext}"),
            "author": {"name": "Alice A."},
            "labels": ["batch"]
        }}))
        .execute(db.pool())
        .await
        .unwrap();
        ids.push(c.id);
    }
    (bc, ids)
}

const CHANGESETS: &str =
    "query($id: ID!, $first: Int, $after: String, $review: ChangesetReviewState,
                                 $search: String, $state: ChangesetState, $closable: Boolean,
                                 $repo: ID) {
    node(id: $id) { ... on BatchChange {
        changesetsStats { total open merged closed archived isCompleted percentComplete }
        diffStat { added deleted }
        changesets(first: $first, after: $after, reviewState: $review, search: $search,
                   state: $state, onlyClosable: $closable, repo: $repo) {
            totalCount
            pageInfo { hasNextPage endCursor }
            nodes {
                __typename id state
                ... on ExternalChangeset {
                    title externalID reviewState checkState repository { name }
                    externalURL { url serviceKind } author { name } labels { text }
                    diffStat { added deleted } ownedByBatchChange
                    currentSpec { id }
                    batchChanges { totalCount nodes { name } }
                }
            }
        }
    } }
}";

#[tokio::test]
async fn changesets_hidden_repositories_close_and_delete() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (bc, ids) = fixture(&env).await;
    let bc_id = marshal_id("BatchChange", bc.id);
    let env = &env;
    let list = |user: UserId, vars: Value| {
        let mut v = json!({"id": bc_id});
        v.as_object_mut()
            .unwrap()
            .extend(vars.as_object().unwrap().clone());
        async move { env.ok(user, CHANGESETS, v).await }
    };

    // Unfiltered: every changeset, the private one hidden; stats count it too.
    let data = list(env.alice, json!({})).await;
    let node = &data["node"];
    assert_eq!(node["changesetsStats"]["total"], 4);
    assert_eq!(node["changesetsStats"]["open"], 3);
    assert_eq!(node["changesetsStats"]["merged"], 1);
    assert_eq!(node["changesetsStats"]["isCompleted"], false);
    assert_eq!(node["changesetsStats"]["percentComplete"], 25);
    assert_eq!(node["diffStat"], json!({"added": 9, "deleted": 3}));
    let cs = &node["changesets"];
    assert_eq!(cs["totalCount"], 4);
    let nodes = cs["nodes"].as_array().unwrap();
    let hidden: Vec<_> = nodes
        .iter()
        .filter(|n| n["__typename"] == "HiddenExternalChangeset")
        .collect();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0]["state"], "OPEN");
    assert!(hidden[0].get("title").is_none());
    let first = &nodes[0];
    assert_eq!(first["__typename"], "ExternalChangeset");
    assert_eq!(first["title"], "Fix the README");
    assert_eq!(first["externalID"], "11");
    assert_eq!(first["reviewState"], "APPROVED");
    assert_eq!(first["checkState"], "PASSED");
    assert_eq!(first["repository"]["name"], PUBLIC);
    assert_eq!(
        first["externalURL"]["url"],
        "https://gitlab.example.com/mr/11"
    );
    assert_eq!(first["externalURL"]["serviceKind"], "GITLAB");
    assert_eq!(first["author"]["name"], "Alice A.");
    assert_eq!(first["labels"][0]["text"], "batch");
    assert_eq!(first["ownedByBatchChange"], bc_id.as_str());
    assert!(first["currentSpec"]["id"].is_string());
    assert_eq!(first["batchChanges"]["nodes"][0]["name"], "fixture");

    // The admin sees every repository.
    let data = list(env.admin, json!({})).await;
    assert!(
        data["node"]["changesets"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["__typename"] == "ExternalChangeset")
    );
    assert_eq!(data["node"]["diffStat"], json!({"added": 12, "deleted": 4}));

    // Filters that could reveal a hidden changeset leave it out (nodes and count).
    let data = list(env.alice, json!({"review": "APPROVED"})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 2);
    let data = list(env.alice, json!({"search": "Fix"})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 2);
    let data = list(env.alice, json!({"search": "secrets"})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 0);
    let data = list(env.admin, json!({"search": "secrets"})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 1);
    let private = json!({"repo": repo_id(env.private)});
    let data = list(env.alice, private.clone()).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 0);
    let data = list(env.admin, private).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 1);
    let data = list(env.alice, json!({"repo": repo_id(env.public)})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 2);
    // A state filter is safe: hidden ones stay (hidden).
    let data = list(env.alice, json!({"state": "OPEN"})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 3);
    let data = list(env.alice, json!({"closable": true})).await;
    assert_eq!(data["node"]["changesets"]["totalCount"], 3);
    let resp = env
        .run(
            env.alice,
            CHANGESETS,
            json!({"id": bc_id, "closable": true, "state": "OPEN"}),
        )
        .await;
    assert_eq!(first_error(CHANGESETS, &resp).0, "INVALID_INPUT");

    // Pagination by id cursors.
    let page1 = list(env.alice, json!({"first": 3})).await;
    let p = &page1["node"]["changesets"];
    assert_eq!(p["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(p["pageInfo"]["hasNextPage"], true);
    let cursor = p["pageInfo"]["endCursor"].clone();
    let page2 = list(env.alice, json!({"first": 3, "after": cursor})).await;
    let p2 = &page2["node"]["changesets"];
    assert_eq!(p2["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(p2["pageInfo"]["hasNextPage"], false);
    assert_eq!(p2["totalCount"], 4);
    let resp = env
        .run(env.alice, CHANGESETS, json!({"id": bc_id, "first": 10001}))
        .await;
    assert_eq!(first_error(CHANGESETS, &resp).0, "ErrInvalidFirstParameter");

    // node(id:) of a changeset: hidden in a private repository.
    let hidden_id = marshal_id("Changeset", ids[3]);
    let q = "query($id: ID!) { node(id: $id) { __typename id
        ... on HiddenExternalChangeset { state batchChanges { totalCount } } } }";
    let data = env.ok(env.alice, q, json!({"id": hidden_id})).await;
    assert_eq!(data["node"]["__typename"], "HiddenExternalChangeset");
    assert_eq!(data["node"]["batchChanges"]["totalCount"], 1);
    let data = env.ok(env.admin, q, json!({"id": hidden_id})).await;
    assert_eq!(data["node"]["__typename"], "ExternalChangeset");

    // The current spec's changeset specs: hidden in the private repository.
    let data = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { ... on BatchChange { currentSpec {
                changesetSpecs(first: 10) { totalCount nodes { __typename type } } } } } }",
            json!({"id": bc_id}),
        )
        .await;
    let specs = &data["node"]["currentSpec"]["changesetSpecs"];
    assert_eq!(specs["totalCount"], 4);
    let hidden_specs = specs["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["__typename"] == "HiddenChangesetSpec")
        .count();
    assert_eq!(hidden_specs, 1);

    // Events of a visible changeset.
    ChangesetStore::new(&env.db)
        .upsert_events(
            ids[0],
            &[
                NewChangesetEvent {
                    kind: "APPROVED".into(),
                    key: "note:1".into(),
                    created_at: chrono::Utc::now(),
                    metadata: json!({}),
                },
                NewChangesetEvent {
                    kind: "COMMENTED".into(),
                    key: "note:2".into(),
                    created_at: chrono::Utc::now(),
                    metadata: json!({}),
                },
            ],
        )
        .await
        .unwrap();
    let data = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { ... on ExternalChangeset {
                events(first: 1) { totalCount pageInfo { hasNextPage }
                    nodes { id createdAt changeset { id } } } } } }",
            json!({"id": marshal_id("Changeset", ids[0])}),
        )
        .await;
    let events = &data["node"]["events"];
    assert_eq!(events["totalCount"], 2);
    assert_eq!(events["pageInfo"]["hasNextPage"], true);
    assert_eq!(
        events["nodes"][0]["changeset"]["id"],
        marshal_id("Changeset", ids[0])
    );

    // viewerCanAdminister and listings by namespace.
    let q =
        "query($id: ID!) { node(id: $id) { ... on BatchChange { viewerCanAdminister state } } }";
    assert_eq!(
        env.ok(env.bob, q, json!({"id": bc_id})).await["node"]["viewerCanAdminister"],
        false
    );
    let data = env
        .ok(
            env.bob,
            "query { user(username: \"alice\") { batchChanges(viewerCanAdminister: true) { totalCount }
                     all: batchChanges { totalCount nodes { state } } } }",
            json!({}),
        )
        .await;
    assert_eq!(data["user"]["batchChanges"]["totalCount"], 0);
    assert_eq!(data["user"]["all"]["totalCount"], 1);

    // Close: the creator or a site admin; closing changesets marks the open ones it owns.
    const CLOSE: &str =
        "mutation($id: ID!) { closeBatchChange(batchChange: $id, closeChangesets: true) {
        state closedAt } }";
    let (code, _) = env.err(env.bob, CLOSE, json!({"id": bc_id})).await;
    assert_eq!(code, "FORBIDDEN");
    let data = env.ok(env.alice, CLOSE, json!({"id": bc_id})).await;
    assert_eq!(data["closeBatchChange"]["state"], "CLOSED");
    assert!(data["closeBatchChange"]["closedAt"].is_string());
    let closing: Vec<i64> =
        sourcegull_db::sqlx::query_scalar("SELECT id FROM changesets WHERE closing ORDER BY id")
            .fetch_all(env.db.pool())
            .await
            .unwrap();
    assert_eq!(closing, vec![ids[0], ids[2], ids[3]]);
    // Closing again is a no-op.
    env.ok(env.alice, CLOSE, json!({"id": bc_id})).await;

    // Delete: the creator or a site admin; the changesets stay.
    const DELETE: &str = "mutation($id: ID!) { deleteBatchChange(batchChange: $id) { alwaysNil } }";
    let (code, _) = env.err(env.bob, DELETE, json!({"id": bc_id})).await;
    assert_eq!(code, "FORBIDDEN");
    env.ok(env.alice, DELETE, json!({"id": bc_id})).await;
    let data = env.ok(env.alice, q, json!({"id": bc_id})).await;
    assert_eq!(data["node"], Value::Null);
    assert!(
        ChangesetStore::new(&env.db)
            .get(ids[0])
            .await
            .unwrap()
            .is_some()
    );
    let (code, _) = env.err(env.alice, DELETE, json!({"id": bc_id})).await;
    assert_eq!(code, "NOT_FOUND");
}

#[tokio::test]
async fn scheduled_changesets_have_an_estimate() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (_, ids) = fixture(&env).await;
    let query = "query($id: ID!) { node(id: $id) { ... on ExternalChangeset {
        state scheduleEstimateAt } } }";
    let estimate = |id: i64| {
        let env = &env;
        async move {
            env.ok(env.alice, query, json!({"id": marshal_id("Changeset", id)}))
                .await["node"]
                .clone()
        }
    };
    // Not scheduled: no estimate.
    let n = estimate(ids[0]).await;
    assert_eq!(n["state"], "OPEN");
    assert_eq!(n["scheduleEstimateAt"], Value::Null);

    // Two scheduled changesets and a window releasing one per hour, open all the time.
    ChangesetStore::new(&env.db)
        .schedule(&[ids[0]])
        .await
        .unwrap();
    ChangesetStore::new(&env.db)
        .schedule(&[ids[2]])
        .await
        .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE changesets SET updated_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(ids[0])
    .execute(env.db.pool())
    .await
    .unwrap();
    // Without windows nothing waits: no estimate.
    assert_eq!(estimate(ids[0]).await["scheduleEstimateAt"], Value::Null);
    env.config.store(
        2,
        SiteConfig {
            batch_changes_rollout_windows: serde_json::from_value(json!([{"rate": "1/hour"}]))
                .unwrap(),
            ..SiteConfig::default()
        },
    );
    let parse = |v: &Value| {
        chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    let now = chrono::Utc::now();
    let first = estimate(ids[0]).await;
    assert_eq!(first["state"], "SCHEDULED");
    let first = parse(&first["scheduleEstimateAt"]);
    assert!((first - now).num_seconds().abs() < 60, "{first}");
    let second = parse(&estimate(ids[2]).await["scheduleEstimateAt"]);
    let gap = second - first - chrono::Duration::hours(1);
    assert!(gap.num_seconds().abs() < 60, "{first} {second}");
}

async fn gitlab() -> MockServer {
    let server = MockServer::start().await;
    let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", "good-token"))
        .respond_with(ok(json!({"id": 1, "username": "alice", "name": "Alice"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/personal_access_tokens/self"))
        .and(header("PRIVATE-TOKEN", "good-token"))
        .respond_with(ok(json!({"scopes": ["api"]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", "readonly-token"))
        .respond_with(ok(json!({"id": 2, "username": "ro", "name": "RO"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/personal_access_tokens/self"))
        .and(header("PRIVATE-TOKEN", "readonly-token"))
        .respond_with(ok(json!({"scopes": ["read_api"]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    server
}

const CREATE_CREDENTIAL: &str = "mutation($user: ID, $url: String!, $token: String!) {
    createBatchChangesCredential(user: $user, externalServiceKind: GITLAB,
                                 externalServiceURL: $url, credential: $token) {
        id externalServiceKind externalServiceURL isSiteCredential sshPublicKey createdAt
    }
}";

const CODE_HOSTS: &str = "query($name: String!) {
    user(username: $name) { batchChangesCodeHosts { totalCount nodes {
        externalServiceKind externalServiceURL requiresSSH requiresUsername hasWebhooks
        credential { id isSiteCredential } } } }
    batchChangesCodeHosts { totalCount nodes { credential { isSiteCredential } } }
}";

#[tokio::test]
async fn credentials_validated_and_encrypted() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let server = gitlab().await;
    env.db
        .external_services(Some(env.keyring.clone()))
        .create(&sourcegull_db::NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: format!(
                r#"{{"url": "{}", "token": "connection-token", "projectQuery": ["none"],
                    "webhooks": [{{"secret": "s3cret"}}]}}"#,
                server.uri()
            ),
        })
        .await
        .unwrap();
    let url = format!("{}/", server.uri());
    let (alice, bob, admin) = (env.alice, env.bob, env.admin);
    let alice_id = ns(alice);

    let data = env.ok(alice, CODE_HOSTS, json!({"name": "alice"})).await;
    let hosts = &data["user"]["batchChangesCodeHosts"];
    assert_eq!(hosts["totalCount"], 1);
    let host = &hosts["nodes"][0];
    assert_eq!(host["externalServiceKind"], "GITLAB");
    assert_eq!(host["externalServiceURL"], url.as_str());
    assert_eq!(host["requiresSSH"], false);
    assert_eq!(host["requiresUsername"], false);
    assert_eq!(host["hasWebhooks"], true);
    assert_eq!(host["credential"], Value::Null);
    let (code, _) = env.err(bob, CODE_HOSTS, json!({"name": "alice"})).await;
    assert_eq!(code, "FORBIDDEN");

    // Tokens the code host rejects (or without the api scope) are not stored.
    for token in ["bad-token", "readonly-token"] {
        let (code, msg) = env
            .err(
                alice,
                CREATE_CREDENTIAL,
                json!({"user": alice_id, "url": url, "token": token}),
            )
            .await;
        assert_eq!(code, "ErrVerifyCredentialFailed", "{token}: {msg}");
    }
    let (code, _) = env
        .err(
            alice,
            CREATE_CREDENTIAL,
            json!({"user": alice_id, "url": url, "token": "  "}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    let (code, _) = env
        .err(
            alice,
            CREATE_CREDENTIAL,
            json!({"user": alice_id, "url": "https://gitlab.unknown.example/", "token": "good-token"}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    let (code, _) = env
        .err(
            bob,
            CREATE_CREDENTIAL,
            json!({"user": alice_id, "url": url, "token": "good-token"}),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    // Changesets are published to GitLab only: other hosts' credentials are invalid input.
    for kind in ["GITHUB", "GITEA"] {
        let (code, message) = env
            .err(
                alice,
                &CREATE_CREDENTIAL.replace("GITLAB", kind),
                json!({"user": alice_id, "url": "https://github.com/", "token": "good-token"}),
            )
            .await;
        assert_eq!(code, "INVALID_INPUT");
        assert!(message.contains("GitLab only"), "{message}");
    }

    // A user credential (the URL as typed, without the trailing slash).
    let data = env
        .ok(
            alice,
            CREATE_CREDENTIAL,
            json!({"user": alice_id, "url": server.uri(), "token": " good-token "}),
        )
        .await;
    let cred = &data["createBatchChangesCredential"];
    assert_eq!(cred["isSiteCredential"], false);
    assert_eq!(cred["externalServiceURL"], url.as_str());
    assert_eq!(cred["sshPublicKey"], Value::Null);
    let cred_id = cred["id"].as_str().unwrap().to_owned();
    let (code, _) = env
        .err(
            alice,
            CREATE_CREDENTIAL,
            json!({"user": alice_id, "url": url, "token": "good-token"}),
        )
        .await;
    assert_eq!(code, "ErrDuplicateCredential");

    // Encrypted at rest; the secret decrypts to the (trimmed) token.
    let store = BatchesCredentialStore::new(&env.db, Some(env.keyring.clone()));
    let stored = store
        .user_credential(alice, "gitlab", &url)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.secret(&stored).await.unwrap().token, "good-token");
    let enc: Vec<u8> = sourcegull_db::sqlx::query_scalar(
        "SELECT credential_enc FROM user_credentials WHERE id = $1",
    )
    .bind(stored.id)
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert!(!enc.windows(10).any(|w| w == b"good-token"));

    // Site credentials: site admins only.
    let (code, _) = env
        .err(
            alice,
            CREATE_CREDENTIAL,
            json!({"url": url, "token": "good-token"}),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
    let data = env
        .ok(
            admin,
            CREATE_CREDENTIAL,
            json!({"url": url, "token": "good-token"}),
        )
        .await;
    assert_eq!(
        data["createBatchChangesCredential"]["isSiteCredential"],
        true
    );
    let site_id = data["createBatchChangesCredential"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The user's credential is used before the site's.
    let data = env.ok(alice, CODE_HOSTS, json!({"name": "alice"})).await;
    let c = &data["user"]["batchChangesCodeHosts"]["nodes"][0]["credential"];
    assert_eq!(c["id"], cred_id.as_str());
    assert_eq!(c["isSiteCredential"], false);
    assert_eq!(
        data["batchChangesCodeHosts"]["nodes"][0]["credential"]["isSiteCredential"],
        true
    );
    let data = env.ok(bob, CODE_HOSTS, json!({"name": "bob"})).await;
    assert_eq!(
        data["user"]["batchChangesCodeHosts"]["nodes"][0]["credential"]["isSiteCredential"],
        true
    );

    // node(id:): the owner and site admins.
    let q = "query($id: ID!) { node(id: $id) { ... on BatchChangesCredential { id } } }";
    env.ok(alice, q, json!({"id": cred_id})).await;
    env.ok(admin, q, json!({"id": site_id})).await;
    assert_eq!(env.err(bob, q, json!({"id": cred_id})).await.0, "FORBIDDEN");
    assert_eq!(
        env.err(alice, q, json!({"id": site_id})).await.0,
        "FORBIDDEN"
    );

    // Deleting.
    const DELETE: &str = "mutation($id: ID!) { deleteBatchChangesCredential(batchChangesCredential: $id) { alwaysNil } }";
    assert_eq!(
        env.err(bob, DELETE, json!({"id": cred_id})).await.0,
        "FORBIDDEN"
    );
    assert_eq!(
        env.err(alice, DELETE, json!({"id": site_id})).await.0,
        "FORBIDDEN"
    );
    env.ok(alice, DELETE, json!({"id": cred_id})).await;
    env.ok(admin, DELETE, json!({"id": site_id})).await;
    assert_eq!(
        env.err(alice, DELETE, json!({"id": cred_id})).await.0,
        "NOT_FOUND"
    );
    assert!(
        store
            .user_credential(alice, "gitlab", &url)
            .await
            .unwrap()
            .is_none()
    );
}

const BULK_FIELDS: &str = "id type state progress changesetCount createdAt finishedAt
    initiator { username } errors { changeset { __typename id } error }";

/// Runs a bulk mutation `name(batchChange:, changesets:, <extra>)`.
fn bulk_mutation(name: &str, extra_decl: &str, extra_args: &str) -> String {
    format!(
        "mutation($bc: ID!, $cs: [ID!]!{extra_decl}) {{
            {name}(batchChange: $bc, changesets: $cs{extra_args}) {{ {BULK_FIELDS} }} }}"
    )
}

/// The jobs of the bulk operation with GraphQL `id`: (changeset id, type, payload).
async fn bulk_jobs(env: &Env, id: &str) -> Vec<(i64, String, Value)> {
    let group: String = sourcegull_db::sqlx::query_scalar(
        "SELECT bulk_group FROM changeset_jobs ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(id, marshal_id_str("BulkOperation", &group));
    sourcegull_db::sqlx::query_as::<_, (i64, String, sourcegull_db::sqlx::types::Json<Value>)>(
        "SELECT changeset_id, job_type, payload FROM changeset_jobs WHERE bulk_group = $1
         ORDER BY changeset_id",
    )
    .bind(&group)
    .fetch_all(env.db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|(c, t, p)| (c, t, p.0))
    .collect()
}

fn marshal_id_str(kind: &str, key: &str) -> String {
    sourcegull_services::relay::marshal_string_id(kind, key)
}

#[tokio::test]
async fn bulk_operations() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (bc, ids) = fixture(&env).await;
    let bc_id = marshal_id("BatchChange", bc.id);
    let cs = |i: usize| marshal_id("Changeset", ids[i]);
    let comment = bulk_mutation(
        "createChangesetComments",
        ", $body: String!",
        ", body: $body",
    );

    // Comment: the creator (or a site admin) only, on published changesets in readable
    // repositories of the bulk change.
    let vars = json!({"bc": bc_id, "cs": [cs(0), cs(1), cs(2)], "body": "Please review"});
    let (code, _) = env.err(env.bob, &comment, vars.clone()).await;
    assert_eq!(code, "FORBIDDEN");
    let data = env.ok(env.alice, &comment, vars).await;
    let op = &data["createChangesetComments"];
    assert_eq!(op["type"], "COMMENT");
    assert_eq!(op["state"], "PROCESSING");
    assert_eq!(op["progress"], 0.0);
    assert_eq!(op["changesetCount"], 3);
    assert_eq!(op["initiator"]["username"], "alice");
    assert_eq!(op["finishedAt"], Value::Null);
    assert_eq!(op["errors"], json!([]));
    let jobs = bulk_jobs(&env, op["id"].as_str().unwrap()).await;
    assert_eq!(jobs.len(), 3);
    assert!(
        jobs.iter()
            .all(|(_, t, p)| t == "comment" && p["message"] == "Please review")
    );
    let comment_op = op["id"].as_str().unwrap().to_owned();

    // The private repository's changeset is not found for alice (nothing is queued), but is
    // for the admin.
    let vars = json!({"bc": bc_id, "cs": [cs(0), cs(3)], "body": "x"});
    let (code, msg) = env.err(env.alice, &comment, vars.clone()).await;
    assert_eq!(
        (code.as_str(), msg.as_str()),
        ("NOT_FOUND", "some changesets could not be found")
    );
    let n: i64 = sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM changeset_jobs")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(n, 3);
    let admin_op = env.ok(env.admin, &comment, vars).await["createChangesetComments"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Empty bodies and empty selections are refused.
    let (code, _) = env
        .err(
            env.alice,
            &comment,
            json!({"bc": bc_id, "cs": [cs(0)], "body": " "}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    let (code, _) = env
        .err(
            env.alice,
            &comment,
            json!({"bc": bc_id, "cs": [], "body": "x"}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    // Changesets of another bulk change are not found.
    let other_spec = BatchSpecStore::new(&env.db)
        .create(&NewBatchSpec {
            raw_spec: SPEC.into(),
            spec: json!({"name": "other"}),
            namespace: sourcegull_db::Namespace::User(env.alice),
            user_id: Some(env.alice),
            created_from_raw: false,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap();
    let other_bc = BatchChangeStore::new(&env.db)
        .create(&NewBatchChange {
            name: "other".into(),
            description: String::new(),
            namespace: sourcegull_db::Namespace::User(env.alice),
            creator_id: Some(env.alice),
            batch_spec_id: other_spec.id,
            applied: true,
        })
        .await
        .unwrap();
    let vars = json!({"bc": marshal_id("BatchChange", other_bc.id), "cs": [cs(0)], "body": "x"});
    assert_eq!(env.err(env.alice, &comment, vars).await.0, "NOT_FOUND");

    // Merge: only open changesets; squash goes into the payload.
    let merge = bulk_mutation("mergeChangesets", ", $squash: Boolean", ", squash: $squash");
    let (code, _) = env
        .err(env.alice, &merge, json!({"bc": bc_id, "cs": [cs(1)]}))
        .await;
    assert_eq!(code, "NOT_FOUND", "a merged changeset cannot be merged");
    let data = env
        .ok(
            env.alice,
            &merge,
            json!({"bc": bc_id, "cs": [cs(0)], "squash": true}),
        )
        .await;
    let jobs = bulk_jobs(&env, data["mergeChangesets"]["id"].as_str().unwrap()).await;
    assert_eq!(
        jobs,
        vec![(ids[0], "merge".into(), json!({"squash": true}))]
    );

    // Close: open or draft changesets.
    let close = bulk_mutation("closeChangesets", "", "");
    let (code, _) = env
        .err(
            env.alice,
            &close,
            json!({"bc": bc_id, "cs": [cs(0), cs(1)]}),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");
    let data = env
        .ok(
            env.alice,
            &close,
            json!({"bc": bc_id, "cs": [cs(0), cs(2)]}),
        )
        .await;
    assert_eq!(data["closeChangesets"]["changesetCount"], 2);

    // Re-enqueue: failed changesets only.
    let reenqueue = bulk_mutation("reenqueueChangesets", "", "");
    let (code, _) = env
        .err(env.alice, &reenqueue, json!({"bc": bc_id, "cs": [cs(2)]}))
        .await;
    assert_eq!(code, "NOT_FOUND");
    sourcegull_db::sqlx::query(
        "UPDATE changesets SET state = 'failed', num_failures = 1 WHERE id = $1",
    )
    .bind(ids[2])
    .execute(env.db.pool())
    .await
    .unwrap();
    let data = env
        .ok(env.alice, &reenqueue, json!({"bc": bc_id, "cs": [cs(2)]}))
        .await;
    assert_eq!(data["reenqueueChangesets"]["type"], "REENQUEUE");

    // Publish (draft option).
    let publish = bulk_mutation("publishChangesets", ", $draft: Boolean", ", draft: $draft");
    let data = env
        .ok(
            env.alice,
            &publish,
            json!({"bc": bc_id, "cs": [cs(0)], "draft": true}),
        )
        .await;
    let jobs = bulk_jobs(&env, data["publishChangesets"]["id"].as_str().unwrap()).await;
    assert_eq!(
        jobs,
        vec![(ids[0], "publish".into(), json!({"draft": true}))]
    );

    // Detach: archived changesets only; archived ones may still be commented on.
    let detach = bulk_mutation("detachChangesets", "", "");
    let (code, _) = env
        .err(env.alice, &detach, json!({"bc": bc_id, "cs": [cs(1)]}))
        .await;
    assert_eq!(code, "NOT_FOUND");
    ChangesetStore::new(&env.db)
        .mark_archived(ids[1], bc.id)
        .await
        .unwrap();
    let data = env
        .ok(env.alice, &detach, json!({"bc": bc_id, "cs": [cs(1)]}))
        .await;
    assert_eq!(data["detachChangesets"]["type"], "DETACH");
    env.ok(
        env.alice,
        &comment,
        json!({"bc": bc_id, "cs": [cs(1)], "body": "x"}),
    )
    .await;
    let (code, _) = env
        .err(env.alice, &close, json!({"bc": bc_id, "cs": [cs(1)]}))
        .await;
    assert_eq!(code, "NOT_FOUND");

    // Listing: newest first, paginated by creation time, `createdAfter`.
    const LIST_OPS: &str = "query($id: ID!, $first: Int, $after: String, $since: DateTime) {
        node(id: $id) { ... on BatchChange {
            bulkOperations(first: $first, after: $after, createdAfter: $since) {
                totalCount pageInfo { hasNextPage endCursor } nodes { id type createdAt } } } } }";
    let data = env.ok(env.alice, LIST_OPS, json!({"id": bc_id})).await;
    let ops = &data["node"]["bulkOperations"];
    assert_eq!(ops["totalCount"], 8);
    let types: Vec<&str> = ops["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "COMMENT",
            "DETACH",
            "PUBLISH",
            "REENQUEUE",
            "CLOSE",
            "MERGE",
            "COMMENT",
            "COMMENT"
        ]
    );
    let data = env
        .ok(env.alice, LIST_OPS, json!({"id": bc_id, "first": 3}))
        .await;
    let page = &data["node"]["bulkOperations"];
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let cursor = page["pageInfo"]["endCursor"].clone();
    let data = env
        .ok(
            env.alice,
            LIST_OPS,
            json!({"id": bc_id, "first": 10, "after": cursor}),
        )
        .await;
    let rest = &data["node"]["bulkOperations"];
    assert_eq!(rest["nodes"].as_array().unwrap().len(), 5);
    assert_eq!(rest["nodes"][0]["type"], "REENQUEUE");
    assert_eq!(rest["pageInfo"]["hasNextPage"], false);
    let since = ops["nodes"][2]["createdAt"].clone();
    let data = env
        .ok(env.alice, LIST_OPS, json!({"id": bc_id, "since": since}))
        .await;
    assert_eq!(data["node"]["bulkOperations"]["totalCount"], 2);

    // Failures: state, progress and errors; errors on hidden changesets are hidden.
    sourcegull_db::sqlx::query(
        "UPDATE changeset_jobs SET state = CASE WHEN changeset_id = $2 THEN 'failed'
             ELSE 'completed' END,
             failure_message = 'boom', finished_at = now()
         WHERE bulk_group = (SELECT bulk_group FROM changeset_jobs WHERE changeset_id = $2
                             ORDER BY id LIMIT 1)",
    )
    .bind(ids[0])
    .bind(ids[3])
    .execute(env.db.pool())
    .await
    .unwrap();
    let q = format!(
        "query($id: ID!) {{ node(id: $id) {{ __typename ... on BulkOperation {{ {BULK_FIELDS} }} }} }}"
    );
    let data = env.ok(env.alice, &q, json!({"id": admin_op})).await;
    let op = &data["node"];
    assert_eq!(op["__typename"], "BulkOperation");
    assert_eq!(op["state"], "FAILED");
    assert_eq!(op["progress"], 1.0);
    assert!(op["finishedAt"].is_string());
    assert_eq!(op["initiator"]["username"], "admin");
    assert_eq!(op["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        op["errors"][0]["changeset"]["__typename"],
        "HiddenExternalChangeset"
    );
    assert_eq!(op["errors"][0]["error"], Value::Null);
    let data = env.ok(env.admin, &q, json!({"id": admin_op})).await;
    assert_eq!(data["node"]["errors"][0]["error"], "boom");
    let data = env.ok(env.alice, &q, json!({"id": comment_op})).await;
    assert_eq!(data["node"]["state"], "PROCESSING");
}

#[tokio::test]
async fn available_bulk_operations() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (bc, ids) = fixture(&env).await;
    let bc_id = marshal_id("BatchChange", bc.id);
    const Q: &str = "query($id: ID!, $cs: [ID!]!) { node(id: $id) { ... on BatchChange {
        availableBulkOperations(changesets: $cs) } } }";
    let available = |user: UserId, idx: Vec<usize>| {
        let cs: Vec<String> = idx
            .iter()
            .map(|i| marshal_id("Changeset", ids[*i]))
            .collect();
        let vars = json!({"id": bc_id, "cs": cs});
        let env = &env;
        async move {
            env.ok(user, Q, vars).await["node"]["availableBulkOperations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    // Open: everything but detach and re-enqueue.
    assert_eq!(
        available(env.alice, vec![0]).await,
        ["COMMENT", "MERGE", "CLOSE", "PUBLISH"]
    );
    // With a merged one: only what both allow.
    assert_eq!(
        available(env.alice, vec![0, 1]).await,
        ["COMMENT", "PUBLISH"]
    );
    assert!(available(env.alice, vec![]).await.is_empty());
    // Failed: re-enqueue instead of merge.
    sourcegull_db::sqlx::query(
        "UPDATE changesets SET state = 'failed', num_failures = 1 WHERE id = $1",
    )
    .bind(ids[2])
    .execute(env.db.pool())
    .await
    .unwrap();
    assert_eq!(
        available(env.alice, vec![2]).await,
        ["COMMENT", "REENQUEUE", "CLOSE", "PUBLISH"]
    );
    // Archived: comment and detach.
    ChangesetStore::new(&env.db)
        .mark_archived(ids[1], bc.id)
        .await
        .unwrap();
    assert_eq!(available(env.alice, vec![1]).await, ["COMMENT", "DETACH"]);
    // Read-only: nothing.
    sourcegull_db::sqlx::query("UPDATE changesets SET external_state = 'READONLY' WHERE id = $1")
        .bind(ids[0])
        .execute(env.db.pool())
        .await
        .unwrap();
    assert!(available(env.alice, vec![0]).await.is_empty());
    // A changeset in a repository alice may not read is not found; the admin sees it.
    let vars = json!({"id": bc_id, "cs": [marshal_id("Changeset", ids[3])]});
    assert_eq!(env.err(env.alice, Q, vars).await.0, "NOT_FOUND");
    assert_eq!(
        available(env.admin, vec![3]).await,
        ["COMMENT", "MERGE", "CLOSE", "PUBLISH"]
    );
}

#[tokio::test]
async fn burndown_and_csv_export() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (bc, ids) = fixture(&env).await;
    let bc_id = marshal_id("BatchChange", bc.id);
    let t = chrono::Utc::now() - chrono::Duration::hours(1);
    let at = |days: i64| t - chrono::Duration::days(days) - chrono::Duration::hours(1);
    // (changeset, opened, events)
    let timelines = [
        (
            ids[0],
            at(5),
            vec![("APPROVED", "note:1", at(4), json!({"author": "bob"}))],
        ),
        (ids[1], at(5), vec![("MERGED", "state:1", at(3), json!({}))]),
        (
            ids[2],
            at(2),
            vec![("COMMENTED", "note:2", at(2), json!({"author": "bob"}))],
        ),
        (ids[3], at(6), vec![("CLOSED", "state:2", at(1), json!({}))]),
    ];
    for (id, opened, events) in timelines {
        sourcegull_db::sqlx::query(
            "UPDATE changesets SET metadata = jsonb_set(metadata, '{mergeRequest,created_at}',
                 to_jsonb($2::text)) WHERE id = $1",
        )
        .bind(id)
        .bind(opened.to_rfc3339())
        .execute(env.db.pool())
        .await
        .unwrap();
        let events: Vec<NewChangesetEvent> = events
            .into_iter()
            .map(|(kind, key, created_at, metadata)| NewChangesetEvent {
                kind: kind.into(),
                key: key.into(),
                created_at,
                metadata,
            })
            .collect();
        ChangesetStore::new(&env.db)
            .upsert_events(id, &events)
            .await
            .unwrap();
    }
    const Q: &str = "query($id: ID!, $from: DateTime, $to: DateTime, $archived: Boolean) {
        node(id: $id) { ... on BatchChange {
            changesetCountsOverTime(from: $from, to: $to, includeArchived: $archived) {
                date total merged closed draft open openApproved openChangesRequested openPending
            } } } }";
    let from = (t - chrono::Duration::days(6)).to_rfc3339();
    let vars = json!({"id": bc_id, "from": from, "to": t.to_rfc3339()});
    let data = env.ok(env.alice, Q, vars.clone()).await;
    let points = data["node"]["changesetCountsOverTime"].as_array().unwrap();
    // (total, open, approved, pending, merged, closed) per day, oldest first.
    let got: Vec<[i64; 6]> = points
        .iter()
        .map(|p| {
            [
                "total",
                "open",
                "openApproved",
                "openPending",
                "merged",
                "closed",
            ]
            .map(|k| p[k].as_i64().unwrap())
        })
        .collect();
    assert_eq!(
        got,
        vec![
            [1, 1, 0, 1, 0, 0],
            [3, 3, 0, 3, 0, 0],
            [3, 3, 1, 2, 0, 0],
            [3, 2, 1, 1, 1, 0],
            [4, 3, 1, 2, 1, 0],
            [4, 2, 1, 1, 1, 1],
            [4, 2, 1, 1, 1, 1],
        ]
    );
    let last = chrono::DateTime::parse_from_rfc3339(points[6]["date"].as_str().unwrap()).unwrap();
    assert_eq!(last.timestamp(), t.timestamp());
    // Archived changesets only with includeArchived.
    ChangesetStore::new(&env.db)
        .mark_archived(ids[2], bc.id)
        .await
        .unwrap();
    let data = env.ok(env.alice, Q, vars.clone()).await;
    assert_eq!(data["node"]["changesetCountsOverTime"][6]["total"], 3);
    let mut with_archived = vars;
    with_archived["archived"] = json!(true);
    let data = env.ok(env.alice, Q, with_archived).await;
    assert_eq!(data["node"]["changesetCountsOverTime"][6]["total"], 4);
    // By default: from the first event (or at least a week back) to now.
    let data = env.ok(env.alice, Q, json!({"id": bc_id})).await;
    let points = data["node"]["changesetCountsOverTime"].as_array().unwrap();
    assert!(points.len() >= 8, "{}", points.len());

    // CSV export: readable changesets only, filtered like the listing or by selection.
    const CSV: &str = "query($id: ID!, $cs: [ID!], $state: ChangesetState) { node(id: $id) {
        ... on BatchChange { changesetsCSV(changesets: $cs, state: $state) } } }";
    let data = env.ok(env.alice, CSV, json!({"id": bc_id})).await;
    let csv = data["node"]["changesetsCSV"].as_str().unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "id,repository,title,state,reviewState,checkState,externalID,externalURL,added,deleted,createdAt,updatedAt"
    );
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[1].starts_with(&format!(
        "{},{PUBLIC},Fix the README,OPEN,APPROVED,PASSED,11,https://gitlab.example.com/mr/11,3,1,",
        marshal_id("Changeset", ids[0])
    )), "{csv}");
    assert!(!csv.contains("secrets"));
    let data = env
        .ok(env.admin, CSV, json!({"id": bc_id, "state": "MERGED"}))
        .await;
    let csv = data["node"]["changesetsCSV"].as_str().unwrap();
    assert_eq!(csv.lines().count(), 2);
    assert!(csv.contains("Fix the docs"));
    let data = env
        .ok(
            env.admin,
            CSV,
            json!({"id": bc_id, "cs": [marshal_id("Changeset", ids[3])]}),
        )
        .await;
    assert!(
        data["node"]["changesetsCSV"]
            .as_str()
            .unwrap()
            .contains("Fix secrets")
    );
}
