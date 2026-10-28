//! The code host permissions sync against the mock GitLab and the test Postgres:
//! `username` (sudo, user- and repo-centric) and `oauth` (user tokens, refresh, expiry)
//! identity providers, the scheduler and the queue priorities.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use sourcegull_authz::sync::{PermsSyncer, SyncError};
use sourcegull_codehost::OAuthToken;
use sourcegull_codehost_gitlab::ClientOptions;
use sourcegull_codehost_gitlab::testing::{MockGitLab, MockUser, project};
use sourcegull_config::{AuthProvider, ConfigHandle, GitlabAuthProvider, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{
    AccountSpec, Db, LinkAccount, ListPermsSyncJobs, NewExternalService, NewUser, PermsSyncReason,
    PermsSyncSubject, migrate,
};
use sourcegull_repos::{Registry, Syncer};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, RepoId, UserId};
use tokio_util::sync::CancellationToken;

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    gitlab: MockGitLab,
    registry: Registry,
    config: ConfigHandle<SiteConfig>,
}

/// Projects: 1 private, 2 private, 3 internal, 4 public. Alice (7): developer on 1, reporter
/// on 3. Bob (8): guest on 2, reporter on 1.
async fn setup(identity: &str) -> Option<(Env, Vec<RepoId>)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [4u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let gitlab = MockGitLab::start("admin-token").await;
    let day = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    gitlab.with(|s| {
        let mut projects: Vec<_> = (1..=4)
            .map(|i| project(i, &format!("acme/p{i}"), day))
            .collect();
        projects[2]["visibility"] = "internal".into();
        projects[3]["visibility"] = "public".into();
        s.projects = projects;
        s.users = vec![
            MockUser::new(7, "alice").member(1, 30).member(3, 20),
            MockUser::new(8, "bob").member(2, 10).member(1, 20),
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
            auth_providers: vec![AuthProvider::Gitlab(GitlabAuthProvider {
                url: gitlab.url.clone(),
                client_id: "mock-client".into(),
                client_secret: "mock-secret".into(),
                allow_signup: true,
                link_by_email: None,
                allow_groups: Vec::new(),
                display_name: None,
            })],
            ..SiteConfig::default()
        },
    );
    let env = Env {
        _t: t,
        db,
        keyring,
        gitlab,
        registry,
        config,
    };
    let text = format!(
        r#"{{"url": "{}", "token": "admin-token", "projectQuery": ["projects?membership=true"],
            "authorization": {{"identityProvider": {{"type": "{identity}"}}}}}}"#,
        env.gitlab.url
    );
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
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
            .get_by_name(&format!("{}/acme/p{i}", host(&env.gitlab.url)))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("repo p{i}"));
        ids.push(r.id);
    }
    Some((env, ids))
}

