//! OAuth sign-in for MCP clients end to end, against the test Postgres and through the HTTP
//! server, as a client script would do it: discovery from the MCP endpoint's 401, dynamic
//! registration, authorization with PKCE and the user's consent, the token exchange, MCP calls,
//! refresh with rotation and reuse detection, code replay, revocation, the endpoints turned off,
//! metadata documents fetched only for signed-in users, the consent page's redirect warning and
//! the janitor.

use std::sync::Arc;

use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;

const EXTERNAL: &str = "https://sourcegull.example.com";
const REDIRECT: &str = "http://127.0.0.1:3000/callback";

struct Env {
    _t: TestDb,
    base: String,
    http: reqwest::Client,
    config: ConfigHandle<SiteConfig>,
    auth: Authenticator,
    /// Session cookie of alice.
    cookie: String,
    /// A personal access token of alice.
    pat: String,
    alice: sourcegull_types::UserId,
    db: Db,
    _cancel: tokio_util::sync::DropGuard,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some(EXTERNAL.into()),
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let cookie = format!(
        "{}={}",
        sourcegull_http_extract::SESSION_COOKIE,
        auth.start_session(alice).await.unwrap().into_string()
    );
    let actor = auth
        .actor_for(alice, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    let (_, pat) = auth
        .create_access_token(&actor, alice, &["user:all".into()], "t", None)
        .await
        .unwrap();

    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth.clone(),
    );
    let routes = sourcegull_http_api::public_routes()
        .merge(sourcegull_mcp::routes())
        .merge(sourcegull_oauth_server::routes());
    let router = sourcegull_http_api::finish(routes, state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    tokio::spawn(sourcegull_http_api::serve(listener, router, cancel.clone()));
    Some(Env {
        _t: t,
        base,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        config,
        auth,
        cookie,
        pat,
        alice,
        db,
        _cancel: cancel.drop_guard(),
    })
}

/// The value of query parameter `name` in `url`.
fn param(url: &str, name: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://x{url}"))
        .or_else(|_| url::Url::parse(url))
        .unwrap();
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

impl Env {
    async fn get_json(&self, path: &str) -> (u16, Value) {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn form(&self, path: &str, form: &[(&str, &str)]) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .form(form)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn mcp(&self, authorization: &str) -> reqwest::Response {
        self.http
            .post(format!("{}{}", self.base, sourcegull_mcp::PATH))
            .header("authorization", authorization)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .send()
            .await
            .unwrap()
    }

    async fn register(&self) -> String {
        let resp = self
            .http
            .post(format!("{}/.oauth/register", self.base))
            .json(&json!({ "client_name": "Test agent", "redirect_uris": [REDIRECT] }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["token_endpoint_auth_method"], "none");
        v["client_id"].as_str().unwrap().to_owned()
    }

    /// Runs the browser part of the flow as alice; returns the code.
    async fn authorize(&self, client_id: &str, verifier: &str) -> String {
        let challenge = sourcegull_crypto::pkce::challenge_s256(verifier);
        let resp = self
            .http
            .get(format!("{}/.oauth/authorize", self.base))
            .query(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", "http://127.0.0.1:41234/callback"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("state", "xyz"),
                ("scope", "mcp"),
                ("resource", &format!("{EXTERNAL}/.api/mcp")),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 302);
        let location = resp.headers()["location"].to_str().unwrap().to_owned();
        assert!(location.starts_with("/oauth/consent?"), "{location}");
        let request = param(&location, "request").expect("request handle");
        let api = format!("{}/.api/oauth/authorizations/{request}", self.base);

        assert_eq!(self.http.get(&api).send().await.unwrap().status(), 401);
        let shown: Value = self
            .http
            .get(&api)
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(shown["clientName"], "Test agent");
        assert_eq!(shown["redirectHost"], "127.0.0.1");
        assert_eq!(shown["redirectWarning"], false);
        assert_eq!(shown["username"], "alice");

        // Cookie-authenticated POSTs need the CSRF header.
        let csrf = self
            .http
            .post(&api)
            .header("cookie", &self.cookie)
            .json(&json!({ "approve": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(csrf.status(), 403);
        // A token cannot approve on the user's behalf.
        let token = self
            .http
            .post(&api)
            .header("authorization", format!("token {}", self.pat))
            .json(&json!({ "approve": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(token.status(), 403);

        let answered: Value = self
            .http
            .post(&api)
            .header("cookie", &self.cookie)
            .header("x-requested-with", "Sourcegull")
            .json(&json!({ "approve": true }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let to = answered["redirectURL"].as_str().unwrap();
        assert!(to.starts_with("http://127.0.0.1:41234/callback?"), "{to}");
        assert_eq!(param(to, "state").as_deref(), Some("xyz"));
        assert_eq!(param(to, "iss").as_deref(), Some(EXTERNAL));
        // The request is answered once.
        let again = self
            .http
            .get(&api)
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 404);
        param(to, "code").expect("code")
    }

    async fn exchange(&self, client_id: &str, code: &str, verifier: &str) -> (u16, Value) {
        self.form(
            "/.oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", client_id),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", "http://127.0.0.1:41234/callback"),
                ("resource", &format!("{EXTERNAL}/.api/mcp")),
            ],
        )
        .await
    }

    async fn refresh(&self, client_id: &str, refresh: &str) -> (u16, Value) {
        self.form(
            "/.oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh),
            ],
        )
        .await
    }
}

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

#[tokio::test]
async fn full_flow() {
    let Some(env) = setup().await else { return };

    // Discovery from the MCP endpoint's challenge.
    let resp = env
        .http
        .post(format!("{}/.api/mcp", env.base))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let challenge = resp.headers()["www-authenticate"].to_str().unwrap();
    assert!(
        challenge.contains(&format!(
            "resource_metadata=\"{EXTERNAL}/.well-known/oauth-protected-resource/.api/mcp\""
        )),
        "{challenge}"
    );
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/.api/mcp",
    ] {
        let (status, prm) = env.get_json(path).await;
        assert_eq!(status, 200);
        assert_eq!(prm["resource"], format!("{EXTERNAL}/.api/mcp"));
        assert_eq!(prm["authorization_servers"], json!([EXTERNAL]));
    }
    let (status, asm) = env
        .get_json("/.well-known/oauth-authorization-server")
        .await;
    assert_eq!(status, 200);
    assert_eq!(asm["issuer"], EXTERNAL);
    assert_eq!(asm["token_endpoint"], format!("{EXTERNAL}/.oauth/token"));
    assert_eq!(
        asm["registration_endpoint"],
        format!("{EXTERNAL}/.oauth/register")
    );
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));

    // Registration, authorization and the exchange.
    let client_id = env.register().await;
    let code = env.authorize(&client_id, VERIFIER).await;
    let (status, err) = env.exchange("sourcegull-other", &code, VERIFIER).await;
    assert_eq!(status, 401, "{err}");
    let (status, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    assert_eq!(status, 200, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "mcp");
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("sko_") && refresh.starts_with("skr_"));

    // The token works at the MCP endpoint, and only there.
    let ok = env.mcp(&format!("Bearer {access}")).await;
    assert_eq!(ok.status(), 200);
    let v: Value = ok.json().await.unwrap();
    assert!(
        v["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
    );
    assert_eq!(env.mcp(&format!("token {access}")).await.status(), 401);
    let other = env
        .http
        .get(format!("{}/.api/search/stream?q=x", env.base))
        .header("authorization", format!("token {access}"))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 401);
    // Personal access tokens keep working.
    assert_eq!(env.mcp(&format!("Bearer {}", env.pat)).await.status(), 200);

    // Connected apps.
    let alice = env
        .auth
        .actor_for(env.alice, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    let grants = env
        .auth
        .list_oauth_grants(&alice, Some(alice.user_id))
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].client_name, "Test agent");
    assert!(grants[0].last_used_at.is_some());

    // Refresh rotates; presenting the old refresh token again revokes the grant.
    let (status, rotated) = env.refresh(&client_id, &refresh).await;
    assert_eq!(status, 200, "{rotated}");
    let access2 = rotated["access_token"].as_str().unwrap().to_owned();
    assert_ne!(rotated["refresh_token"].as_str().unwrap(), refresh);
    assert_eq!(env.mcp(&format!("Bearer {access2}")).await.status(), 200);
    let (status, err) = env.refresh(&client_id, &refresh).await;
    assert_eq!(status, 400);
    assert_eq!(err["error"], "invalid_grant");
    let revoked = env.mcp(&format!("Bearer {access2}")).await;
    assert_eq!(revoked.status(), 401);
    assert!(
        revoked.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("error=\"invalid_token\"")
    );
    let (status, _) = env
        .refresh(&client_id, rotated["refresh_token"].as_str().unwrap())
        .await;
    assert_eq!(status, 400);
    assert!(
        env.auth
            .list_oauth_grants(&alice, Some(alice.user_id))
            .await
            .unwrap()
            .is_empty()
    );

    // Revocation by the client.
    let code = env.authorize(&client_id, VERIFIER).await;
    let (_, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(env.mcp(&format!("Bearer {access}")).await.status(), 200);
    let (status, _) = env
        .form(
            "/.oauth/revoke",
            &[
                ("client_id", &client_id),
                ("token", tokens["refresh_token"].as_str().unwrap()),
            ],
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(env.mcp(&format!("Bearer {access}")).await.status(), 401);

    // Revocation in Connected apps.
    let code = env.authorize(&client_id, VERIFIER).await;
    let (_, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    let access = tokens["access_token"].as_str().unwrap();
    let grants = env
        .auth
        .list_oauth_grants(&alice, Some(alice.user_id))
        .await
        .unwrap();
    env.auth
        .revoke_oauth_grant(&alice, grants[0].id)
        .await
        .unwrap();
    assert_eq!(env.mcp(&format!("Bearer {access}")).await.status(), 401);

    // Turned off: no endpoints, no discovery hint, OAuth tokens rejected.
    let code = env.authorize(&client_id, VERIFIER).await;
    let (_, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    let access = tokens["access_token"].as_str().unwrap();
    env.config.store(
        2,
        SiteConfig {
            external_url: Some(EXTERNAL.into()),
            mcp_oauth_enabled: false,
            ..SiteConfig::default()
        },
    );
    let (status, _) = env
        .get_json("/.well-known/oauth-authorization-server")
        .await;
    assert_eq!(status, 404);
    let resp = env.mcp(&format!("Bearer {access}")).await;
    assert_eq!(resp.status(), 401);
    assert!(
        !resp.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("resource_metadata")
    );
}

#[tokio::test]
async fn authorize_errors() {
    let Some(env) = setup().await else { return };
    let client_id = env.register().await;
    let get = |query: Vec<(&'static str, String)>| {
        let http = env.http.clone();
        let url = format!("{}/.oauth/authorize", env.base);
        async move {
            let resp = http.get(url).query(&query).send().await.unwrap();
            assert_eq!(resp.status(), 302);
            resp.headers()["location"].to_str().unwrap().to_owned()
        }
    };
    let challenge = sourcegull_crypto::pkce::challenge_s256(VERIFIER);
    // Unknown clients and unregistered redirect URIs are shown on the consent page.
    let to = get(vec![
        ("client_id", "sourcegull-nope".into()),
        ("response_type", "code".into()),
    ])
    .await;
    assert!(to.starts_with("/oauth/consent?error="), "{to}");
    let to = get(vec![
        ("client_id", client_id.clone()),
        ("redirect_uri", "https://evil.example.com/cb".into()),
    ])
    .await;
    assert!(to.starts_with("/oauth/consent?error="), "{to}");
    // Other errors go back to the client.
    let to = get(vec![
        ("client_id", client_id.clone()),
        ("response_type", "code".into()),
        ("code_challenge", challenge.clone()),
        ("code_challenge_method", "plain".into()),
        ("state", "s".into()),
    ])
    .await;
    assert!(to.starts_with(REDIRECT), "{to}");
    assert_eq!(param(&to, "error").as_deref(), Some("invalid_request"));
    assert_eq!(param(&to, "state").as_deref(), Some("s"));
    let to = get(vec![
        ("client_id", client_id.clone()),
        ("response_type", "code".into()),
        ("code_challenge", challenge.clone()),
        ("code_challenge_method", "S256".into()),
        ("resource", "https://other.example.com/mcp".into()),
    ])
    .await;
    assert_eq!(param(&to, "error").as_deref(), Some("invalid_target"));
    let to = get(vec![
        ("client_id", client_id.clone()),
        ("response_type", "code".into()),
        ("code_challenge", challenge.clone()),
        ("code_challenge_method", "S256".into()),
        ("scope", "admin".into()),
    ])
    .await;
    assert_eq!(param(&to, "error").as_deref(), Some("invalid_scope"));

    // A wrong verifier fails the exchange.
    let code = env.authorize(&client_id, VERIFIER).await;
    let (status, err) = env
        .exchange(
            &client_id,
            &code,
            "wrong-verifier-wrong-verifier-wrong-verifier-x",
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(err["error"], "invalid_grant");

    // Registration rejects redirect URIs that are not loopback or https.
    let resp = env
        .http
        .post(format!("{}/.oauth/register", env.base))
        .json(&json!({ "redirect_uris": ["http://example.com/cb"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // And is off when dynamic registration is.
    env.config.store(
        2,
        SiteConfig {
            external_url: Some(EXTERNAL.into()),
            mcp_oauth_dynamic_client_registration: false,
            ..SiteConfig::default()
        },
    );
    let resp = env
        .http
        .post(format!("{}/.oauth/register", env.base))
        .json(&json!({ "redirect_uris": [REDIRECT] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let (_, asm) = env
        .get_json("/.well-known/oauth-authorization-server")
        .await;
    assert!(asm.get("registration_endpoint").is_none());
}

#[tokio::test]
async fn code_replay_revokes_tokens() {
    let Some(env) = setup().await else { return };
    let client_id = env.register().await;
    let code = env.authorize(&client_id, VERIFIER).await;
    let (status, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    assert_eq!(status, 200, "{tokens}");
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(env.mcp(&format!("Bearer {access}")).await.status(), 200);

    // A code works once; presenting it again revokes the tokens issued for it.
    let (status, err) = env.exchange(&client_id, &code, VERIFIER).await;
    assert_eq!(status, 400);
    assert_eq!(err["error"], "invalid_grant");
    assert_eq!(env.mcp(&format!("Bearer {access}")).await.status(), 401);
    let (status, _) = env.refresh(&client_id, refresh).await;
    assert_eq!(status, 400);

    // Other grants stay.
    let code = env.authorize(&client_id, VERIFIER).await;
    let (_, tokens) = env.exchange(&client_id, &code, VERIFIER).await;
    let other = tokens["access_token"].as_str().unwrap();
    let (status, _) = env.exchange(&client_id, "not-a-code", VERIFIER).await;
    assert_eq!(status, 400);
    assert_eq!(env.mcp(&format!("Bearer {other}")).await.status(), 200);
}

#[tokio::test]
async fn redirect_host_warning() {
    let Some(env) = setup().await else { return };
    let resp = env
        .http
        .post(format!("{}/.oauth/register", env.base))
        .json(&json!({ "client_name": "Official agent", "redirect_uris": ["https://agent.example.com/cb"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let client_id = resp.json::<Value>().await.unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resp = env
        .http
        .get(format!("{}/.oauth/authorize", env.base))
        .query(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            (
                "code_challenge",
                &sourcegull_crypto::pkce::challenge_s256(VERIFIER),
            ),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    let location = resp.headers()["location"].to_str().unwrap().to_owned();
    let request = param(&location, "request").expect("request handle");
    let shown: Value = env
        .http
        .get(format!("{}/.api/oauth/authorizations/{request}", env.base))
        .header("cookie", &env.cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["redirectHost"], "agent.example.com");
    assert_eq!(shown["redirectWarning"], true);
}

/// A TCP listener counting the connections made to it.
async fn counting_listener() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });
    (port, count)
}

#[tokio::test]
async fn metadata_documents_need_sign_in() {
    let Some(env) = setup().await else { return };
    let (port, connections) = counting_listener().await;
    let client_id = format!("https://127.0.0.1:{port}/client.json");
    let query = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("code_challenge", "x"),
        ("code_challenge_method", "S256"),
    ];
    let authorize = |cookie: Option<String>| {
        let mut req = env
            .http
            .get(format!("{}/.oauth/authorize", env.base))
            .query(&query);
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        async move {
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), 302);
            resp.headers()["location"].to_str().unwrap().to_owned()
        }
    };

    // Anonymous: sent to sign in, with the request to come back to; nothing is fetched.
    let to = authorize(None).await;
    assert!(to.starts_with("/oauth/consent?authorize="), "{to}");
    let back = param(&to, "authorize").unwrap();
    assert_eq!(
        param(&format!("/?{back}"), "client_id"),
        Some(client_id.clone())
    );
    assert!(env.db.oauth().client(&client_id).await.unwrap().is_none());

    // Signed in: private addresses are refused, even when webhooks may reach them, and the
    // error does not tell why.
    for allowed in [
        vec![],
        vec!["127.0.0.1".to_owned()],
        vec!["127.0.0.0/8".to_owned()],
    ] {
        env.config.store(
            2,
            SiteConfig {
                external_url: Some(EXTERNAL.into()),
                webhooks_outbound_allowed_hosts: allowed,
                ..SiteConfig::default()
            },
        );
        let to = authorize(Some(env.cookie.clone())).await;
        assert!(to.starts_with("/oauth/consent?error="), "{to}");
        assert_eq!(
            param(&to, "error").as_deref(),
            Some(sourcegull_oauth_server::clients::FETCH_FAILED)
        );
    }
    // A host name in the list is refused before it is resolved.
    env.config.store(
        3,
        SiteConfig {
            external_url: Some(EXTERNAL.into()),
            webhooks_outbound_allowed_hosts: vec!["*.internal.example".to_owned()],
            ..SiteConfig::default()
        },
    );
    let to = env
        .http
        .get(format!("{}/.oauth/authorize", env.base))
        .query(&[("client_id", "https://docs.internal.example/client.json")])
        .header("cookie", &env.cookie)
        .send()
        .await
        .unwrap()
        .headers()["location"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        param(&to, "error").as_deref(),
        Some(sourcegull_oauth_server::clients::FETCH_FAILED)
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no connection was made"
    );
}

#[tokio::test]
async fn janitor_deletes_expired_and_unused() {
    let Some(env) = setup().await else { return };
    let store = env.db.oauth();
    let client_id = env.register().await;
    let client = store.client(&client_id).await.unwrap().unwrap();
    let expired = store
        .create_authorization(&sourcegull_db::NewOAuthAuthorization {
            request_hash: &[7; 32],
            client_id: client.id,
            redirect_uri: REDIRECT,
            state: None,
            code_challenge: "c",
            resource: "r",
            scopes: &["mcp".to_owned()],
            expires_at: chrono::Utc::now() - chrono::Duration::days(2),
        })
        .await
        .unwrap();
    assert!(expired > 0);
    let cleaned = sourcegull_oauth_server::janitor::clean(&env.db)
        .await
        .unwrap();
    assert_eq!(cleaned.authorizations, 1);
    assert_eq!(cleaned.registered_clients, 0);

    // Metadata document clients without a live grant go.
    let used = store
        .upsert_metadata_client(&sourcegull_db::NewOAuthClient {
            client_id: "https://used.example.com/c.json",
            client_name: "Used",
            client_uri: None,
            redirect_uris: &[REDIRECT.to_owned()],
        })
        .await
        .unwrap();
    store
        .upsert_metadata_client(&sourcegull_db::NewOAuthClient {
            client_id: "https://unused.example.com/c.json",
            client_name: "Unused",
            client_uri: None,
            redirect_uris: &[REDIRECT.to_owned()],
        })
        .await
        .unwrap();
    let code = store
        .create_authorization(&sourcegull_db::NewOAuthAuthorization {
            request_hash: &[8; 32],
            client_id: used.id,
            redirect_uri: REDIRECT,
            state: None,
            code_challenge: "c",
            resource: "r",
            scopes: &["mcp".to_owned()],
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        })
        .await
        .unwrap();
    let authorization = store
        .approve_authorization(
            &[8; 32],
            env.alice,
            &[9; 32],
            chrono::Utc::now() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(authorization.id, code);
    let (access, refresh) = ([10; 32], [11; 32]);
    let token = |hash| sourcegull_db::NewOAuthToken {
        hash,
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };
    store
        .create_grant(&authorization, env.alice, token(&access), token(&refresh))
        .await
        .unwrap();
    let deleted = store
        .delete_unused_metadata_clients(chrono::Utc::now() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(
        store
            .client("https://used.example.com/c.json")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .client("https://unused.example.com/c.json")
            .await
            .unwrap()
            .is_none()
    );
}
