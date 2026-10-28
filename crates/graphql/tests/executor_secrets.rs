//! Executor secrets over GraphQL against the test Postgres: who may list and change
//! global, user and organization secrets, key and value validation, values never readable,
//! access logs and security events.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{Db, ExecutorSecretScope, NewUser, SecretAccessor, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, UserId};

const CREATE: &str = "mutation($scope: ExecutorSecretScope!, $key: String!, $value: String!,
                               $namespace: ID) {
    createExecutorSecret(scope: $scope, key: $key, value: $value, namespace: $namespace) {
        id key scope overwritesGlobalSecret
        namespace { id }
        creator { username }
    }
}";

const GLOBAL: &str = "query($scope: ExecutorSecretScope!) {
    executorSecrets(scope: $scope, first: 1) {
        totalCount pageInfo { hasNextPage endCursor }
        nodes { key scope namespace { id } }
    }
}";

const USER: &str = "query($name: String!) {
    user(username: $name) {
        executorSecrets(scope: BATCHES) {
            totalCount
            nodes { id key overwritesGlobalSecret namespace { id } }
        }
    }
}";

const ORG: &str = "query($name: String!) {
    organization(name: $name) {
        executorSecrets(scope: BATCHES) { totalCount nodes { key namespace { id } } }
    }
}";

const UPDATE: &str = "mutation($scope: ExecutorSecretScope!, $id: ID!, $value: String!) {
    updateExecutorSecret(scope: $scope, id: $id, value: $value) { id key updatedAt }
}";

const DELETE: &str = "mutation($scope: ExecutorSecretScope!, $id: ID!) {
    deleteExecutorSecret(scope: $scope, id: $id) { alwaysNil }
}";

const NODE: &str = "query($id: ID!) {
    node(id: $id) {
        ... on ExecutorSecret {
            key
            accessLogs { totalCount nodes { id machineUser user { username } executorSecret { key } } }
        }
    }
}";

struct Env {
    schema: SourcegullSchema,
    auth: Authenticator,
}

