//! Outbound webhooks over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): create, list, update and delete as a site admin with
//! validation, everything refused to other users, logs and stats, `node(id:)`, and the
//! `batch_change:close`/`batch_change:delete` events of `closeBatchChange` and
//! `deleteBatchChange`.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_batches::store::{BatchChangeStore, BatchSpecStore, NewBatchChange, NewBatchSpec};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_outbound_webhooks::store::{
    LoggedRequest, LoggedResponse, NewOutboundWebhookLog, OutboundWebhookStore,
};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    keyring: Arc<Keyring>,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.example.com".into()),
            webhooks_outbound_allowed_hosts: vec!["10.0.0.0/8".into()],
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
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
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
        keyring,
        schema,
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

    /// The queued events: type and decrypted payload, oldest first.
    async fn queued(&self) -> Vec<(String, Value)> {
        let store = OutboundWebhookStore::new(&self.db, Some(self.keyring.clone()));
        let ids: Vec<i64> =
            sourcegull_db::sqlx::query_scalar("SELECT id FROM outbound_webhook_jobs ORDER BY id")
                .fetch_all(self.db.pool())
                .await
                .unwrap();
        let mut out = Vec::new();
        for id in ids {
            let job = store.job(id).await.unwrap().unwrap().payload;
            let payload = serde_json::from_slice(&store.payload(&job).unwrap()).unwrap();
            out.push((job.event_type, payload));
        }
        out
    }
}

const CREATE: &str = "mutation($input: OutboundWebhookCreateInput!) {
    createOutboundWebhook(input: $input) {
        id url eventTypes { eventType scope } stats { total errored }
        createdBy { username } updatedBy { username }
    }
}";

const UPDATE: &str = "mutation($id: ID!, $input: OutboundWebhookUpdateInput!) {
    updateOutboundWebhook(id: $id, input: $input) { id url eventTypes { eventType scope } }
}";

const DELETE: &str = "mutation($id: ID!) { deleteOutboundWebhook(id: $id) { alwaysNil } }";

const LIST: &str =
    "query($types: [OutboundWebhookScopedEventTypeInput!], $first: Int, $after: String) {
    outboundWebhooks(eventTypes: $types, first: $first, after: $after) {
        totalCount pageInfo { hasNextPage endCursor } nodes { id url }
    }
}";

fn create_input(url: &str, types: Value) -> Value {
    json!({"input": {"url": url, "secret": "s3cret", "eventTypes": types}})
}

