//! `/.oauth/authorize` is limited per client address (`auth.rateLimits`).

use std::sync::Arc;

use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, RequestLimit, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn authorize_is_limited_per_address() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let mut site = SiteConfig {
        external_url: Some("https://sourcegull.example.com".into()),
        ..SiteConfig::default()
    };
    site.auth_rate_limits.oauth_authorize_per_address = RequestLimit::new(3, 60);
    let config = ConfigHandle::new(1, site);
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db,
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    );
    let routes = sourcegull_http_api::public_routes().merge(sourcegull_oauth_server::routes());
    let router = sourcegull_http_api::finish(routes, state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/.oauth/authorize?response_type=code&client_id=nope",
        listener.local_addr().unwrap()
    );
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    tokio::spawn(sourcegull_http_api::serve(listener, router, cancel));
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for _ in 0..3 {
        let status = http.get(&url).send().await.unwrap().status();
        assert_ne!(status, 429);
    }
    let resp = http.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 429);
    let retry_after: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
}
