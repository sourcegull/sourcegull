//! Watch actions delivered as outbound webhook channels, against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and a local `wiremock` receiver: HTTP actions are queued as
//! channel jobs on the outbound webhook queue, whose sender retries a failed attempt; the SSRF
//! guard refuses a host that is not allowed; every attempt is in the delivery log with the URL
//! reduced to its origin; action jobs of HTTP actions on the watches' own queue are still
//! delivered; turning a watch off cancels its waiting channel jobs.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_monitors::actions::ActionHandler;
use sourcegull_monitors::store::{
    ActionKind, ActionSpec, MonitorStore, NewMonitor, Priority, TURNED_OFF,
};
use sourcegull_monitors::{
    ActionEvent, ActionSender, ChannelAction, MonitorResult, Notification, channel_message,
};
use sourcegull_outbound_webhooks::guard::{Lookup, LookupFuture};
use sourcegull_outbound_webhooks::store::OutboundWebhookStore;
use sourcegull_outbound_webhooks::{SendHandler, Sender};
use sourcegull_pgqueue::{Queue, Worker, WorkerOptions};
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Resolves the names in the table, like a DNS server would.
struct Table(HashMap<String, Vec<IpAddr>>);

impl Lookup for Table {
    fn lookup(&self, host: &str) -> LookupFuture {
        let r = self
            .0
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"));
        Box::pin(async move { r })
    }
}

fn table() -> Arc<Table> {
    Arc::new(Table(HashMap::from([
        ("hooks.test".to_owned(), vec!["127.0.0.1".parse().unwrap()]),
        (
            "internal.test".to_owned(),
            vec!["127.0.0.1".parse().unwrap()],
        ),
    ])))
}

fn spec(kind: ActionKind, url: String) -> ActionSpec {
    ActionSpec {
        kind,
        enabled: true,
        include_results: false,
        url,
        chat_id: String::new(),
        priority: Priority::Normal,
        header: String::new(),
        recipients: Vec::new(),
    }
}

async fn finished(store: &MonitorStore, action_id: i64) -> ActionEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (events, _) = store
            .action_events(Some(action_id), None, 1, 0)
            .await
            .unwrap();
        if let Some(e) = events.into_iter().next()
            && matches!(e.state.as_str(), "completed" | "failed")
        {
            return e;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "action {action_id} not delivered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn worker_options(id: &str) -> WorkerOptions {
    let mut opts = WorkerOptions::new(id);
    opts.poll_interval = Duration::from_millis(50);
    opts
}

