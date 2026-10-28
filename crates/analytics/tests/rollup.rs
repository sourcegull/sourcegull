//! The usage rollup and its queries against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): daily
//! rows per user, active users per day, week and month, searches by type and latency, code
//! navigation by repository, idempotent reruns, late events of today, retention that never loses
//! events not rolled up, and per-user activity.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};
use sourcegull_analytics::names;
use sourcegull_analytics::query::{
    self, DateRange, EventFilter, Grouping, UserListOptions, UserOrder, Window,
};
use sourcegull_analytics::rollup::{Period, prune, rollup};
use sourcegull_db::{Db, Fence, NewUsageEvent, NewUser, migrate, sqlx};
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn day(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

async fn insert(db: &Db, ts: &str, name: &str, user: Option<UserId>, anon: &str, arg: Value) {
    db.event_logs()
        .insert(&[NewUsageEvent {
            name: name.into(),
            user_id: user,
            anonymous_user_id: anon.into(),
            source: "WEB".into(),
            argument: arg,
            ..Default::default()
        }])
        .await
        .unwrap();
    sqlx::query("UPDATE event_logs SET timestamp = $1 WHERE id = (SELECT max(id) FROM event_logs)")
        .bind(at(ts))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Every rollup row, as text, in a stable order.
async fn snapshot(db: &Db) -> Vec<String> {
    let mut out = Vec::new();
    for (table, order) in [
        (
            "event_logs_daily",
            "day, name, source, user_id, anonymous_user_id",
        ),
        ("event_logs_active_users", "period, period_start"),
        (
            "event_logs_search_daily",
            "day, search_type, latency_bucket_ms",
        ),
        (
            "event_logs_codeintel_daily",
            "day, name, repo_name, language",
        ),
    ] {
        let rows: Vec<String> =
            sqlx::query_scalar(&format!("SELECT t::text FROM {table} t ORDER BY {order}"))
                .fetch_all(db.pool())
                .await
                .unwrap();
        out.extend(rows.into_iter().map(|r| format!("{table} {r}")));
    }
    out
}

async fn active(db: &Db, period: &str, start: &str) -> (i32, i32, i32) {
    sqlx::query_as(
        "SELECT users, registered_users, anonymous_users FROM event_logs_active_users
         WHERE period = $1 AND period_start = $2",
    )
    .bind(period)
    .bind(day(start))
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn rollup_aggregates_idempotently_and_prunes() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let user = |name: &str| NewUser {
        username: name.into(),
        ..Default::default()
    };
    let alice = db.users().create(&user("alice")).await.unwrap().id;
    let bob = db.users().create(&user("bob")).await.unwrap().id;

    let search = names::SEARCH_RESULTS_QUERIED;
    insert(
        &db,
        "2026-09-30T12:00:00Z",
        names::VIEW_BLOB,
        Some(alice),
        "",
        json!({}),
    )
    .await;
    insert(
        &db,
        "2026-10-19T10:00:00Z",
        search,
        Some(alice),
        "c",
        json!({"type": "keyword", "durationMs": 40}),
    )
    .await;
    insert(
        &db,
        "2026-10-19T10:01:00Z",
        names::VIEW_BLOB,
        Some(alice),
        "",
        json!({}),
    )
    .await;
    insert(
        &db,
        "2026-10-19T11:00:00Z",
        search,
        None,
        "visitor",
        json!({"type": "symbol", "durationMs": 300}),
    )
    .await;
    insert(
        &db,
        "2026-10-20T23:59:00Z",
        search,
        Some(bob),
        "",
        json!({"type": "keyword", "durationMs": 2000}),
    )
    .await;
    insert(
        &db,
        "2026-10-20T23:59:30Z",
        "codeintel.lsifHover",
        Some(bob),
        "",
        json!({"repoName": "r1", "languageId": "Go"}),
    )
    .await;
    insert(
        &db,
        "2026-10-21T08:00:00Z",
        "codeintel.searchReferences.xrepo",
        Some(alice),
        "",
        json!({"repoName": "r1", "languageId": "Go"}),
    )
    .await;
    insert(
        &db,
        "2026-10-21T08:00:01Z",
        search,
        Some(alice),
        "",
        json!({"type": "regexp"}),
    )
    .await;

    let now = at("2026-10-21T12:00:00Z");
    let stats = rollup(&db, now, &Fence::none("test")).await.unwrap();
    assert_eq!(stats.days.len(), 22, "2026-09-30 through 2026-10-21");
    assert_eq!(stats.days.first(), Some(&day("2026-09-30")));

    // Daily rows: per day, name, source and user.
    let n: i64 = sqlx::query_scalar("SELECT sum(count) FROM event_logs_daily WHERE day = $1")
        .bind(day("2026-10-19"))
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 3);
    // Signed-in users are counted by id, not by their cookie.
    let anon: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT anonymous_user_id FROM event_logs_daily ORDER BY 1")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(anon, vec!["".to_owned(), "visitor".to_owned()]);

    // Active users.
    assert_eq!(active(&db, "day", "2026-10-19").await, (2, 1, 1));
    assert_eq!(active(&db, "day", "2026-10-20").await, (1, 1, 0));
    assert_eq!(active(&db, "week", "2026-10-19").await, (3, 2, 1));
    assert_eq!(active(&db, "month", "2026-10-01").await, (3, 2, 1));
    assert_eq!(active(&db, "month", "2026-09-01").await, (1, 1, 0));
    assert_eq!(active(&db, "day", "2026-10-05").await, (0, 0, 0));

    // Searches by type and latency bucket (-1: unknown latency).
    let searches: Vec<(NaiveDate, String, i32, i32)> = sqlx::query_as(
        "SELECT day, search_type, latency_bucket_ms, count FROM event_logs_search_daily
         ORDER BY 1, 2",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        searches,
        vec![
            (day("2026-10-19"), "keyword".into(), 50, 1),
            (day("2026-10-19"), "symbol".into(), 500, 1),
            (day("2026-10-20"), "keyword".into(), 2500, 1),
            (day("2026-10-21"), "regexp".into(), -1, 1),
        ]
    );

    // Days that ended are complete; today is not.
    let complete: Vec<(NaiveDate, bool)> = sqlx::query_as(
        "SELECT day, complete FROM event_logs_rollup_days WHERE day >= '2026-10-20' ORDER BY 1",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        complete,
        vec![(day("2026-10-20"), true), (day("2026-10-21"), false)]
    );

    // A rerun only redoes today and changes nothing.
    let before = snapshot(&db).await;
    let again = rollup(&db, now, &Fence::none("test")).await.unwrap();
    assert_eq!(again.days, vec![day("2026-10-21")]);
    assert_eq!(snapshot(&db).await, before);
    // Recomputing everything from scratch gives the same rows.
    sqlx::query("DELETE FROM event_logs_rollup_days")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        rollup(&db, now, &Fence::none("test"))
            .await
            .unwrap()
            .days
            .len(),
        22
    );
    assert_eq!(snapshot(&db).await, before);

    // A later event of today is picked up by the next run.
    insert(
        &db,
        "2026-10-21T13:00:00Z",
        names::VIEW_BLOB,
        Some(bob),
        "",
        json!({}),
    )
    .await;
    rollup(&db, at("2026-10-21T13:30:00Z"), &Fence::none("test"))
        .await
        .unwrap();
    assert_eq!(active(&db, "day", "2026-10-21").await, (2, 2, 0));

    // Queries over the rollups.
    db.users().soft_delete(bob).await.unwrap();
    let today = day("2026-10-21");
    let week = Window::new(DateRange::LastWeek, Grouping::Daily, today);
    let s = query::events(&db, &EventFilter::names(&[search]), &week)
        .await
        .unwrap();
    assert_eq!(s.nodes.len(), 8);
    assert_eq!(s.summary.total_count, 4);
    assert_eq!(
        s.summary.total_unique_users, 3,
        "alice, bob and the visitor"
    );
    assert_eq!(s.summary.total_registered_users, 1, "bob was deleted");
    let n19 = s
        .nodes
        .iter()
        .find(|n| n.date == day("2026-10-19"))
        .unwrap();
    assert_eq!(
        (n19.count, n19.unique_users, n19.registered_users),
        (2, 2, 1)
    );
    let weekly = Window::new(DateRange::LastWeek, Grouping::Weekly, today);
    let s = query::events(&db, &EventFilter::all(), &weekly)
        .await
        .unwrap();
    assert_eq!(
        s.nodes.iter().map(|n| n.date).collect::<Vec<_>>(),
        vec![day("2026-10-12"), day("2026-10-19")]
    );
    assert_eq!(s.nodes[1].count, 8);
    let web = EventFilter::names(&names::codeintel_events()).only_source("WEB");
    assert_eq!(
        query::events(&db, &web, &week)
            .await
            .unwrap()
            .summary
            .total_count,
        2
    );
    let backend = EventFilter::names(&names::codeintel_events()).only_source("BACKEND");
    assert_eq!(
        query::events(&db, &backend, &week)
            .await
            .unwrap()
            .summary
            .total_count,
        0
    );
    let precise = EventFilter::names(&names::codeintel_events_by_precision(true));
    assert_eq!(
        query::events(&db, &precise, &week)
            .await
            .unwrap()
            .summary
            .total_count,
        1
    );

    let by_type = query::searches_by_type(&db, &week).await.unwrap();
    assert_eq!(by_type[0], ("keyword".to_owned(), 2));
    let latency = query::search_latency(&db, &week).await.unwrap();
    assert_eq!(names::latency_quantile(&latency, 0.5), Some(500.0));

    let top = query::codeintel_top_repositories(&db, &week, 5)
        .await
        .unwrap();
    assert_eq!(top.len(), 2);
    assert!(
        top.iter()
            .all(|r| r.repo_name == "r1" && r.language == "Go")
    );
    assert!(top.iter().any(|r| r.kind == "hover" && r.precise));
    assert!(top.iter().any(|r| r.kind == "references" && !r.precise));
    let langs = query::codeintel_by_language(&db, &week).await.unwrap();
    assert_eq!(langs.len(), 2);

    let freq = query::frequencies(&db, &week).await.unwrap();
    // alice: 19th and 21st; bob: 20th and 21st; the visitor: the 19th.
    assert_eq!(
        freq.iter()
            .map(|f| (f.days_used, f.users))
            .collect::<Vec<_>>(),
        vec![(1, 1), (2, 2)]
    );
    let daus = query::active_users(&db, Period::Day, 3, today)
        .await
        .unwrap();
    assert_eq!(
        daus.iter().map(|p| (p.start, p.users)).collect::<Vec<_>>(),
        vec![
            (day("2026-10-21"), 2),
            (day("2026-10-20"), 1),
            (day("2026-10-19"), 2)
        ]
    );
    let maus = query::active_users(&db, Period::Month, 2, today)
        .await
        .unwrap();
    assert_eq!(maus[1].start, day("2026-09-01"));
    assert_eq!(maus[1].users, 1);

    // Users with activity, most events first.
    let opts = UserListOptions {
        order: UserOrder::EventsCount,
        descending: true,
        limit: 10,
        ..Default::default()
    };
    let users = query::list_users(&db, &opts).await.unwrap();
    assert_eq!(
        users
            .iter()
            .map(|u| (u.username.as_str(), u.events_count))
            .collect::<Vec<_>>(),
        vec![("alice", 5), ("bob", 3)]
    );
    assert_eq!(users[0].last_active_at, Some(at("2026-10-21T08:00:01Z")));
    assert!(users[1].deleted_at.is_some());
    let live = UserListOptions {
        deleted_at: Some(query::TimeRange {
            empty: Some(true),
            ..Default::default()
        }),
        query: Some("ALI".into()),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(query::count_users(&db, &live).await.unwrap(), 1);
    let recent = UserListOptions {
        last_active_at: Some(query::TimeRange {
            gte: Some(at("2026-10-21T10:00:00Z")),
            ..Default::default()
        }),
        limit: 10,
        ..Default::default()
    };
    let r = query::list_users(&db, &recent).await.unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].username, "bob");
    let not_recent = UserListOptions {
        last_active_at: Some(query::TimeRange {
            gte: Some(at("2026-10-21T10:00:00Z")),
            not: true,
            ..Default::default()
        }),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(query::count_users(&db, &not_recent).await.unwrap(), 1);

    // Retention: raw events older than a day go (the rollups stay); a day not rolled up
    // completely is never pruned.
    let now = at("2026-10-21T14:00:00Z");
    let before = snapshot(&db).await;
    let pruned = prune(
        &db,
        now,
        Duration::from_secs(86_400),
        Duration::from_secs(400 * 86_400),
    )
    .await
    .unwrap();
    assert_eq!(pruned.events, 4, "the 30th and the 19th");
    assert_eq!(pruned.rollups, 0);
    assert_eq!(snapshot(&db).await, before);
    rollup(&db, now, &Fence::none("test")).await.unwrap();
    assert_eq!(
        snapshot(&db).await,
        before,
        "complete days are not recomputed"
    );

    // Without a complete rollup of a day, its raw events stay whatever the retention.
    sqlx::query("UPDATE event_logs_rollup_days SET complete = false WHERE day = '2026-10-20'")
        .execute(db.pool())
        .await
        .unwrap();
    let pruned = prune(&db, now, Duration::ZERO, Duration::from_secs(400 * 86_400))
        .await
        .unwrap();
    assert_eq!(pruned.events, 0);

    // Rollups past their retention go; periods that have not ended stay.
    let pruned = prune(&db, now, Duration::ZERO, Duration::from_secs(10 * 86_400))
        .await
        .unwrap();
    assert!(pruned.rollups > 0);
    let months: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT period_start FROM event_logs_active_users WHERE period = 'month'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(months, vec![day("2026-10-01")]);
    let oldest: Option<NaiveDate> = sqlx::query_scalar("SELECT min(day) FROM event_logs_daily")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(oldest, Some(day("2026-10-19")));
}
