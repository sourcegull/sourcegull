//! Whose permissions a watch runs with, and who its email actions reach, against the test
//! Postgres (`SOURCEGULL_TEST_DATABASE_URL`).

use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_monitors::store::{MonitorStore, MonitorUpdate, NewMonitor};
use sourcegull_monitors::trigger::owner_actor;
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, UserId};

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

async fn runs_as(db: &Db, store: &MonitorStore, id: i64) -> Option<String> {
    let m = store.get(id).await.unwrap().unwrap();
    owner_actor(db, &m).await.unwrap().map(|a| a.username)
}

#[tokio::test]
async fn org_watches_run_as_their_last_editor() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let store = MonitorStore::new(&db);
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    for u in [alice, bob] {
        db.orgs().add_member(org, u, OrgRole::Member).await.unwrap();
    }

    let m = store
        .create(&NewMonitor {
            namespace: Namespace::Org(org),
            description: "deploys".into(),
            enabled: true,
            query: "type:diff deploy".into(),
            actions: Vec::new(),
            created_by: alice,
            unfiltered: false,
        })
        .await
        .unwrap();
    assert_eq!(runs_as(&db, &store, m.id).await.as_deref(), Some("alice"));

    // Bob edits the query: the watch now searches with Bob's permissions, not Alice's.
    store
        .update(
            m.id,
            &MonitorUpdate {
                namespace: Namespace::Org(org),
                description: "deploys".into(),
                enabled: true,
                query: "type:diff secret".into(),
                actions: Vec::new(),
                updated_by: bob,
                unfiltered: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(runs_as(&db, &store, m.id).await.as_deref(), Some("bob"));

    // An editor who left the org runs nothing.
    db.orgs().remove_member(org, bob).await.unwrap();
    assert_eq!(runs_as(&db, &store, m.id).await, None);
}

#[tokio::test]
async fn deactivated_users_get_no_watch_emails() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let store = MonitorStore::new(&db);
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    for (u, email) in [(alice, "alice@example.com"), (bob, "bob@example.com")] {
        db.orgs().add_member(org, u, OrgRole::Member).await.unwrap();
        db.user_emails().add(u, email).await.unwrap();
        db.user_emails().verify(u, email).await.unwrap();
    }
    let mut emails = store.recipient_emails(Namespace::Org(org)).await.unwrap();
    emails.sort();
    assert_eq!(emails, ["alice@example.com", "bob@example.com"]);

    db.users().soft_delete(bob).await.unwrap();
    assert_eq!(
        store.recipient_emails(Namespace::Org(org)).await.unwrap(),
        ["alice@example.com"]
    );
    assert!(
        store
            .recipient_emails(Namespace::User(bob))
            .await
            .unwrap()
            .is_empty()
    );
}
