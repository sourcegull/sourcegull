//! `/.api/gitlab-webhooks` and `/.api/webhooks/{uuid}` against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{Db, NewExternalService, migrate};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_types::ExternalServiceKind;
use tower::ServiceExt;

const PUSH: &str = r#"{"object_kind": "push", "before": "a", "after": "b", "ref": "refs/heads/main", "project_id": 1}"#;

async fn post(
    router: &Router,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::post(uri)
        .header("content-type", "application/json")
        .header("x-gitlab-event", "Push Hook");
    if let Some(t) = token {
        req = req.header("x-gitlab-token", t);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_routes_verify_and_log() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [1u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let svc = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: r#"{"url": "https://gitlab.example.com", "token": "t",
                        "webhooks": [{"secret": "s3"}]}"#
                .into(),
        })
        .await
        .unwrap();

    let config = SiteConfigHandle::default();
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_keyring(Some(keyring));
    let router = public_router(state);

    let uri = format!("/.api/webhooks/{}", svc.webhook_uuid);
    let (status, body) = post(&router, &uri, Some("s3"), PUSH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    let (status, body) = post(&router, &uri, Some("wrong"), PUSH).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "UNAUTHORIZED");
    let (status, _) = post(&router, "/.api/webhooks/nope", Some("s3"), PUSH).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post(&router, "/.api/gitlab-webhooks", Some("s3"), PUSH).await;
    assert_eq!(status, StatusCode::OK);
    let narrowed = format!("/.api/gitlab-webhooks?externalServiceID={}", svc.id);
    assert_eq!(
        post(&router, &narrowed, Some("s3"), "{").await.0,
        StatusCode::BAD_REQUEST
    );
    let relay = sourcegull_services::relay::marshal_id("ExternalService", svc.id.get());
    let by_node_id = format!("/.api/gitlab-webhooks?externalServiceID={relay}");
    assert_eq!(
        post(&router, &by_node_id, Some("s3"), PUSH).await.0,
        StatusCode::OK
    );
    assert_eq!(
        post(
            &router,
            "/.api/gitlab-webhooks?externalServiceID=x",
            Some("s3"),
            PUSH
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(&router, "/.api/gitlab-webhooks", None, PUSH).await.0,
        StatusCode::UNAUTHORIZED
    );

    let logs = db.webhook_logs().list(None, 100).await.unwrap();
    assert_eq!(logs.len(), 7, "every routed delivery is logged");
}
