//! Ownership signals and analytics against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): the recent contributions window synced incrementally equals
//! a full recompute; recent views counted from `ViewBlob` events and subtracted when they
//! leave the window; coverage stats; signal configurations; and signals in the ranked owner
//! list after the owners.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use sourcegull_db::{Db, DbError, NewUser, migrate};
use sourcegull_own::ownership::{self, OwnershipQuery, Reason};
use sourcegull_own::resolve::{OwnerResolver, ResolvedOwner};
use sourcegull_own::signals::{
    Aggregate, Contribution, PathStats, RECENT_CONTRIBUTORS, RECENT_VIEWS, SignalConfigUpdate,
    SignalStore,
};
use sourcegull_own::store::{Assignee, OwnStore};
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoId, UserId};

async fn setup() -> Option<(TestDb, Db)> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    Some((t, db))
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

async fn repo(db: &Db, name: &str) -> RepoId {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .fetch_one(db.pool())
    .await
    .unwrap();
    RepoId(id)
}

fn day(d: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).unwrap() + TimeDelta::days(d)
}

fn commit(i: u32, author: &str, paths: &[&str]) -> Contribution {
    Contribution {
        commit: format!("{i:040x}"),
        author_name: author.into(),
        author_email: format!("{}@example.com", author.to_lowercase()),
        authored_at: day(i64::from(i)),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    }
}

fn history() -> Vec<Contribution> {
    let authors = ["Ann", "Bob", "Cid"];
    let files = [
        "src/a.rs",
        "src/b.rs",
        "src/x/c.rs",
        "README.md",
        "docs/d.md",
    ];
    (0..40u32)
        .map(|i| {
            let n = 1 + (i as usize % 3);
            let paths: Vec<&str> = (0..n).map(|k| files[(i as usize * 2 + k) % 5]).collect();
            commit(i, authors[(i as usize * 5 + i as usize / 4) % 3], &paths)
        })
        .collect()
}

#[tokio::test]
async fn contributions_incremental_equals_full_recompute() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = SignalStore::new(&db);
    let a = repo(&db, "gitlab.example.com/acme/a").await;
    let b = repo(&db, "gitlab.example.com/acme/b").await;
    let h = history();
    // Windows sliding over the history, plus a rewritten history (a force push).
    let mut rewritten = h[30..40].to_vec();
    for c in &mut rewritten[5..] {
        c.commit = c.commit.replace('0', "f");
        c.author_name = "Dee".into();
    }
    let windows: Vec<Vec<Contribution>> = vec![
        h[0..10].to_vec(),
        h[5..20].to_vec(),
        h[5..20].to_vec(),
        h[18..35].to_vec(),
        rewritten,
        h[39..40].to_vec(),
        Vec::new(),
        h[0..40].to_vec(),
    ];
    for (i, w) in windows.iter().enumerate() {
        let stats = store.sync_contributions(a, w).await.unwrap();
        if i == 2 {
            assert_eq!((stats.added, stats.removed), (0, 0));
        }
        let incremental = store.contribution_aggregate(a).await.unwrap();
        // The same window from scratch, in another repository.
        store.clear_contributions(b).await.unwrap();
        store.sync_contributions(b, w).await.unwrap();
        let full = store.contribution_aggregate(b).await.unwrap();
        assert_eq!(incremental, full, "window {i}");
        assert_eq!(incremental, Aggregate::full(w), "window {i}");
    }

    // Ranked per path: most commits first.
    let top = store.recent_contributors(a, "src", 10).await.unwrap();
    assert!(!top.is_empty(), "{top:?}");
    assert!(top.windows(2).all(|w| w[0].count >= w[1].count));
    let total: i64 = store
        .recent_contributors(a, "", 10)
        .await
        .unwrap()
        .iter()
        .map(|c| c.count)
        .sum();
    assert_eq!(total, 40);
}

