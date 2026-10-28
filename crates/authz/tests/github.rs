//! The permissions sync of GitHub connections against the mock GitHub and the test Postgres:
//! user-centric with the users' OAuth tokens (missing scope, revocation) and repo-centric with
//! the connection token.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use sourcegull_authz::sync::{PermsSyncer, SyncError};
use sourcegull_codehost::{ClientOptions, OAuthToken};
use sourcegull_codehost_github::testing::{MockGitHub, MockUser, repo};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{AccountSpec, Db, LinkAccount, NewExternalService, NewUser, migrate};
use sourcegull_repos::{Registry, Syncer};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, RepoId, UserId};
use tokio_util::sync::CancellationToken;

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    github: MockGitHub,
    registry: Registry,
    config: ConfigHandle<SiteConfig>,
}

/// Repos: 1 private, 2 private, 3 internal, 4 public. Alice (7) reads 1, 3 and 4; bob (8)
/// reads 1 and 2.
async fn setup() -> Option<(Env, Vec<RepoId>)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let github = MockGitHub::start("admin-token").await;
    github.with(|s| {
        s.repos = (1..=4).map(|i| repo(i, &format!("acme/r{i}"))).collect();
        s.repos[2]["visibility"] = "internal".into();
        s.repos[3]["visibility"] = "public".into();
        s.repos[3]["private"] = false.into();
        s.users = vec![
            MockUser::new(7, "alice").reads(1).reads(3).reads(4),
            MockUser::new(8, "bob").reads(1).reads(2),
        ];
    });
    let registry = Registry {
        client_options: ClientOptions {
            retry_backoff: Duration::from_millis(1),
            ..ClientOptions::default()
        },
        ..Registry::default()
    };
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("http://sourcegull.test".into()),
            ..SiteConfig::default()
        },
    );
    let text = format!(
        r#"{{"url": "{}", "token": "admin-token", "orgs": ["acme"],
            "authorization": {{"identityProvider": {{"type": "oauth"}}}}}}"#,
        github.url
    );
    let svc = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitHub,
            display_name: "GitHub".into(),
            config: text,
        })
        .await
        .unwrap();
    Syncer::new(
        db.clone(),
        Some(keyring.clone()),
        registry.clone(),
        config.clone(),
    )
    .sync(svc.id, None, &CancellationToken::new())
    .await
    .expect("discovery sync");
    let service_id = format!("{}/", github.url);
    let mut ids = Vec::new();
    for i in 1..=4 {
        let r = db
            .repos()
            .get_by_external_id("github", &service_id, &i.to_string())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("repo r{i}"));
        ids.push(r.id);
    }
    let env = Env {
        _t: t,
        db,
        keyring,
        github,
        registry,
        config,
    };
    Some((env, ids))
}

impl Env {
    fn syncer(&self) -> PermsSyncer {
        PermsSyncer::new(
            self.db.clone(),
            Some(self.keyring.clone()),
            self.registry.clone(),
            self.config.clone(),
        )
    }

