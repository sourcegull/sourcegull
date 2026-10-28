//! Watches end to end against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and
//! an in-process repo node: a monitor created over GraphQL records the heads on its first
//! evaluation without notifying, a push of matching commits fires its webhook action (a local
//! HTTP server receives the results), a push without matches fires nothing, the run and
//! action logs show up under the monitor, "send test" reaches the webhook, and other users
//! can neither see nor change the monitor. With encryption keys, webhook URLs and Telegram bot
//! tokens, and email headers, are stored encrypted (reads mask them; an empty value on update
//! or test keeps the stored one), and an email action's header is added to the messages it
//! sends. Mattermost and Telegram actions post to a local server standing in for both.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use async_graphql::{Request, Response, Variables};
use chrono::Utc;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, RefEventSource, RefUpdate, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_monitors::{ActionSender, Evaluator, EvaluatorConfig};
use sourcegull_search_web::SearchBackend;
use sourcegull_services::SiteConfigSource;
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO: &str = "gitlab.example.com/acme/web";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    backend: SearchBackend,
    config: sourcegull_config::SiteConfigHandle,
    node: RepoNode,
    repo: RepoId,
    alice: UserId,
    bob: UserId,
    admin: UserId,
    keyring: Option<Arc<sourcegull_crypto::Keyring>>,
    /// Receives the test emails the API sends.
    mailer: MemoryEmailSender,
}

async fn setup(source: &SourceRepo) -> Option<Env> {
    setup_with(source, false).await
}

/// `encrypt`: give the schema a keyring, as with `SOURCEGULL_KEYS_FILE` set.
async fn setup_with(source: &SourceRepo, encrypt: bool) -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let keyring = if encrypt {
        let provider = Arc::new(sourcegull_crypto::StaticKeyProvider::new("k1", [7; 32]).unwrap());
        Some(Arc::new(
            db.encryption_keys().load_keyring(provider).await.unwrap(),
        ))
    } else {
        None
    };
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.example.com".into()),
            // The test webhook receiver listens on the loopback interface.
            webhooks_outbound_allowed_hosts: vec!["127.0.0.1".into()],
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
    let node = RepoNode::start(source.remote()).await;
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
    let mailer = MemoryEmailSender::default();
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle());
    let app = sourcegull_services::AppState::new(db.clone(), config.clone(), metrics, auth.clone())
        .with_email_sender(Arc::new(mailer.clone()));
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: keyring.clone(),
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz: authz.clone(),
        app: Some(app),
    });
    let backend = sourcegull_services::search_backend(db.clone(), authz, node.clients.clone());
    Some(Env {
        _t: t,
        db,
        auth,
        schema,
        backend,
        config,
        node,
        repo,
        alice: users[0],
        bob: users[1],
        admin,
        keyring,
        mailer,
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

    async fn err_code(&self, user: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(user, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        err.extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Fetches the source again and records the ref update as the repo node would.
    async fn push(&self, old: &str, new: &str) {
        self.node.fetch_repo(self.repo, REPO).await;
        self.db
            .ref_events()
            .record(
                self.repo,
                &[RefUpdate {
                    name: "refs/heads/main".into(),
                    old_oid: Some(old.into()),
                    new_oid: Some(new.into()),
                }],
                RefEventSource::Fetch,
            )
            .await
            .unwrap();
    }

    async fn trigger_jobs(&self) -> i64 {
        sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM cm_trigger_jobs")
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

const CREATE: &str = "mutation($monitor: MonitorInput!, $trigger: MonitorTriggerInput!,
        $actions: [MonitorActionInput!]!) {
    createCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) {
        id description enabled url
        owner { id }
        trigger { ... on MonitorQuery { id query } }
        actions { totalCount nodes {
            ... on MonitorWebhook { id url includeResults enabled }
            ... on MonitorEmail { id recipients { nodes { id } } }
        } }
    }
}";

const EVENTS: &str = "query($id: ID!) { node(id: $id) { ... on Monitor {
    trigger { ... on MonitorQuery { events { totalCount nodes {
        status message resultCount query ranges { repository revision from to }
        actions { nodes { ... on MonitorWebhook { events { nodes { status message } } } } }
    } } } }
} } }";

