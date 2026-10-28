//! The code host permissions sync against the mock Gitea and the test Postgres:
//! `username` (sudo, user- and repo-centric) and `oauth` (user tokens, refresh, expiry)
//! identity providers.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sourcegull_authz::sync::{PermsSyncer, SyncError};
use sourcegull_codehost::{ClientOptions, OAuthToken};
use sourcegull_codehost_gitea::testing::{MockGitea, MockTeam, MockUser, repo};
use sourcegull_config::{AuthProvider, ConfigHandle, GiteaAuthProvider, SiteConfig};
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
    gitea: MockGitea,
    registry: Registry,
    config: ConfigHandle<SiteConfig>,
}

/// Repositories: 1 private, 2 private, 3 internal, 4 public, all of org `acme`. Alice (7):
/// collaborator on 1, on team 10 (read) with 3. Bob (8): on team 11 (code unit `none`) with
/// 2, collaborator on 1.
async fn setup(identity: &str) -> Option<(Env, Vec<RepoId>)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [4u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let gitea = MockGitea::start("admin-token").await;
    gitea.with(|s| {
        s.repos = (1..=4).map(|i| repo(i, &format!("acme/p{i}"))).collect();
        s.repos[2]["private"] = false.into();
        s.repos[2]["internal"] = true.into();
        s.repos[3]["private"] = false.into();
        s.users = vec![MockUser::new(7, "alice"), MockUser::new(8, "bob")];
        s.collaborators.insert(1, vec![7, 8]);
        let mut readers = MockTeam::new(10, "acme", "read");
        readers.repos = vec![3];
        readers.members = vec![7];
        let mut no_code = MockTeam::new(11, "acme", "none");
        no_code.code = Some("none".into());
        no_code.repos = vec![2];
        no_code.members = vec![8];
        s.teams = vec![readers, no_code];
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
            auth_providers: vec![AuthProvider::Gitea(GiteaAuthProvider {
                url: gitea.url.clone(),
                client_id: "mock-client".into(),
                client_secret: "mock-secret".into(),
                allow_signup: true,
                link_by_email: None,
                allow_orgs: Vec::new(),
                display_name: None,
            })],
            ..SiteConfig::default()
        },
    );
    let env = Env {
        _t: t,
        db,
        keyring,
        gitea,
        registry,
        config,
    };
    let text = format!(
        r#"{{"url": "{}", "token": "admin-token", "repositoryQuery": ["all"],
            "authorization": {{"identityProvider": {{"type": "{identity}"}}}}}}"#,
        env.gitea.url
    );
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::Gitea,
            display_name: "Gitea".into(),
            config: text,
        })
        .await
        .unwrap();
    Syncer::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        env.registry.clone(),
        env.config.clone(),
    )
    .sync(svc.id, None, &CancellationToken::new())
    .await
    .expect("discovery sync");
    let mut ids = Vec::new();
    for i in 1..=4 {
        let r = env
            .db
            .repos()
            .get_by_name(&format!("{}/acme/p{i}", host(&env.gitea.url)))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("repo p{i}"));
        ids.push(r.id);
    }
    Some((env, ids))
}