async fn view(db: &Db, user: Option<UserId>, repo: &str, path: &str, at: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO event_logs (name, user_id, argument, timestamp) \
         VALUES ('ViewBlob', $1, jsonb_build_object('repoName', $2::text, 'filePath', $3::text), $4)",
    )
    .bind(user.map(|u| u.get()))
    .bind(repo)
    .bind(path)
    .bind(at)
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn views_counted_and_expired() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = SignalStore::new(&db);
    let r = repo(&db, "gitlab.example.com/acme/web").await;
    let ann = user(&db, "ann").await;
    let bob = user(&db, "bob").await;
    let name = "gitlab.example.com/acme/web";
    view(&db, Some(ann), name, "a.go", day(0)).await;
    view(&db, Some(ann), name, "a.go", day(50)).await;
    view(&db, Some(bob), name, "a.go", day(60)).await;
    view(&db, None, name, "a.go", day(60)).await; // anonymous: not counted
    view(
        &db,
        Some(bob),
        "gitlab.example.com/acme/gone",
        "a.go",
        day(60),
    )
    .await;
    sqlx::query(
        "INSERT INTO event_logs (name, user_id, argument) VALUES ('SearchSubmitted', $1, '{}')",
    )
    .bind(ann.get())
    .execute(db.pool())
    .await
    .unwrap();

    store.index_views(day(70)).await.unwrap();
    assert_eq!(
        store.recent_viewers(r, "a.go", 10).await.unwrap(),
        vec![(ann, 2), (bob, 1)]
    );
    // Running again counts nothing twice.
    store.index_views(day(70)).await.unwrap();
    assert_eq!(store.recent_viewers(r, "a.go", 10).await.unwrap().len(), 2);
    // Day 0 leaves the window at day 91, day 50 at day 141.
    view(&db, Some(bob), name, "a.go", day(100)).await;
    store.index_views(day(95)).await.unwrap();
    assert_eq!(
        store.recent_viewers(r, "a.go", 10).await.unwrap(),
        vec![(bob, 2), (ann, 1)]
    );
    store.index_views(day(145)).await.unwrap();
    assert_eq!(
        store.recent_viewers(r, "a.go", 10).await.unwrap(),
        vec![(bob, 2)]
    );
    store.index_views(day(195)).await.unwrap();
    assert!(
        store
            .recent_viewers(r, "a.go", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn configurations_stats_and_ranked_signals() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = SignalStore::new(&db);
    let r = repo(&db, "gitlab.example.com/acme/web").await;
    let other = repo(&db, "gitlab.example.com/other/x").await;

    // Off by default; excluded patterns are globs over names.
    let configs = store.configs().await.unwrap();
    assert_eq!(configs.0.len(), 3);
    assert!(configs.0.iter().all(|c| !c.enabled));
    let configs = store
        .update_configs(&[SignalConfigUpdate {
            name: RECENT_CONTRIBUTORS.into(),
            enabled: true,
            excluded_repo_patterns: Some(vec!["gitlab.example.com/other/*".into()]),
        }])
        .await
        .unwrap();
    assert!(configs.applies(RECENT_CONTRIBUTORS, "gitlab.example.com/acme/web"));
    assert!(!configs.applies(RECENT_CONTRIBUTORS, "gitlab.example.com/other/x"));
    assert!(!configs.applies(RECENT_VIEWS, "gitlab.example.com/acme/web"));
    let bad = store
        .update_configs(&[SignalConfigUpdate {
            name: "nope".into(),
            enabled: true,
            excluded_repo_patterns: None,
        }])
        .await;
    assert!(matches!(bad, Err(DbError::Invalid(_))));
    let bad = store
        .update_configs(&[SignalConfigUpdate {
            name: RECENT_VIEWS.into(),
            enabled: true,
            excluded_repo_patterns: Some(vec!["a[".into()]),
        }])
        .await;
    assert!(matches!(bad, Err(DbError::Invalid(_))));

    // Coverage: summed over repositories' roots.
    let stats = |path: &str, total, codeowned, assigned, owned| PathStats {
        path: path.into(),
        total_files: total,
        codeowned_files: codeowned,
        assigned_files: assigned,
        owned_files: owned,
        updated_at: None,
    };
    store
        .save_path_stats(r, &[stats("", 10, 4, 2, 5), stats("src", 6, 4, 0, 4)])
        .await
        .unwrap();
    store
        .save_path_stats(other, &[stats("", 5, 0, 1, 1)])
        .await
        .unwrap();
    let all = store.instance_stats().await.unwrap();
    assert_eq!(
        (
            all.total_files,
            all.codeowned_files,
            all.assigned_files,
            all.owned_files
        ),
        (15, 4, 3, 6)
    );
    assert!(all.updated_at.is_some());
    assert_eq!(
        store
            .path_stats(r, "src")
            .await
            .unwrap()
            .unwrap()
            .owned_files,
        4
    );
    assert!(store.path_stats(r, "lib").await.unwrap().is_none());
    let by_repo = store.repo_stats(10, None).await.unwrap();
    assert_eq!(
        by_repo
            .iter()
            .map(|(_, n, _)| n.as_str())
            .collect::<Vec<_>>(),
        vec!["gitlab.example.com/acme/web", "gitlab.example.com/other/x"]
    );
    let page2 = store
        .repo_stats(10, Some("gitlab.example.com/acme/web"))
        .await
        .unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(store.count_repo_stats().await.unwrap(), 2);

    // Signals rank after owners: an assigned owner, then contributors by commits, then
    // viewers. A contributor with a verified email is the user.
    let ann = user(&db, "ann").await;
    let carol = user(&db, "carol").await;
    sqlx::query(
        "INSERT INTO user_emails (user_id, email, verified_at) VALUES ($1, 'ann@example.com', now())",
    )
    .bind(ann.get())
    .execute(db.pool())
    .await
    .unwrap();
    OwnStore::new(&db)
        .assign(Assignee::User(carol), r, "", None)
        .await
        .unwrap();
    store
        .sync_contributions(
            r,
            &[
                commit(1, "Ann", &["src/a.rs"]),
                commit(2, "Bob", &["src/a.rs"]),
                commit(3, "Bob", &["src/a.rs", "README.md"]),
            ],
        )
        .await
        .unwrap();
    view(
        &db,
        Some(carol),
        "gitlab.example.com/acme/web",
        "src/a.rs",
        day(3),
    )
    .await;
    view(
        &db,
        Some(ann),
        "gitlab.example.com/acme/web",
        "src/a.rs",
        day(3),
    )
    .await;
    store.index_views(day(4)).await.unwrap();
    let resolver = OwnerResolver::new(&db);
    let list = ownership::compute(
        &OwnStore::new(&db),
        &resolver,
        OwnershipQuery {
            repo: r,
            path: "src/a.rs",
            is_dir: false,
            codeowners: None,
            contributors: true,
            viewers: true,
        },
    )
    .await
    .unwrap();
    let got: Vec<(String, Vec<Reason>)> = list
        .iter()
        .map(|o| {
            let reasons = o
                .reasons
                .iter()
                .filter(|r| !matches!(r, Reason::AssignedOwner { .. }))
                .cloned()
                .collect();
            (o.owner.display(), reasons)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("carol".to_owned(), vec![Reason::RecentView { count: 1 }]),
            (
                "Bob".to_owned(),
                vec![Reason::RecentContributor { count: 2 }]
            ),
            (
                "ann".to_owned(),
                vec![
                    Reason::RecentContributor { count: 1 },
                    Reason::RecentView { count: 1 }
                ]
            ),
        ]
    );
    assert!(matches!(
        &list[1].owner,
        ResolvedOwner::Person { name: Some(n), email: Some(e), .. } if n == "Bob" && e == "bob@example.com"
    ));
    assert!(matches!(&list[2].owner, ResolvedOwner::User(u) if u.id == ann));
    // Directories have contributors but no views; signals can be left out.
    let dir = ownership::compute(
        &OwnStore::new(&db),
        &resolver,
        OwnershipQuery {
            repo: r,
            path: "",
            is_dir: true,
            codeowners: None,
            contributors: false,
            viewers: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(dir.len(), 1);
}
