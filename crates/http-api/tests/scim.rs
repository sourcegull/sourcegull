//! SCIM 2.0 conformance (`/.api/scim/v2`, RFC 7643/7644) against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`), through HTTP. The scenarios follow what the published
//! identity provider test suites check: Okta's SCIM 2.0 spec tests (list, filter by
//! `userName`, create, duplicate → 409, get unknown → 404, replace, deactivate with PATCH)
//! and the Microsoft Entra ID SCIM validator (filters on `externalId` and
//! `emails[type eq "work"].value`, PATCH with `Replace`, string booleans and value-filter
//! paths, enterprise extension, DELETE).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, PasswordParams, PgSessionStore};
use sourcegull_config::{AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewAccount, NewUser, migrate};
use sourcegull_http_api::{auth_routes, finish, public_routes};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;
use tower::ServiceExt;

const TOKEN: &str = "scim-secret-token-0123456789";
const CORE: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const ENTERPRISE: &str = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
const LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PW: &str = "correct horse battery";

struct Env {
    _t: TestDb,
    db: Db,
    router: Router,
    auth: Authenticator,
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn setup(token: Option<&str>) -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.example.com".into()),
            auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider {
                allow_signup: false,
            })],
            scim_auth_token: token.map(str::to_owned),
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = MetricsHandle::from_prometheus(recorder.handle());
    let state = AppState::new(db.clone(), config, metrics, auth.clone());
    let router = finish(public_routes().merge(auth_routes()), state);
    Some(Env {
        _t: t,
        db,
        router,
        auth,
    })
}

