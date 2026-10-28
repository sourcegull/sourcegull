//! Repository permission store tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`):
//! the `repo_restricted()` rule, grants and their bitmaps, pending permissions and the
//! authorized repo lookups.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use roaring::RoaringBitmap;
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{
    BindKind, Db, ListReposOptions, ListedRepo, NewExternalService, NewUser, PermsSource, migrate,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, RepoAccess, RepoId, UserId, Visibility};

struct Env {
    _t: TestDb,
    db: Db,
    /// Public, private, private-on-an-unrestricted-connection.
    public: RepoId,
    private: RepoId,
    open: RepoId,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let services = db.external_services(Some(keyring));
    let mut ids = Vec::new();
    for (name, unrestricted) in [("authz", false), ("open", true)] {
        let svc = services
            .create(&NewExternalService {
                kind: ExternalServiceKind::GitLab,
                display_name: name.into(),
                config: r#"{"url": "https://gitlab.example.com", "token": "t"}"#.into(),
            })
            .await
            .unwrap();
        services
            .set_unrestricted(svc.id, unrestricted)
            .await
            .unwrap();
        ids.push(svc.id);
    }
    let repos = db.repos();
    let a = repos
        .upsert_listed(
            ids[0],
            &[listed(1, "public", false), listed(2, "private", true)],
        )
        .await
        .unwrap();
    let b = repos
        .upsert_listed(ids[1], &[listed(3, "open", true)])
        .await
        .unwrap();
    Some(Env {
        _t: t,
        db,
        public: a.ids[0],
        private: a.ids[1],
        open: b.ids[0],
    })
}

fn listed(id: u32, name: &str, private: bool) -> ListedRepo {
    ListedRepo {
        name: format!("gitlab.example.com/acme/{name}"),
        uri: name.into(),
        url: String::new(),
        description: String::new(),
        fork: false,
        archived: false,
        private,
        visibility: if private {
            Visibility::Private
        } else {
            Visibility::Public
        },
        stars: 0,
        topics: Vec::new(),
        default_branch: None,
        last_activity_at: None,
        external_id: id.to_string(),
        external_service_type: "gitlab".into(),
        external_service_id: "https://gitlab.example.com/".into(),
        metadata: serde_json::json!({}),
        clone_url: format!("https://gitlab.example.com/acme/{name}.git"),
    }
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            display_name: None,
            avatar_url: None,
            password_hash: None,
        })
        .await
        .unwrap()
        .id
}

fn bitmap(ids: &[RepoId]) -> RoaringBitmap {
    ids.iter().map(|r| r.get() as u32).collect()
}

impl Env {
    async fn access(&self, user: Option<UserId>, user_mapping: bool) -> RepoAccess {
        let perms = self.db.perms();
        RepoAccess::Restricted {
            user_id: user,
            user_mapping,
            unrestricted: Arc::new(perms.unrestricted_repos(user_mapping).await.unwrap()),
            granted: Arc::new(match user {
                Some(u) => perms.user_bitmap(u).await.unwrap(),
                None => RoaringBitmap::new(),
            }),
        }
    }

