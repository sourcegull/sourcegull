//! The code host permissions sync: user-centric and repo-centric runs that rewrite the
//! `sync` rows of `user_repo_permissions` from what the code hosts report.
//!
//! - **User-centric** ([`PermsSyncer::sync_user`]): for every connection with an
//!   `authorization` block, find the user's account on that code host (their OAuth account for
//!   `identityProvider: oauth`, refreshing the token first when it expires within
//!   [`REFRESH_BEFORE`]; for `username`, the linked account or the code host user with the same
//!   username, which is then linked), list the projects it can read, and replace the user's
//!   synced grants on that code host's repositories. No account means no grants; a token the
//!   code host rejects is marked expired and its grants are removed (fail closed), and so are
//!   the grants of a token without the scope the listing needs (the state says to sign in
//!   again). Other errors keep the existing grants.
//! - **Repo-centric** ([`PermsSyncer::sync_repo`], providers that support it): list the
//!   repository's members and replace the repository's synced grants with the Sourcegull users
//!   linked to them, linking members whose code host username equals a Sourcegull username when
//!   the provider does not need user tokens.
//!
//! Providers are kept per connection (rebuilt when its config changes) so their API clients and
//! rate limiters are shared across runs, and at most [`CONCURRENCY_PER_CODE_HOST`] runs talk to
//! one connection at a time.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sourcegull_codehost::{
    AuthzProvider, CodeHostError, ExternalAccountData, OAuthLoginProvider, OAuthToken,
};
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{
    AccountSpec, Db, DbError, ExternalAccount, LinkAccount, PermsSource, PermsSyncResult,
};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalServiceId, ExternalServiceKind, RepoId, UserId};
use tokio::sync::Semaphore;

/// OAuth tokens expiring within this window are refreshed before use.
pub const REFRESH_BEFORE: Duration = Duration::from_secs(5 * 60);
/// Runs talking to one code host connection at the same time.
pub const CONCURRENCY_PER_CODE_HOST: usize = 2;

/// Why a run failed.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Some code hosts failed; what succeeded was saved (see the result's states).
    #[error("{message}")]
    Partial {
        result: PermsSyncResult,
        message: String,
    },
    /// Connection configs could not be read.
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// One connection's provider.
#[derive(Clone)]
struct Provider {
    id: ExternalServiceId,
    provider: Arc<dyn AuthzProvider>,
    permits: Arc<Semaphore>,
}

struct Cached {
    updated_at: DateTime<Utc>,
    provider: Option<Provider>,
}

/// The outcome for one connection, recorded in the job's `code_host_states`.
struct HostState {
    id: ExternalServiceId,
    /// The connection's code host type (`gitlab`) and instance URL.
    service_type: String,
    service_id: String,
    status: &'static str,
    message: String,
    /// The user must sign in again with this code host: the token lacks a scope
    /// (`missingScope`) or was rejected (`expired`).
    sign_in: Option<&'static str>,
}

impl HostState {
    fn json(&self) -> Value {
        let mut v = json!({
            "externalServiceID": self.id.get(),
            "serviceType": self.service_type,
            "serviceID": self.service_id,
            "status": self.status,
            "message": self.message,
        });
        if let Some(reason) = self.sign_in {
            v["signIn"] = json!(reason);
        }
        v
    }
}

/// Runs permissions syncs. Cheap to clone; clones share the provider cache.
#[derive(Clone)]
pub struct PermsSyncer {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    registry: Registry,
    config: SiteConfigHandle,
    providers: Arc<Mutex<HashMap<ExternalServiceId, Cached>>>,
}

impl std::fmt::Debug for PermsSyncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermsSyncer").finish_non_exhaustive()
    }
}

impl PermsSyncer {
    /// A syncer reading connection configs and tokens with `keyring`, and the OAuth apps
    /// (for token refresh) from `config`.
    pub fn new(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        registry: Registry,
        config: SiteConfigHandle,
    ) -> Self {
        Self {
            db,
            keyring,
            registry,
            config,
            providers: Arc::default(),
        }
    }

