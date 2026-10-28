//! The reconciler against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): creating, updating,
//! taking over, deleting and releasing declared objects, site admins and role members, invalid
//! files, exclusive kinds and the global settings file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sourcegull_config::refs::RefSource;
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    ContextAccess, Db, ExternalServiceUpdate, NewAccount, NewExternalService, NewUser, Ownership,
    RESOURCES_SOURCE, migrate,
};
use sourcegull_declarative::{Files, Reconciler};
use sourcegull_outbound_webhooks::store::OutboundWebhookStore;
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, SettingsSubject, UserId};

/// Environment variables for `${env:…}`, changeable during a test.
#[derive(Default)]
struct Env(Mutex<HashMap<String, String>>);

impl Env {
    fn set(&self, k: &str, v: &str) {
        self.0.lock().unwrap().insert(k.to_owned(), v.to_owned());
    }
}

impl RefSource for Env {
    fn env(&self, name: &str) -> Option<String> {
        self.0.lock().unwrap().get(name).cloned()
    }
    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

struct Fixture {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    dir: PathBuf,
    env: Arc<Env>,
}

impl Fixture {
    async fn new(name: &str) -> Option<Self> {
        let t = TestDb::new().await?;
        migrate::up(&t.pool).await.unwrap();
        let db = Db::from_pool(t.pool.clone());
        let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
        let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
        let dir =
            std::env::temp_dir().join(format!("sourcegull-declared-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = Arc::new(Env::default());
        env.set("GITLAB_TOKEN", "glpat-one");
        env.set("HOOK_SECRET", "hook-one");
        Some(Self {
            _t: t,
            db,
            keyring,
            dir,
            env,
        })
    }

    fn reconciler(&self, resources: Option<&str>, settings: Option<&str>) -> Reconciler {
        let write = |name: &str, text: Option<&str>| {
            text.map(|t| {
                let p = self.dir.join(name);
                std::fs::write(&p, t).unwrap();
                p
            })
        };
        Reconciler::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            Files {
                resources: write("resources.jsonc", resources),
                global_settings: write("settings.jsonc", settings),
            },
            None,
        )
        .with_refs(self.env.clone())
    }

    async fn user(&self, name: &str, email: &str, verified: bool) -> UserId {
        self.db
            .accounts()
            .create(&NewAccount {
                user: NewUser {
                    username: name.to_owned(),
                    ..Default::default()
                },
                email: Some(email.to_owned()),
                email_verified: verified,
                only_if_first: false,
            })
            .await
            .unwrap()
            .user
            .id
    }

    async fn is_admin(&self, user: UserId) -> bool {
        self.db
            .users()
            .get_by_id(user)
            .await
            .unwrap()
            .unwrap()
            .site_admin
    }

    async fn connections(&self) -> Vec<(i64, String, String)> {
        let store = self.db.external_services(Some(self.keyring.clone()));
        let mut out = Vec::new();
        for s in store.list(None, 100, 0).await.unwrap() {
            let config = store.config(s.id).await.unwrap();
            out.push((s.id.0, s.display_name, config));
        }
        out
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const FILE: &str = r#"{
    "connections": [{
        "key": "gitlab-main", "kind": "gitlab", "displayName": "GitLab",
        "config": {"url": "https://gitlab.example.com", "token": "${env:GITLAB_TOKEN}", "projectQuery": ["none"]},
    }],
    "searchContexts": [
        {"name": "backend", "description": "Backend code", "query": "repo:^gitlab\\.example\\.com/acme/"},
        {"name": "api", "repositories": [{"repository": "gitlab.example.com/acme/api"}]},
    ],
    "roles": [{"key": "editors", "name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": ["bob@example.com"]}],
    "outboundWebhooks": [{"key": "audit", "url": "https://hooks.example.com/sourcegull", "secret": "${env:HOOK_SECRET}", "eventTypes": ["batch_change:apply"]}],
    "executorSecrets": [{"scope": "batches", "name": "GITLAB_TOKEN", "value": "${env:GITLAB_TOKEN}"}],
    "codeIntelPolicies": [{"key": "main", "name": "Default branches", "type": "GIT_TREE", "pattern": "HEAD", "indexingEnabled": true}],
    "featureFlags": [{"name": "new-ui", "value": true}],
}"#;

fn changes(report: &sourcegull_declarative::PassReport) -> Vec<String> {
    let r = report.resources.as_ref().expect("applied");
    r.changes
        .iter()
        .map(|(k, key, a)| format!("{k}[{key}] {a}"))
        .collect()
}

#[tokio::test]
async fn applies_updates_and_deletes_declared_objects() {
    let Some(f) = Fixture::new("apply").await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let report = f.reconciler(Some(FILE), None).apply_once().await;
    assert_eq!(report.resources_error, None, "{report:?}");
    let mut created = changes(&report);
    created.sort();
    assert_eq!(
        created,
        [
            "codeIntelPolicies[main] created",
            "connections[gitlab-main] created",
            "executorSecrets[batches/GITLAB_TOKEN] created",
            "featureFlags[new-ui] created",
            "outboundWebhooks[audit] created",
            "roles[editors] created",
            "searchContexts[api] created",
            "searchContexts[backend] created",
        ]
    );
    // The repository of `api` is not synced yet.
    assert!(
        report
            .resources
            .as_ref()
            .unwrap()
            .warnings
            .iter()
            .any(|w| w.contains("not synced yet")),
        "{report:?}"
    );
    let conns = f.connections().await;
    assert_eq!(conns.len(), 1);
    assert!(conns[0].2.contains("glpat-one"));
    let state =
        f.db.declared()
            .source(RESOURCES_SOURCE)
            .await
            .unwrap()
            .unwrap();
    assert!(state.error.is_none());
    assert!(state.kinds.contains(&"connections".to_owned()));

    // The objects are read-only in the UI.
    let conn_id = conns[0].0.to_string();
    assert_eq!(
        f.db.declared()
            .ownership("connections", &conn_id)
            .await
            .unwrap(),
        Some(Ownership::Declared {
            key: "gitlab-main".into()
        })
    );
    assert!(!f.db.declared().is_exclusive("connections").await.unwrap());

    // A second pass changes nothing.
    let again = f.reconciler(Some(FILE), None).apply_once().await;
    assert!(changes(&again).is_empty(), "{again:?}");

    // A rotated secret updates the connection in place.
    f.env.set("GITLAB_TOKEN", "glpat-two");
    let rotated = f.reconciler(Some(FILE), None).apply_once().await;
    let mut updated = changes(&rotated);
    updated.sort();
    assert_eq!(
        updated,
        [
            "connections[gitlab-main] updated",
            "executorSecrets[batches/GITLAB_TOKEN] updated"
        ]
    );
    let conns = f.connections().await;
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0].0.to_string(), conn_id, "same connection");
    assert!(conns[0].2.contains("glpat-two"));