    /// Names readable through the SQL path, checked against the bitmap path.
    async fn readable(&self, access: &RepoAccess) -> Vec<String> {
        let repos = self
            .db
            .repos()
            .list(&ListReposOptions {
                access: Some(access.clone()),
                ..ListReposOptions::default()
            })
            .await
            .unwrap();
        let all = [self.public, self.private, self.open];
        let by_bitmap: Vec<RepoId> = all.into_iter().filter(|r| access.can_read(*r)).collect();
        let mut by_sql: Vec<RepoId> = repos.iter().map(|r| r.id).collect();
        by_sql.sort();
        assert_eq!(by_sql, by_bitmap, "SQL and bitmap paths disagree");
        let mut names: Vec<String> = repos
            .into_iter()
            .map(|r| r.name.rsplit('/').next().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrestricted_rule() {
    let Some(env) = setup().await else {
        return;
    };
    let perms = env.db.perms();
    assert_eq!(
        perms.unrestricted_repos(false).await.unwrap(),
        bitmap(&[env.public, env.open])
    );
    // Explicit permissions disregard unrestricted connections.
    assert_eq!(
        perms.unrestricted_repos(true).await.unwrap(),
        bitmap(&[env.public])
    );
    let anon = env.access(None, false).await;
    assert_eq!(env.readable(&anon).await, ["open", "public"]);
    assert_eq!(env.readable(&RepoAccess::All).await.len(), 3);

    perms.set_unrestricted(&[env.private], true).await.unwrap();
    assert_eq!(
        env.readable(&env.access(None, true).await).await,
        ["private", "public"]
    );
    let info = perms.repo_info(env.private).await.unwrap().unwrap();
    assert!(info.unrestricted);
    perms.set_unrestricted(&[env.private], false).await.unwrap();
    assert_eq!(
        env.readable(&env.access(None, true).await).await,
        ["public"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_and_revocations() {
    let Some(env) = setup().await else {
        return;
    };
    let (alice, bob) = (user(&env.db, "alice").await, user(&env.db, "bob").await);
    let perms = env.db.perms();
    perms
        .set_repo_perms(env.private, PermsSource::Explicit, &[alice, bob], None)
        .await
        .unwrap();
    assert_eq!(
        perms.user_bitmap(alice).await.unwrap(),
        bitmap(&[env.private])
    );
    let a = env.access(Some(alice), false).await;
    assert_eq!(env.readable(&a).await, ["open", "private", "public"]);
    let repos = env.db.repos();
    assert!(
        repos
            .get_by_name_authorized("GITLAB.example.com/acme/private", &a)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        perms.repo_users(env.private, 10, 0).await.unwrap(),
        (vec![alice, bob], 2)
    );

    // Replacing the explicit grants revokes bob; sync rows are separate.
    perms
        .set_repo_perms(env.private, PermsSource::Sync, &[bob], None)
        .await
        .unwrap();
    perms
        .set_repo_perms(env.private, PermsSource::Explicit, &[alice], None)
        .await
        .unwrap();
    assert_eq!(
        perms.user_bitmap(bob).await.unwrap(),
        bitmap(&[env.private])
    );
    perms
        .set_repo_perms(env.private, PermsSource::Sync, &[], None)
        .await
        .unwrap();
    assert!(perms.user_bitmap(bob).await.unwrap().is_empty());
    let b = env.access(Some(bob), false).await;
    assert_eq!(env.readable(&b).await, ["open", "public"]);
    assert!(
        repos
            .get_authorized(env.private, &b)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repos
            .readable_ids(&[env.private, env.public], &b)
            .await
            .unwrap(),
        [env.public]
    );
    assert!(
        perms
            .repo_info(env.private)
            .await
            .unwrap()
            .unwrap()
            .synced_at
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_permissions_bind_on_creation_and_verification() {
    let Some(env) = setup().await else {
        return;
    };
    let perms = env.db.perms();
    perms
        .set_repo_perms(
            env.private,
            PermsSource::Explicit,
            &[],
            Some((BindKind::Username, &["Carol".to_owned()])),
        )
        .await
        .unwrap();
    assert_eq!(
        perms.pending_bind_ids(BindKind::Username).await.unwrap(),
        ["carol"]
    );
    assert_eq!(
        perms
            .pending_repos(BindKind::Username, "CAROL")
            .await
            .unwrap(),
        [env.private]
    );
    let carol = user(&env.db, "carol").await;
    assert_eq!(perms.user_repos(carol).await.unwrap(), [env.private]);
    assert_eq!(
        perms.user_bitmap(carol).await.unwrap(),
        bitmap(&[env.private])
    );
    assert!(
        perms
            .pending_bind_ids(BindKind::Username)
            .await
            .unwrap()
            .is_empty()
    );

    // By email: granted when the address is verified, not when it is added.
    perms
        .set_repo_perms(
            env.open,
            PermsSource::Explicit,
            &[],
            Some((BindKind::Email, &["dave@example.com".to_owned()])),
        )
        .await
        .unwrap();
    let dave = user(&env.db, "dave").await;
    let emails = env.db.user_emails();
    emails.add(dave, "Dave@example.com").await.unwrap();
    assert!(perms.user_repos(dave).await.unwrap().is_empty());
    emails.verify(dave, "Dave@example.com").await.unwrap();
    assert_eq!(perms.user_repos(dave).await.unwrap(), [env.open]);
}