impl Env {
    async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Resp {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// A SCIM request authenticated with `auth` (`Authorization` header value).
    async fn call_as(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        auth: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/.api/scim/v2{path}"))
            .header(header::CONTENT_TYPE, "application/scim+json");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        self.send(req.body(body).unwrap()).await
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        let bearer = format!("Bearer {TOKEN}");
        let r = self.call_as(method, path, body, Some(&bearer), &[]).await;
        if r.status != StatusCode::NO_CONTENT && r.status != StatusCode::NOT_MODIFIED {
            assert_eq!(
                r.headers[header::CONTENT_TYPE],
                "application/scim+json",
                "{path}"
            );
        }
        r
    }

    async fn get(&self, path: &str) -> Resp {
        self.call(Method::GET, path, None).await
    }

    async fn filter(&self, filter: &str) -> Resp {
        let q = utf8_percent_encode(filter, NON_ALPHANUMERIC);
        let r = self.get(&format!("/Users?filter={q}")).await;
        assert_eq!(r.status, StatusCode::OK, "{filter}: {}", r.body);
        assert_eq!(r.body["schemas"], json!([LIST]));
        r
    }

    async fn create(&self, user: Value) -> Resp {
        self.call(Method::POST, "/Users", Some(user)).await
    }

    async fn patch(&self, id: &str, ops: Value) -> Resp {
        self.call(
            Method::PATCH,
            &format!("/Users/{id}"),
            Some(json!({"schemas": [PATCH_OP], "Operations": ops})),
        )
        .await
    }

    /// `/-/auth-state` as seen with a cookie or an `Authorization` header.
    async fn auth_state(&self, cookie: Option<&str>, authorization: Option<&str>) -> Resp {
        let mut req = Request::get("/-/auth-state");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        if let Some(a) = authorization {
            req = req.header(header::AUTHORIZATION, a);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn sign_in(&self, username: &str) -> StatusCode {
        let req = Request::post("/-/sign-in")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Requested-With", "Sourcegull")
            .body(Body::from(
                json!({"username": username, "password": PW}).to_string(),
            ))
            .unwrap();
        self.send(req).await.status
    }
}

/// An Okta-style create request.
fn okta_user(user_name: &str, given: &str, family: &str) -> Value {
    json!({
        "schemas": [CORE],
        "userName": user_name,
        "name": {"givenName": given, "familyName": family},
        "emails": [{"primary": true, "value": user_name, "type": "work"}],
        "displayName": format!("{given} {family}"),
        "locale": "en-US",
        "externalId": format!("00u-{user_name}"),
        "groups": [],
        "password": "1mz050nq",
        "active": true
    })
}

fn assert_error(r: &Resp, status: StatusCode, scim_type: Option<&str>) {
    assert_eq!(r.status, status, "{}", r.body);
    assert_eq!(r.body["schemas"], json!([ERROR]), "{}", r.body);
    assert_eq!(r.body["status"], status.as_u16().to_string());
    match scim_type {
        Some(t) => assert_eq!(r.body["scimType"], t, "{}", r.body),
        None => assert!(r.body.get("scimType").is_none(), "{}", r.body),
    }
}

fn id_of(r: &Resp) -> String {
    r.body["id"].as_str().expect("id").to_owned()
}

fn user_id(id: &str) -> UserId {
    UserId(id.parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_without_token_and_bearer_auth_required() {
    let Some(env) = setup(None).await else { return };
    let r = env.get("/ServiceProviderConfig").await;
    assert_error(&r, StatusCode::NOT_FOUND, None);
    assert!(
        r.body["detail"]
            .as_str()
            .unwrap()
            .contains("scim.authToken")
    );

    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    for auth in [
        None,
        Some("Bearer wrong-token-0123456789-abcdef"),
        Some("Bearer"),
    ] {
        let r = env.call_as(Method::GET, "/Users", None, auth, &[]).await;
        assert_error(&r, StatusCode::UNAUTHORIZED, None);
        assert!(
            r.headers[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Bearer")
        );
    }
    // An access token scheme is not accepted (the access token check rejects it first).
    let r = env
        .call_as(
            Method::GET,
            "/Users",
            None,
            Some(&format!("token {TOKEN}")),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = env
        .call_as(
            Method::POST,
            "/Users",
            Some(okta_user("x@example.com", "X", "Y")),
            Some("Bearer nope"),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.db.users().count().await.unwrap(), 0, "nothing created");
    // The scheme is case-insensitive.
    let r = env
        .call_as(
            Method::GET,
            "/Users",
            None,
            Some(&format!("bearer {TOKEN}")),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_endpoints() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let r = env.get("/ServiceProviderConfig").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["patch"]["supported"], true);
    assert_eq!(r.body["filter"]["supported"], true);
    assert_eq!(r.body["bulk"]["supported"], false);
    assert_eq!(r.body["etag"]["supported"], true);
    assert_eq!(
        r.body["authenticationSchemes"][0]["type"],
        "oauthbearertoken"
    );

    let r = env.get("/ResourceTypes").await;
    assert_eq!(r.body["totalResults"], 1);
    assert_eq!(r.body["Resources"][0]["endpoint"], "/Users");
    assert_eq!(r.body["Resources"][0]["schema"], CORE);
    assert_eq!(env.get("/ResourceTypes/User").await.body["name"], "User");
    assert_error(
        &env.get("/ResourceTypes/Group").await,
        StatusCode::NOT_FOUND,
        None,
    );

    let r = env.get("/Schemas").await;
    let ids: Vec<&str> = r.body["Resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [CORE, ENTERPRISE]);
    let r = env.get(&format!("/Schemas/{CORE}")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "userName" && a["uniqueness"] == "server")
    );
    assert_error(&env.get("/Groups").await, StatusCode::NOT_FOUND, None);
    assert_error(&env.get("/Bulk").await, StatusCode::NOT_FOUND, None);
}

/// Okta's SCIM 2.0 spec test, step by step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn okta_user_lifecycle() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    // "Required test: Test Users endpoint".
    let r = env.get("/Users?count=1&startIndex=1").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["schemas"], json!([LIST]));
    assert!(r.body["Resources"].is_array());
    assert!(r.body["itemsPerPage"].is_number());
    assert!(r.body["startIndex"].is_number());
    assert!(r.body["totalResults"].is_number());

    // "Get a random user that does not exist".
    let r = env.filter(r#"userName eq "random-1f8e@atko.com""#).await;
    assert_eq!(r.body["totalResults"], 0);
    assert_eq!(r.body["Resources"], json!([]));

    // "Create Okta user with realistic values".
    let r = env
        .create(okta_user("ina.ward@atko.com", "Ina", "Ward"))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id = id_of(&r);
    assert_eq!(r.body["userName"], "ina.ward@atko.com");
    assert_eq!(r.body["active"], true);
    assert_eq!(r.body["name"]["familyName"], "Ward");
    assert_eq!(r.body["externalId"], "00u-ina.ward@atko.com");
    assert!(
        r.body.get("password").is_none(),
        "password is never returned"
    );
    assert_eq!(r.body["meta"]["resourceType"], "User");
    let location = format!("https://sourcegull.example.com/.api/scim/v2/Users/{id}");
    assert_eq!(r.body["meta"]["location"], location);
    assert_eq!(r.headers[header::LOCATION], location.as_str());
    assert_eq!(
        r.headers[header::ETAG].to_str().unwrap(),
        r.body["meta"]["version"].as_str().unwrap()
    );
    // The Sourcegull user: derived username, verified primary email, display name.
    let user = env
        .db
        .users()
        .get_by_id(user_id(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "ina.ward-atko.com");
    assert_eq!(user.display_name.as_deref(), Some("Ina Ward"));
    assert!(!user.site_admin, "SCIM users never become site admins");
    let emails = env.db.user_emails().list(user.id).await.unwrap();
    assert_eq!(emails.len(), 1);
    assert!(emails[0].is_primary && emails[0].is_verified());

    // "Verify that user was created" (by id and by filter, case-insensitively).
    let r = env.get(&format!("/Users/{id}")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["userName"], "ina.ward@atko.com");
    let r = env.filter(r#"userName eq "INA.WARD@atko.com""#).await;
    assert_eq!(r.body["totalResults"], 1);
    assert_eq!(r.body["Resources"][0]["id"], id.as_str());

    // "Expect failure when recreating user with same values".
    assert_error(
        &env.create(okta_user("ina.ward@atko.com", "Ina", "Ward"))
            .await,
        StatusCode::CONFLICT,
        Some("uniqueness"),
    );
    // The same userName with another address is a duplicate too.
    let mut dup = okta_user("ina.ward@atko.com", "Ina", "Ward");
    dup["emails"] = json!([{"value": "other@atko.com", "primary": true}]);
    dup["externalId"] = json!("00uOTHER");
    assert_error(
        &env.create(dup).await,
        StatusCode::CONFLICT,
        Some("uniqueness"),
    );

    // "Username Case Sensitivity Check".
    let r = env.filter(r#"userName eq "ina.WARD@ATKO.COM""#).await;
    assert_eq!(r.body["totalResults"], 1);

    // "Get a user that does not exist".
    assert_error(&env.get("/Users/010101").await, StatusCode::NOT_FOUND, None);
    assert_error(
        &env.get("/Users/not-a-number").await,
        StatusCode::NOT_FOUND,
        None,
    );

    // Replace (PUT) with a new last name and display name.
    let mut full = r.body["Resources"][0].clone();
    full["name"]["familyName"] = json!("Ward-Smith");
    full["displayName"] = json!("Ina Ward-Smith");
    let r = env
        .call(Method::PUT, &format!("/Users/{id}"), Some(full))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["name"]["familyName"], "Ward-Smith");
    assert_eq!(r.body["active"], true);
    let user = env
        .db
        .users()
        .get_by_id(user_id(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.display_name.as_deref(), Some("Ina Ward-Smith"));
    assert_eq!(user.username, "ina.ward-atko.com", "username kept");

    // "Deactivate a user" (Okta sends a path-less replace).
    let r = env
        .patch(&id, json!([{"op": "replace", "value": {"active": false}}]))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["active"], false);
    assert_eq!(r.body["name"]["familyName"], "Ward-Smith");
    assert!(
        env.db
            .users()
            .get_by_id(user_id(&id))
            .await
            .unwrap()
            .is_none(),
        "deactivated users are soft-deleted"
    );
    let r = env.get(&format!("/Users/{id}")).await;
    assert_eq!(r.body["active"], false);
    let r = env.filter(r#"userName eq "ina.ward@atko.com""#).await;
    assert_eq!(r.body["totalResults"], 1, "deactivated users stay visible");
    assert_eq!(r.body["Resources"][0]["active"], false);
    let r = env.filter("active eq false").await;
    assert_eq!(r.body["totalResults"], 1);

    // Reactivate.
    let r = env
        .patch(
            &id,
            json!([{"op": "replace", "path": "active", "value": true}]),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["active"], true);
    let user = env
        .db
        .users()
        .get_by_id(user_id(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "ina.ward-atko.com");
}

/// The Microsoft Entra ID SCIM validator's user tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entra_user_provisioning() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let r = env
        .create(json!({
            "schemas": [CORE, ENTERPRISE],
            "externalId": "5d48a0a8-e3f3-4b8f-9d8c-0b4f2a1c3e7d",
            "userName": "Test_User_ab6490ee@contoso.com",
            "active": true,
            "emails": [{"primary": true, "type": "work", "value": "Test_User_fd0ea19b@contoso.com"}],
            "meta": {"resourceType": "User"},
            "name": {"formatted": "givenName familyName", "familyName": "familyName", "givenName": "givenName"},
            "roles": [],
            ENTERPRISE: {"department": "Sales", "employeeNumber": "42", "manager": {"value": "1"}}
        }))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id = id_of(&r);
    assert_eq!(r.body["schemas"], json!([CORE, ENTERPRISE]));
    assert_eq!(r.body[ENTERPRISE]["department"], "Sales");

    // Lookups Entra issues before creating or updating.
    for f in [
        r#"externalId eq "5d48a0a8-e3f3-4b8f-9d8c-0b4f2a1c3e7d""#,
        r#"userName eq "test_user_ab6490ee@contoso.com""#,
        r#"emails[type eq "work"].value eq "Test_User_fd0ea19b@contoso.com""#,
        r#"userName eq "nobody@contoso.com" or externalId eq "5d48a0a8-e3f3-4b8f-9d8c-0b4f2a1c3e7d""#,
        r#"urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:employeeNumber eq "42""#,
        r#"name.familyName eq "familyName" and active eq true"#,
    ] {
        let r = env.filter(f).await;
        assert_eq!(r.body["totalResults"], 1, "{f}");
        assert_eq!(r.body["Resources"][0]["id"], id.as_str(), "{f}");
    }
    assert_eq!(
        env.filter(r#"externalId eq "5D48A0A8-E3F3-4B8F-9D8C-0B4F2A1C3E7D""#)
            .await
            .body["totalResults"],
        0,
        "externalId is case-exact"
    );
    assert_error(
        &env.get("/Users?filter=userName%20eq").await,
        StatusCode::BAD_REQUEST,
        Some("invalidFilter"),
    );

    // Entra's attribute updates: PascalCase ops, value-filter paths, dotted names.
    let r = env
        .patch(
            &id,
            json!([
                {"op": "Replace", "path": "emails[type eq \"work\"].value", "value": "updated@contoso.com"},
                {"op": "Replace", "path": "name.familyName", "value": "Updated"},
                {"op": "Add", "path": "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department", "value": "Marketing"},
                {"op": "Replace", "path": "displayName", "value": "Updated Name"},
                {"op": "Add", "path": "phoneNumbers[type eq \"mobile\"].value", "value": "+1 555 0100"}
            ]),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["emails"][0]["value"], "updated@contoso.com");
    assert_eq!(r.body["name"]["familyName"], "Updated");
    assert_eq!(r.body[ENTERPRISE]["department"], "Marketing");
    assert_eq!(r.body["phoneNumbers"][0]["value"], "+1 555 0100");
    let emails = env.db.user_emails().list(user_id(&id)).await.unwrap();
    let addresses: Vec<&str> = emails.iter().map(|e| e.email.as_str()).collect();
    assert_eq!(addresses, ["updated@contoso.com"], "old address removed");
    assert!(emails[0].is_primary && emails[0].is_verified());
    let user = env
        .db
        .users()
        .get_by_id(user_id(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.display_name.as_deref(), Some("Updated Name"));

    // Rename (userName) changes the Sourcegull username.
    let r = env
        .patch(
            &id,
            json!([{"op": "Replace", "path": "userName", "value": "renamed@contoso.com"}]),
        )
        .await;
    assert_eq!(r.body["userName"], "renamed@contoso.com");
    let user = env
        .db
        .users()
        .get_by_id(user_id(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "renamed-contoso.com");

    // Errors Entra expects to be reported, not ignored.
    assert_error(
        &env.patch(&id, json!([{"op": "Replace", "path": "id", "value": "1"}]))
            .await,
        StatusCode::BAD_REQUEST,
        Some("mutability"),
    );
    assert_error(
        &env.patch(&id, json!([{"op": "Remove"}])).await,
        StatusCode::BAD_REQUEST,
        Some("noTarget"),
    );
    assert_error(
        &env.patch(
            &id,
            json!([{"op": "Replace", "path": "userName", "value": ""}]),
        )
        .await,
        StatusCode::BAD_REQUEST,
        Some("invalidValue"),
    );
    assert_error(
        &env.call(Method::POST, "/Users", Some(json!("not an object")))
            .await,
        StatusCode::BAD_REQUEST,
        Some("invalidSyntax"),
    );
    let r = env
        .call_as(
            Method::POST,
            "/Users",
            None,
            Some(&format!("Bearer {TOKEN}")),
            &[],
        )
        .await;
    assert_error(&r, StatusCode::BAD_REQUEST, Some("invalidSyntax"));

    // Disable with Entra's string boolean.
    let r = env
        .patch(
            &id,
            json!([{"op": "Replace", "path": "active", "value": "False"}]),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["active"], false);
    assert!(
        env.db
            .users()
            .get_by_id(user_id(&id))
            .await
            .unwrap()
            .is_none()
    );

    // Delete: gone for good.
    let r = env
        .call(Method::DELETE, &format!("/Users/{id}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_error(
        &env.get(&format!("/Users/{id}")).await,
        StatusCode::NOT_FOUND,
        None,
    );
    assert_error(
        &env.call(Method::DELETE, &format!("/Users/{id}"), None)
            .await,
        StatusCode::NOT_FOUND,
        None,
    );
    assert!(env.db.scim().get(user_id(&id)).await.unwrap().is_none());
    let rows: i64 = sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(user_id(&id))
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0, "hard-deleted");
    // The address is free again.
    let r = env
        .create(
            json!({"userName": "fresh@contoso.com", "emails": [{"value": "updated@contoso.com"}]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pagination_search_and_projection() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let mut ids = Vec::new();
    for i in 0..5 {
        let r = env
            .create(okta_user(
                &format!("user{i}@example.com"),
                "U",
                &format!("N{i}"),
            ))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        ids.push(id_of(&r));
    }
    let page = |r: &Resp| -> Vec<String> {
        r.body["Resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let r = env.get("/Users?startIndex=2&count=2").await;
    assert_eq!(r.body["totalResults"], 5);
    assert_eq!(r.body["startIndex"], 2);
    assert_eq!(r.body["itemsPerPage"], 2);
    assert_eq!(page(&r), ids[1..3]);
    let r = env.get("/Users?startIndex=5&count=10").await;
    assert_eq!(page(&r), ids[4..]);
    let r = env.get("/Users?count=0").await;
    assert_eq!(r.body["totalResults"], 5);
    assert_eq!(r.body["Resources"], json!([]));
    let r = env.get("/Users?startIndex=0&count=-3").await;
    assert_eq!(r.body["startIndex"], 1, "startIndex below 1 means 1");
    assert_eq!(r.body["itemsPerPage"], 0, "negative count means 0");
    assert_error(
        &env.get("/Users?count=many").await,
        StatusCode::BAD_REQUEST,
        Some("invalidValue"),
    );

    // Filtered and paged.
    let f = utf8_percent_encode(
        r#"userName sw "user" and name.familyName ne "N0""#,
        NON_ALPHANUMERIC,
    );
    let r = env
        .get(&format!("/Users?filter={f}&startIndex=3&count=5"))
        .await;
    assert_eq!(r.body["totalResults"], 4);
    assert_eq!(page(&r), ids[3..]);

    // POST /Users/.search.
    let r = env
        .call(
            Method::POST,
            "/Users/.search",
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:SearchRequest"],
                "filter": "userName eq \"user2@example.com\"",
                "attributes": ["userName"],
                "startIndex": 1,
                "count": 10
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["totalResults"], 1);
    assert_eq!(
        r.body["Resources"][0],
        json!({"schemas": [CORE], "id": ids[2], "userName": "user2@example.com"})
    );

    // attributes / excludedAttributes on GET.
    let r = env
        .get(&format!(
            "/Users/{}?attributes=name.familyName,emails.value",
            ids[0]
        ))
        .await;
    assert_eq!(
        r.body,
        json!({"schemas": [CORE], "id": ids[0], "name": {"familyName": "N0"},
               "emails": [{"value": "user0@example.com"}]})
    );
    let r = env
        .get(&format!("/Users/{}?excludedAttributes=emails,meta", ids[0]))
        .await;
    assert!(r.body.get("emails").is_none() && r.body.get("meta").is_none());
    assert_eq!(r.body["userName"], "user0@example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deactivation_ends_sessions_and_access_tokens() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let r = env.create(okta_user("dana@example.com", "Dana", "K")).await;
    let id = id_of(&r);
    let uid = user_id(&id);
    let username = env
        .db
        .users()
        .get_by_id(uid)
        .await
        .unwrap()
        .unwrap()
        .username;
    env.auth.set_password(uid, PW).await.unwrap();
    assert_eq!(env.sign_in(&username).await, StatusCode::OK);
    let session = env.auth.start_session(uid).await.unwrap();
    let cookie = format!("sourcegull_session={}", session.as_str());
    let actor = env
        .auth
        .actor_for(uid, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    let (_, token) = env
        .auth
        .create_access_token(&actor, uid, &["user:all".into()], "ci", None)
        .await
        .unwrap();
    let token_header = format!("token {token}");
    assert_eq!(
        env.auth_state(Some(&cookie), None).await.body["authenticated"],
        true
    );
    assert_eq!(
        env.auth_state(None, Some(&token_header)).await.body["authenticated"],
        true
    );

    let r = env
        .patch(&id, json!([{"op": "replace", "value": {"active": false}}]))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    assert_eq!(
        env.auth_state(Some(&cookie), None).await.body["authenticated"],
        false,
        "the session ended"
    );
    assert_eq!(
        env.auth_state(None, Some(&token_header)).await.status,
        StatusCode::UNAUTHORIZED,
        "the access token was revoked"
    );
    assert_eq!(env.sign_in(&username).await, StatusCode::UNAUTHORIZED);

    // Reactivation lets the user sign in again; the revoked token stays revoked.
    let r = env
        .patch(&id, json!([{"op": "replace", "value": {"active": true}}]))
        .await;
    assert_eq!(r.body["active"], true);
    assert_eq!(env.sign_in(&username).await, StatusCode::OK);
    assert_eq!(
        env.auth_state(None, Some(&token_header)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Deleting also ends sessions.
    let session = env.auth.start_session(uid).await.unwrap();
    let cookie = format!("sourcegull_session={}", session.as_str());
    let r = env
        .call(Method::DELETE, &format!("/Users/{id}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        env.auth_state(Some(&cookie), None).await.body["authenticated"],
        false
    );

    let events: Vec<String> = sourcegull_db::sqlx::query_scalar(
        "SELECT name FROM security_event_logs WHERE name LIKE 'SCIM%' ORDER BY id",
    )
    .fetch_all(env.db.pool())
    .await
    .unwrap();
    assert_eq!(
        events,
        [
            "SCIMUserCreated",
            "SCIMUserDeactivated",
            "SCIMUserUpdated",
            "SCIMUserDeleted"
        ],
        "audited as security events"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_users_takeover_conflicts_and_etags() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let new = |username: &str, email: &str| NewAccount {
        user: NewUser {
            username: username.into(),
            ..Default::default()
        },
        email: Some(email.into()),
        email_verified: true,
        only_if_first: false,
    };
    let admin = env
        .db
        .accounts()
        .create(&new("admin", "admin@example.com"))
        .await
        .unwrap();
    let carol = env
        .db
        .accounts()
        .create(&new("carol", "carol@example.com"))
        .await
        .unwrap();

    // Existing users are listed, so providers can match them.
    let r = env.filter(r#"userName eq "carol""#).await;
    assert_eq!(r.body["totalResults"], 1);
    assert_eq!(
        r.body["Resources"][0]["emails"][0]["value"],
        "carol@example.com"
    );
    let r = env.get("/Users").await;
    assert_eq!(r.body["totalResults"], 2);
    // Not SCIM-controlled: cannot be deleted through SCIM.
    assert_error(
        &env.call(Method::DELETE, &format!("/Users/{}", admin.user.id), None)
            .await,
        StatusCode::BAD_REQUEST,
        Some("mutability"),
    );

    // Creating a user with carol's verified address takes carol over.
    let r = env
        .create(json!({
            "schemas": [CORE], "userName": "carol.c", "externalId": "ext-carol",
            "emails": [{"value": "CAROL@example.com", "primary": true}]
        }))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(id_of(&r), carol.user.id.to_string());
    let user = env
        .db
        .users()
        .get_by_id(carol.user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "carol.c");
    assert!(env.db.scim().is_controlled(carol.user.id).await.unwrap());
    assert!(!env.db.scim().is_controlled(admin.user.id).await.unwrap());
    // A second takeover of the same address is a duplicate.
    assert_error(
        &env.create(json!({"userName": "c2", "emails": [{"value": "carol@example.com"}]}))
            .await,
        StatusCode::CONFLICT,
        Some("uniqueness"),
    );
    // So is a duplicate externalId.
    assert_error(
        &env.create(json!({"userName": "c3", "externalId": "ext-carol"}))
            .await,
        StatusCode::CONFLICT,
        Some("uniqueness"),
    );

    // A username held by a user the provider does not manage gets a suffix.
    let r = env
        .create(json!({"userName": "admin", "emails": [{"value": "other-admin@example.com"}]}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["userName"], "admin");
    let other = env
        .db
        .users()
        .get_by_id(user_id(&id_of(&r)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other.username, "admin-2");

    // ETags.
    let path = format!("/Users/{}", carol.user.id);
    let r = env.get(&path).await;
    let etag = r.headers[header::ETAG].to_str().unwrap().to_owned();
    assert!(etag.starts_with("W/\""));
    let bearer = format!("Bearer {TOKEN}");
    let r = env
        .call_as(
            Method::GET,
            &path,
            None,
            Some(&bearer),
            &[("if-none-match", &etag)],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    let body = json!({"schemas": [CORE], "userName": "carol.c", "displayName": "Carol",
                      "externalId": "ext-carol", "emails": [{"value": "carol@example.com"}]});
    let r = env
        .call_as(
            Method::PUT,
            &path,
            Some(body.clone()),
            Some(&bearer),
            &[("if-match", "W/\"stale\"")],
        )
        .await;
    assert_error(&r, StatusCode::PRECONDITION_FAILED, None);
    let r = env
        .call_as(
            Method::PUT,
            &path,
            Some(body),
            Some(&bearer),
            &[("if-match", &etag)],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let new_etag = r.headers[header::ETAG].to_str().unwrap().to_owned();
    assert_ne!(new_etag, etag, "the version changes with the resource");
    let r = env
        .call_as(
            Method::GET,
            &path,
            None,
            Some(&bearer),
            &[("if-none-match", &etag)],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}
