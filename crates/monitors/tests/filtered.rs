//! Email results filtered per recipient, against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): each member of a recipient org gets only the results in
//! repositories they can read, and none when none are left; an unfiltered watch sends every
//! result to everyone, only while the user it runs as may make it unfiltered.

use std::sync::Arc;

use chrono::Utc;
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_db::{Db, Namespace, NewUser, PermsSource, migrate};
use sourcegull_monitors::actions::ActionHandler;
use sourcegull_monitors::store::{
    ActionEdit, ActionJob, ActionKind, ActionSpec, MonitorStore, MonitorUpdate, NewMonitor,
    Priority,
};
use sourcegull_monitors::{ActionSender, MonitorResult};
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, RepoId, UserId};

async fn user(db: &Db, name: &str) -> UserId {
    let id = db
        .users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let email = format!("{name}@example.com");
    db.user_emails().add(id, &email).await.unwrap();
    db.user_emails().verify(id, &email).await.unwrap();
    id
}

async fn repo(db: &Db, name: &str, private: bool) -> i32 {
    sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
         VALUES ($1, $2, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .bind(private)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

fn result(repo_id: i32, repo: &str) -> MonitorResult {
    MonitorResult {
        kind: "commit".into(),
        repository: repo.into(),
        repository_id: repo_id,
        commit: format!("{repo_id:0>7}"),
        url: format!("/{repo}/-/commit/{repo_id}"),
        subject: format!("change in {repo}"),
        message: String::new(),
        author_name: "Alice".into(),
        author_date: "2026-10-26T00:00:00Z".into(),
        diff: String::new(),
    }
}

/// What each address got: `(to, text)`, sorted by address.
fn inbox(email: &MemoryEmailSender) -> Vec<(String, String)> {
    let mut got: Vec<_> = email
        .messages()
        .into_iter()
        .map(|m| (m.to, m.text))
        .collect();
    got.sort();
    got
}

#[tokio::test]
async fn org_emails_hold_what_each_member_can_read() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    for u in [alice, bob] {
        db.orgs().add_member(org, u, OrgRole::Member).await.unwrap();
    }
    let public = repo(&db, "acme/public", false).await;
    let secret = repo(&db, "acme/secret", true).await;
    db.perms()
        .set_repo_perms(RepoId(secret), PermsSource::Explicit, &[alice], None)
        .await
        .unwrap();

    let store = MonitorStore::new(&db);
    let email_spec = ActionSpec {
        kind: ActionKind::Email,
        enabled: true,
        include_results: true,
        url: String::new(),
        chat_id: String::new(),
        priority: Priority::Normal,
        header: String::new(),
        recipients: vec![Namespace::Org(org)],
    };
    let monitor = store
        .create(&NewMonitor {
            namespace: Namespace::Org(org),
            description: "deploys".into(),
            enabled: true,
            query: "type:commit deploy".into(),
            actions: vec![email_spec.clone()],
            created_by: alice,
            unfiltered: false,
        })
        .await
        .unwrap();
    let action = store.actions(monitor.id).await.unwrap()[0].id;
    let trigger_job = |results: Vec<MonitorResult>| {
        let store = store.clone();
        async move {
            let job = store
                .record_evaluation(
                    &[],
                    &[],
                    &[(monitor.id, "type:commit deploy".to_owned(), Vec::new())],
                    &[],
                    Utc::now(),
                    &sourcegull_db::Fence::none("test"),
                )
                .await
                .unwrap()[0];
            store.finish_trigger_job(job, &results, &[]).await.unwrap();
            job
        }
    };
    let deliver = |config: SiteConfig, job: i64| {
        let db = db.clone();
        async move {
            let email = MemoryEmailSender::default();
            let sender = ActionSender::new(
                &db,
                SiteConfigHandle::new(1, config),
                Arc::new(email.clone()),
            );
            ActionHandler::new(sender)
                .deliver(
                    0,
                    &ActionJob {
                        action_id: action,
                        trigger_job_id: job,
                    },
                )
                .await
                .unwrap();
            inbox(&email)
        }
    };

    // Alice gets both results, Bob only the public one; both are told why.
    let both = trigger_job(vec![
        result(public, "acme/public"),
        result(secret, "acme/secret"),
    ])
    .await;
    let got = deliver(SiteConfig::default(), both).await;
    assert_eq!(got.len(), 2);
    let (alice_mail, bob_mail) = (&got[0].1, &got[1].1);
    assert_eq!(got[0].0, "alice@example.com");
    assert!(alice_mail.contains("acme/secret") && alice_mail.contains("acme/public"));
    assert!(alice_mail.contains("found 2 new results"), "{alice_mail}");
    assert!(bob_mail.contains("acme/public"));
    assert!(!bob_mail.contains("acme/secret"), "{bob_mail}");
    assert!(bob_mail.contains("found 1 new result"), "{bob_mail}");
    assert!(bob_mail.contains("Results are limited to repositories you can read."));

    // Nothing Bob can read: no email for Bob.
    let secret_only = trigger_job(vec![result(secret, "acme/secret")]).await;
    let got = deliver(SiteConfig::default(), secret_only).await;
    assert_eq!(
        got.iter().map(|(to, _)| to.as_str()).collect::<Vec<_>>(),
        ["alice@example.com"]
    );

    // Alice makes it unfiltered. Without the setting she may not, so it stays filtered.
    let update = |by: UserId, unfiltered: bool| MonitorUpdate {
        namespace: Namespace::Org(org),
        description: "deploys".into(),
        enabled: true,
        query: "type:commit deploy".into(),
        actions: vec![ActionEdit {
            id: Some(action),
            spec: email_spec.clone(),
        }],
        updated_by: by,
        unfiltered,
    };
    let m = store
        .update(monitor.id, &update(alice, true))
        .await
        .unwrap();
    assert_eq!(m.unfiltered_by, Some(alice));
    assert!(m.unfiltered_at.is_some());
    assert_eq!(deliver(SiteConfig::default(), secret_only).await.len(), 1);

    // With the setting, Bob gets Alice's results, with the warning.
    let allowed = SiteConfig {
        watches_allow_unfiltered_results: true,
        ..SiteConfig::default()
    };
    let got = deliver(allowed.clone(), secret_only).await;
    assert_eq!(got.len(), 2);
    let bob_mail = &got[1].1;
    assert!(bob_mail.contains("acme/secret"));
    assert!(bob_mail.contains(
        "Unfiltered: every recipient gets all results this watch finds with alice's access"
    ));

    // Saving again keeps when it was turned on; Bob taking the watch over clears it.
    let again = store
        .update(monitor.id, &update(alice, true))
        .await
        .unwrap();
    assert_eq!(again.unfiltered_at, m.unfiltered_at);
    let m = store.set_enabled(monitor.id, true, bob).await.unwrap();
    assert_eq!(m.unfiltered_by, None);
    assert_eq!(deliver(allowed, secret_only).await.len(), 1);
}