fn host(url: &str) -> String {
    url::Url::parse(url).unwrap().host_str().unwrap().to_owned()
        + &url::Url::parse(url)
            .unwrap()
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default()
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
        format!("{}/", self.gitlab.url)
    }

    /// Links `user` to GitLab user `account` with an OAuth token.
    async fn link_oauth(&self, user: UserId, account: &str, token: &OAuthToken) {
        let bytes = serde_json::to_vec(token).unwrap();
        let service_id = self.service_id();
        self.db
            .external_accounts(Some(self.keyring.clone()))
            .link(
                user,
                &LinkAccount {
                    spec: AccountSpec {
                        service_type: "gitlab",
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

    // Alice reads p1 (developer) and p3 (internal, reporter); the user is found by username
    // and acted as with Sudo.
    let r = syncer.sync_user(alice).await.unwrap();
    assert_eq!(env.grants(alice).await, vec![repos[0], repos[2]]);
    assert_eq!((r.added, r.removed, r.found), (2, 0, 2));
    assert!(
        env.gitlab
            .requests()
            .iter()
            .any(|q| q.starts_with("/api/v4/users?username=alice"))
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

    // Bob is a guest on p2 (cannot read code) and a reporter on p1.
    syncer.sync_user(bob).await.unwrap();
    assert_eq!(env.grants(bob).await, vec![repos[0]]);
    // Carol has no GitLab account: no grants, no error.
    let r = syncer.sync_user(carol).await.unwrap();
    assert_eq!(
        r.code_host_states[0]["message"],
        "no account on this code host"
    );
    assert!(env.grants(carol).await.is_empty());

    // Membership changes: alice loses p3, gains p2.
    env.gitlab.with(|s| {
        let a = s.users.iter_mut().find(|u| u.id == 7).unwrap();
        a.projects.remove(&3);
        a.projects.insert(2, 40);
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

    // Repo-centric: p1's members with read access are alice and bob; bob is already linked,
    // and a GitLab member with a Sourcegull username gets linked on the way.
    env.db
        .perms()
        .set_repo_perms(repos[0], sourcegull_db::PermsSource::Sync, &[], None)
        .await
        .unwrap();
    env.gitlab
        .with(|s| s.users.push(MockUser::new(9, "carol").member(1, 50)));
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
    assert!(!env.gitlab.requests().iter().any(|q| q.contains("/users?")));
    assert!(syncer.repo_centric_services().await.unwrap().is_empty());

    // A valid token: the projects the token's user reads.
    let token = env.gitlab.issue_token(7);
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
    env.gitlab.with(|s| {
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
        .get_for_user(alice, "gitlab", &env.service_id())
        .await
        .unwrap()
        .unwrap();
    let stored: OAuthToken = serde_json::from_slice(acct.auth_data.as_ref().unwrap()).unwrap();
    assert_ne!(stored.access_token, "stale");
    assert_ne!(stored.refresh_token.as_deref(), Some("rt-1"));

    // The code host revokes the token: the account expires and the grants go (fail closed).
    env.gitlab.with(|s| s.user_tokens.clear());
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

#[tokio::test]
async fn scheduler_and_priorities() {
    let Some((env, repos)) = setup("username").await else {
        return;
    };
    let alice = env.user("alice").await;
    let bob = env.user("bob").await;
    let jobs = env.db.perms_sync_jobs();
    // Discovery queued a repo-centric sync of each new private repo.
    let queued = jobs
        .list(&ListPermsSyncJobs {
            state: Some("queued".into()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut repo_jobs: Vec<RepoId> = queued.iter().filter_map(|j| j.repository_id).collect();
    repo_jobs.sort();
    assert_eq!(repo_jobs, vec![repos[0], repos[1], repos[2]]);
    assert!(queued.iter().all(|j| j.reason == "repo_created"));

    // A scheduled job is raised by a sign-in, not duplicated.
    let scheduled = jobs
        .enqueue(
            PermsSyncSubject::User(alice),
            PermsSyncReason::Scheduled,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let raised = jobs
        .enqueue(
            PermsSyncSubject::User(alice),
            PermsSyncReason::UserSignedIn,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scheduled, raised);
    let alice_jobs = jobs
        .list(&ListPermsSyncJobs {
            subject: Some(PermsSyncSubject::User(alice)),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(alice_jobs.len(), 1);
    assert_eq!(alice_jobs[0].reason, "user_signed_in");
    assert_eq!(alice_jobs[0].priority, 50);

    // The scheduler queues bob (alice already has a queued job); synced users wait out the
    // backoff.
    let syncer = env.syncer();
    sourcegull_authz::worker::schedule_once(&env.db, &syncer, &env.config).await;
    let bob_jobs = jobs
        .list(&ListPermsSyncJobs {
            subject: Some(PermsSyncSubject::User(bob)),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(bob_jobs.len(), 1);
    assert_eq!(bob_jobs[0].reason, "scheduled");
    jobs.mark_user_synced(bob).await.unwrap();
    sourcegull_db::sqlx::query("UPDATE permission_sync_jobs SET state = 'completed'")
        .execute(env.db.pool())
        .await
        .unwrap();
    assert!(
        jobs.enqueue_due_users(10, Utc::now() - chrono::Duration::seconds(60))
            .await
            .unwrap()
            .contains(&alice)
    );
    assert!(
        !jobs
            .enqueue_due_users(10, Utc::now() - chrono::Duration::seconds(60))
            .await
            .unwrap()
            .contains(&bob)
    );

    // History is pruned to the newest N finished jobs per subject.
    for _ in 0..3 {
        jobs.enqueue(PermsSyncSubject::User(bob), PermsSyncReason::Manual, None)
            .await
            .unwrap();
        sourcegull_db::sqlx::query("UPDATE permission_sync_jobs SET state = 'completed'")
            .execute(env.db.pool())
            .await
            .unwrap();
    }
    jobs.prune_history(2).await.unwrap();
    let bob_jobs = jobs
        .count(&ListPermsSyncJobs {
            subject: Some(PermsSyncSubject::User(bob)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(bob_jobs, 2);

    // Without connections that have an authorization block, nothing is queued.
    sourcegull_db::sqlx::query("UPDATE external_services SET unrestricted = true")
        .execute(env.db.pool())
        .await
        .unwrap();
    assert_eq!(
        jobs.enqueue(PermsSyncSubject::User(bob), PermsSyncReason::Manual, None)
            .await
            .unwrap(),
        None
    );
}