    /// The providers of every live connection with an `authorization` block.
    async fn providers(&self) -> Result<Vec<Provider>, SyncError> {
        let store = self.db.external_services(self.keyring.clone());
        let mut services = Vec::new();
        for kind in ExternalServiceKind::ALL {
            services.extend(
                store
                    .list_with_config(kind)
                    .await
                    .map_err(|e| SyncError::Config(e.to_string()))?,
            );
        }
        let mut cache = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.retain(|id, _| services.iter().any(|(s, _)| s.id == *id));
        let mut out = Vec::new();
        for (svc, text) in services {
            let fresh = cache
                .get(&svc.id)
                .is_some_and(|c| c.updated_at == svc.updated_at);
            if !fresh {
                let provider = match self
                    .registry
                    .for_connection(svc.id.get())
                    .authz_provider(svc.kind, &text)
                {
                    Ok(p) => p.map(|provider| Provider {
                        id: svc.id,
                        provider,
                        permits: Arc::new(Semaphore::new(CONCURRENCY_PER_CODE_HOST)),
                    }),
                    Err(e) => {
                        tracing::warn!(external_service = svc.id.get(), error = %e,
                            "connection has an unusable configuration; skipping its permissions");
                        None
                    }
                };
                cache.insert(
                    svc.id,
                    Cached {
                        updated_at: svc.updated_at,
                        provider,
                    },
                );
            }
            if let Some(p) = cache.get(&svc.id).and_then(|c| c.provider.clone()) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Connections whose providers support repo-centric sync (for the scheduler).
    pub async fn repo_centric_services(&self) -> Result<Vec<ExternalServiceId>, SyncError> {
        Ok(self
            .providers()
            .await?
            .into_iter()
            .filter(|p| p.provider.supports_repo_perms())
            .map(|p| p.id)
            .collect())
    }

    /// Syncs the repositories `user` can read on every code host. A deleted user is a no-op.
    pub async fn sync_user(&self, user: UserId) -> Result<PermsSyncResult, SyncError> {
        let Some(u) = self.db.users().get_by_id(user).await? else {
            return Ok(PermsSyncResult::default());
        };
        let emails: Vec<String> = self
            .db
            .user_emails()
            .list(user)
            .await?
            .into_iter()
            .filter(|e| e.verified_at.is_some())
            .map(|e| e.email)
            .collect();
        let accounts = self.db.external_accounts(self.keyring.clone());
        let perms = self.db.perms();
        let mut result = PermsSyncResult::default();
        let mut states = Vec::new();
        for p in self.providers().await? {
            let _permit = p.permits.acquire().await;
            let provider = &p.provider;
            let mut state = HostState {
                id: p.id,
                service_type: provider.service_type().to_owned(),
                service_id: provider.service_id().to_owned(),
                status: "ok",
                message: String::new(),
                sign_in: None,
            };
            let account = match self.account_for(&u.username, &emails, user, provider).await {
                Ok(a) => a,
                Err(e) => {
                    state.status = "error";
                    state.message = format!("finding the account: {e}");
                    states.push(state);
                    continue;
                }
            };
            let repos = match account {
                None => {
                    state.message = "no account on this code host".into();
                    Vec::new()
                }
                Some((ref row, ref data)) => match provider.fetch_user_perms(data).await {
                    Ok(found) => {
                        result.found += i32::try_from(found.external_ids.len()).unwrap_or(i32::MAX);
                        perms
                            .repos_by_external_ids(
                                provider.service_type(),
                                provider.service_id(),
                                &found.external_ids,
                            )
                            .await?
                    }
                    Err(CodeHostError::Unauthorized) if provider.needs_user_token() => {
                        if let Some(row) = row {
                            accounts.mark_expired(*row).await?;
                        }
                        state.status = "error";
                        state.message = "the code host rejected the user's token".into();
                        state.sign_in = Some("expired");
                        Vec::new()
                    }
                    // The token cannot list private repositories until the user signs in
                    // again with the scope.
                    Err(e @ CodeHostError::MissingScope(_)) => {
                        state.status = "error";
                        state.sign_in = Some("missingScope");
                        state.message = e.to_string();
                        Vec::new()
                    }
                    Err(e) => {
                        state.status = "error";
                        state.message = e.to_string();
                        states.push(state);
                        continue;
                    }
                },
            };
            let change = perms
                .set_user_sync_perms(user, provider.service_id(), &repos)
                .await?;
            result.added += i32::try_from(change.added).unwrap_or(i32::MAX);
            result.removed += i32::try_from(change.removed).unwrap_or(i32::MAX);
            states.push(state);
        }
        self.db.perms_sync_jobs().mark_user_synced(user).await?;
        finish(result, &states)
    }

    /// The user's account on `provider`'s code host, with its row id when linked: the OAuth
    /// account with a usable token, or (username identity) the linked or matching account.
    async fn account_for(
        &self,
        username: &str,
        emails: &[String],
        user: UserId,
        provider: &Arc<dyn AuthzProvider>,
    ) -> Result<
        Option<(
            Option<sourcegull_types::ExternalAccountId>,
            ExternalAccountData,
        )>,
        SyncError,
    > {
        let accounts = self.db.external_accounts(self.keyring.clone());
        let row = accounts
            .get_for_user(user, provider.service_type(), provider.service_id())
            .await?;
        if provider.needs_user_token() {
            let Some(row) = row else { return Ok(None) };
            return Ok(self
                .usable_token(&row)
                .await?
                .map(|data| (Some(row.id), data)));
        }
        if let Some(row) = row {
            let data = ExternalAccountData {
                account_id: row.account_id.clone(),
                username: account_username(&row),
                access_token: None,
                expires_at: None,
            };
            return Ok(Some((Some(row.id), data)));
        }
        let found = provider
            .fetch_account(username, emails)
            .await
            .map_err(|e| SyncError::Config(e.to_string()))?;
        let Some(data) = found else { return Ok(None) };
        let link = LinkAccount {
            spec: AccountSpec {
                service_type: provider.service_type(),
                service_id: provider.service_id(),
                account_id: &data.account_id,
            },
            client_id: "",
            account_data: json!({ "username": data.username }),
            auth_data: None,
        };
        let id = match accounts.link(user, &link).await {
            Ok(id) => Some(id),
            // Linked to another user, or this user has a different account linked.
            Err(DbError::AlreadyExists { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some((id, data)))
    }

    /// The account's OAuth token, refreshed when it expires soon. `None` when there is no
    /// usable token (none stored, marked expired, or the refresh was rejected).
    async fn usable_token(
        &self,
        row: &ExternalAccount,
    ) -> Result<Option<ExternalAccountData>, SyncError> {
        let accounts = self.db.external_accounts(self.keyring.clone());
        if row.expired_at.is_some() {
            return Ok(None);
        }
        let Some(raw) = &row.auth_data else {
            return Ok(None);
        };
        let Ok(mut token) = serde_json::from_slice::<OAuthToken>(raw) else {
            tracing::warn!(account = row.id.get(), "stored OAuth token is unreadable");
            return Ok(None);
        };
        let soon = Utc::now() + chrono::Duration::from_std(REFRESH_BEFORE).unwrap_or_default();
        let expiring = token.expires_at.is_some_and(|t| t <= soon);
        if expiring {
            let refreshed = match (&token.refresh_token, self.oauth_app(row)) {
                (Some(rt), Some(app)) => Some(app.refresh(rt).await),
                _ => None,
            };
            match refreshed {
                Some(Ok(new)) => {
                    let bytes =
                        serde_json::to_vec(&new).map_err(|e| SyncError::Config(e.to_string()))?;
                    accounts.set_auth_data(row.id, &bytes).await?;
                    token = new;
                }
                Some(Err(CodeHostError::Unauthorized)) => {
                    accounts.mark_expired(row.id).await?;
                    return Ok(None);
                }
                Some(Err(e)) => {
                    return Err(SyncError::Config(format!("refreshing the token: {e}")));
                }
                None if token.expires_at.is_some_and(|t| t <= Utc::now()) => {
                    accounts.mark_expired(row.id).await?;
                    return Ok(None);
                }
                None => {}
            }
        }
        Ok(Some(ExternalAccountData {
            account_id: row.account_id.clone(),
            username: account_username(row),
            access_token: Some(token.access_token),
            expires_at: token.expires_at,
        }))
    }

    /// The sign-in OAuth app that issued the account's token, from `auth.providers`.
    fn oauth_app(&self, row: &ExternalAccount) -> Option<Arc<dyn OAuthLoginProvider>> {
        let config = self.config.load();
        let external = config.value.external_url.as_deref()?;
        let kind = ExternalServiceKind::ALL
            .into_iter()
            .find(|k| k.service_type() == row.service_type)?;
        self.registry.account_oauth_app(
            kind,
            &config.value.auth_providers,
            &row.service_id,
            &row.client_id,
            external,
        )
    }

    /// Syncs the users who can read `repo` from its code host (repo-centric). A deleted repo,
    /// or one whose connections do not support repo-centric sync, is a no-op.
    pub async fn sync_repo(&self, repo: RepoId) -> Result<PermsSyncResult, SyncError> {
        // Internal: the sync job reads the repository it was given, for every viewer.
        #[allow(clippy::disallowed_methods)]
        let Some(r) = self
            .db
            .repos()
            .get(repo)
            .await?
            .filter(|r| r.deleted_at.is_none())
        else {
            return Ok(PermsSyncResult::default());
        };
        let sources: Vec<ExternalServiceId> = self
            .db
            .repos()
            .sources(repo)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let Some(p) = self
            .providers()
            .await?
            .into_iter()
            .find(|p| sources.contains(&p.id) && p.provider.supports_repo_perms())
        else {
            return Ok(PermsSyncResult::default());
        };
        let _permit = p.permits.acquire().await;
        let provider = &p.provider;
        let mut state = HostState {
            id: p.id,
            service_type: p.provider.service_type().to_owned(),
            service_id: p.provider.service_id().to_owned(),
            status: "ok",
            message: String::new(),
            sign_in: None,
        };
        let members = match provider.fetch_repo_perms(&r.external_id).await {
            Ok(m) => m,
            Err(e) => {
                state.status = "error";
                state.message = e.to_string();
                return finish(PermsSyncResult::default(), &[state]);
            }
        };
        let accounts = self.db.external_accounts(self.keyring.clone());
        let ids: Vec<String> = members.iter().map(|m| m.account_id.clone()).collect();
        let linked: BTreeMap<String, UserId> = accounts
            .users_for_accounts(provider.service_type(), provider.service_id(), &ids)
            .await?
            .into_iter()
            .collect();
        let mut users: Vec<UserId> = linked.values().copied().collect();
        if !provider.needs_user_token() {
            for m in members
                .iter()
                .filter(|m| !linked.contains_key(&m.account_id))
            {
                let Some(u) = self.db.users().get_by_username(&m.username).await? else {
                    continue;
                };
                let link = LinkAccount {
                    spec: AccountSpec {
                        service_type: provider.service_type(),
                        service_id: provider.service_id(),
                        account_id: &m.account_id,
                    },
                    client_id: "",
                    account_data: json!({ "username": m.username }),
                    auth_data: None,
                };
                match accounts.link(u.id, &link).await {
                    Ok(_) => users.push(u.id),
                    // The user is linked to a different account on this code host; skip
                    // rather than guess.
                    Err(DbError::AlreadyExists { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let change = self
            .db
            .perms()
            .set_repo_perms(repo, PermsSource::Sync, &users, None)
            .await?;
        let result = PermsSyncResult {
            added: i32::try_from(change.added).unwrap_or(i32::MAX),
            removed: i32::try_from(change.removed).unwrap_or(i32::MAX),
            found: i32::try_from(members.len()).unwrap_or(i32::MAX),
            code_host_states: Value::Null,
        };
        finish(result, &[state])
    }
}

fn account_username(row: &ExternalAccount) -> String {
    row.account_data["username"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Attaches the per-connection states; any error makes the run [`SyncError::Partial`].
fn finish(mut result: PermsSyncResult, states: &[HostState]) -> Result<PermsSyncResult, SyncError> {
    result.code_host_states = Value::Array(states.iter().map(HostState::json).collect());
    let errors: Vec<String> = states
        .iter()
        .filter(|s| s.status == "error")
        .map(|s| format!("connection {}: {}", s.id.get(), s.message))
        .collect();
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(SyncError::Partial {
            result,
            message: errors.join("; "),
        })
    }
}
