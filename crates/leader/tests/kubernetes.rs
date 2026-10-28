//! The Kubernetes `Lease` backend against a fake API server (always), and against a real
//! cluster when `SOURCEGULL_TEST_KUBERNETES_LEASES=1` (the cluster comes from `KUBECONFIG`, e.g.
//! k3s from `scripts/dev k3s up` with `KUBECONFIG=/src/.k3s/kubeconfig`).
//!
//! The fake implements just what the backend uses — get, create and replace of
//! `coordination.k8s.io/v1` leases — with the API server's optimistic concurrency: a replace
//! whose `metadata.resourceVersion` is not the stored one fails with 409 Conflict.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use sourcegull_leader::kubernetes::KubernetesLeases;
use sourcegull_leader::{LeaseBackend, run_singleton};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fake {
    leases: HashMap<(String, String), Value>,
    version: u64,
    writes: usize,
}

type Shared = Arc<Mutex<Fake>>;

fn status(code: StatusCode, reason: &str) -> Response {
    let body = json!({
        "kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
        "message": reason, "reason": reason, "code": code.as_u16(),
    });
    (code, Json(body)).into_response()
}

async fn get_lease(State(s): State<Shared>, Path((ns, name)): Path<(String, String)>) -> Response {
    match s.lock().unwrap().leases.get(&(ns, name)) {
        Some(l) => Json(l.clone()).into_response(),
        None => status(StatusCode::NOT_FOUND, "NotFound"),
    }
}

