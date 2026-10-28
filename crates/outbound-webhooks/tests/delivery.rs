//! Outbound webhook delivery against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`)
//! and local `wiremock` receivers: signed requests, retries with backoff that skip webhooks
//! already delivered to, delivery logs, the SSRF guard (IP literals, DNS answers, redirects and
//! the allowlist), giving up after the last attempt, and the log janitor.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, migrate};
use sourcegull_outbound_webhooks::guard::{Lookup, LookupFuture};
use sourcegull_outbound_webhooks::store::{
    NewOutboundWebhook, OutboundWebhookJobRow, OutboundWebhookStore, ScopedEventType,
};
use sourcegull_outbound_webhooks::{
    CHANGESET_CLOSE, CHANGESET_PUBLISH, DELIVERY_HEADER, EVENT_TYPE_HEADER, SIGNATURE_HEADER,
    SendHandler, Sender, enqueue_event, sign,
};
use sourcegull_pgqueue::{JobState, Queue, Worker, WorkerOptions};
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    config: SiteConfigHandle,
}

async fn setup(allowed_hosts: &[&str]) -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let config = SiteConfigHandle::new(
        1,
        SiteConfig {
            webhooks_outbound_allowed_hosts: allowed_hosts.iter().map(|h| h.to_string()).collect(),
            ..SiteConfig::default()
        },
    );
    Some(Env {
        _t: t,
        db,
        keyring,
        config,
    })
}

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

fn subscribed(event_type: &str) -> Vec<ScopedEventType> {
    vec![ScopedEventType {
        event_type: event_type.into(),
        scope: None,
    }]
}

impl Env {
    fn store(&self) -> OutboundWebhookStore {
        OutboundWebhookStore::new(&self.db, Some(self.keyring.clone()))
    }

    async fn webhook(&self, url: &str, secret: &str, event_type: &str) -> i64 {
        self.store()
            .create(&NewOutboundWebhook {
                url: url.into(),
                secret: secret.into(),
                event_types: subscribed(event_type),
                created_by: None,
            })
            .await
            .unwrap()
            .id
    }