#[tokio::test]
async fn http_actions_are_channel_jobs_of_the_outbound_queue() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring: Arc<Keyring> =
        Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let config = SiteConfigHandle::new(
        1,
        SiteConfig {
            webhooks_outbound_allowed_hosts: vec!["hooks.test".to_owned()],
            external_url: Some("https://sourcegull.example.com".to_owned()),
            ..SiteConfig::default()
        },
    );

    // The Slack webhook fails once, then accepts.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/services/T0/B0/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("try again"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/services/T0/B0/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let port = server.address().port();

    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let store = MonitorStore::new(&db).with_keyring(Some(keyring.clone()));
    let mut email = spec(ActionKind::Email, String::new());
    email.recipients = vec![Namespace::User(alice)];
    let monitor = store
        .create(&NewMonitor {
            namespace: Namespace::User(alice),
            description: "TODOs".into(),
            enabled: true,
            query: "type:diff TODO".into(),
            actions: vec![
                spec(
                    ActionKind::SlackWebhook,
                    format!("http://hooks.test:{port}/services/T0/B0/token"),
                ),
                // Resolves to loopback and is not in allowedHosts.
                spec(
                    ActionKind::Webhook,
                    format!("http://internal.test:{port}/hook"),
                ),
                email,
            ],
            created_by: alice,
            unfiltered: false,
        })
        .await
        .unwrap();
    let actions = store.actions(monitor.id).await.unwrap();
    let (slack, webhook, email) = (actions[0].id, actions[1].id, actions[2].id);

    let result = MonitorResult {
        kind: "diff".into(),
        repository: "acme/app".into(),
        repository_id: 1,
        commit: "abc123".into(),
        url: "/acme/app/-/commit/abc123".into(),
        subject: "add todo".into(),
        message: "add todo".into(),
        author_name: "Alice".into(),
        author_date: "2026-10-26T00:00:00Z".into(),
        diff: String::new(),
    };
    let results = [result];
    // What the trigger job worker renders for the HTTP actions.
    let n = Notification {
        monitor_id: monitor.id,
        description: "TODOs",
        query: "type:diff TODO",
        results: &results,
        external_url: "https://sourcegull.example.com",
        test: false,
    };
    let channels: Vec<ChannelAction> = actions
        .iter()
        .filter_map(|a| {
            let (channel, body) = channel_message(&a.spec, &n)?;
            Some(ChannelAction {
                action_id: a.id,
                channel,
                target: a.spec.url.clone(),
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
        .collect();
    assert_eq!(channels.len(), 2);
    let new_trigger_job = || async {
        store
            .record_evaluation(
                &[],
                &[],
                &[(monitor.id, "type:diff TODO".to_owned(), Vec::new())],
                &[],
                Utc::now(),
                &sourcegull_db::Fence::none("test"),
            )
            .await
            .unwrap()[0]
    };

    // A trigger job that found a commit queues the HTTP actions as channel jobs and email as
    // an action job, once each even when it finishes twice.
    let job = new_trigger_job().await;
    store
        .finish_trigger_job(job, &results, &channels)
        .await
        .unwrap();
    store
        .finish_trigger_job(job, &results, &channels)
        .await
        .unwrap();
    let count = |sql: &'static str| {
        let pool = t.pool.clone();
        async move {
            sourcegull_db::sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM outbound_webhook_jobs WHERE channel IS NOT NULL").await,
        2
    );
    let (email_events, _) = store.action_events(Some(email), None, 10, 0).await.unwrap();
    assert_eq!(email_events.len(), 1);
    assert!(!email_events[0].channel_job);
    assert_eq!(count("SELECT count(*) FROM cm_action_jobs").await, 1);
    // The target is encrypted.
    let target: Vec<u8> = sourcegull_db::sqlx::query_scalar(
        "SELECT target_enc FROM outbound_webhook_jobs WHERE cm_action_id = $1",
    )
    .bind(slack)
    .fetch_one(&t.pool)
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&target).contains("token"));

    // The outbound webhook sender delivers them with its retries.
    let sender =
        Sender::new(db.clone(), Some(keyring.clone()), config.clone()).with_lookup(table());
    let mut queue_config = OutboundWebhookStore::queue_config();
    queue_config.max_failures = 2;
    queue_config.backoff_base = Duration::from_millis(50);
    queue_config.backoff_max = Duration::from_millis(200);
    let queue = Queue::new(db.pool().clone(), queue_config).unwrap();
    let worker = Worker::new(
        queue,
        SendHandler::new(sender),
        worker_options("test-outbound"),
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(worker.run(cancel.clone()));
    let slack_event = finished(&store, slack).await;
    let webhook_event = finished(&store, webhook).await;
    cancel.cancel();
    task.await.unwrap();

    // Slack: retried after the 500, then delivered; both attempts are logged by job.
    assert!(slack_event.channel_job);
    assert_eq!(slack_event.state, "completed");
    assert_eq!(slack_event.num_failures, 1);
    assert_eq!(slack_event.trigger_job_id, job);
    let got = server.received_requests().await.unwrap();
    let slack_requests: Vec<_> = got
        .iter()
        .filter(|r| r.url.path() == "/services/T0/B0/token")
        .collect();
    assert_eq!(slack_requests.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&slack_requests[1].body).unwrap();
    assert!(body.to_string().contains("TODOs"), "{body}");
    let outbound = OutboundWebhookStore::new(&db, Some(keyring.clone()));
    let logs = outbound.channel_job_logs(slack_event.id).await.unwrap();
    assert_eq!(
        logs.iter().map(|l| l.status_code).collect::<Vec<_>>(),
        [500, 200]
    );
    assert!(logs.iter().all(|l| l.channel == "slack_webhook"));
    assert!(logs.iter().all(|l| l.job_id == Some(slack_event.id)));
    assert_eq!(logs[0].request.url, format!("http://hooks.test:{port}/…"));
    assert_eq!(logs[0].error.as_deref(), Some("unexpected status 500"));
    assert_eq!(logs[1].response.as_ref().unwrap().body, "ok");

    // The generic webhook: refused by the guard on every attempt, never sent.
    assert!(webhook_event.channel_job);
    assert_eq!(webhook_event.state, "failed");
    let message = webhook_event.failure_message.clone().unwrap();
    assert!(message.contains("not a public address"), "{message}");
    assert!(!message.contains("/hook"), "{message}");
    assert!(got.iter().all(|r| r.url.path() != "/hook"));
    let logs = outbound.channel_job_logs(webhook_event.id).await.unwrap();
    assert_eq!(
        logs.len(),
        usize::try_from(webhook_event.num_failures).unwrap()
    );
    assert!(
        logs.iter()
            .all(|l| l.status_code == 0 && l.channel == "webhook")
    );
    // Webhook logs of registered webhooks do not include channel attempts.
    assert!(outbound.log(logs[0].id).await.unwrap().is_none());
    assert!(
        outbound
            .job_logs(webhook_event.id)
            .await
            .unwrap()
            .is_empty()
    );

    // Action jobs of HTTP actions on the watches' queue (queued without encryption keys) are
    // still delivered by the action job worker and logged by action job.
    let job = new_trigger_job().await;
    store.finish_trigger_job(job, &results, &[]).await.unwrap();
    let action_sender = ActionSender::new(&db, config, Arc::new(MemoryEmailSender::default()))
        .with_keyring(Some(keyring.clone()))
        .with_lookup(table());
    let queue = Queue::new(db.pool().clone(), MonitorStore::action_queue_config()).unwrap();
    let worker = Worker::new(
        queue,
        ActionHandler::new(action_sender),
        worker_options("test-actions"),
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(worker.run(cancel.clone()));
    let legacy = finished(&store, slack).await;
    cancel.cancel();
    task.await.unwrap();
    assert!(!legacy.channel_job);
    assert_eq!(legacy.state, "completed");
    let logs = outbound.channel_logs(legacy.id).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|l| (l.status_code, l.cm_action_job_id))
            .collect::<Vec<_>>(),
        [(200, Some(legacy.id))]
    );
    // The action log lists both deliveries, newest first.
    let (events, total) = store.action_events(Some(slack), None, 10, 0).await.unwrap();
    assert_eq!(total, 2);
    assert_eq!(
        events.iter().map(|e| e.channel_job).collect::<Vec<_>>(),
        [false, true]
    );

    // Turning the watch off cancels the channel jobs that wait for an attempt.
    let job = new_trigger_job().await;
    store
        .finish_trigger_job(job, &results, &channels)
        .await
        .unwrap();
    store.set_enabled(monitor.id, false, alice).await.unwrap();
    let (events, _) = store
        .action_events(Some(slack), Some(job), 10, 0)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, "canceled");
    assert_eq!(events[0].failure_message.as_deref(), Some(TURNED_OFF));
}
