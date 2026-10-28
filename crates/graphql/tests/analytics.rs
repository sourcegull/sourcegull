//! Usage analytics over GraphQL against the test Postgres: events from `logEvents`
//! and from the server (a notebook creation), anonymous visitors only with `auth.public`,
//! the rollup, `site.analytics`,
//! `site.usageStatistics`, `site.users` with activity, and site-admin-only access.

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
use sourcegull_db::{Db, NewUsageEvent, NewUser, migrate, sqlx};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

const REPO: &str = "gitlab.example.com/acme/api";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    config: ConfigHandle<SiteConfig>,
}

impl Env {
    async fn run(&self, user: Option<UserId>, query: &str, vars: Value) -> Response {
        let actor = match user {
            Some(u) => self.auth.actor_for(u, AuthVia::Session).await.unwrap(),
            None => None,
        };
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, user: Option<UserId>, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn error_code(&self, user: Option<UserId>, query: &str) -> String {
        let resp = self.run(user, query, json!({})).await;
        let e = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        e.extensions
            .as_ref()
            .and_then(|x| x.get("code"))
            .map(|c| c.to_string().trim_matches('"').to_owned())
            .unwrap_or_default()
    }

    async fn count(&self, name: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM event_logs WHERE name = $1")
            .bind(name)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

const LOG: &str = r#"mutation($events: [Event!]) { logEvents(events: $events) { alwaysNil } }"#;

fn event(name: &str, cookie: &str) -> Value {
    json!({ "event": name, "userCookieID": cookie, "url": "http://sourcegull/x", "source": "WEB" })
}

const ANALYTICS: &str = r#"query($range: AnalyticsDateRange!, $grouping: AnalyticsGrouping!) {
  site { analytics {
    updatedAt
    search(dateRange: $range, grouping: $grouping) {
      searches { summary { totalCount totalUniqueUsers totalRegisteredUsers }
                 nodes { date count uniqueUsers registeredUsers } }
      resultClicks { summary { totalCount } }
      fileViews { summary { totalCount } }
      searchesByType { key count }
      latency { p50 p90 p99 }
    }
    users(dateRange: $range, grouping: $grouping) {
      activity { summary { totalCount totalUniqueUsers totalRegisteredUsers } }
      frequencies { daysUsed frequency percentage }
      monthlyActiveUsers { date count }
      signIns { summary { totalCount } }
    }
    codeIntel(dateRange: $range, grouping: $grouping) {
      referenceClicks { summary { totalCount } }
      definitionClicks { summary { totalCount } }
      inAppEvents { summary { totalCount } }
      codeHostEvents { summary { totalCount } }
      preciseEvents { summary { totalCount } }
      searchBasedEvents { summary { totalCount } }
      crossRepoEvents { summary { totalCount } }
    }
    codeIntelByLanguage(dateRange: $range) { language precision count }
    codeIntelTopRepositories(dateRange: $range) { name language kind precision events hasPrecise }
    repos { count preciseCodeIntelCount }
    notebooks(dateRange: $range, grouping: $grouping) {
      creations { summary { totalCount totalUniqueUsers } } views { summary { totalCount } }
    }
    codeMonitors(dateRange: $range, grouping: $grouping) { views { summary { totalCount } } }
    batchChanges(dateRange: $range, grouping: $grouping) {
      changesetsCreated { summary { totalCount } } changesetsMerged { summary { totalCount } }
    }
    codeInsights(dateRange: $range, grouping: $grouping) { insightHovers { summary { totalCount } } }
    extensions(dateRange: $range, grouping: $grouping) { vscode { summary { totalCount } } }
  } }
}"#;

#[tokio::test]
async fn analytics_from_local_events_for_site_admins() {
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
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    sqlx::query(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/')",
    )
    .bind(REPO)
    .execute(db.pool())
    .await
    .unwrap();
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    let env = Env {
        _t: t,
        db: db.clone(),
        auth,
        schema,
        config,
    };

    // Events from the web app.
    env.ok(
        Some(alice),
        LOG,
        json!({ "events": [
            event("SearchResultClicked", "c1"),
            event("goToDefinition", "c1"),
            event("findReferences", "c1"),
            event("SearchNotebookPageViewed", "c1"),
            event("CodeMonitoringPageViewed", "c1"),
        ] }),
    )
    .await;
    env.ok(
        Some(admin),
        LOG,
        json!({ "events": [event("SearchResultClicked", "c2")] }),
    )
    .await;
    // Signed-out visitors count only where anonymous access is allowed.
    env.ok(
        None,
        LOG,
        json!({ "events": [event("SearchResultClicked", "v1")] }),
    )
    .await;
    assert_eq!(env.count("SearchResultClicked").await, 2);
    let public = SiteConfig {
        auth_public: true,
        ..SiteConfig::default()
    };
    env.config.store(2, public);
    env.ok(
        None,
        LOG,
        json!({ "events": [event("SearchResultClicked", "v1")] }),
    )
    .await;
    assert_eq!(env.count("SearchResultClicked").await, 3);
    let anon: String = sqlx::query_scalar(
        "SELECT anonymous_user_id FROM event_logs WHERE user_id IS NULL AND name = 'SearchResultClicked'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(anon, "v1");

    // Events the server records (as the search stream and code navigation do).
    let server = |name: &str, user: UserId, argument: Value| NewUsageEvent {
        name: name.into(),
        user_id: Some(user),
        source: "BACKEND".into(),
        argument,
        ..Default::default()
    };
    db.event_logs()
        .insert(&[
            server(
                "SearchResultsQueried",
                alice,
                json!({"type": "keyword", "durationMs": 120}),
            ),
            server(
                "SearchResultsQueried",
                alice,
                json!({"type": "keyword", "durationMs": 80}),
            ),
            server(
                "SearchResultsQueried",
                admin,
                json!({"type": "symbol", "durationMs": 900}),
            ),
            server(
                "codeintel.lsifHover",
                alice,
                json!({"repoName": REPO, "languageId": "Go"}),
            ),
            server(
                "codeintel.searchReferences.xrepo",
                alice,
                json!({"repoName": REPO, "languageId": "Go"}),
            ),
            // Repositories the admin cannot see (here: missing) are left out of the top list.
            server(
                "codeintel.lsifHover",
                admin,
                json!({"repoName": "gitlab.example.com/gone", "languageId": "Go"}),
            ),
        ])
        .await
        .unwrap();
    // A server event from a mutation: creating a notebook.
    env.ok(
        Some(alice),
        "mutation($nb: NotebookInput!) { createNotebook(notebook: $nb) { id } }",
        json!({ "nb": { "title": "N", "namespace": marshal_id("User", alice.get()),
                        "public": false, "blocks": [] } }),
    )
    .await;
    let mut waited = 0;
    while env.count("SearchNotebookCreated").await == 0 {
        assert!(waited < 50, "the notebook creation event was not recorded");
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }

    // Non-admins and signed-out users cannot read analytics.
    for q in [
        "{ site { analytics { search { searches { summary { totalCount } } } } } }",
        "{ site { usageStatistics { daus { userCount } } } }",
        "{ site { users { totalCount } } }",
    ] {
        assert_eq!(env.error_code(Some(alice), q).await, "FORBIDDEN", "{q}");
        assert_eq!(env.error_code(None, q).await, "UNAUTHORIZED", "{q}");
    }

    // Before the first rollup there is nothing yet.
    let vars = json!({ "range": "LAST_WEEK", "grouping": "DAILY" });
    let empty = env.ok(Some(admin), ANALYTICS, vars.clone()).await;
    assert_eq!(empty["site"]["analytics"]["updatedAt"], Value::Null);
    assert_eq!(
        empty["site"]["analytics"]["search"]["searches"]["summary"]["totalCount"],
        0.0
    );

    sourcegull_analytics::rollup(&db, Utc::now(), &sourcegull_db::Fence::none("test"))
        .await
        .unwrap();
    let data = env.ok(Some(admin), ANALYTICS, vars).await;
    let a = &data["site"]["analytics"];
    assert!(a["updatedAt"].is_string());
    let searches = &a["search"]["searches"];
    assert_eq!(
        searches["summary"],
        json!({ "totalCount": 3.0, "totalUniqueUsers": 2.0, "totalRegisteredUsers": 2.0 })
    );
    let nodes = searches["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 8, "a week of days, today included");
    assert_eq!(nodes[7]["count"], 3.0);
    assert!(nodes[7]["date"].as_str().unwrap().ends_with("T00:00:00Z"));
    assert_eq!(nodes[0]["count"], 0.0);
    assert_eq!(a["search"]["resultClicks"]["summary"]["totalCount"], 3.0);
    assert_eq!(
        a["search"]["searchesByType"],
        json!([{ "key": "keyword", "count": 2.0 }, { "key": "symbol", "count": 1.0 }])
    );
    assert_eq!(
        a["search"]["latency"],
        json!({ "p50": 250.0, "p90": 1000.0, "p99": 1000.0 })
    );
    let users = &a["users"];
    // alice, the admin (sign-up and events) and the visitor.
    assert_eq!(users["activity"]["summary"]["totalUniqueUsers"], 3.0);
    assert_eq!(users["activity"]["summary"]["totalRegisteredUsers"], 2.0);
    assert_eq!(
        users["frequencies"],
        json!([{ "daysUsed": 1.0, "frequency": 3.0, "percentage": 100.0 }])
    );
    let maus = users["monthlyActiveUsers"].as_array().unwrap();
    assert_eq!(maus.len(), 3);
    assert_eq!(maus[2]["count"], 3.0);
    assert_eq!(
        maus[2]["date"],
        Utc::now().date_naive().format("%Y-%m").to_string()
    );
    let ci = &a["codeIntel"];
    assert_eq!(ci["referenceClicks"]["summary"]["totalCount"], 1.0);
    assert_eq!(ci["definitionClicks"]["summary"]["totalCount"], 1.0);
    assert_eq!(ci["inAppEvents"]["summary"]["totalCount"], 3.0);
    assert_eq!(ci["codeHostEvents"]["summary"]["totalCount"], 0.0);
    assert_eq!(ci["preciseEvents"]["summary"]["totalCount"], 2.0);
    assert_eq!(ci["searchBasedEvents"]["summary"]["totalCount"], 1.0);
    assert_eq!(ci["crossRepoEvents"]["summary"]["totalCount"], 1.0);
    assert_eq!(
        a["codeIntelByLanguage"],
        json!([
            { "language": "Go", "precision": "precise", "count": 2.0 },
            { "language": "Go", "precision": "search-based", "count": 1.0 },
        ])
    );
    let top = a["codeIntelTopRepositories"].as_array().unwrap();
    assert_eq!(top.len(), 2);
    assert!(
        top.iter()
            .all(|r| r["name"] == REPO && r["hasPrecise"] == false)
    );
    assert_eq!(
        a["repos"],
        json!({ "count": 1.0, "preciseCodeIntelCount": 0.0 })
    );
    assert_eq!(
        a["notebooks"]["creations"]["summary"],
        json!({ "totalCount": 1.0, "totalUniqueUsers": 1.0 })
    );
    assert_eq!(a["notebooks"]["views"]["summary"]["totalCount"], 1.0);
    assert_eq!(a["codeMonitors"]["views"]["summary"]["totalCount"], 1.0);
    assert_eq!(
        a["batchChanges"]["changesetsCreated"]["summary"]["totalCount"],
        0.0
    );
    assert_eq!(
        a["codeInsights"]["insightHovers"]["summary"]["totalCount"],
        0.0
    );
    assert_eq!(a["extensions"]["vscode"]["summary"]["totalCount"], 0.0);

    // Weekly grouping and the other ranges.
    let weekly = env
        .ok(
            Some(admin),
            "query { site { analytics { search(dateRange: LAST_THREE_MONTHS, grouping: WEEKLY) {
                searches { nodes { date count } summary { totalCount } } } } } }",
            json!({}),
        )
        .await;
    let w = &weekly["site"]["analytics"]["search"]["searches"];
    assert!(w["nodes"].as_array().unwrap().len() >= 13);
    assert_eq!(w["summary"]["totalCount"], 3.0);
    let custom = env
        .run(
            Some(admin),
            "{ site { analytics { search(dateRange: CUSTOM) { searches { summary { totalCount } } } } } }",
            json!({}),
        )
        .await;
    assert!(!custom.errors.is_empty());