    // A synced repository joins the context.
    sqlx::query("INSERT INTO repo (name, external_service_type, external_service_id, external_id) VALUES ('gitlab.example.com/acme/api', 'gitlab', 'https://gitlab.example.com/', '1')")
        .execute(f.db.pool())
        .await
        .unwrap();
    let synced = f.reconciler(Some(FILE), None).apply_once().await;
    assert_eq!(
        changes(&synced),
        ["searchContexts[api] updated"],
        "{synced:?}"
    );
    let api =
        f.db.search_contexts()
            .get_by_spec("api", ContextAccess::All)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(f.db.search_contexts().repos(api.id).await.unwrap().len(), 1);

    // Removing entries deletes their objects; removing sections releases them.
    let smaller = r#"{
        "connections": [],
        "searchContexts": [{"name": "backend", "query": "repo:acme"}],
        "featureFlags": [{"name": "new-ui", "value": false}],
    }"#;
    let shrunk = f.reconciler(Some(smaller), None).apply_once().await;
    assert_eq!(shrunk.resources_error, None, "{shrunk:?}");
    let mut c = changes(&shrunk);
    c.sort();
    assert_eq!(
        c,
        [
            "codeIntelPolicies[*] released",
            "connections[gitlab-main] deleted",
            "executorSecrets[*] released",
            "featureFlags[new-ui] updated",
            "outboundWebhooks[*] released",
            "roles[*] released",
            "searchContexts[api] deleted",
            "searchContexts[backend] updated",
        ]
    );
    assert!(f.connections().await.is_empty());
    let hooks = OutboundWebhookStore::new(&f.db, Some(f.keyring.clone()))
        .list(&Default::default())
        .await
        .unwrap();
    assert_eq!(hooks.len(), 1, "a released webhook stays");
    assert_eq!(
        f.db.declared()
            .ownership("outboundWebhooks", &hooks[0].id.to_string())
            .await
            .unwrap(),
        None
    );