    fn sender(&self) -> Sender {
        let table = Table(HashMap::from([
            ("hooks.test".to_owned(), vec!["127.0.0.1".parse().unwrap()]),
            (
                "internal.test".to_owned(),
                vec!["127.0.0.1".parse().unwrap()],
            ),
        ]));
        Sender::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            self.config.clone(),
        )
        .with_lookup(Arc::new(table))
        .with_timeout(Duration::from_secs(5))
    }

    /// Runs the sender worker (fast backoff, `max_failures` attempts) until job `id` is
    /// finished, and returns it.
    async fn run_until_done(&self, id: i64, max_failures: i32) -> OutboundWebhookJobRow {
        let mut config = OutboundWebhookStore::queue_config();
        config.max_failures = max_failures;
        config.backoff_base = Duration::from_millis(50);
        config.backoff_max = Duration::from_millis(200);
        let queue = Queue::new(self.db.pool().clone(), config).unwrap();
        let mut opts = WorkerOptions::new("test-worker");
        opts.poll_interval = Duration::from_millis(50);
        let worker = Worker::new(queue, SendHandler::new(self.sender()), opts);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(worker.run(cancel.clone()));
        let store = self.store();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let job = loop {
            let job = store.job(id).await.unwrap().unwrap();
            if matches!(
                job.state,
                JobState::Completed | JobState::Failed | JobState::Canceled
            ) {
                break job;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "job {id} not finished: {:?} {:?}",
                job.state,
                job.failure_message
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        cancel.cancel();
        task.await.unwrap();
        job
    }

    async fn enqueue(&self, event_type: &str) -> i64 {
        enqueue_event(
            &self.db,
            Some(self.keyring.clone()),
            event_type,
            None,
            &json!({"id": "Q2hhbmdlc2V0OjE=", "state": "OPEN"}),
        )
        .await
        .unwrap()
        .expect("a webhook subscribes")
    }
}

#[tokio::test]
async fn delivers_signed_events_and_retries_failed_webhooks() {
    let Some(env) = setup(&["127.0.0.1"]).await else {
        return;
    };
    let server = MockServer::start().await;
    // `/flaky` fails once, then accepts.
    Mock::given(method("POST"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(500).set_body_string("try again"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/steady"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/other"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let flaky = env
        .webhook(&format!("{}/flaky", server.uri()), "s1", CHANGESET_PUBLISH)
        .await;
    let steady = env
        .webhook(&format!("{}/steady", server.uri()), "s2", CHANGESET_PUBLISH)
        .await;
    let other = env
        .webhook(&format!("{}/other", server.uri()), "s3", CHANGESET_CLOSE)
        .await;

    let id = env.enqueue(CHANGESET_PUBLISH).await;
    let job = env.run_until_done(id, 5).await;
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(job.num_failures, 1);

    let requests = server.received_requests().await.unwrap();
    let to = |p: &str| requests.iter().filter(|r| r.url.path() == p).count();
    // The retry only went to the webhook that failed.
    assert_eq!((to("/flaky"), to("/steady"), to("/other")), (2, 1, 0));
    for r in &requests {
        let secret = if r.url.path() == "/flaky" { "s1" } else { "s2" };
        let header = |name: &str| r.headers.get(name).unwrap().to_str().unwrap().to_owned();
        assert_eq!(header(SIGNATURE_HEADER), sign(secret, &r.body));
        assert_eq!(header(EVENT_TYPE_HEADER), CHANGESET_PUBLISH);
        assert_eq!(header(DELIVERY_HEADER), id.to_string());
        assert_eq!(header("content-type"), "application/json");
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["state"], "OPEN");
    }

    let store = env.store();
    let logs = store.logs(flaky, false, 0, 0).await.unwrap();
    assert_eq!(logs.len(), 2);
    // Newest first.
    assert_eq!((logs[0].status_code, logs[1].status_code), (200, 500));
    assert_eq!(logs[1].error.as_deref(), Some("unexpected status 500"));
    let resp = logs[1].response.as_ref().unwrap();
    assert_eq!(resp.body, "try again");
    assert!(logs[0].error.is_none());
    assert_eq!(logs[0].request.method, "POST");
    assert_eq!(logs[0].request.url, format!("{}/flaky", server.uri()));
    assert_eq!(
        logs[0].request.headers["x-sourcegull-webhook-event-type"],
        vec![CHANGESET_PUBLISH.to_owned()]
    );
    assert!(logs[0].request.body.contains("\"OPEN\""));
    let stats = store.log_stats(flaky).await.unwrap();
    assert_eq!((stats.total, stats.errored), (2, 1));
    assert_eq!(store.logs(flaky, true, 0, 0).await.unwrap().len(), 1);
    assert_eq!(
        store.logs(steady, false, 0, 0).await.unwrap()[0].status_code,
        204
    );
    assert!(store.logs(other, false, 0, 0).await.unwrap().is_empty());
    assert_eq!(store.job_logs(id).await.unwrap().len(), 3);

    // Stored encrypted: no URL or secret in the clear.
    let (url_enc, secret_enc): (Vec<u8>, Vec<u8>) = sourcegull_db::sqlx::query_as(
        "SELECT url_enc, secret_enc FROM outbound_webhooks WHERE id = $1",
    )
    .bind(flaky)
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    let url_bytes = format!("{}/flaky", server.uri()).into_bytes();
    assert!(!url_enc.windows(url_bytes.len()).any(|w| w == url_bytes));
    assert!(!secret_enc.windows(2).any(|w| w == b"s1"));
}

#[tokio::test]
async fn gives_up_after_the_last_attempt() {
    let Some(env) = setup(&["127.0.0.0/8"]).await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let hook = env
        .webhook(&format!("{}/down", server.uri()), "s", CHANGESET_PUBLISH)
        .await;
    let id = env.enqueue(CHANGESET_PUBLISH).await;
    let job = env.run_until_done(id, 3).await;
    assert_eq!(job.state, JobState::Failed);
    assert!(
        job.failure_message
            .as_deref()
            .unwrap()
            .contains(&format!("webhook {hook}")),
        "{:?}",
        job.failure_message
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    let stats = env.store().log_stats(hook).await.unwrap();
    assert_eq!((stats.total, stats.errored), (3, 3));
}

#[tokio::test]
async fn ssrf_guard_refuses_internal_addresses() {
    let Some(env) = setup(&[]).await else { return };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let port = server.address().port();
    // An IP literal and a name resolving to loopback.
    let literal = env
        .webhook(
            &format!("http://127.0.0.1:{port}/a"),
            "s",
            CHANGESET_PUBLISH,
        )
        .await;
    let named = env
        .webhook(
            &format!("http://internal.test:{port}/b"),
            "s",
            CHANGESET_PUBLISH,
        )
        .await;
    let metadata = env
        .webhook(
            "http://169.254.169.254/latest/meta-data/",
            "s",
            CHANGESET_PUBLISH,
        )
        .await;
    let id = env.enqueue(CHANGESET_PUBLISH).await;
    let job = env.run_until_done(id, 1).await;
    assert_eq!(job.state, JobState::Failed);
    assert!(server.received_requests().await.unwrap().is_empty());
    for hook in [literal, named, metadata] {
        let logs = env.store().logs(hook, false, 0, 0).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status_code, 0);
        let error = logs[0].error.as_deref().unwrap();
        assert!(error.contains("not a public address"), "{error}");
        assert!(logs[0].response.is_none());
    }
}

#[tokio::test]
async fn allowlisted_hosts_and_checked_redirects() {
    // `hooks.test` and `internal.test` resolve to 127.0.0.1; only `hooks.test` is allowed.
    let Some(env) = setup(&["hooks.test"]).await else {
        return;
    };
    let server = MockServer::start().await;
    let port = server.address().port();
    Mock::given(method("POST"))
        .and(path("/direct"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    // Redirects to the allowed name are followed, to the IP literal or the other name not.
    Mock::given(method("POST"))
        .and(path("/to-allowed"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("http://hooks.test:{port}/final").as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/to-literal"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("http://127.0.0.1:{port}/final").as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/to-internal"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("http://internal.test:{port}/final").as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/final"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let base = format!("http://hooks.test:{port}");
    let direct = env
        .webhook(&format!("{base}/direct"), "s", CHANGESET_PUBLISH)
        .await;
    let to_allowed = env
        .webhook(&format!("{base}/to-allowed"), "s", CHANGESET_PUBLISH)
        .await;
    let to_literal = env
        .webhook(&format!("{base}/to-literal"), "s", CHANGESET_PUBLISH)
        .await;
    let to_internal = env
        .webhook(&format!("{base}/to-internal"), "s", CHANGESET_PUBLISH)
        .await;
    let id = env.enqueue(CHANGESET_PUBLISH).await;
    env.run_until_done(id, 1).await;

    let status = |hook: i64| {
        let store = env.store();
        async move { store.logs(hook, false, 0, 0).await.unwrap()[0].clone() }
    };
    assert_eq!(status(direct).await.status_code, 200);
    assert_eq!(status(to_allowed).await.status_code, 200);
    for hook in [to_literal, to_internal] {
        let log = status(hook).await;
        assert_eq!(log.status_code, 0);
        assert!(
            log.error
                .as_deref()
                .unwrap()
                .contains("not a public address"),
            "{:?}",
            log.error
        );
    }
    let finals = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/final")
        .count();
    assert_eq!(finals, 1);
}

#[tokio::test]
async fn events_are_only_queued_for_subscribers_and_old_logs_expire() {
    let Some(env) = setup(&["127.0.0.1"]).await else {
        return;
    };
    // Nobody subscribes: nothing is queued.
    let none = enqueue_event(
        &env.db,
        Some(env.keyring.clone()),
        CHANGESET_PUBLISH,
        None,
        &json!({}),
    )
    .await
    .unwrap();
    assert_eq!(none, None);

    // Scoped subscriptions match their scope only; unscoped ones match every scope.
    let store = env.store();
    store
        .create(&NewOutboundWebhook {
            url: "https://hooks.example.com/".into(),
            secret: "s".into(),
            event_types: vec![ScopedEventType {
                event_type: CHANGESET_CLOSE.into(),
                scope: Some("repo:1".into()),
            }],
            created_by: None,
        })
        .await
        .unwrap();
    assert!(
        store
            .has_subscribers(CHANGESET_CLOSE, Some("repo:1"))
            .await
            .unwrap()
    );
    assert!(
        !store
            .has_subscribers(CHANGESET_CLOSE, Some("repo:2"))
            .await
            .unwrap()
    );
    assert!(!store.has_subscribers(CHANGESET_CLOSE, None).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let hook = env.webhook(&server.uri(), "s", CHANGESET_PUBLISH).await;
    let id = env.enqueue(CHANGESET_PUBLISH).await;
    env.run_until_done(id, 1).await;
    assert_eq!(store.log_stats(hook).await.unwrap().total, 1);

    // The janitor keeps recent logs and deletes old ones with their finished jobs.
    let (logs, jobs) = sourcegull_outbound_webhooks::janitor::clean(&env.db, &env.config)
        .await
        .unwrap();
    assert_eq!((logs, jobs), (0, 0));
    for sql in [
        "UPDATE outbound_webhook_logs SET sent_at = now() - interval '8 days'",
        "UPDATE outbound_webhook_jobs SET finished_at = now() - interval '8 days'",
    ] {
        sourcegull_db::sqlx::query(sql)
            .execute(env.db.pool())
            .await
            .unwrap();
    }
    let (logs, jobs) = sourcegull_outbound_webhooks::janitor::clean(&env.db, &env.config)
        .await
        .unwrap();
    assert_eq!((logs, jobs), (1, 1));
    assert!(store.job(id).await.unwrap().is_none());
    assert_eq!(store.log_stats(hook).await.unwrap().total, 0);
}