    // DAU/WAU/MAU.
    let usage = env
        .ok(
            Some(admin),
            "{ site { usageStatistics(days: 2, weeks: 1, months: 1) {
                daus { startTime userCount registeredUserCount anonymousUserCount }
                waus { userCount } maus { userCount } } } }",
            json!({}),
        )
        .await;
    let u = &usage["site"]["usageStatistics"];
    assert_eq!(u["daus"].as_array().unwrap().len(), 2);
    assert_eq!(u["daus"][0]["userCount"], 3);
    assert_eq!(u["daus"][0]["registeredUserCount"], 2);
    assert_eq!(u["daus"][0]["anonymousUserCount"], 1);
    assert_eq!(u["daus"][1]["userCount"], 0);
    assert_eq!(u["waus"][0]["userCount"], 3);
    assert_eq!(u["maus"][0]["userCount"], 3);

    // Users with activity.
    let users = env
        .ok(
            Some(admin),
            "{ site { users(siteAdmin: false) { totalCount
                nodes(orderBy: EVENTS_COUNT, descending: true) {
                    id username email siteAdmin eventsCount lastActiveAt deletedAt locked } } } }",
            json!({}),
        )
        .await;
    let su = &users["site"]["users"];
    assert_eq!(su["totalCount"], 1.0);
    let alice_row = &su["nodes"][0];
    assert_eq!(alice_row["username"], "alice");
    assert_eq!(alice_row["id"], marshal_id("User", alice.get()));
    assert_eq!(alice_row["eventsCount"], 10.0);
    assert!(alice_row["lastActiveAt"].is_string());
    assert_eq!(alice_row["email"], Value::Null);
    let all = env
        .ok(
            Some(admin),
            r#"{ site { users(query: "adm") { totalCount nodes { username email eventsCount } } } }"#,
            json!({}),
        )
        .await;
    assert_eq!(
        all["site"]["users"],
        json!({ "totalCount": 1.0, "nodes": [
            { "username": "admin", "email": "admin@example.com", "eventsCount": 4.0 }
        ] })
    );
}