    // Unsetting the file releases everything.
    let unset = f.reconciler(None, None).apply_once().await;
    assert_eq!(unset.resources_error, None);
    assert!(
        f.db.declared()
            .source(RESOURCES_SOURCE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.db.declared().all_objects().await.unwrap().is_empty());
    assert!(f.db.feature_flags().get("new-ui").await.unwrap().is_some());
}

#[tokio::test]
async fn takes_over_existing_objects() {
    let Some(f) = Fixture::new("adopt").await else {
        return;
    };
    let store = f.db.external_services(Some(f.keyring.clone()));
    let existing = store
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "Old name".into(),
            config: r#"{"url": "https://gitlab.example.com/", "token": "old", "projectQuery": ["none"]}"#.into(),
        })
        .await
        .unwrap();
    f.db.feature_flags()
        .create("new-ui", sourcegull_db::FeatureFlagValue::Bool(false))
        .await
        .unwrap();
    let report = f.reconciler(Some(FILE), None).apply_once().await;
    let c = changes(&report);
    assert!(
        c.contains(&"connections[gitlab-main] adopted".to_owned()),
        "{c:?}"
    );
    assert!(
        c.contains(&"featureFlags[new-ui] adopted".to_owned()),
        "{c:?}"
    );
    let conns = f.connections().await;
    assert_eq!(conns.len(), 1, "no second connection");
    assert_eq!(conns[0].0, existing.id.0);
    assert_eq!(conns[0].1, "GitLab");
    assert!(
        f.db.feature_flags()
            .get("new-ui")
            .await
            .unwrap()
            .unwrap()
            .value
            == sourcegull_db::FeatureFlagValue::Bool(true)
    );
}