#[tokio::test]
async fn crud_as_site_admin() {
    let Some(env) = setup().await else { return };

    let data = env
        .ok(
            env.admin,
            "{ outboundWebhookEventTypes { key description } }",
            json!({}),
        )
        .await;
    let keys: Vec<&str> = data["outboundWebhookEventTypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec![
            "batch_change:apply",
            "batch_change:close",
            "batch_change:delete",
            "changeset:close",
            "changeset:publish",
            "changeset:publish_error",
            "changeset:update",
            "changeset:update_error",
        ]
    );

    let data = env
        .ok(
            env.admin,
            CREATE,
            create_input(
                "https://hooks.example.com/sourcegull?token=abcdef123456",
                json!([
                    {"eventType": "changeset:publish"},
                    {"eventType": "batch_change:close", "scope": "acme"},
                    {"eventType": "changeset:publish"},
                ]),
            ),
        )
        .await;
    let hook = &data["createOutboundWebhook"];
    let id = hook["id"].as_str().unwrap().to_owned();
    // Reads mask the URL: the origin and the last characters.
    assert_eq!(hook["url"], "https://hooks.example.com/…3456");
    // Sorted and deduplicated.
    assert_eq!(
        hook["eventTypes"],
        json!([
            {"eventType": "batch_change:close", "scope": "acme"},
            {"eventType": "changeset:publish", "scope": null},
        ])
    );
    assert_eq!(hook["stats"], json!({"total": 0, "errored": 0}));
    assert_eq!(hook["createdBy"]["username"], "admin");

    // Validation.
    for (input, message) in [
        (
            create_input(
                "ftp://hooks.example.com/",
                json!([{"eventType": "changeset:close"}]),
            ),
            "scheme",
        ),
        (
            create_input("not a url", json!([{"eventType": "changeset:close"}])),
            "invalid URL",
        ),
        (
            create_input(
                "http://127.0.0.1:8080/",
                json!([{"eventType": "changeset:close"}]),
            ),
            "not a public address",
        ),
        (
            create_input(
                "http://169.254.169.254/",
                json!([{"eventType": "changeset:close"}]),
            ),
            "not a public address",
        ),
        (
            create_input("https://hooks.example.com/", json!([])),
            "at least one event type",
        ),
        (
            create_input(
                "https://hooks.example.com/",
                json!([{"eventType": "repo:push"}]),
            ),
            "unknown event type",
        ),
        (
            json!({"input": {"url": "https://hooks.example.com/", "secret": "",
                             "eventTypes": [{"eventType": "changeset:close"}]}}),
            "secret cannot be empty",
        ),
    ] {
        let (code, msg) = env.err(env.admin, CREATE, input).await;
        assert_eq!(code, "INVALID_INPUT", "{msg}");
        assert!(msg.contains(message), "{msg} should mention {message}");
    }
    // An allowlisted private network is accepted.
    let data = env
        .ok(
            env.admin,
            CREATE,
            create_input(
                "http://10.1.2.3/hook",
                json!([{"eventType": "changeset:close"}]),
            ),
        )
        .await;
    let second = data["createOutboundWebhook"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Listing, filtered and paged.
    let data = env.ok(env.admin, LIST, json!({})).await;
    assert_eq!(data["outboundWebhooks"]["totalCount"], 2);
    let data = env
        .ok(
            env.admin,
            LIST,
            json!({"types": [{"eventType": "changeset:close"}]}),
        )
        .await;
    assert_eq!(data["outboundWebhooks"]["totalCount"], 1);
    assert_eq!(data["outboundWebhooks"]["nodes"][0]["id"], second);
    let data = env.ok(env.admin, LIST, json!({"first": 1})).await;
    let page = &data["outboundWebhooks"];
    assert_eq!(page["nodes"][0]["id"], id);
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let data = env
        .ok(
            env.admin,
            LIST,
            json!({"first": 1, "after": page["pageInfo"]["endCursor"]}),
        )
        .await;
    assert_eq!(data["outboundWebhooks"]["nodes"][0]["id"], second);
    assert_eq!(data["outboundWebhooks"]["pageInfo"]["hasNextPage"], false);

    // Update: URL and event types replaced, secret kept unless given.
    let data = env
        .ok(
            env.admin,
            UPDATE,
            json!({"id": id, "input": {"url": "https://other.example.com/",
                   "eventTypes": [{"eventType": "batch_change:delete"}]}}),
        )
        .await;
    assert_eq!(
        data["updateOutboundWebhook"]["url"],
        "https://other.example.com/…"
    );
    assert_eq!(
        data["updateOutboundWebhook"]["eventTypes"],
        json!([{"eventType": "batch_change:delete", "scope": null}])
    );
    let key = sourcegull_services::relay::unmarshal_int_id("OutboundWebhook", &id).unwrap();
    let store = OutboundWebhookStore::new(&env.db, Some(env.keyring.clone()));
    assert_eq!(store.get(key).await.unwrap().unwrap().secret, "s3cret");
    env.ok(
        env.admin,
        UPDATE,
        json!({"id": id, "input": {"url": "https://other.example.com/", "secret": "new",
               "eventTypes": [{"eventType": "batch_change:delete"}]}}),
    )
    .await;
    assert_eq!(store.get(key).await.unwrap().unwrap().secret, "new");
    // An empty URL, or the masked one sent back, keeps the stored URL.
    for url in ["", "https://other.example.com/…"] {
        env.ok(
            env.admin,
            UPDATE,
            json!({"id": id, "input": {"url": url,
                   "eventTypes": [{"eventType": "batch_change:delete"}]}}),
        )
        .await;
        assert_eq!(
            store.get(key).await.unwrap().unwrap().url,
            "https://other.example.com/",
            "{url:?}"
        );
    }
    // Creating needs a URL.
    let (code, _) = env
        .err(
            env.admin,
            CREATE,
            create_input("", json!([{"eventType": "changeset:close"}])),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    let (code, _) = env
        .err(
            env.admin,
            UPDATE,
            json!({"id": id, "input": {"url": "http://[::1]/",
                   "eventTypes": [{"eventType": "batch_change:delete"}]}}),
        )
        .await;
    assert_eq!(code, "INVALID_INPUT");
    let missing = marshal_id("OutboundWebhook", 999);
    let (code, _) = env
        .err(
            env.admin,
            UPDATE,
            json!({"id": missing, "input": {"url": "https://x.example.com/",
                   "eventTypes": [{"eventType": "batch_change:delete"}]}}),
        )
        .await;
    assert_eq!(code, "NOT_FOUND");

    // node(id:).
    let data = env
        .ok(
            env.admin,
            "query($id: ID!) { node(id: $id) { __typename ... on OutboundWebhook { url } } }",
            json!({"id": id}),
        )
        .await;
    assert_eq!(data["node"]["__typename"], "OutboundWebhook");

    // Only site admins.
    for (query, vars) in [
        (LIST, json!({})),
        ("{ outboundWebhookEventTypes { key } }", json!({})),
        (
            CREATE,
            create_input(
                "https://hooks.example.com/",
                json!([{"eventType": "changeset:close"}]),
            ),
        ),
        (
            UPDATE,
            json!({"id": id, "input": {"url": "https://x.example.com/",
                   "eventTypes": [{"eventType": "batch_change:delete"}]}}),
        ),
        (DELETE, json!({"id": id})),
        (
            "query($id: ID!) { node(id: $id) { id } }",
            json!({"id": id}),
        ),
    ] {
        let (code, _) = env.err(env.alice, query, vars).await;
        assert_eq!(code, "FORBIDDEN", "{query}");
    }

    // Delete.
    env.ok(env.admin, DELETE, json!({"id": id})).await;
    let (code, _) = env.err(env.admin, DELETE, json!({"id": id})).await;
    assert_eq!(code, "NOT_FOUND");
    let data = env.ok(env.admin, LIST, json!({})).await;
    assert_eq!(data["outboundWebhooks"]["totalCount"], 1);

    // Changes are security events.
    let names: Vec<String> = sourcegull_db::sqlx::query_scalar(
        "SELECT name FROM security_event_logs WHERE name LIKE 'OutboundWebhook%' ORDER BY id",
    )
    .fetch_all(env.db.pool())
    .await
    .unwrap();
    assert_eq!(
        names,
        vec![
            "OutboundWebhookCreated",
            "OutboundWebhookCreated",
            "OutboundWebhookUpdated",
            "OutboundWebhookUpdated",
            "OutboundWebhookUpdated",
            "OutboundWebhookUpdated",
            "OutboundWebhookDeleted",
        ]
    );
}

#[tokio::test]
async fn logs_and_stats() {
    let Some(env) = setup().await else { return };
    let data = env
        .ok(
            env.admin,
            CREATE,
            create_input(
                "https://hooks.example.com/",
                json!([{"eventType": "changeset:close"}]),
            ),
        )
        .await;
    let id = data["createOutboundWebhook"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = sourcegull_services::relay::unmarshal_int_id("OutboundWebhook", &id).unwrap();
    let store = OutboundWebhookStore::new(&env.db, Some(env.keyring.clone()));
    let job = store
        .enqueue("changeset:close", None, br#"{"id":"x"}"#)
        .await
        .unwrap();
    let request = LoggedRequest {
        method: "POST".into(),
        url: "https://hooks.example.com/".into(),
        version: "HTTP/1.1".into(),
        headers: [(
            "x-sourcegull-webhook-event-type".to_owned(),
            vec!["changeset:close".to_owned()],
        )]
        .into(),
        body: r#"{"id":"x"}"#.into(),
    };
    store
        .create_log(&NewOutboundWebhookLog {
            job_id: job,
            outbound_webhook_id: key,
            status_code: 0,
            request: request.clone(),
            response: None,
            error: Some("connection refused".into()),
        })
        .await
        .unwrap();
    store
        .create_log(&NewOutboundWebhookLog {
            job_id: job,
            outbound_webhook_id: key,
            status_code: 200,
            request,
            response: Some(LoggedResponse {
                status_code: 200,
                version: "HTTP/1.1".into(),
                headers: Default::default(),
                body: "thanks".into(),
            }),
            error: None,
        })
        .await
        .unwrap();

    let query = "query($id: ID!, $errors: Boolean) { node(id: $id) { ... on OutboundWebhook {
        stats { total errored }
        logs(onlyErrors: $errors) { totalCount nodes {
            id statusCode error sentAt
            job { eventType scope payload }
            request { method url version body headers { name values } }
            response { statusCode body }
        } }
    } } }";
    let data = env.ok(env.admin, query, json!({"id": id})).await;
    let node = &data["node"];
    assert_eq!(node["stats"], json!({"total": 2, "errored": 1}));
    assert_eq!(node["logs"]["totalCount"], 2);
    let newest = &node["logs"]["nodes"][0];
    assert_eq!(newest["statusCode"], 200);
    assert_eq!(newest["response"]["body"], "thanks");
    assert_eq!(
        newest["job"],
        json!({"eventType": "changeset:close", "scope": null, "payload": "{\"id\":\"x\"}"})
    );
    assert_eq!(newest["request"]["method"], "POST");
    assert_eq!(newest["request"]["url"], "https://hooks.example.com/…");
    assert_eq!(
        newest["request"]["headers"],
        json!([{"name": "x-sourcegull-webhook-event-type", "values": ["changeset:close"]}])
    );
    let data = env
        .ok(env.admin, query, json!({"id": id, "errors": true}))
        .await;
    let logs = &data["node"]["logs"];
    assert_eq!(logs["totalCount"], 1);
    assert_eq!(logs["nodes"][0]["statusCode"], 0);
    assert_eq!(logs["nodes"][0]["error"], "connection refused");
    assert_eq!(logs["nodes"][0]["response"], Value::Null);

    // A log by node ID.
    let log_id = logs["nodes"][0]["id"].as_str().unwrap();
    let data = env
        .ok(
            env.admin,
            "query($id: ID!) { node(id: $id) { ... on OutboundWebhookLog { statusCode } } }",
            json!({"id": log_id}),
        )
        .await;
    assert_eq!(data["node"]["statusCode"], 0);
    let (code, _) = env
        .err(
            env.alice,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({"id": log_id}),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");
}

#[tokio::test]
async fn close_and_delete_emit_batch_change_events() {
    let Some(env) = setup().await else { return };
    let spec = BatchSpecStore::new(&env.db)
        .create(&NewBatchSpec {
            raw_spec: "name: hello\n".into(),
            spec: json!({"name": "hello"}),
            namespace: Namespace::User(env.admin),
            user_id: Some(env.admin),
            created_from_raw: true,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap()
        .id;
    let new_bc = |name: &str| NewBatchChange {
        name: name.into(),
        description: "Says hello".into(),
        namespace: Namespace::User(env.admin),
        creator_id: Some(env.admin),
        batch_spec_id: spec,
        applied: true,
    };
    let quiet = BatchChangeStore::new(&env.db)
        .create(&new_bc("quiet"))
        .await
        .unwrap();
    let bc = BatchChangeStore::new(&env.db)
        .create(&new_bc("hello"))
        .await
        .unwrap();
    const CLOSE: &str = "mutation($id: ID!) { closeBatchChange(batchChange: $id) { state } }";
    const DELETE_BC: &str =
        "mutation($id: ID!) { deleteBatchChange(batchChange: $id) { alwaysNil } }";

    // Without subscribers nothing is queued.
    env.ok(
        env.admin,
        CLOSE,
        json!({"id": marshal_id("BatchChange", quiet.id)}),
    )
    .await;
    assert!(env.queued().await.is_empty());

    env.ok(
        env.admin,
        CREATE,
        create_input(
            "https://hooks.example.com/",
            json!([{"eventType": "batch_change:close"}, {"eventType": "batch_change:delete"}]),
        ),
    )
    .await;
    let bc_id = marshal_id("BatchChange", bc.id);
    env.ok(env.admin, CLOSE, json!({"id": bc_id})).await;
    // Closing again is a no-op without an event.
    env.ok(env.admin, CLOSE, json!({"id": bc_id})).await;
    env.ok(env.admin, DELETE_BC, json!({"id": bc_id})).await;

    let events = env.queued().await;
    assert_eq!(events.len(), 2, "{events:?}");
    let (close, payload) = &events[0];
    assert_eq!(close, "batch_change:close");
    assert_eq!(payload["id"], bc_id);
    assert_eq!(payload["name"], "hello");
    assert_eq!(payload["description"], "Says hello");
    assert_eq!(payload["state"], "CLOSED");
    assert_eq!(payload["namespace_id"], marshal_id("User", env.admin.get()));
    assert_eq!(
        payload["creator_user_id"],
        marshal_id("User", env.admin.get())
    );
    assert_eq!(
        payload["url"],
        "https://sourcegull.example.com/users/admin/bulk-changes/hello"
    );
    assert!(payload["closed_at"].is_string());
    assert_eq!(events[1].0, "batch_change:delete");
    assert_eq!(events[1].1["id"], bc_id);
}

#[tokio::test]
async fn apply_emits_batch_change_apply() {
    let Some(env) = setup().await else { return };
    // A spec without changeset specs, uploaded by a command-line client (not server-side, so no
    // execution).
    let spec = BatchSpecStore::new(&env.db)
        .create(&NewBatchSpec {
            raw_spec: "name: hello\n".into(),
            spec: json!({"name": "hello"}),
            namespace: Namespace::User(env.admin),
            user_id: Some(env.admin),
            created_from_raw: false,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap();
    env.ok(
        env.admin,
        CREATE,
        create_input(
            "https://hooks.example.com/",
            json!([{"eventType": "batch_change:apply"}]),
        ),
    )
    .await;
    let data = env
        .ok(
            env.admin,
            "mutation($spec: ID!) { applyBatchChange(batchSpec: $spec) { id } }",
            json!({"spec": sourcegull_services::relay::marshal_string_id("BatchSpec", &spec.rand_id)}),
        )
        .await;

    let events = env.queued().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "batch_change:apply");
    assert_eq!(events[0].1["id"], data["applyBatchChange"]["id"]);
    assert_eq!(events[0].1["name"], "hello");
}
