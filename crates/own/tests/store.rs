//! The ownership store and owner resolution against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): teams and members, ingested CODEOWNERS files, assignments
//! inherited downward, resolution of GitLab handles, emails and groups, and the ranked
//! owner list.

use sourcegull_db::{Db, DbError, NewUser, migrate};
use sourcegull_own::codeowners::CompiledCodeowners;
use sourcegull_own::ownership::{self, CodeownersOrigin, OwnershipQuery, Reason};
use sourcegull_own::resolve::{OwnerResolver, ResolvedOwner};
use sourcegull_own::store::{Assignee, ListTeams, NewTeam, OwnStore};
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

#[tokio::test]
async fn teams_members_and_names() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = OwnStore::new(&db);
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let parent = store
        .create_team(&NewTeam {
            name: "acme".into(),
            display_name: Some("ACME".into()),
            creator_id: Some(alice),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(store.is_member(parent.id, alice).await.unwrap());
    let child = store
        .create_team(&NewTeam {
            name: "acme/backend".into(),
            parent_team_id: Some(parent.id),
            ..Default::default()
        })
        .await
        .unwrap();
    // Names are unique among teams, users and orgs, case-insensitively.
    for name in ["ACME", "Bob"] {
        let err = store
            .create_team(&NewTeam {
                name: name.into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, DbError::AlreadyExists { .. }),
            "{name}: {err}"
        );
    }
    assert!(matches!(
        store
            .create_team(&NewTeam {
                name: "bad name".into(),
                ..Default::default()
            })
            .await,
        Err(DbError::Invalid(_))
    ));
    // A team cannot move under its own descendant.
    assert!(matches!(
        store
            .update_team(parent.id, None, Some(Some(child.id)))
            .await,
        Err(DbError::Invalid(_))
    ));
    let updated = store
        .update_team(child.id, Some(Some("Backend".into())), Some(None))
        .await
        .unwrap();
    assert_eq!(updated.display_name.as_deref(), Some("Backend"));
    assert_eq!(updated.parent_team_id, None);

    store.add_members(child.id, &[alice, bob]).await.unwrap();
    store.add_members(child.id, &[bob]).await.unwrap();
    assert_eq!(store.count_members(child.id, None).await.unwrap(), 2);
    assert_eq!(
        store
            .list_members(child.id, Some("bo"), 10, 0)
            .await
            .unwrap(),
        vec![bob]
    );
    store.set_members(child.id, &[bob]).await.unwrap();
    assert_eq!(
        store.list_members(child.id, None, 10, 0).await.unwrap(),
        vec![bob]
    );
    store.remove_members(child.id, &[bob]).await.unwrap();
    assert_eq!(store.count_members(child.id, None).await.unwrap(), 0);

    let roots = ListTeams {
        roots_only: true,
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.count_teams(&roots).await.unwrap(), 2);
    let except = ListTeams {
        except_ancestor: Some(parent.id),
        limit: 10,
        ..Default::default()
    };
    let names: Vec<_> = store
        .list_teams(&except)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["acme/backend"]);
    let mine = ListTeams {
        member: Some(alice),
        search: Some("ac".into()),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.list_teams(&mine).await.unwrap().len(), 1);
    store.delete_team(parent.id).await.unwrap();
    assert!(store.get_team(parent.id).await.unwrap().is_none());
    assert!(
        store
            .delete_team(parent.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn ingested_files_and_assignments() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = OwnStore::new(&db);
    let r = repo(&db, "gitlab.example.com/acme/web").await;
    let other = repo(&db, "gitlab.example.com/acme/api").await;
    let f = store.add_codeowners(r, "* @alice").await.unwrap();
    assert!(matches!(
        store.add_codeowners(r, "* @bob").await,
        Err(DbError::AlreadyExists { .. })
    ));
    assert!(
        store
            .update_codeowners(other, "x")
            .await
            .unwrap_err()
            .is_not_found()
    );
    let f2 = store.update_codeowners(r, "* @bob").await.unwrap();
    assert_eq!((f2.id, f2.contents.as_str()), (f.id, "* @bob"));
    assert_eq!(store.count_codeowners().await.unwrap(), 1);
    assert_eq!(
        store.list_codeowners(10, Some(f.id)).await.unwrap().len(),
        0
    );
    assert_eq!(
        store
            .codeowners_for_repos(&[r.get(), other.get()])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.delete_codeowners(&[r, other]).await.unwrap(), 1);
    assert!(store.codeowners_for_repo(r).await.unwrap().is_none());

    let alice = user(&db, "alice").await;
    let team = store
        .create_team(&NewTeam {
            name: "docs-team".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .assign(Assignee::User(alice), r, "/src/", Some(alice))
        .await
        .unwrap();
    store
        .assign(Assignee::User(alice), r, "src", None)
        .await
        .unwrap();
    store
        .assign(Assignee::Team(team.id), r, "", None)
        .await
        .unwrap();
    let on = |p: &'static str| {
        let store = store.clone();
        async move {
            store
                .assignments_for_path(r, p)
                .await
                .unwrap()
                .into_iter()
                .map(|a| (a.assignee, a.file_path))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        on("src/lib/a.rs").await,
        vec![
            (Assignee::User(alice), "src".to_owned()),
            (Assignee::Team(team.id), String::new())
        ]
    );
    assert_eq!(
        on("srcx/a.rs").await,
        vec![(Assignee::Team(team.id), String::new())]
    );
    let mut entries = store.assigned_entries(&[r.get()]).await.unwrap();
    entries.sort_by(|a, b| a.1.owner.cmp(&b.1.owner));
    let owners: Vec<_> = entries.iter().map(|(_, e)| e.owner.as_str()).collect();
    assert_eq!(owners, vec!["@alice", "@docs-team"]);
    store
        .remove_assignment(Assignee::User(alice), r, "src/")
        .await
        .unwrap();
    assert!(
        store
            .remove_assignment(Assignee::User(alice), r, "src")
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn resolution_and_ranked_ownership() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = OwnStore::new(&db);
    let resolver = OwnerResolver::new(&db);
    let alice = user(&db, "alice").await;
    let carol = user(&db, "carol").await;
    // alice is `ali-gl` on GitLab and has a verified address; an unverified one resolves
    // nobody.
    sqlx::query(
        "INSERT INTO user_external_accounts (user_id, service_type, service_id, account_id, \
         account_data) VALUES ($1, 'gitlab', 'https://gitlab.example.com/', '7', \
         '{\"username\": \"Ali-GL\"}')",
    )
    .bind(alice.get())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_emails (user_id, email, verified_at) VALUES \
         ($1, 'Alice@Example.com', now()), ($2, 'carol@example.com', NULL)",
    )
    .bind(alice.get())
    .bind(carol.get())
    .execute(db.pool())
    .await
    .unwrap();
    let team = store
        .create_team(&NewTeam {
            name: "acme/backend".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let texts: Vec<String> = [
        "@ali-gl",
        "alice@example.com",
        "@Carol",
        "carol@example.com",
        "@acme/backend",
        "@acme/frontend",
        "@nobody",
        "@@maintainer",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let r = resolver.resolve(&texts).await.unwrap();
    let user_of = |k: &str| match &r[k] {
        ResolvedOwner::User(u) => Some(u.id),
        _ => None,
    };
    assert_eq!(user_of("ali-gl"), Some(alice));
    assert_eq!(user_of("alice@example.com"), Some(alice));
    assert_eq!(user_of("carol"), Some(carol));
    assert_eq!(user_of("carol@example.com"), None);
    assert!(matches!(&r["acme/backend"], ResolvedOwner::Team(t) if t.id == team.id));
    assert_eq!(
        r["acme/frontend"],
        ResolvedOwner::ExternalTeam {
            handle: "acme/frontend".into()
        }
    );
    assert_eq!(
        r["nobody"],
        ResolvedOwner::Person {
            handle: Some("nobody".into()),
            email: None,
            name: None,
        }
    );
    assert!(matches!(r["@maintainer"], ResolvedOwner::Person { .. }));

    let keys = resolver.term_keys("@alice").await.unwrap();
    for k in ["alice", "ali-gl", "alice@example.com"] {
        assert!(keys.contains(k), "{k}: {keys:?}");
    }
    let keys = resolver.term_keys("Alice@example.com").await.unwrap();
    assert!(keys.contains("ali-gl"));
    let keys = resolver.term_keys("@acme/backend").await.unwrap();
    assert_eq!(keys.len(), 1);
    assert!(
        resolver
            .term_keys("@ghost")
            .await
            .unwrap()
            .contains("ghost")
    );

    // The ranked list: alice through two CODEOWNERS references and an assignment is listed
    // once; a direct assignment ranks first.
    let rp = repo(&db, "gitlab.example.com/acme/web").await;
    store
        .assign(Assignee::User(alice), rp, "src", None)
        .await
        .unwrap();
    store
        .assign(Assignee::Team(team.id), rp, "src/main.go", None)
        .await
        .unwrap();
    let c = CompiledCodeowners::compile(
        "* @nobody\n[Go]\n*.go @ali-gl alice@example.com @acme/frontend\n",
    );
    let list = ownership::compute(
        &store,
        &resolver,
        OwnershipQuery {
            repo: rp,
            path: "src/main.go",
            is_dir: false,
            codeowners: Some((
                &c,
                CodeownersOrigin::Repo {
                    path: ".gitlab/CODEOWNERS".into(),
                },
            )),
            contributors: true,
            viewers: true,
        },
    )
    .await
    .unwrap();
    let names: Vec<_> = list.iter().map(|o| o.owner.display()).collect();
    assert_eq!(
        names,
        vec!["acme/backend", "nobody", "alice", "acme/frontend"]
    );
    let alice_reasons = &list[2].reasons;
    assert_eq!(alice_reasons.len(), 2, "{alice_reasons:?}");
    assert!(matches!(
        alice_reasons[0],
        Reason::CodeownersFileEntry { line: 3, .. }
    ));
    assert!(matches!(
        alice_reasons[1],
        Reason::AssignedOwner { direct: false, .. }
    ));
    // The directory: CODEOWNERS `*` and alice's assignment on it.
    let dir = ownership::compute(
        &store,
        &resolver,
        OwnershipQuery {
            repo: rp,
            path: "src",
            is_dir: true,
            codeowners: Some((&c, CodeownersOrigin::Ingested)),
            contributors: true,
            viewers: true,
        },
    )
    .await
    .unwrap();
    let names: Vec<_> = dir.iter().map(|o| o.owner.display()).collect();
    assert_eq!(names, vec!["alice", "nobody"]);
}