async fn create_lease(
    State(s): State<Shared>,
    Path(ns): Path<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let mut f = s.lock().unwrap();
    let name = body["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if f.leases.contains_key(&(ns.clone(), name.clone())) {
        return status(StatusCode::CONFLICT, "AlreadyExists");
    }
    f.version += 1;
    f.writes += 1;
    body["metadata"]["resourceVersion"] = json!(f.version.to_string());
    body["metadata"]["namespace"] = json!(ns);
    f.leases.insert((ns, name), body.clone());
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn replace_lease(
    State(s): State<Shared>,
    Path((ns, name)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    let mut f = s.lock().unwrap();
    let key = (ns, name);
    let Some(current) = f.leases.get(&key) else {
        return status(StatusCode::NOT_FOUND, "NotFound");
    };
    if body["metadata"]["resourceVersion"] != current["metadata"]["resourceVersion"] {
        return status(StatusCode::CONFLICT, "Conflict");
    }
    f.version += 1;
    f.writes += 1;
    body["metadata"]["resourceVersion"] = json!(f.version.to_string());
    f.leases.insert(key, body.clone());
    Json(body).into_response()
}

struct FakeServer {
    state: Shared,
    client: kube::Client,
}

async fn fake() -> FakeServer {
    let state: Shared = Arc::default();
    let app = axum::Router::new()
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{ns}/leases",
            post(create_lease),
        )
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{ns}/leases/{name}",
            get(get_lease).put(replace_lease),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let config = kube::Config::new(format!("http://127.0.0.1:{port}").parse().unwrap());
    FakeServer {
        state,
        client: kube::Client::try_from(config).unwrap(),
    }
}

fn leases(client: &kube::Client, ns: &str, prefix: &str, identity: &str) -> KubernetesLeases {
    KubernetesLeases::new(client.clone(), ns, prefix, identity)
        .with_duration(Duration::from_secs(1))
}

/// The protocol, against any API server.
async fn protocol(client: kube::Client, ns: &str, prefix: &str) {
    let a = leases(&client, ns, prefix, "pod-a").lease("janitor");
    let b = leases(&client, ns, prefix, "pod-b").lease("janitor");
    let c = leases(&client, ns, prefix, "pod-c").lease("janitor");
    let other = leases(&client, ns, prefix, "pod-b").lease("scheduler");
    assert_eq!(a.object_name(), format!("{prefix}-janitor"));

    // Create, then exclusive while renewed.
    assert!(a.try_acquire().await.unwrap());
    assert!(
        a.try_acquire().await.unwrap(),
        "re-acquire while held is a renew"
    );
    assert!(!b.try_acquire().await.unwrap());
    assert!(!b.renew().await.unwrap());
    assert!(other.try_acquire().await.unwrap(), "leases are independent");
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(a.renew().await.unwrap());
        assert!(
            !b.try_acquire().await.unwrap(),
            "a renewed lease never expires"
        );
    }

    // `a` stops renewing (a crashed pod): `b` takes over once the record has been unchanged
    // for the lease duration, and `a` learns it lost the lease on its next renewal.
    let start = tokio::time::Instant::now();
    while !b.try_acquire().await.unwrap() {
        assert!(start.elapsed() < Duration::from_secs(5), "never expired");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert!(!a.renew().await.unwrap());
    assert!(!a.try_acquire().await.unwrap());
    assert!(b.renew().await.unwrap());

    // A release hands over at once.
    b.release().await;
    assert!(c.try_acquire().await.unwrap());
    assert!(!b.renew().await.unwrap());
    c.release().await;
    other.release().await;

    // Racing candidates for a free lease: exactly one wins.
    let racers: Vec<_> = (0..6)
        .map(|i| Arc::new(leases(&client, ns, prefix, &format!("racer-{i}")).lease("race")))
        .collect();
    let mut set = tokio::task::JoinSet::new();
    for r in &racers {
        let r = r.clone();
        set.spawn(async move { r.try_acquire().await.unwrap() });
    }
    let mut winners = 0;
    while let Some(won) = set.join_next().await {
        winners += usize::from(won.unwrap());
    }
    assert_eq!(winners, 1);
    for r in &racers {
        r.release().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_protocol_against_fake_api_server() {
    let f = fake().await;
    protocol(f.client.clone(), "sourcegull", "sourcegull").await;
    let fake = f.state.lock().unwrap();
    let lease = &fake.leases[&("sourcegull".to_owned(), "sourcegull-janitor".to_owned())];
    assert_eq!(lease["spec"]["holderIdentity"], Value::Null, "{lease}");
    assert_eq!(lease["spec"]["leaseDurationSeconds"], 1);
    assert_eq!(lease["spec"]["leaseTransitions"], 2, "a → b → c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn singleton_fails_over_between_pods() {
    let f = fake().await;
    let running = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let mut pods = Vec::new();
    for pod in ["pod-a", "pod-b"] {
        let cancel = CancellationToken::new();
        let backend = leases(&f.client, "sourcegull", "sourcegull", pod)
            .with_duration(Duration::from_secs(2))
            .lease("ranker");
        let (running, overlap, starts) = (running.clone(), overlap.clone(), starts.clone());
        let handle = tokio::spawn(run_singleton(
            "ranker",
            backend,
            None,
            Duration::from_millis(200),
            move |token, _fence| {
                let (running, overlap) = (running.clone(), overlap.clone());
                starts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if running.fetch_add(1, Ordering::SeqCst) > 0 {
                        overlap.store(true, Ordering::SeqCst);
                    }
                    token.cancelled().await;
                    running.fetch_sub(1, Ordering::SeqCst);
                }
            },
            cancel.clone(),
        ));
        pods.push((cancel, handle));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(running.load(Ordering::SeqCst), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    // The leader shuts down gracefully (releases): the other pod takes over within a renewal
    // interval or two, not a lease duration.
    let leader = f.state.lock().unwrap().leases
        [&("sourcegull".to_owned(), "sourcegull-ranker".to_owned())]["spec"]["holderIdentity"]
        .as_str()
        .unwrap()
        .to_owned();
    let idx = usize::from(leader == "pod-b");
    let (cancel, handle) = pods.remove(idx);
    cancel.cancel();
    handle.await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(running.load(Ordering::SeqCst), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(!overlap.load(Ordering::SeqCst));
    let (cancel, handle) = pods.remove(0);
    cancel.cancel();
    handle.await.unwrap();
    // Renewals are one write each; nothing polls in a tight loop.
    let writes = f.state.lock().unwrap().writes;
    assert!(writes < 40, "{writes} writes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_protocol_against_a_cluster() {
    if std::env::var("SOURCEGULL_TEST_KUBERNETES_LEASES").as_deref() != Ok("1") {
        eprintln!("skipping: SOURCEGULL_TEST_KUBERNETES_LEASES is not 1");
        return;
    }
    let client = kube::Client::try_default().await.expect("a cluster");
    let prefix = format!(
        "sourcegull-test-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    protocol(client.clone(), "default", &prefix).await;
    // Clean up.
    let api: kube::Api<k8s_openapi::api::coordination::v1::Lease> =
        kube::Api::namespaced(client, "default");
    for name in ["janitor", "scheduler", "race"] {
        let _ = api
            .delete(&format!("{prefix}-{name}"), &Default::default())
            .await;
    }
    eprintln!("lease protocol passed against the cluster");
}