async fn wait_for_requests(server: &MockServer, n: usize) -> Vec<wiremock::Request> {
    for _ in 0..300 {
        let got = server.received_requests().await.unwrap_or_default();
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the webhook did not receive {n} requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_fire_webhook_actions() {
    let source = SourceRepo::new();
    let first = source.commit("initial", &[("README.md", "hello\n")]);
    // With encryption keys: the webhook is queued as a channel job on the outbound webhook
    // queue.
    let Some(env) = setup_with(&source, true).await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let hook = format!("{}/hook", server.uri());
    let alice_id = marshal_id("User", env.alice.get());

    // Validation: the query must be a commit or diff search; URLs must be http(s).
    let bad = env
        .err_code(
            env.alice,
            CREATE,
            json!({
                "monitor": {"namespace": alice_id, "description": "x", "enabled": true},
                "trigger": {"query": "TODO"},
                "actions": [{"webhook": {"enabled": true, "includeResults": false, "url": "ftp://x"}}],
            }),
        )
        .await;
    assert_eq!(bad, "VALIDATION");
    // Bob may not create monitors for alice.
    let forbidden = env
        .err_code(
            env.bob,
            CREATE,
            json!({
                "monitor": {"namespace": alice_id, "description": "x", "enabled": true},
                "trigger": {"query": "type:diff TODO"},
                "actions": [],
            }),
        )
        .await;
    assert_eq!(forbidden, "FORBIDDEN");

    let data = env
        .ok(
            env.alice,
            CREATE,
            json!({
                "monitor": {"namespace": alice_id, "description": "New TODOs", "enabled": true},
                "trigger": {"query": "type:diff TODO"},
                "actions": [
                    {"webhook": {"enabled": true, "includeResults": true, "url": hook}},
                    {"email": {"enabled": true, "includeResults": false, "priority": "NORMAL",
                        "recipients": [alice_id], "header": ""}},
                ],
            }),
        )
        .await;
    let m = &data["createCodeMonitor"];
    let monitor_id = m["id"].as_str().unwrap().to_owned();
    assert_eq!(m["trigger"]["query"], "type:diff TODO");
    assert_eq!(m["actions"]["totalCount"], 2);
    // Reads mask the URL: the origin only, as the path is short.
    assert_eq!(
        m["actions"]["nodes"][0]["url"],
        format!("{}/…", server.uri())
    );
    let webhook_id = m["actions"]["nodes"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        m["actions"]["nodes"][1]["recipients"]["nodes"][0]["id"],
        alice_id
    );

    // Bob cannot see it; alice lists it.
    let hidden = env
        .ok(
            env.bob,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({"id": monitor_id}),
        )
        .await;
    assert_eq!(hidden["node"], Value::Null);
    let mine = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { ... on User { monitors { totalCount nodes { id } } } } }",
            json!({"id": alice_id}),
        )
        .await;
    assert_eq!(mine["node"]["monitors"]["totalCount"], 1);

    // Workers: searches and deliveries (the webhook by the outbound webhook sender; email to
    // a memory mailer, but alice has no verified address, so that action fails and is
    // retried, which this test does not wait for).
    let cancel = CancellationToken::new();
    let (worker, resetter) = sourcegull_monitors::trigger_workers(
        env.backend.clone(),
        env.keyring.clone(),
        "test-trigger",
    )
    .unwrap();
    tokio::spawn(worker.run(cancel.clone()));
    tokio::spawn(resetter.run(cancel.clone()));
    let outbound = sourcegull_outbound_webhooks::Sender::new(
        env.db.clone(),
        env.keyring.clone(),
        env.config.clone(),
    );
    let (worker, _) = sourcegull_outbound_webhooks::sender_workers(
        outbound,
        sourcegull_outbound_webhooks::store::OutboundWebhookStore::queue_config(),
        "test-outbound",
    )
    .unwrap();
    tokio::spawn(worker.run(cancel.clone()));
    let sender = ActionSender::new(
        &env.db,
        env.config.clone(),
        Arc::new(MemoryEmailSender::default()),
    )
    .with_keyring(env.keyring.clone());
    let (worker, resetter) =
        sourcegull_monitors::action_workers(&env.db, sender, "test-action").unwrap();
    tokio::spawn(worker.run(cancel.clone()));
    tokio::spawn(resetter.run(cancel.clone()));

    // First evaluation: the head is recorded, nothing is searched or sent.
    let mut ev = Evaluator::new(env.backend.clone(), EvaluatorConfig::default());
    let r = ev.sweep(Utc::now()).await.unwrap();
    assert_eq!((r.repos, r.recorded, r.jobs), (1, 1, 0));
    assert_eq!(env.trigger_jobs().await, 0);
    // Nothing moved: a second sweep does nothing.
    let r = ev.sweep(Utc::now()).await.unwrap();
    assert_eq!((r.recorded, r.jobs), (0, 0));

    // A push with a matching commit and one without.
    source.commit(
        "add todo",
        &[("src/app.rs", "fn main() {\n    // TODO: fix\n}\n")],
    );
    let head = source.commit("docs", &[("README.md", "hello again\n")]);
    env.push(&first, &head).await;
    let r = ev.handle_events(Utc::now()).await.unwrap();
    assert_eq!(r.jobs, 1);

    let got = wait_for_requests(&server, 1).await;
    let body: Value = serde_json::from_slice(&got[0].body).unwrap();
    assert_eq!(got[0].url.path(), "/hook");
    assert_eq!(body["monitorDescription"], "New TODOs");
    assert_eq!(body["query"], "type:diff TODO");
    assert_eq!(body["resultCount"], 1);
    assert_eq!(body["test"], false);
    assert_eq!(
        body["monitorURL"],
        format!("https://sourcegull.example.com/watches/{monitor_id}")
    );
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["subject"], "add todo");
    assert_eq!(results[0]["repository"], REPO);
    assert_eq!(results[0]["kind"], "diff");
    assert!(
        results[0]["diff"]
            .as_str()
            .unwrap()
            .contains("+    // TODO: fix")
    );

    // The run log: one successful run with one result over the pushed range, and the
    // webhook's delivery.
    let mut events = Value::Null;
    for _ in 0..100 {
        events = env.ok(env.alice, EVENTS, json!({"id": monitor_id})).await;
        let e = &events["node"]["trigger"]["events"]["nodes"][0];
        if e["actions"]["nodes"][0]["events"]["nodes"][0]["status"] == "SUCCESS" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let runs = &events["node"]["trigger"]["events"];
    assert_eq!(runs["totalCount"], 1);
    let run = &runs["nodes"][0];
    assert_eq!(run["status"], "SUCCESS");
    assert_eq!(run["resultCount"], 1);
    assert_eq!(
        run["ranges"],
        json!([{"repository": REPO, "revision": "", "from": first, "to": head}])
    );
    assert_eq!(
        run["actions"]["nodes"][0]["events"]["nodes"][0]["status"],
        "SUCCESS"
    );
    let channel_jobs: i64 = sourcegull_db::sqlx::query_scalar(
        "SELECT count(*) FROM outbound_webhook_jobs WHERE channel = 'webhook' AND state = 'completed'",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(channel_jobs, 1);

    // Site admins see the attempt in the channel delivery log, with the watch and only the
    // host; other users may not read it.
    const DELIVERIES: &str = "{ channelDeliveryLogs(first: 10) {
        totalCount nodes { channel host statusCode error watchId watchDescription } } }";
    let logs = env.ok(env.admin, DELIVERIES, json!({})).await;
    let logs = &logs["channelDeliveryLogs"];
    assert_eq!(logs["totalCount"], 1);
    let host = server.address().ip().to_string();
    assert_eq!(
        logs["nodes"][0],
        json!({
            "channel": "WEBHOOK", "host": host, "statusCode": 200, "error": null,
            "watchId": monitor_id, "watchDescription": "New TODOs",
        })
    );
    assert!(!logs.to_string().contains("/hook"));
    let errors = env
        .ok(
            env.admin,
            "{ channelDeliveryLogs(onlyErrors: true) { totalCount } }",
            json!({}),
        )
        .await;
    assert_eq!(errors["channelDeliveryLogs"]["totalCount"], 0);
    assert_eq!(
        env.err_code(env.alice, DELIVERIES, json!({})).await,
        "FORBIDDEN"
    );

    // A push without matches runs the trigger but sends nothing.
    let next = source.commit("more docs", &[("README.md", "bye\n")]);
    env.push(&head, &next).await;
    assert_eq!(ev.handle_events(Utc::now()).await.unwrap().jobs, 1);
    for _ in 0..100 {
        let done: i64 = sourcegull_db::sqlx::query_scalar(
            "SELECT count(*) FROM cm_trigger_jobs WHERE state = 'completed'",
        )
        .fetch_one(env.db.pool())
        .await
        .unwrap();
        if done == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events = env.ok(env.alice, EVENTS, json!({"id": monitor_id})).await;
    let latest = &events["node"]["trigger"]["events"]["nodes"][0];
    assert_eq!(latest["status"], "SUCCESS");
    assert_eq!(latest["resultCount"], 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // "Send test" reaches the webhook at once.
    env.ok(
        env.alice,
        "mutation($ns: ID!, $hook: MonitorWebhookInput!) {
            triggerTestWebhookAction(namespace: $ns, description: \"New TODOs\", webhook: $hook) { alwaysNil }
        }",
        json!({"ns": alice_id, "hook": {"enabled": true, "includeResults": false, "url": hook}}),
    )
    .await;
    let got = wait_for_requests(&server, 2).await;
    let test: Value = serde_json::from_slice(&got[1].body).unwrap();
    assert_eq!(test["test"], true);

    // A test of the stored action with an empty URL uses the stored one; bob may not
    // test alice's action.
    const TEST_STORED: &str = "mutation($ns: ID!, $hook: MonitorWebhookInput!, $action: ID) {
        triggerTestWebhookAction(namespace: $ns, description: \"New TODOs\", webhook: $hook,
            action: $action) { alwaysNil }
    }";
    let empty = json!({"enabled": true, "includeResults": false, "url": ""});
    env.ok(
        env.alice,
        TEST_STORED,
        json!({"ns": alice_id, "hook": empty, "action": webhook_id}),
    )
    .await;
    let got = wait_for_requests(&server, 3).await;
    assert_eq!(got[2].url.path(), "/hook");
    let bob_id = marshal_id("User", env.bob.get());
    assert_eq!(
        env.err_code(
            env.bob,
            TEST_STORED,
            json!({"ns": bob_id, "hook": empty, "action": webhook_id}),
        )
        .await,
        "NOT_FOUND"
    );

    // Disabled monitors are not evaluated; bob cannot toggle or delete alice's monitor.
    assert_eq!(
        env.err_code(
            env.bob,
            "mutation($id: ID!) { toggleCodeMonitor(id: $id, enabled: false) { id } }",
            json!({"id": monitor_id}),
        )
        .await,
        "NOT_FOUND"
    );
    env.ok(
        env.alice,
        "mutation($id: ID!) { toggleCodeMonitor(id: $id, enabled: false) { enabled } }",
        json!({"id": monitor_id}),
    )
    .await;
    // The evaluator rebuilds its index on the change notification.
    ev.sweep(Utc::now()).await.unwrap();
    let later = source.commit("another todo", &[("b.rs", "// TODO again\n")]);
    env.push(&next, &later).await;
    assert_eq!(ev.handle_events(Utc::now()).await.unwrap().jobs, 0);

    // Site admins list every monitor; deleting removes it.
    let all = env
        .ok(
            env.admin,
            "{ monitors(first: 10) { totalCount } }",
            json!({}),
        )
        .await;
    assert_eq!(all["monitors"]["totalCount"], 1);
    env.ok(
        env.alice,
        "mutation($id: ID!) { deleteCodeMonitor(id: $id) { alwaysNil } }",
        json!({"id": monitor_id}),
    )
    .await;
    let all = env
        .ok(
            env.admin,
            "{ monitors(first: 10) { totalCount } }",
            json!({}),
        )
        .await;
    assert_eq!(all["monitors"]["totalCount"], 0);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_replaces_actions_and_rewatches() {
    let source = SourceRepo::new();
    source.commit("initial", &[("README.md", "hello\n")]);
    let Some(env) = setup(&source).await else {
        return;
    };
    let alice_id = marshal_id("User", env.alice.get());
    let data = env
        .ok(
            env.alice,
            CREATE,
            json!({
                "monitor": {"namespace": alice_id, "description": "Fixes", "enabled": true},
                "trigger": {"query": "type:commit fix"},
                "actions": [
                    {"webhook": {"enabled": true, "includeResults": false, "url": "https://hooks.example.com/a"}},
                    {"slackWebhook": {"enabled": true, "includeResults": true, "url": "https://hooks.slack.com/b"}},
                ],
            }),
        )
        .await;
    let m = &data["createCodeMonitor"];
    let id = m["id"].as_str().unwrap();
    let trigger = m["trigger"]["id"].as_str().unwrap();
    let webhook = m["actions"]["nodes"][0]["id"].as_str().unwrap();

    let mut ev = Evaluator::new(env.backend.clone(), EvaluatorConfig::default());
    assert_eq!(ev.sweep(Utc::now()).await.unwrap().recorded, 1);

    // Keep the webhook (changed), drop the Slack webhook, add an email; change the query.
    const UPDATE: &str = "mutation($monitor: MonitorEditInput!, $trigger: MonitorEditTriggerInput!,
            $actions: [MonitorEditActionInput!]!) {
        updateCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) {
            description trigger { ... on MonitorQuery { query } }
            actions { nodes {
                __typename
                ... on MonitorWebhook { id url includeResults }
                ... on MonitorEmail { id priority }
            } }
        }
    }";
    let data = env
        .ok(
            env.alice,
            UPDATE,
            json!({
                "monitor": {"id": id, "update": {"namespace": alice_id, "description": "Fixes!", "enabled": true}},
                "trigger": {"id": trigger, "update": {"query": "type:commit fix author:ann"}},
                "actions": [
                    {"webhook": {"id": webhook, "update": {"enabled": true, "includeResults": true, "url": "https://hooks.example.com/c"}}},
                    {"email": {"update": {"enabled": true, "includeResults": true, "priority": "CRITICAL", "recipients": [alice_id], "header": ""}}},
                ],
            }),
        )
        .await;
    let u = &data["updateCodeMonitor"];
    assert_eq!(u["description"], "Fixes!");
    assert_eq!(u["trigger"]["query"], "type:commit fix author:ann");
    let nodes = u["actions"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["id"], webhook);
    assert_eq!(nodes[0]["url"], "https://hooks.example.com/…");
    assert_eq!(nodes[0]["includeResults"], true);
    let stored_url = |action: &str| {
        let id = sourcegull_services::relay::unmarshal_int_id("CodeMonitorActionWebhook", action);
        let pool = env.db.pool().clone();
        async move {
            sourcegull_db::sqlx::query_scalar::<_, String>(
                "SELECT url FROM cm_actions WHERE id = $1",
            )
            .bind(id.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(stored_url(webhook).await, "https://hooks.example.com/c");

    // An empty URL, or the masked one sent back, keeps the stored URL; creating needs one.
    for url in ["", "https://hooks.example.com/…"] {
        env.ok(
            env.alice,
            UPDATE,
            json!({
                "monitor": {"id": id, "update": {"namespace": alice_id, "description": "Fixes!", "enabled": true}},
                "trigger": {"id": trigger, "update": {"query": "type:commit fix author:ann"}},
                "actions": [
                    {"webhook": {"id": webhook, "update": {"enabled": true, "includeResults": true, "url": url}}},
                    {"email": {"id": nodes[1]["id"], "update": {"enabled": true, "includeResults": true, "priority": "CRITICAL", "recipients": [alice_id], "header": ""}}},
                ],
            }),
        )
        .await;
        assert_eq!(
            stored_url(webhook).await,
            "https://hooks.example.com/c",
            "{url:?}"
        );
    }
    assert_eq!(
        env.err_code(
            env.alice,
            UPDATE,
            json!({
                "monitor": {"id": id, "update": {"namespace": alice_id, "description": "Fixes!", "enabled": true}},
                "trigger": {"id": trigger, "update": {"query": "type:commit fix author:ann"}},
                "actions": [
                    {"webhook": {"update": {"enabled": true, "includeResults": true, "url": ""}}},
                ],
            }),
        )
        .await,
        "VALIDATION"
    );
    assert_eq!(nodes[1]["__typename"], "MonitorEmail");
    assert_eq!(nodes[1]["priority"], "CRITICAL");

    // A new query records the heads again instead of searching.
    let r = ev.sweep(Utc::now()).await.unwrap();
    assert_eq!((r.recorded, r.jobs), (1, 0));

    // resetTriggerQueryTimestamps (site admins) searches the whole history once.
    assert_eq!(
        env.err_code(
            env.alice,
            "mutation($id: ID!) { resetTriggerQueryTimestamps(id: $id) { alwaysNil } }",
            json!({"id": trigger}),
        )
        .await,
        "FORBIDDEN"
    );
    env.ok(
        env.admin,
        "mutation($id: ID!) { resetTriggerQueryTimestamps(id: $id) { alwaysNil } }",
        json!({"id": trigger}),
    )
    .await;
    let r = ev.sweep(Utc::now()).await.unwrap();
    assert_eq!(r.jobs, 1);
    let ranges: Value = sourcegull_db::sqlx::query_scalar(
        "SELECT ranges FROM cm_trigger_jobs ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(ranges[0]["from"], Value::Null);
    assert_eq!(ev.sweep(Utc::now()).await.unwrap().jobs, 0);

    // A trigger ID of another monitor is refused.
    let wrong = marshal_id("CodeMonitorTriggerQuery", 999);
    assert_eq!(
        env.err_code(
            env.alice,
            UPDATE,
            json!({
                "monitor": {"id": id, "update": {"namespace": alice_id, "description": "x", "enabled": true}},
                "trigger": {"id": wrong, "update": {"query": "type:commit x"}},
                "actions": [],
            }),
        )
        .await,
        "INVALID_INPUT"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_urls_are_encrypted_and_email_header_is_sent() {
    let source = SourceRepo::new();
    source.commit("initial", &[("README.md", "hello\n")]);
    let Some(env) = setup_with(&source, true).await else {
        return;
    };
    let admin_id = marshal_id("User", env.admin.get());
    let hook = "https://hooks.example.com/secret-token";
    let slack = "https://hooks.slack.com/services/T0/B0/xyz";
    let create = |header: &str| {
        json!({
            "monitor": {"namespace": admin_id, "description": "Secrets", "enabled": true},
            "trigger": {"query": "type:diff TODO"},
            "actions": [
                {"webhook": {"enabled": true, "includeResults": false, "url": hook}},
                {"slackWebhook": {"enabled": true, "includeResults": false, "url": slack}},
                {"email": {"enabled": true, "includeResults": false, "priority": "NORMAL",
                    "recipients": [admin_id], "header": header}},
            ],
        })
    };

    // The header must be one "Name: value" field that does not override the message's own.
    for bad in [
        "not a header",
        "Bcc: eve@example.com",
        "Approved: x\nBcc: eve@example.com",
    ] {
        assert_eq!(
            env.err_code(env.admin, CREATE, create(bad)).await,
            "VALIDATION",
            "{bad:?}"
        );
    }
    let data = env
        .ok(env.admin, CREATE, create("Approved: list-password"))
        .await;
    let m = &data["createCodeMonitor"];
    assert_eq!(
        m["actions"]["nodes"][0]["url"],
        "https://hooks.example.com/…oken"
    );
    let monitor = sourcegull_services::relay::unmarshal_int_id(
        sourcegull_monitors::MONITOR_KIND,
        m["id"].as_str().unwrap(),
    )
    .unwrap();

    // At rest: no plaintext URL, and the ciphertext is the watch webhook class's.
    let rows: Vec<(String, String, Option<Vec<u8>>)> = sourcegull_db::sqlx::query_as(
        "SELECT kind, url, url_enc FROM cm_actions WHERE monitor_id = $1 ORDER BY id",
    )
    .bind(monitor)
    .fetch_all(env.db.pool())
    .await
    .unwrap();
    let keyring = env.keyring.clone().unwrap();
    for (kind, url, enc) in &rows[..2] {
        assert_eq!(url, "", "{kind}");
        let plain = keyring
            .decrypt(
                sourcegull_crypto::RecordClass::CodeMonitorWebhook,
                enc.as_ref().unwrap(),
            )
            .unwrap();
        assert!(plain.starts_with(b"https://hooks."), "{kind}");
    }
    assert_eq!(rows[2].2, None, "email actions have no URL");
    let raw = String::from_utf8_lossy(&rows[0].2.clone().unwrap()).into_owned();
    assert!(!raw.contains("secret-token"));
    // The email header is encrypted the same way.
    let stored_header = || async {
        let (header, enc): (String, Option<Vec<u8>>) = sourcegull_db::sqlx::query_as(
            "SELECT header, header_enc FROM cm_actions WHERE monitor_id = $1 AND kind = 'email'",
        )
        .bind(monitor)
        .fetch_one(env.db.pool())
        .await
        .unwrap();
        assert_eq!(header, "");
        let plain = keyring
            .decrypt(
                sourcegull_crypto::RecordClass::CodeMonitorWebhook,
                enc.as_ref().unwrap(),
            )
            .unwrap();
        String::from_utf8(plain.to_vec()).unwrap()
    };
    assert_eq!(stored_header().await, "Approved: list-password");

    // GraphQL reads return masked URLs; the store decrypts with the keyring, and without it
    // the URL is unreadable.
    let actions = env
        .ok(
            env.admin,
            "query($id: ID!) { node(id: $id) { ... on Monitor { actions { nodes {
                ... on MonitorWebhook { url }
                ... on MonitorSlackWebhook { url }
                ... on MonitorEmail { header }
            } } } } }",
            json!({"id": m["id"]}),
        )
        .await;
    assert_eq!(
        actions["node"]["actions"]["nodes"],
        json!([
            {"url": "https://hooks.example.com/…oken"},
            {"url": "https://hooks.slack.com/…/xyz"},
            {"header": "Approved: …word"}
        ])
    );
    let store = sourcegull_monitors::MonitorStore::new(&env.db).with_keyring(env.keyring.clone());
    let stored = store.actions(monitor).await.unwrap();
    assert_eq!(stored[1].spec.url, slack);
    assert!(
        sourcegull_monitors::MonitorStore::new(&env.db)
            .actions(monitor)
            .await
            .is_err()
    );

    // The email carries the header.
    sourcegull_db::sqlx::query("UPDATE user_emails SET verified_at = now() WHERE user_id = $1")
        .bind(env.admin)
        .execute(env.db.pool())
        .await
        .unwrap();
    let mailer = MemoryEmailSender::default();
    let sender = ActionSender::new(&env.db, env.config.clone(), Arc::new(mailer.clone()))
        .with_keyring(env.keyring.clone());
    let n = sourcegull_monitors::Notification {
        monitor_id: monitor,
        description: "Secrets",
        query: "type:diff TODO",
        results: &[],
        external_url: "https://sourcegull.example.com",
        test: true,
    };
    sender.send(&stored[2].spec, &n).await.unwrap();
    let sent = mailer.messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "admin@example.com");
    assert_eq!(
        sent[0].headers,
        vec![("Approved".to_owned(), "list-password".to_owned())]
    );
    // Without a header, none is added.
    let mut plain = stored[2].spec.clone();
    plain.header = String::new();
    sender.send(&plain, &n).await.unwrap();
    assert!(mailer.messages()[1].headers.is_empty());

    // An update with an empty or the masked header keeps the stored one; a new header
    // replaces it, and `removeHeader` removes it.
    const UPDATE: &str = "mutation($monitor: MonitorEditInput!, $trigger: MonitorEditTriggerInput!,
            $actions: [MonitorEditActionInput!]!) {
        updateCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) {
            actions { nodes { ... on MonitorEmail { header } } }
        }
    }";
    let trigger = m["trigger"]["id"].clone();
    let email_id = m["actions"]["nodes"][2]["id"].clone();
    let update = |header: &str, remove: bool| {
        json!({
            "monitor": {"id": m["id"], "update": {"namespace": admin_id, "description": "Secrets", "enabled": true}},
            "trigger": {"id": trigger, "update": {"query": "type:diff TODO"}},
            "actions": [
                {"email": {"id": email_id, "update": {"enabled": true, "includeResults": false,
                    "priority": "NORMAL", "recipients": [admin_id], "header": header,
                    "removeHeader": remove}}},
            ],
        })
    };
    for header in ["", "Approved: …word"] {
        let data = env.ok(env.admin, UPDATE, update(header, false)).await;
        assert_eq!(
            data["updateCodeMonitor"]["actions"]["nodes"][0]["header"],
            "Approved: …word"
        );
        assert_eq!(
            stored_header().await,
            "Approved: list-password",
            "{header:?}"
        );
    }

    // A test send with the action's ID uses the stored header.
    const TEST_EMAIL: &str = "mutation($ns: ID!, $email: MonitorEmailInput!, $action: ID) {
        triggerTestEmailAction(namespace: $ns, description: \"Secrets\", email: $email,
            action: $action) { alwaysNil }
    }";
    let email = json!({"enabled": true, "includeResults": false, "priority": "NORMAL",
        "recipients": [admin_id], "header": "Approved: …word"});
    env.ok(
        env.admin,
        TEST_EMAIL,
        json!({"ns": admin_id, "email": email, "action": email_id}),
    )
    .await;
    assert_eq!(
        env.mailer.messages().last().unwrap().headers,
        vec![("Approved".to_owned(), "list-password".to_owned())]
    );
    env.ok(env.admin, UPDATE, update("Approved: other-password", false))
        .await;
    assert_eq!(stored_header().await, "Approved: other-password");
    let data = env.ok(env.admin, UPDATE, update("", true)).await;
    assert_eq!(
        data["updateCodeMonitor"]["actions"]["nodes"][0]["header"],
        ""
    );
    let header: String =
        sourcegull_db::sqlx::query_scalar("SELECT header FROM cm_actions WHERE monitor_id = $1")
            .bind(monitor)
            .fetch_one(env.db.pool())
            .await
            .unwrap();
    assert_eq!(header, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mattermost_and_telegram_actions() {
    use wiremock::matchers::path;

    let source = SourceRepo::new();
    source.commit("initial", &[("README.md", "hello\n")]);
    let Some(env) = setup_with(&source, true).await else {
        return;
    };
    let server = MockServer::start().await;
    let token = "123456:ABC-secret_token";
    Mock::given(method("POST"))
        .and(path("/hooks/mm"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{token}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"ok": false, "description": "Bad Request: chat not found"})),
        )
        .mount(&server)
        .await;
    let mattermost = format!("{}/hooks/mm", server.uri());
    let alice_id = marshal_id("User", env.alice.get());
    let create = |token: &str, chat: &str| {
        json!({
            "monitor": {"namespace": alice_id, "description": "Chats", "enabled": true},
            "trigger": {"query": "type:diff TODO"},
            "actions": [
                {"mattermostWebhook": {"enabled": true, "includeResults": true, "url": mattermost}},
                {"telegram": {"enabled": true, "includeResults": false, "botToken": token,
                    "chatId": chat}},
            ],
        })
    };
    for (token, chat) in [("not-a-token", "-100123"), (token, "general")] {
        assert_eq!(
            env.err_code(env.alice, CREATE, create(token, chat)).await,
            "VALIDATION",
            "{token} {chat}"
        );
    }
    let data = env
        .ok(env.alice, CREATE, create(&format!(" {token} "), "-100123"))
        .await;
    let id = data["createCodeMonitor"]["id"].as_str().unwrap().to_owned();
    let read =
        "query($id: ID!) { node(id: $id) { ... on Monitor { trigger { ... on MonitorQuery { id } }
        actions { nodes {
            __typename
            ... on MonitorMattermostWebhook { id url includeResults }
            ... on MonitorTelegram { id botToken chatId includeResults }
        } } } } }";
    let m = env.ok(env.alice, read, json!({"id": id})).await;
    let nodes = &m["node"]["actions"]["nodes"];
    assert_eq!(nodes[0]["__typename"], "MonitorMattermostWebhook");
    assert_eq!(nodes[0]["url"], format!("{}/…", server.uri()));
    assert_eq!(nodes[1]["__typename"], "MonitorTelegram");
    assert_eq!(nodes[1]["botToken"], "…oken");
    assert_eq!(nodes[1]["chatId"], "-100123");

    // The bot token is encrypted at rest like webhook URLs.
    let rows: Vec<(String, String, Option<Vec<u8>>, String)> = sourcegull_db::sqlx::query_as(
        "SELECT kind, url, url_enc, chat_id FROM cm_actions ORDER BY id",
    )
    .fetch_all(env.db.pool())
    .await
    .unwrap();
    assert_eq!(rows[0].0, "mattermost_webhook");
    assert_eq!(rows[1].0, "telegram");
    assert_eq!(rows[1].1, "");
    assert_eq!(rows[1].3, "-100123");
    assert!(!String::from_utf8_lossy(rows[1].2.as_ref().unwrap()).contains("secret_token"));

    // Deliveries: Mattermost gets Markdown, Telegram a sendMessage call to the chat.
    let store = sourcegull_monitors::MonitorStore::new(&env.db).with_keyring(env.keyring.clone());
    let monitor =
        sourcegull_services::relay::unmarshal_int_id(sourcegull_monitors::MONITOR_KIND, &id)
            .unwrap();
    let stored = store.actions(monitor).await.unwrap();
    let sender = ActionSender::new(
        &env.db,
        env.config.clone(),
        Arc::new(MemoryEmailSender::default()),
    )
    .with_telegram_api(&server.uri());
    let n = sourcegull_monitors::Notification {
        monitor_id: monitor,
        description: "Chats",
        query: "type:diff TODO",
        results: &[],
        external_url: "https://sourcegull.example.com",
        test: false,
    };
    sender.send(&stored[0].spec, &n).await.unwrap();
    sender.send(&stored[1].spec, &n).await.unwrap();
    let got = wait_for_requests(&server, 2).await;
    let mm: Value = serde_json::from_slice(&got[0].body).unwrap();
    assert!(
        mm["text"].as_str().unwrap().starts_with(
            "Watch \"Chats\" found 0 new results.\n[Chats](https://sourcegull.example.com/watches/"
        ),
        "{mm}"
    );
    assert_eq!(got[1].url.path(), format!("/bot{token}/sendMessage"));
    let tg: Value = serde_json::from_slice(&got[1].body).unwrap();
    assert_eq!(tg["chat_id"], "-100123");
    assert_eq!(tg["parse_mode"], "HTML");
    assert!(
        tg["text"]
            .as_str()
            .unwrap()
            .starts_with("<b>Watch &quot;Chats&quot;")
    );

    // A refusal reports the Bot API's description, never the token.
    let mut wrong = stored[1].spec.clone();
    wrong.url = "999:other".to_owned();
    let err = sender.send(&wrong, &n).await.unwrap_err();
    assert_eq!(err, "unexpected status 400: Bad Request: chat not found");
    let unreachable = ActionSender::new(
        &env.db,
        env.config.clone(),
        Arc::new(MemoryEmailSender::default()),
    )
    .with_telegram_api("http://127.0.0.1:1");
    let err = unreachable.send(&stored[1].spec, &n).await.unwrap_err();
    assert!(!err.contains("secret_token"), "{err}");

    // Updates keep the actions by id, and an empty bot token keeps the stored one.
    let trigger = m["node"]["trigger"]["id"].clone();
    let update = "mutation($monitor: MonitorEditInput!, $trigger: MonitorEditTriggerInput!,
            $actions: [MonitorEditActionInput!]!) {
        updateCodeMonitor(monitor: $monitor, trigger: $trigger, actions: $actions) { id }
    }";
    env.ok(
        env.alice,
        update,
        json!({
            "monitor": {"id": id, "update": {"namespace": alice_id, "description": "Chats",
                "enabled": true}},
            "trigger": {"id": trigger, "update": {"query": "type:diff TODO"}},
            "actions": [
                {"telegram": {"id": nodes[1]["id"], "update": {"enabled": false,
                    "includeResults": true, "botToken": "", "chatId": "@sourcegull_alerts"}}},
            ],
        }),
    )
    .await;
    let after = store.actions(monitor).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, stored[1].id);
    assert_eq!(after[0].spec.chat_id, "@sourcegull_alerts");
    assert_eq!(after[0].spec.url, token);
    assert!(!after[0].spec.enabled && after[0].spec.include_results);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_predicates_filter_monitor_searches() {
    use sourcegull_monitors::TriggerHandler;
    use sourcegull_monitors::store::{CommitRange, TriggerJob};

    let source = SourceRepo::new();
    let first = source.commit("initial", &[("README.md", "hello\n")]);
    source.commit("fix one", &[("src/app.rs", "fn main() {}\n")]);
    let head = source.commit("fix two", &[("README.md", "hello again\n")]);
    let Some(env) = setup(&source).await else {
        return;
    };
    let alice_id = marshal_id("User", env.alice.get());
    env.ok(
        env.alice,
        CREATE,
        json!({
            "monitor": {"namespace": alice_id, "description": "Fixes", "enabled": true},
            "trigger": {"query": "type:commit fix"},
            "actions": [],
        }),
    )
    .await;
    let monitor_id: i64 = sourcegull_db::sqlx::query_scalar("SELECT id FROM cm_monitors")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    let handler = TriggerHandler::new(env.backend.clone());
    let search = |query: &str| {
        let job = TriggerJob {
            monitor_id,
            ranges: vec![CommitRange {
                repo_id: env.repo.get(),
                repo: REPO.into(),
                rev: String::new(),
                from: Some(first.clone()),
                to: head.clone(),
            }],
            query: query.into(),
        };
        let handler = handler.clone();
        async move {
            let mut subjects: Vec<String> = handler
                .search(&job)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.subject)
                .collect();
            subjects.sort();
            subjects
        }
    };
    let both = vec!["fix one".to_owned(), "fix two".to_owned()];
    // The commits are from November 2023.
    for (query, found) in [
        ("type:commit fix", true),
        ("type:commit repo:has.path(app\\.rs) fix", true),
        ("type:commit repo:has.path(nothing\\.rs) fix", false),
        ("type:commit -repo:has.path(app\\.rs) fix", false),
        ("type:commit repo:has.content(hello) fix", true),
        (
            "type:commit repo:has.file(path:README content:bye) fix",
            false,
        ),
        ("type:commit repo:has.commit.after(2023-01-01) fix", true),
        ("type:commit repo:has.commit.after(2024-06-01) fix", false),
        ("type:commit -repo:has.commit.after(2024-06-01) fix", true),
    ] {
        let want = if found { both.clone() } else { Vec::new() };
        assert_eq!(search(query).await, want, "{query}");
    }
    // Invalid arguments are refused as in a search.
    assert!(
        sourcegull_monitors::validate_query("type:commit repo:has.commit.after(someday) x")
            .is_err()
    );
    assert!(sourcegull_monitors::validate_query("type:commit repo:has.path([) x").is_err());

    // `rev:at.time()` fixes the revision to one commit, which never moves: the monitor
    // watches nothing.
    let at = env
        .ok(
            env.alice,
            CREATE,
            json!({
                "monitor": {"namespace": alice_id, "description": "Pinned", "enabled": true},
                "trigger": {"query": "type:commit rev:at.time(2024-01-01) fix"},
                "actions": [],
            }),
        )
        .await;
    assert_eq!(at["createCodeMonitor"]["enabled"], true);
    let mut ev = Evaluator::new(env.backend.clone(), EvaluatorConfig::default());
    // Only the first monitor's head is recorded.
    assert_eq!(ev.sweep(Utc::now()).await.unwrap().recorded, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unfiltered_results_need_a_site_admin_or_the_setting() {
    let source = SourceRepo::new();
    source.commit("initial", &[("README.md", "hello\n")]);
    let Some(env) = setup(&source).await else {
        return;
    };
    const CAN: &str = "{ viewerCanShareUnfilteredWatchResults }";
    const SHOW: &str = "query($id: ID!) { node(id: $id) { ... on Monitor {
        runsAs { username } unfilteredResults unfilteredBy { username } unfilteredAt
    } } }";
    let create = |user: UserId| {
        json!({
            "monitor": {
                "namespace": marshal_id("User", user.get()),
                "description": "Secrets",
                "enabled": true,
                "unfilteredResults": true,
            },
            "trigger": {"query": "type:diff secret"},
            "actions": [{"webhook": {"enabled": true, "includeResults": false, "url": "https://hooks.example.com/a"}}],
        })
    };

    assert_eq!(
        env.ok(env.alice, CAN, json!({})).await,
        json!({"viewerCanShareUnfilteredWatchResults": false})
    );
    assert_eq!(
        env.err_code(env.alice, CREATE, create(env.alice)).await,
        "FORBIDDEN"
    );

    assert_eq!(
        env.ok(env.admin, CAN, json!({})).await,
        json!({"viewerCanShareUnfilteredWatchResults": true})
    );
    let data = env.ok(env.admin, CREATE, create(env.admin)).await;
    let id = data["createCodeMonitor"]["id"].as_str().unwrap().to_owned();
    let shown = env.ok(env.admin, SHOW, json!({ "id": id })).await;
    let m = &shown["node"];
    assert_eq!(m["runsAs"]["username"], "admin");
    assert_eq!(m["unfilteredResults"], true);
    assert_eq!(m["unfilteredBy"]["username"], "admin");
    assert!(m["unfilteredAt"].is_string());

    // The setting lets every user turn it on.
    let mut config = env.config.load().value.clone();
    config.watches_allow_unfiltered_results = true;
    env.config.store(env.config.version() + 1, config);
    assert_eq!(
        env.ok(env.alice, CAN, json!({})).await,
        json!({"viewerCanShareUnfilteredWatchResults": true})
    );
    let data = env.ok(env.alice, CREATE, create(env.alice)).await;
    let id = data["createCodeMonitor"]["id"].as_str().unwrap().to_owned();
    let shown = env.ok(env.alice, SHOW, json!({ "id": id })).await;
    assert_eq!(shown["node"]["unfilteredResults"], true);
}