fn host(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    format!("{}:{}", u.host_str().unwrap(), u.port().unwrap())
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

    fn service_id(&self) -> String {
        format!("{}/", self.gitea.url)
    }

    /// Links `user` to Gitea user `account` with an OAuth token.
    async fn link_oauth(&self, user: UserId, account: &str, token: &OAuthToken) {
        let bytes = serde_json::to_vec(token).unwrap();
        let service_id = self.service_id();
        self.db
            .external_accounts(Some(self.keyring.clone()))
            .link(
                user,
                &LinkAccount {
                    spec: AccountSpec {
                        service_type: "gitea",
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
async fn username_identity_user_and_repo_centric() {
    let Some((env, repos)) = setup("username").await else {
        return;
    };
    let alice = env.user("alice").await;
    let bob = env.user("bob").await;
    let carol = env.user("carol").await;
    let syncer = env.syncer();

    // Alice reads p1 (collaborator) and p3 (internal, team); the user is found by username
    // and acted as with Sudo.
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[2]]);
    assert_eq!((r.added, r.removed, r.found), (2, 0, 2));
    assert!(
        env.gitea
            .requests()
            .iter()
            .any(|q| q.starts_with("/api/v1/users/alice"))
    );
    let accounts = env
        .db
        .external_accounts(None)
        .list_summaries(alice)
        .await
        .unwrap();
    assert_eq!(accounts[0].account_id, "7");
    assert!(!accounts[0].has_token);
    assert!(
        env.db
            .perms_sync_jobs()
            .user_synced_at(alice)
            .await
            .unwrap()
            .is_some()
    );

    // Bob's team does not read code on p2; he is a collaborator on p1.
    syncer.sync_user(bob).await.unwrap();
    assert_eq!(env.grants(bob).await, vec![repos[0]]);
    // Carol has no Gitea account: no grants, no error.
    let r = syncer.sync_user(carol).await.unwrap();
    assert_eq!(
        r.code_host_states[0]["message"],
        "no account on this code host"
    );
    assert!(env.grants(carol).await.is_empty());

    // Membership changes: alice loses p3, gains p2.
    env.gitea.with(|s| {
        s.teams[0].members.clear();
        s.collaborators.insert(2, vec![7]);
    });
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[1]]);
    assert_eq!((r.added, r.removed), (1, 1));

    // Explicit grants survive a sync.
    env.db
        .perms()
        .set_repo_perms(
            repos[2],
            sourcegull_db::PermsSource::Explicit,
            &[alice],
            None,
        )
        .await
        .unwrap();
    syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[1], repos[2]]);

    // Repo-centric: p1's collaborators are alice and bob; bob is already linked, and a Gitea
    // user with a Sourcegull username gets linked on the way.
    env.db
        .perms()
        .set_repo_perms(repos[0], sourcegull_db::PermsSource::Sync, &[], None)
        .await
        .unwrap();
    env.gitea.with(|s| {
        s.users.push(MockUser::new(9, "carol"));
        s.collaborators.insert(1, vec![7, 8, 9]);
    });
    let r = syncer.sync_repo(repos[0]).await.unwrap();
    assert_eq!((r.found, r.added), (3, 3));
    let (users, _) = env.db.perms().repo_users(repos[0], 10, 0).await.unwrap();
    assert_eq!(users, vec![alice, bob, carol]);
    assert!(
        env.db
            .perms()
            .repo_info(repos[0])
            .await
            .unwrap()
            .unwrap()
            .synced_at
            .is_some()
    );
    assert_eq!(syncer.repo_centric_services().await.unwrap().len(), 1);
}

#[tokio::test]
async fn oauth_identity_tokens_refresh_and_expiry() {
    let Some((env, repos)) = setup("oauth").await else {
        return;
    };
    let alice = env.user("alice").await;
    let syncer = env.syncer();

    // No OAuth account yet: nothing, and no username lookup.
    syncer.sync_user(alice).await.unwrap();
    assert!(env.grants(alice).await.is_empty());
    assert!(!env.gitea.requests().iter().any(|q| q.contains("/users/")));
    assert!(syncer.repo_centric_services().await.unwrap().is_empty());

    // A valid token: the repositories the token's user reads.
    let token = env.gitea.issue_token(7);
    env.link_oauth(
        alice,
        "7",
        &OAuthToken {
            access_token: token,
            refresh_token: None,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            scopes: vec![],
        },
    )
    .await;
    syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[2]]);

    // About to expire: refreshed with the auth provider's app, and the new token is stored.
    env.gitea.with(|s| {
        s.refresh_tokens.insert("rt-1".into(), 7);
        s.user_tokens.clear();
    });
    env.link_oauth(
        alice,
        "7",
        &OAuthToken {
            access_token: "stale".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(1)),
            scopes: vec![],
        },
    )
    .await;
    syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[2]]);
    let acct = env
        .db
        .external_accounts(Some(env.keyring.clone()))
        .get_for_user(alice, "gitea", &env.service_id())
        .await
        .unwrap()
        .unwrap();
    let stored: OAuthToken = serde_json::from_slice(acct.auth_data.as_ref().unwrap()).unwrap();
    assert_ne!(stored.access_token, "stale");
    assert_ne!(stored.refresh_token.as_deref(), Some("rt-1"));

    // The code host revokes the token: the account expires and the grants go (fail closed).
    env.gitea.with(|s| s.user_tokens.clear());
    let err = syncer.sync_user(alice).await.unwrap_err();
    let SyncError::Partial { result, message } = err else {
        panic!("{err:?}")
    };
    assert!(message.contains("rejected"), "{message}");
    assert_eq!(result.removed, 2);
    assert!(env.grants(alice).await.is_empty());
    let summary = env
        .db
        .external_accounts(None)
        .list_summaries(alice)
        .await
        .unwrap();
    assert!(summary[0].expired_at.is_some());
    // An expired account is skipped until the user signs in again.
    syncer.sync_user(alice).await.unwrap();

    // Repo-centric sync is a no-op for oauth connections.
    let r = syncer.sync_repo(repos[0]).await.unwrap();
    assert_eq!(r.found, 0);
}