impl Env {
    async fn run(&self, user: UserId, query: &str, vars: Value) -> async_graphql::Response {
        let actor = self.auth.actor_for(user, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    /// The first error's code and message.
    async fn err(&self, user: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(user, query, vars).await;
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
}

#[tokio::test]
async fn executor_secrets_access_and_validation() {
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
    let org = db.orgs().create("acme", None).await.unwrap().id;
    db.orgs()
        .add_member(org, alice, OrgRole::Member)
        .await
        .unwrap();
    let provider = Arc::new(StaticKeyProvider::new("k1", [8u8; 32]).unwrap());
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
        authz: Checker::new(db.clone(), config),
        app: None,
    });
    let env = Env {
        schema,
        auth: auth.clone(),
    };
    let alice_ns = marshal_id("User", alice.get());
    let org_ns = marshal_id("Org", org.get());

    // Global secrets: site admins only.
    let create = |key: &str, value: &str, ns: Option<&str>| json!({"scope": "BATCHES", "key": key, "value": value, "namespace": ns});
    assert_eq!(
        env.err(alice, CREATE, create("GITHUB_TOKEN", "x", None))
            .await
            .0,
        "FORBIDDEN"
    );
    let global = env
        .ok(
            admin,
            CREATE,
            create("GITHUB_TOKEN", "global-secret-1", None),
        )
        .await;
    let global = &global["createExecutorSecret"];
    assert_eq!(global["scope"], "BATCHES");
    assert_eq!(global["namespace"], Value::Null);
    assert_eq!(global["creator"]["username"], "admin");
    let global_id = global["id"].as_str().unwrap().to_owned();
    env.ok(admin, CREATE, create("NPM_TOKEN", "global-secret-2", None))
        .await;
    assert_eq!(
        env.err(alice, GLOBAL, json!({"scope": "BATCHES"})).await.0,
        "FORBIDDEN"
    );
    let list = env.ok(admin, GLOBAL, json!({"scope": "BATCHES"})).await;
    assert_eq!(list["executorSecrets"]["totalCount"], 2);
    assert_eq!(list["executorSecrets"]["nodes"][0]["key"], "GITHUB_TOKEN");
    assert_eq!(list["executorSecrets"]["pageInfo"]["hasNextPage"], true);
    let list = env.ok(admin, GLOBAL, json!({"scope": "CODEINTEL"})).await;
    assert_eq!(list["executorSecrets"]["totalCount"], 0);

    // Validation.
    let (_, m) = env.err(admin, CREATE, create("", "v", None)).await;
    assert_eq!(m, "key cannot be empty string");
    let (_, m) = env
        .err(admin, CREATE, create("lower-case", "v", None))
        .await;
    assert_eq!(m, "invalid key format, should be a valid env var name");
    let (_, m) = env.err(admin, CREATE, create("EMPTY", "", None)).await;
    assert_eq!(m, "value cannot be empty string");
    let (_, m) = env
        .err(
            admin,
            CREATE,
            create("DOCKER_AUTH_CONFIG", r#"{"credsStore": "desktop"}"#, None),
        )
        .await;
    assert!(m.contains("credential stores"), "{m}");
    let (code, _) = env
        .err(admin, CREATE, create("GITHUB_TOKEN", "again", None))
        .await;
    assert_eq!(code, "ErrDuplicateExecutorSecret");

    // A user's namespace: the user (or a site admin), nobody else.
    assert_eq!(
        env.err(bob, CREATE, create("GITHUB_TOKEN", "x", Some(&alice_ns)))
            .await
            .0,
        "FORBIDDEN"
    );
    let mine = env
        .ok(
            alice,
            CREATE,
            create("GITHUB_TOKEN", "alice-secret-1", Some(&alice_ns)),
        )
        .await;
    assert_eq!(mine["createExecutorSecret"]["overwritesGlobalSecret"], true);
    assert_eq!(mine["createExecutorSecret"]["namespace"]["id"], alice_ns);
    let mine_id = mine["createExecutorSecret"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let data = env.ok(alice, USER, json!({"name": "alice"})).await;
    let secrets = &data["user"]["executorSecrets"];
    assert_eq!(secrets["totalCount"], 2, "own plus non-overwritten global");
    assert_eq!(secrets["nodes"][0]["id"], mine_id);
    assert_eq!(secrets["nodes"][1]["key"], "NPM_TOKEN");
    assert_eq!(secrets["nodes"][1]["namespace"], Value::Null);
    assert_eq!(
        env.err(bob, USER, json!({"name": "alice"})).await.0,
        "FORBIDDEN"
    );
    env.ok(admin, USER, json!({"name": "alice"})).await;

    // An org's namespace: its members (or site admins).
    env.ok(
        alice,
        CREATE,
        create("ORG_TOKEN", "org-secret-1", Some(&org_ns)),
    )
    .await;
    let data = env.ok(alice, ORG, json!({"name": "acme"})).await;
    assert_eq!(data["organization"]["executorSecrets"]["totalCount"], 3);
    assert_eq!(
        env.err(bob, ORG, json!({"name": "acme"})).await.0,
        "FORBIDDEN"
    );
    assert_eq!(
        env.err(bob, CREATE, create("X", "y", Some(&org_ns)))
            .await
            .0,
        "FORBIDDEN"
    );

    // Values are write-only.
    let resp = env
        .run(
            admin,
            "query { executorSecrets(scope: BATCHES) { nodes { value } } }",
            json!({}),
        )
        .await;
    assert!(!resp.errors.is_empty(), "there is no value field");
    let everything = env.ok(admin, GLOBAL, json!({"scope": "BATCHES"})).await;
    assert!(!everything.to_string().contains("global-secret"));

    // Nodes follow the namespace rules.
    let resp = env.run(bob, NODE, json!({"id": mine_id})).await;
    assert!(!resp.errors.is_empty(), "bob may not see alice's secret");
    let resp = env.run(bob, NODE, json!({"id": global_id})).await;
    assert!(
        !resp.errors.is_empty(),
        "global secrets are for site admins"
    );

    // Updates and deletes: namespace access, matching scope.
    let update = |id: &str, scope: &str| json!({"scope": scope, "id": id, "value": "new-value-1"});
    assert_eq!(
        env.err(bob, UPDATE, update(&mine_id, "BATCHES")).await.0,
        "FORBIDDEN"
    );
    let (_, m) = env.err(alice, UPDATE, update(&mine_id, "CODEINTEL")).await;
    assert_eq!(m, "scope mismatch");
    env.ok(alice, UPDATE, update(&mine_id, "BATCHES")).await;
    let store = db.executor_secrets(Some(keyring));
    let (_, row_id) = {
        let spec: String = {
            let (_, s) = sourcegull_services::relay::unmarshal_id(&mine_id).unwrap();
            serde_json::from_str(&s).unwrap()
        };
        let (scope, id) = spec.split_once(':').unwrap();
        (scope.to_owned(), id.parse::<i64>().unwrap())
    };
    let secret = store
        .get(ExecutorSecretScope::Batches, row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .value(&secret, SecretAccessor::User(alice))
            .await
            .unwrap(),
        "new-value-1"
    );

    // Reads show up in the access logs.
    let data = env.ok(alice, NODE, json!({"id": mine_id})).await;
    let logs = &data["node"]["accessLogs"];
    assert_eq!(logs["totalCount"], 1);
    assert_eq!(logs["nodes"][0]["user"]["username"], "alice");
    assert_eq!(logs["nodes"][0]["machineUser"], "");
    assert_eq!(logs["nodes"][0]["executorSecret"]["key"], "GITHUB_TOKEN");
    let log_id = logs["nodes"][0]["id"].as_str().unwrap().to_owned();
    let resp = env
        .run(
            bob,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({"id": log_id}),
        )
        .await;
    assert!(
        !resp.errors.is_empty(),
        "bob may not see alice's access logs"
    );

    assert_eq!(
        env.err(bob, DELETE, json!({"scope": "BATCHES", "id": mine_id}))
            .await
            .0,
        "FORBIDDEN"
    );
    env.ok(alice, DELETE, json!({"scope": "BATCHES", "id": mine_id}))
        .await;
    let data = env.ok(alice, USER, json!({"name": "alice"})).await;
    assert_eq!(
        data["user"]["executorSecrets"]["nodes"][0]["namespace"],
        Value::Null
    );

    // Changes are security events.
    let events: Vec<String> = db
        .security_events()
        .list_recent(None, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    for name in [
        "ExecutorSecretCreated",
        "ExecutorSecretUpdated",
        "ExecutorSecretDeleted",
    ] {
        assert!(events.iter().any(|e| e == name), "{name} in {events:?}");
    }
}
