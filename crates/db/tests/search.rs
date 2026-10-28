//! Search context and saved search store tests against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`). See `db.rs` for why tests use a multi-threaded runtime.

use sourcegull_db::{
    ContextAccess, ContextOrder, Db, DbError, ListContextsOptions, ListSavedSearchesOptions,
    Namespace, NewUser, SavedSearchOrder, SavedSearchVisibility, SearchContextInput, migrate,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, RepoId, UserId};

async fn setup() -> Option<(TestDb, Db)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    Some((t, db))
}

async fn user(db: &Db, name: &str, site_admin: bool) -> UserId {
    let id = db
        .users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .expect("create user")
        .id;
    if site_admin {
        db.users().set_site_admin(id, true).await.unwrap();
    }
    id
}

async fn repo(db: &Db, name: &str) -> RepoId {
    sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

fn input(name: &str, public: bool) -> SearchContextInput {
    SearchContextInput {
        name: name.into(),
        description: format!("{name} repos"),
        public,
        ..Default::default()
    }
}

fn specs(list: &[sourcegull_db::SearchContext]) -> Vec<String> {
    list.iter().map(|c| c.spec()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_contexts() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice", false).await;
    let bob = user(&db, "bob", false).await;
    let admin = user(&db, "admin", true).await;
    let org = db.orgs().create("acme", None).await.unwrap();
    db.orgs()
        .add_member(org.id, bob, OrgRole::Member)
        .await
        .unwrap();
    let (api, web) = (
        repo(&db, "gitlab.com/acme/api").await,
        repo(&db, "gitlab.com/acme/web").await,
    );
    let store = db.search_contexts();

    let mut listed = input("core", false);
    listed.repos = vec![(api, vec![]), (web, vec!["main".into(), "v1".into()])];
    let alice_core = store
        .create(Some(Namespace::User(alice)), &listed)
        .await
        .unwrap();
    assert_eq!(alice_core.spec(), "@alice/core");
    assert_eq!(alice_core.namespace_name.as_deref(), Some("alice"));
    let repos = store.repos(alice_core.id).await.unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].repo_name, "gitlab.com/acme/api");
    assert_eq!(repos[0].revisions, [""]);
    assert_eq!(repos[1].revisions, ["main", "v1"]);

    // Names are unique per owner, case-insensitively.
    assert!(matches!(
        store
            .create(Some(Namespace::User(alice)), &input("CORE", true))
            .await,
        Err(DbError::AlreadyExists { .. })
    ));
    store
        .create(Some(Namespace::User(bob)), &input("core", true))
        .await
        .unwrap();
    let mut q = input("frontend", false);
    q.query = Some("repo:web".into());
    q.repos = vec![(api, vec![])];
    let acme_frontend = store
        .create(Some(Namespace::Org(org.id)), &q)
        .await
        .unwrap();
    assert!(store.repos(acme_frontend.id).await.unwrap().is_empty());
    let instance_private = store.create(None, &input("secret", false)).await.unwrap();
    store
        .create(None, &input("everything", true))
        .await
        .unwrap();

    // Read access: public, the owner, org members, site admins for
    // instance-level contexts only.
    let visible = |viewer: Option<UserId>| {
        let store = store.clone();
        async move {
            let (list, total) = store
                .list(&ListContextsOptions {
                    access: ContextAccess::Viewer(viewer),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(total as usize, list.len());
            specs(&list)
        }
    };
    assert_eq!(visible(None).await, ["everything", "@bob/core"]);
    assert_eq!(
        visible(Some(alice)).await,
        ["everything", "@alice/core", "@bob/core"]
    );
    assert_eq!(
        visible(Some(bob)).await,
        ["everything", "@acme/frontend", "@bob/core"]
    );
    assert_eq!(
        visible(Some(admin)).await,
        ["everything", "secret", "@bob/core"]
    );
    let (all, _) = store
        .list(&ListContextsOptions {
            access: ContextAccess::All,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 5);

    // By spec, case-insensitively, with the same access rules.
    let get = |spec: &'static str, viewer: Option<UserId>| {
        let store = store.clone();
        async move {
            store
                .get_by_spec(spec, ContextAccess::Viewer(viewer))
                .await
                .unwrap()
                .map(|c| c.spec())
        }
    };
    assert_eq!(
        get("@Alice/Core", Some(alice)).await.as_deref(),
        Some("@alice/core")
    );
    assert_eq!(get("@alice/core", Some(bob)).await, None);
    assert_eq!(
        get("@acme/frontend", Some(bob)).await.as_deref(),
        Some("@acme/frontend")
    );
    assert_eq!(get("@acme/frontend", Some(admin)).await, None);
    assert_eq!(get("secret", Some(admin)).await.as_deref(), Some("secret"));
    assert_eq!(get("secret", Some(alice)).await, None);
    assert_eq!(get("core", Some(alice)).await, None, "not instance-level");
    assert_eq!(get("@alice", Some(alice)).await, None);
    assert!(
        store
            .get(instance_private.id, ContextAccess::Viewer(Some(bob)))
            .await
            .unwrap()
            .is_none()
    );

    // Filters and order.
    let (list, total) = store
        .list(&ListContextsOptions {
            access: ContextAccess::All,
            query: Some("@a".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (specs(&list), total),
        (vec!["@acme/frontend".into(), "@alice/core".into()], 2)
    );
    let (list, _) = store
        .list(&ListContextsOptions {
            access: ContextAccess::All,
            namespaces: Some(vec![None, Some(Namespace::Org(org.id))]),
            order: ContextOrder::Spec,
            descending: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(specs(&list), ["@acme/frontend", "secret", "everything"]);
    let (page, total) = store
        .list(&ListContextsOptions {
            access: ContextAccess::All,
            limit: 2,
            offset: 3,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!((page.len(), total), (2, 5));

    // Stars and the default come first and are flagged for the viewer.
    store
        .set_starred(acme_frontend.id, bob, true)
        .await
        .unwrap();
    store
        .set_starred(acme_frontend.id, bob, true)
        .await
        .unwrap();
    let bob_core = store
        .get_by_spec("@bob/core", ContextAccess::Viewer(Some(bob)))
        .await
        .unwrap()
        .unwrap();
    store.set_default(bob, Some(bob_core.id)).await.unwrap();
    let (list, _) = store
        .list(&ListContextsOptions {
            access: ContextAccess::Viewer(Some(bob)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(specs(&list), ["@bob/core", "@acme/frontend", "everything"]);
    assert!(list[0].viewer_default && !list[0].viewer_starred);
    assert!(list[1].viewer_starred && !list[1].viewer_default);
    assert_eq!(
        store.default_for(bob).await.unwrap().map(|c| c.id),
        Some(bob_core.id)
    );
    assert!(store.default_for(alice).await.unwrap().is_none());
    // Another viewer sees no flags.
    let c = store
        .get(acme_frontend.id, ContextAccess::All)
        .await
        .unwrap()
        .unwrap();
    assert!(!c.viewer_starred);
    store
        .set_starred(acme_frontend.id, bob, false)
        .await
        .unwrap();
    store.set_default(bob, None).await.unwrap();
    assert!(store.default_for(bob).await.unwrap().is_none());

    // A default the viewer can no longer read is not returned.
    store
        .set_default(alice, Some(acme_frontend.id))
        .await
        .unwrap();
    assert!(store.default_for(alice).await.unwrap().is_none());

    // Update replaces the contents; delete cascades.
    let mut changed = input("core2", true);
    changed.repos = vec![(web, vec![])];
    let updated = store.update(alice_core.id, &changed).await.unwrap();
    assert_eq!((updated.name.as_str(), updated.public), ("core2", true));
    assert!(updated.updated_at >= alice_core.updated_at);
    let repos = store.repos(alice_core.id).await.unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].repo_id, web);
    store.delete(alice_core.id).await.unwrap();
    assert!(
        store
            .delete(alice_core.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        store
            .update(alice_core.id, &changed)
            .await
            .unwrap_err()
            .is_not_found()
    );

    // Contexts of deleted users disappear.
    db.users().soft_delete(bob).await.unwrap();
    assert_eq!(visible(None).await, ["everything"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_searches() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let alice = user(&db, "alice", false).await;
    let bob = user(&db, "bob", false).await;
    let org = db.orgs().create("acme", None).await.unwrap();
    db.orgs()
        .add_member(org.id, bob, OrgRole::Member)
        .await
        .unwrap();
    let store = db.saved_searches();

    let a = store
        .create(
            Namespace::User(alice),
            "TODOs",
            "TODO patterntype:keyword",
            false,
            alice,
        )
        .await
        .unwrap();
    assert_eq!(a.owner, Namespace::User(alice));
    assert_eq!(a.visibility, SavedSearchVisibility::Secret);
    assert_eq!((a.created_by, a.updated_by), (Some(alice), Some(alice)));
    let b = store
        .create(
            Namespace::Org(org.id),
            "Panics",
            "panic( patterntype:keyword",
            true,
            bob,
        )
        .await
        .unwrap();
    let c = store
        .create(
            Namespace::User(bob),
            "Unsafe",
            "unsafe patterntype:keyword",
            false,
            bob,
        )
        .await
        .unwrap();
    store
        .set_visibility(c.id, SavedSearchVisibility::Public, bob)
        .await
        .unwrap();

    let list = |opts: ListSavedSearchesOptions| {
        let store = store.clone();
        async move {
            let (list, total) = store
                .list(&ListSavedSearchesOptions { limit: 50, ..opts })
                .await
                .unwrap();
            assert_eq!(total as usize, list.len());
            list.into_iter().map(|s| s.description).collect::<Vec<_>>()
        }
    };
    let all = ListSavedSearchesOptions {
        include_drafts: true,
        ..Default::default()
    };
    // Newest first by default.
    assert_eq!(list(all.clone()).await, ["Unsafe", "Panics", "TODOs"]);
    assert_eq!(
        list(ListSavedSearchesOptions {
            order: SavedSearchOrder::Description,
            ..all.clone()
        })
        .await,
        ["Panics", "TODOs", "Unsafe"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            include_drafts: false,
            ..Default::default()
        })
        .await,
        ["Unsafe", "TODOs"]
    );
    // Affiliated: own, own orgs', and public ones.
    assert_eq!(
        list(ListSavedSearchesOptions {
            affiliated_user: Some(alice),
            ..all.clone()
        })
        .await,
        ["Unsafe", "TODOs"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            affiliated_user: Some(bob),
            ..all.clone()
        })
        .await,
        ["Unsafe", "Panics"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            public_only: true,
            ..all.clone()
        })
        .await,
        ["Unsafe"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            owner: Some(Namespace::Org(org.id)),
            ..all.clone()
        })
        .await,
        ["Panics"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            query: Some("todo".into()),
            ..all.clone()
        })
        .await,
        ["TODOs"]
    );
    assert_eq!(
        list(ListSavedSearchesOptions {
            query: Some("50%".into()),
            ..all.clone()
        })
        .await,
        Vec::<String>::new()
    );

    // Update, transfer, delete.
    let updated = store
        .update(b.id, "Panics!", "panic patterntype:keyword", false, alice)
        .await
        .unwrap();
    assert_eq!(
        (
            updated.description.as_str(),
            updated.draft,
            updated.updated_by
        ),
        ("Panics!", false, Some(alice))
    );
    assert_eq!(updated.created_by, Some(bob));
    let moved = store
        .transfer(b.id, Namespace::User(alice), alice)
        .await
        .unwrap();
    assert_eq!(moved.owner, Namespace::User(alice));
    assert_eq!(
        store.get(b.id).await.unwrap().unwrap().owner,
        Namespace::User(alice)
    );
    store.delete(b.id).await.unwrap();
    assert!(store.get(b.id).await.unwrap().is_none());
    assert!(store.delete(b.id).await.unwrap_err().is_not_found());
    assert!(
        store
            .update(b.id, "x", "y", false, alice)
            .await
            .unwrap_err()
            .is_not_found()
    );

    // Deleting the owner deletes their saved searches; creators are kept as NULL.
    sourcegull_db::sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(alice)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(store.get(a.id).await.unwrap().is_none());
}