    async fn user(&self, name: &str) -> UserId {
        self.db
            .users()
            .create(&NewUser {
                username: name.into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }

    async fn grants(&self, user: UserId) -> Vec<RepoId> {
        self.db.perms().user_repos(user).await.unwrap()
    }

    /// Links `user` to GitHub user `account` with OAuth token `token` (no expiry, as GitHub
    /// OAuth app tokens).
    async fn link_oauth(&self, user: UserId, account: &str, token: &str) {
        let token = OAuthToken {
            access_token: token.into(),
            refresh_token: None,
            expires_at: None,
            scopes: vec!["read:user".into(), "repo".into()],
        };
        let bytes = serde_json::to_vec(&token).unwrap();
        let service_id = format!("{}/", self.github.url);
        self.db
            .external_accounts(Some(self.keyring.clone()))
            .link(
                user,
                &LinkAccount {
                    spec: AccountSpec {
                        service_type: "github",
                        service_id: &service_id,
                        account_id: account,
                    },
                    client_id: "mock-client",
                    account_data: serde_json::json!({"username": "x"}),
                    auth_data: Some(&bytes),
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn oauth_tokens_scope_and_revocation() {
    let Some((env, repos)) = setup().await else {
        return;
    };
    let alice = env.user("alice").await;
    let bob = env.user("bob").await;
    let syncer = env.syncer();

    // Private repos need permissions; the public one does not.
    let private: Vec<bool> = {
        let mut v = Vec::new();
        for id in &repos {
            v.push(env.db.repos().get(*id).await.unwrap().unwrap().private);
        }
        v
    };
    assert_eq!(private, [true, true, true, false]);

    // No GitHub account yet: nothing, and no username matching.
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(
        r.code_host_states[0]["message"],
        "no account on this code host"
    );
    assert!(env.grants(alice).await.is_empty());

    // With their tokens: the private and internal repositories each user reads.
    env.link_oauth(alice, "7", "token-7").await;
    env.link_oauth(bob, "8", "token-8").await;
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[2]]);
    assert_eq!((r.added, r.removed, r.found), (2, 0, 2));
    syncer.sync_user(bob).await.unwrap();
    assert_eq!(env.grants(bob).await, vec![repos[0], repos[1]]);

    // Access changes on GitHub.
    env.github.with(|s| {
        s.users[0].repos.remove(&3);
        s.users[0].repos.insert(2);
    });
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[1]]);
    assert_eq!((r.added, r.removed), (1, 1));

    // A token without `repo` (signed in before permissions were enabled): the sync says to
    // sign in again, and the grants go (fail closed).
    env.github
        .with(|s| s.users[0].scopes = Some(vec!["read:user".into(), "user:email".into()]));
    let err = syncer.sync_user(alice).await.unwrap_err();
    let SyncError::Partial { result, message } = err else {
        panic!("{err:?}")
    };
    assert!(message.contains("sign in again"), "{message}");
    assert_eq!(result.removed, 2);
    // The state names the code host, for the user's sign-in prompt.
    assert_eq!(result.code_host_states[0]["signIn"], "missingScope");
    assert_eq!(result.code_host_states[0]["serviceType"], "github");
    assert!(env.grants(alice).await.is_empty());
    // The account is not expired: it still signs in.
    let summary = env
        .db
        .external_accounts(None)
        .list_summaries(alice)
        .await
        .unwrap();
    assert!(summary[0].expired_at.is_none());

    // GitHub revokes bob's token: the account expires and the grants go.
    env.github.with(|s| s.users[1].token = "rotated".into());
    let err = syncer.sync_user(bob).await.unwrap_err();
    let SyncError::Partial { message, .. } = err else {
        panic!("{err:?}")
    };
    assert!(message.contains("rejected"), "{message}");
    assert!(env.grants(bob).await.is_empty());
    let summary = env
        .db
        .external_accounts(None)
        .list_summaries(bob)
        .await
        .unwrap();
    assert!(summary[0].expired_at.is_some());
    // An expired account is skipped until the user signs in again.
    syncer.sync_user(bob).await.unwrap();
}

#[tokio::test]
async fn repo_centric_with_the_connection_token() {
    let Some((env, repos)) = setup().await else {
        return;
    };
    let alice = env.user("alice").await;
    // Bob has a Sourcegull account but never signed in with GitHub: not matched by username.
    let bob = env.user("bob").await;
    env.link_oauth(alice, "7", "token-7").await;
    let syncer = env.syncer();
    assert_eq!(syncer.repo_centric_services().await.unwrap().len(), 1);

    // Repo 1's collaborators are alice and bob; only alice is linked.
    let r = syncer.sync_repo(repos[0]).await.unwrap();
    assert_eq!((r.found, r.added), (2, 1));
    let (users, _) = env.db.perms().repo_users(repos[0], 10, 0).await.unwrap();
    assert_eq!(users, vec![alice]);
    assert!(env.grants(bob).await.is_empty());
    assert!(
        env.github
            .requests()
            .iter()
            .any(|q| q.starts_with("/api/v3/repos/acme/r1/collaborators"))
    );

    // A connection token without admin access: a clear error, grants kept.
    env.github.with(|s| s.collaborators_forbidden = true);
    let err = syncer.sync_repo(repos[0]).await.unwrap_err();
    let SyncError::Partial { message, .. } = err else {
        panic!("{err:?}")
    };
    assert!(message.contains("admin access"), "{message}");
    let (users, _) = env.db.perms().repo_users(repos[0], 10, 0).await.unwrap();
    assert_eq!(users, vec![alice]);
}