#[tokio::test]
async fn system_writes_to_a_declared_connection_are_kept() {
    let Some(f) = Fixture::new("keep").await else {
        return;
    };
    f.reconciler(Some(FILE), None).apply_once().await;
    let (id, _, config) = f.connections().await.remove(0);
    // A token refresh writes the connection's config; the unchanged file does not undo it.
    let refreshed = config.replace("glpat-one", "refreshed");
    f.db.external_services(Some(f.keyring.clone()))
        .update(
            sourcegull_types::ExternalServiceId(id),
            &ExternalServiceUpdate {
                config: Some(refreshed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let again = f.reconciler(Some(FILE), None).apply_once().await;
    assert!(changes(&again).is_empty(), "{again:?}");
    assert!(f.connections().await[0].2.contains("refreshed"));
}

#[tokio::test]
async fn invalid_files_change_nothing() {
    let Some(f) = Fixture::new("invalid").await else {
        return;
    };
    f.reconciler(Some(FILE), None).apply_once().await;
    let broken = FILE.replace("${env:GITLAB_TOKEN}", "${env:NOT_SET}");
    let report = f.reconciler(Some(&broken), None).apply_once().await;
    let err = report.resources_error.expect("error");
    assert!(
        err.contains("connections[gitlab-main].config.token"),
        "{err}"
    );
    assert!(err.contains("NOT_SET"), "{err}");
    assert!(report.resources.is_none());
    let state =
        f.db.declared()
            .source(RESOURCES_SOURCE)
            .await
            .unwrap()
            .unwrap();
    assert!(state.error.as_deref().unwrap().contains("NOT_SET"));
    assert!(
        state.kinds.contains(&"connections".to_owned()),
        "last applied kinds stay"
    );
    assert_eq!(f.connections().await.len(), 1);
    // Fixing it clears the error.
    let fixed = f.reconciler(Some(FILE), None).apply_once().await;
    assert_eq!(fixed.resources_error, None);
    let state =
        f.db.declared()
            .source(RESOURCES_SOURCE)
            .await
            .unwrap()
            .unwrap();
    assert!(state.error.is_none());

    // An entry the database rejects fails alone.
    let unknown = FILE.replace("BATCH_CHANGES#WRITE", "NOPE#NEVER");
    let report = f.reconciler(Some(&unknown), None).apply_once().await;
    let err = report.resources_error.expect("error");
    assert!(
        err.contains("roles[editors]") && err.contains("unknown permission"),
        "{err}"
    );
    assert!(report.resources.unwrap().errors.len() == 1);
}

#[tokio::test]
async fn exclusive_kinds() {
    let Some(f) = Fixture::new("exclusive").await else {
        return;
    };
    let file = r#"{"exclusive": true, "featureFlags": [{"name": "a", "value": true}]}"#;
    f.reconciler(Some(file), None).apply_once().await;
    assert!(f.db.declared().is_exclusive("featureFlags").await.unwrap());
    assert!(!f.db.declared().is_exclusive("connections").await.unwrap());
    assert_eq!(
        f.db.declared()
            .ownership("featureFlags", "other")
            .await
            .unwrap(),
        Some(Ownership::Exclusive)
    );
}

#[tokio::test]
async fn site_admins_and_role_members_follow_verified_emails() {
    let Some(f) = Fixture::new("admins").await else {
        return;
    };
    let first = f.user("first", "first@example.com", true).await;
    assert!(
        f.is_admin(first).await,
        "the first user is admin without declared admins"
    );
    let alice = f.user("alice", "alice@example.com", true).await;
    let bob = f.user("bob", "bob@example.com", true).await;
    let mallory = f.user("mallory", "mallory@example.com", false).await;
    let file = r#"{
        "siteAdmins": ["Alice@example.com", "mallory@example.com"],
        "roles": [{"name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": ["bob@example.com"]}],
    }"#;
    let report = f.reconciler(Some(file), None).apply_once().await;
    assert_eq!(report.resources_error, None, "{report:?}");
    assert!(f.is_admin(alice).await);
    assert!(!f.is_admin(mallory).await, "unverified emails do not count");
    assert!(f.is_admin(first).await, "admins made in the UI stay");
    let roles = f.db.rbac().roles_for_user(bob).await.unwrap();
    assert!(roles.iter().any(|r| r.name == "bulk-editors"));

    // A role taken away in the UI comes back on the next pass.
    let role =
        f.db.rbac()
            .roles_for_user(bob)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.name == "bulk-editors")
            .unwrap();
    f.db.rbac().unassign(bob, role.id).await.unwrap();
    let file_again = r#"{
        "siteAdmins": ["Alice@example.com", "mallory@example.com"],
        "roles": [{"name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": [" bob@example.com"]}],
    }"#;
    f.reconciler(Some(file_again), None).apply_once().await;
    assert!(
        f.db.rbac()
            .roles_for_user(bob)
            .await
            .unwrap()
            .iter()
            .any(|r| r.name == "bulk-editors"),
        "re-granted, with the address trimmed"
    );

    // Removing them takes back only what the file granted.
    let file = r#"{
        "siteAdmins": ["mallory@example.com"],
        "roles": [{"name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": []}],
    }"#;
    f.reconciler(Some(file), None).apply_once().await;
    assert!(!f.is_admin(alice).await);
    assert!(f.is_admin(first).await);
    assert!(
        f.db.rbac()
            .roles_for_user(bob)
            .await
            .unwrap()
            .iter()
            .all(|r| r.name != "bulk-editors")
    );

    // Exclusive: only declared admins stay, once one of them is admin.
    let file = r#"{"exclusive": true, "siteAdmins": ["alice@example.com"]}"#;
    f.reconciler(Some(file), None).apply_once().await;
    assert!(f.is_admin(alice).await);
    assert!(!f.is_admin(first).await);
}

#[tokio::test]
async fn declared_admins_replace_the_first_user_rule() {
    let Some(f) = Fixture::new("bootstrap").await else {
        return;
    };
    f.reconciler(Some(r#"{"siteAdmins": ["admin@example.com"]}"#), None)
        .apply_once()
        .await;
    // A first sign-up with another email is not admin, and with sign-up closed is refused.
    let refused =
        f.db.accounts()
            .create(&NewAccount {
                user: NewUser {
                    username: "random".into(),
                    ..Default::default()
                },
                email: Some("random@example.com".into()),
                email_verified: true,
                only_if_first: true,
            })
            .await;
    assert!(refused.is_err());
    // Typing the admin's email without verifying it does not help either.
    let unverified = f.user("squatter", "admin@example.com", false).await;
    assert!(!f.is_admin(unverified).await);
    let emails: Vec<(String, bool)> =
        sqlx::query_as("SELECT email, verified_at IS NOT NULL FROM user_emails WHERE user_id = $1")
            .bind(unverified)
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    assert_eq!(emails, [("admin@example.com".to_owned(), false)]);
    // With sign-up closed and users present, a declared address still creates its account.
    let other = f.user("other", "other@example.com", true).await;
    assert!(!f.is_admin(other).await);

    // The identity provider vouches for it: admin.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(unverified)
        .execute(f.db.pool())
        .await
        .unwrap();
    let admin =
        f.db.accounts()
            .create(&NewAccount {
                user: NewUser {
                    username: "admin".into(),
                    ..Default::default()
                },
                email: Some("ADMIN@example.com".into()),
                email_verified: true,
                only_if_first: true,
            })
            .await
            .unwrap();
    assert!(admin.user.site_admin);
}

#[tokio::test]
async fn global_settings_file() {
    let Some(f) = Fixture::new("settings").await else {
        return;
    };
    let text = r#"{"search.defaultPatternType": "regexp"}"#;
    let report = f.reconciler(None, Some(text)).apply_once().await;
    assert!(report.global_settings_written, "{report:?}");
    let latest =
        f.db.settings()
            .latest(SettingsSubject::Global)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(latest.contents, text);
    assert!(
        f.db.declared()
            .source("global_settings")
            .await
            .unwrap()
            .is_some()
    );
    let again = f.reconciler(None, Some(text)).apply_once().await;
    assert!(!again.global_settings_written);
    let bad = f.reconciler(None, Some("[1]")).apply_once().await;
    assert!(bad.global_settings_error.is_some());
    assert_eq!(
        f.db.settings()
            .latest(SettingsSubject::Global)
            .await
            .unwrap()
            .unwrap()
            .id,
        latest.id
    );
    f.reconciler(None, None).apply_once().await;
    assert!(
        f.db.declared()
            .source("global_settings")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_passes_make_one_change() {
    let Some(f) = Fixture::new("concurrent").await else {
        return;
    };
    let a = f.reconciler(Some(FILE), None);
    let b = f.reconciler(Some(FILE), None);
    let (ra, rb) = tokio::join!(a.apply_once(), b.apply_once());
    let total = changes(&ra).len() + changes(&rb).len();
    assert_eq!(total, 8, "{ra:?} {rb:?}");
    assert_eq!(f.connections().await.len(), 1);
}
