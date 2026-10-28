//! [`Authenticator`]: built-in sign-up/sign-in, sessions and access tokens.

use crate::actor::{KNOWN_SCOPES, SCOPE_SITE_ADMIN_SUDO, SCOPE_USER_ALL};
use crate::header::{TokenCredential, parse_authorization};
use crate::session::{SessionStore, SessionToken};
use crate::validate::{validate_email, validate_password, validate_username};
use crate::{Actor, AuthError, AuthVia, PermissionSet};
use chrono::Utc;
use serde_json::{Value, json};
use sourcegull_config::{
    AccessTokenAllow, AuthProvider, BuiltinAuthProvider, ConfigHandle, SecurityEventLogLocation,
    SiteConfig,
};
use sourcegull_crypto::access_token;
use sourcegull_crypto::password::{PasswordParams, hash_password, verify_password};
use sourcegull_db::{
    AccessToken, Db, DbError, LockoutPolicy, NewAccessToken, NewAccount, NewSecurityEvent, NewUser,
};
use sourcegull_types::{AccessTokenId, User, UserId};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// How long resolved user info and permissions are cached per process.
const PRINCIPAL_CACHE_TTL: Duration = Duration::from_secs(30);

/// The key a client address is counted under by the lockout and rate limits: the address
/// itself for IPv4 (and IPv4-mapped IPv6), its /64 network for IPv6 (one host usually has a
/// whole /64), and `""` when the address is not known.
pub fn client_address(ip: Option<IpAddr>) -> String {
    match ip.map(|ip| ip.to_canonical()) {
        None => String::new(),
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// Security event names.
pub mod events {
    pub const SIGN_UP_SUCCEEDED: &str = "SignUpSucceeded";
    pub const SIGN_IN_SUCCEEDED: &str = "SignInSucceeded";
    pub const SIGN_IN_FAILED: &str = "SignInFailed";
    pub const ACCOUNT_LOCKED: &str = "AccountLocked";
    pub const SIGN_OUT_SUCCEEDED: &str = "SignOutSucceeded";
    pub const ACCESS_TOKEN_CREATED: &str = "AccessTokenCreated";
    pub const ACCESS_TOKEN_DELETED: &str = "AccessTokenDeleted";
    pub const ACCESS_TOKEN_SUDO: &str = "AccessTokenImpersonated";
    pub const PASSWORD_CHANGED: &str = "PasswordChanged";
    pub const USERNAME_CHANGED: &str = "UsernameChanged";
    pub const OAUTH_GRANT_CREATED: &str = "OAuthGrantCreated";
    pub const OAUTH_TOKEN_REFRESHED: &str = "OAuthTokenRefreshed";
    pub const OAUTH_REFRESH_TOKEN_REUSED: &str = "OAuthRefreshTokenReused";
    pub const OAUTH_CODE_REPLAYED: &str = "OAuthCodeReplayed";
    pub const OAUTH_GRANT_REVOKED: &str = "OAuthGrantRevoked";
}

/// User info needed to build an [`Actor`], cached briefly.
#[derive(Debug, Clone)]
struct Principal {
    username: String,
    site_admin: bool,
    permissions: PermissionSet,
}

/// The authentication service. Cheap to clone.
#[derive(Clone)]
pub struct Authenticator {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    sessions: Arc<dyn SessionStore>,
    config: ConfigHandle<SiteConfig>,
    params: PasswordParams,
    /// Hash verified against for unknown users, so they take as long as known ones.
    dummy_hash: tokio::sync::OnceCell<String>,
    principals: moka::future::Cache<UserId, Principal>,
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator").finish_non_exhaustive()
    }
}

impl Authenticator {
    /// Creates the service. `params` sets the argon2id cost of new hashes.
    pub fn new(
        db: Db,
        sessions: Arc<dyn SessionStore>,
        config: ConfigHandle<SiteConfig>,
        params: PasswordParams,
    ) -> Self {
        let principals = moka::future::Cache::builder()
            .max_capacity(100_000)
            .time_to_live(PRINCIPAL_CACHE_TTL)
            .build();
        Self {
            inner: Arc::new(Inner {
                db,
                sessions,
                config,
                params,
                dummy_hash: tokio::sync::OnceCell::new(),
                principals,
            }),
        }
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.db
    }

    pub(crate) fn config(&self) -> &ConfigHandle<SiteConfig> {
        &self.inner.config
    }

    /// The session store.
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.inner.sessions
    }

    /// The built-in provider's config, if it is enabled.
    fn builtin(&self) -> Option<BuiltinAuthProvider> {
        self.inner
            .config
            .load()
            .value
            .auth_providers
            .iter()
            .find_map(|p| match p {
                AuthProvider::Builtin(b) => Some(b.clone()),
                _ => None,
            })
    }

    /// Whether password sign-in (the built-in provider) is enabled.
    pub fn builtin_enabled(&self) -> bool {
        self.builtin().is_some()
    }

    /// Whether self-service sign-up is open (built-in provider with `allowSignup`).
    /// The first user may always sign up, see [`needs_site_init`](Self::needs_site_init).
    pub fn signup_allowed(&self) -> bool {
        self.builtin().is_some_and(|b| b.allow_signup)
    }

    /// Whether no users exist yet (the next sign-up becomes site admin).
    pub async fn needs_site_init(&self) -> Result<bool, AuthError> {
        Ok(self.inner.db.users().count().await? == 0)
    }

    /// Whether a sign-up may proceed although sign-up is closed, because it could create a
    /// site admin: no users exist yet, or site admins are declared (the account is then created
    /// only for a declared, verified email; see `sourcegull_db::NewAccount::only_if_first`).
    pub async fn may_create_first_admin(&self) -> Result<bool, AuthError> {
        if self.needs_site_init().await? {
            return Ok(true);
        }
        Ok(!self
            .inner
            .db
            .declared()
            .site_admin_emails()
            .await?
            .is_empty())
    }

    /// Checks `password` against `auth.passwordPolicy`.
    pub fn check_password_policy(&self, password: &str) -> Result<(), AuthError> {
        validate_password(
            password,
            &self.inner.config.load().value.auth_password_policy,
        )
    }

    /// Sets `user`'s password (after the policy check), e.g. from an approved access request's
    /// setup link.
    pub async fn set_password(&self, user: UserId, password: &str) -> Result<(), AuthError> {
        self.check_password_policy(password)?;
        let hash = self.hash(password).await?;
        self.inner
            .db
            .users()
            .set_password_hash(user, Some(&hash))
            .await?;
        self.invalidate_user(user).await;
        self.log_event(events::PASSWORD_CHANGED, Some(user), json!({}))
            .await;
        Ok(())
    }

    /// Changes the signed-in user's own password (built-in provider only). `old` must match the
    /// stored password when the user has one. Ends the user's other sessions, keeping `keep`
    /// (the session the change was made from, if any).
    pub async fn change_password(
        &self,
        user: UserId,
        old: Option<&str>,
        new: &str,
        keep: Option<&SessionToken>,
    ) -> Result<(), AuthError> {
        if self.builtin().is_none() {
            return Err(AuthError::forbidden("Password sign-in is not enabled."));
        }
        let users = self.inner.db.users();
        if let Some(phc) = users.password_hash(user).await? {
            let old = old.unwrap_or_default();
            if !self.verify(old, Some(phc)).await? {
                return Err(AuthError::InvalidInput(
                    "The current password is incorrect.".into(),
                ));
            }
        }
        self.set_password(user, new).await?;
        match keep {
            Some(keep) => self.inner.sessions.delete_other_sessions(user, keep).await,
            None => self.inner.sessions.delete_for_user(user).await,
        }
    }

    /// Ends every session of `user` and revokes their OAuth grants (after a deactivation).
    pub async fn end_user_sessions(&self, user: UserId) -> Result<(), AuthError> {
        self.inner.sessions.delete_for_user(user).await?;
        self.revoke_oauth_grants_for_user(user, "user deactivated")
            .await;
        self.invalidate_user(user).await;
        Ok(())
    }

    /// After a password reset: ends every session of `user`, deletes their access tokens and
    /// revokes their OAuth grants, since whoever held the old password may have created them.
    pub async fn end_user_credentials(&self, user: UserId) -> Result<(), AuthError> {
        self.inner.sessions.delete_for_user(user).await?;
        let deleted = self.inner.db.access_tokens().delete_for_user(user).await?;
        if deleted > 0 {
            self.log_event(
                events::ACCESS_TOKEN_DELETED,
                Some(user),
                json!({ "subjectUserId": user, "tokens": deleted, "reason": "password reset" }),
            )
            .await;
        }
        self.revoke_oauth_grants_for_user(user, "password reset")
            .await;
        self.invalidate_user(user).await;
        Ok(())
    }

    /// Session lifetime from `auth.sessionExpiry`.
    pub fn session_ttl(&self) -> Duration {
        self.inner.config.load().value.auth_session_expiry.get()
    }

    /// Creates a built-in account. Allowed when `allowSignup` is set, or when
    /// no users exist yet; the first user becomes site admin (decided
    /// atomically in the database) and their email is marked verified. Other
    /// users' emails start unverified. Does not sign the user in; call
    /// [`start_session`](Self::start_session) for that.
    pub async fn sign_up(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<User, AuthError> {
        let username = username.trim();
        let email = email.trim();
        validate_username(username)?;
        validate_email(email)?;
        let policy = self.inner.config.load().value.auth_password_policy.clone();
        validate_password(password, &policy)?;
        let db = &self.inner.db;
        let allow = self.signup_allowed();
        // Cheap early exit before hashing; the authoritative check is atomic below.
        if !allow && !self.may_create_first_admin().await? {
            return Err(AuthError::SignupDisabled);
        }
        if db
            .user_emails()
            .user_by_verified_email(email)
            .await?
            .is_some()
        {
            return Err(AuthError::EmailTaken);
        }
        let hash = self.hash(password).await?;
        let created = db
            .accounts()
            .create(&NewAccount {
                user: NewUser {
                    username: username.to_owned(),
                    password_hash: Some(hash),
                    ..Default::default()
                },
                email: Some(email.to_owned()),
                email_verified: false,
                only_if_first: !allow,
            })
            .await
            .map_err(|e| match e {
                DbError::Invalid(_) => AuthError::SignupDisabled,
                DbError::AlreadyExists { constraint } if constraint.contains("email") => {
                    AuthError::EmailTaken
                }
                DbError::AlreadyExists { .. } => AuthError::UsernameTaken,
                other => other.into(),
            })?;
        let user = created.user;
        self.log_event(
            events::SIGN_UP_SUCCEEDED,
            Some(user.id),
            json!({ "firstUser": created.first_user }),
        )
        .await;
        Ok(user)
    }

    /// Signs in with a username (or verified email) and password, returning
    /// the user and a new session. Applies `auth.lockout` per account and client address, so
    /// failures from one address do not lock the account for others. Unknown users, wrong
    /// passwords and locked accounts get the same error and cost the same password
    /// verification.
    pub async fn sign_in(
        &self,
        username_or_email: &str,
        password: &str,
        client_ip: Option<IpAddr>,
    ) -> Result<(User, SessionToken), AuthError> {
        if self.builtin().is_none() {
            return Err(AuthError::forbidden("Password sign-in is not enabled."));
        }
        let login = username_or_email.trim();
        let ip = client_ip.map(|ip| ip.to_string());
        let address = client_address(client_ip);
        let users = self.inner.db.users();
        let mut user = users.get_by_username(login).await?;
        if user.is_none() && login.contains('@') {
            if let Some(id) = self
                .inner
                .db
                .user_emails()
                .user_by_verified_email(login)
                .await?
            {
                user = users.get_by_id(id).await?;
            }
        }
        let locked = match &user {
            Some(u) => users.login_locked_until(u.id, &address).await?,
            None => None,
        };
        let phc = match &user {
            Some(u) => users.password_hash(u.id).await?,
            None => None,
        };
        let ok = self.verify(password, phc).await?;
        if let (Some(u), Some(until)) = (&user, locked) {
            self.log_event(
                events::SIGN_IN_FAILED,
                Some(u.id),
                json!({ "reason": "locked", "ip": ip, "until": until }),
            )
            .await;
            return Err(AuthError::InvalidCredentials);
        }
        let user = match user {
            Some(u) if ok => u,
            other => {
                let id = other.map(|u| u.id);
                return Err(self.failed_sign_in(id, login, ip, &address).await?);
            }
        };
        users.clear_failed_logins_from(user.id, &address).await?;
        let token = self.start_session(user.id).await?;
        self.log_event(
            events::SIGN_IN_SUCCEEDED,
            Some(user.id),
            json!({ "ip": ip }),
        )
        .await;
        Ok((user, token))
    }

    /// Records a failed sign-in and returns the error to report.
    async fn failed_sign_in(
        &self,
        user: Option<UserId>,
        login: &str,
        ip: Option<String>,
        address: &str,
    ) -> Result<AuthError, AuthError> {
        let Some(id) = user else {
            self.log_event(
                events::SIGN_IN_FAILED,
                None,
                json!({ "reason": "unknown user", "login": login, "ip": ip }),
            )
            .await;
            return Ok(AuthError::InvalidCredentials);
        };
        let lockout = self.inner.config.load().value.auth_lockout.clone();
        let policy = LockoutPolicy {
            max_failures: i32::try_from(lockout.failed_attempt_threshold.max(1))
                .unwrap_or(i32::MAX),
            window: Duration::from_secs(lockout.consecutive_period),
            lockout: Duration::from_secs(lockout.lockout_period),
        };
        let failure = self
            .inner
            .db
            .users()
            .record_failed_login(id, address, policy)
            .await?;
        self.log_event(
            events::SIGN_IN_FAILED,
            Some(id),
            json!({ "reason": "invalid credentials", "ip": ip }),
        )
        .await;
        if let Some(until) = failure.locked_until.filter(|t| *t > Utc::now()) {
            self.log_event(
                events::ACCOUNT_LOCKED,
                Some(id),
                json!({ "until": until, "ip": ip }),
            )
            .await;
        }
        Ok(AuthError::InvalidCredentials)
    }

    /// Creates a session for `user_id` with the configured lifetime.
    pub async fn start_session(&self, user_id: UserId) -> Result<SessionToken, AuthError> {
        self.inner
            .sessions
            .create(user_id, self.session_ttl())
            .await
    }

    /// Ends a session. Unknown tokens are ignored.
    pub async fn sign_out(&self, token: &SessionToken) -> Result<(), AuthError> {
        let session = self.inner.sessions.get(token).await?;
        self.inner.sessions.delete(token).await?;
        if let Some(s) = session {
            self.log_event(events::SIGN_OUT_SUCCEEDED, Some(s.user_id), Value::Null)
                .await;
        }
        Ok(())
    }

    /// Resolves a session cookie to an actor. `Ok(None)` if the session is
    /// unknown or expired or its user was deleted.
    pub async fn authenticate_session(
        &self,
        token: &SessionToken,
    ) -> Result<Option<Actor>, AuthError> {
        let Some(session) = self.inner.sessions.get(token).await? else {
            return Ok(None);
        };
        self.actor_for(session.user_id, AuthVia::Session).await
    }

    /// Resolves an `Authorization` header value. `Ok(None)` when the header
    /// is not a `token`/`token-sudo` credential; [`AuthError::TokenInvalid`]
    /// when the token is malformed (checksum checked before any database
    /// lookup), unknown, expired, deleted or lacks the needed scope;
    /// [`AuthError::TokensDisabled`] when `auth.accessTokens.allow` is `none`.
    pub async fn authenticate_access_token(
        &self,
        header_value: &str,
    ) -> Result<Option<Actor>, AuthError> {
        let Some(cred) = parse_authorization(header_value)? else {
            return Ok(None);
        };
        if self.inner.config.load().value.auth_access_tokens.allow == AccessTokenAllow::None {
            return Err(AuthError::TokensDisabled);
        }
        let (token, sudo_user) = match &cred {
            TokenCredential::Token(t) => (t.as_str(), None),
            TokenCredential::Sudo { user, token } => (token.as_str(), Some(user.as_str())),
        };
        access_token::parse(token).map_err(|_| AuthError::TokenInvalid)?;
        let tokens = self.inner.db.access_tokens();
        let record = tokens
            .lookup_by_hash(&access_token::hash_token(token))
            .await?
            .ok_or(AuthError::TokenInvalid)?;
        let has_scope = |s: &str| record.scopes.iter().any(|x| x == s);
        let actor = match sudo_user {
            None => {
                if !has_scope(SCOPE_USER_ALL) {
                    return Err(AuthError::TokenInvalid);
                }
                let via = AuthVia::Token {
                    id: record.id,
                    scopes: record.scopes.clone(),
                };
                self.actor_for(record.subject_user_id, via)
                    .await?
                    .ok_or(AuthError::TokenInvalid)?
            }
            Some(username) => {
                if !has_scope(SCOPE_SITE_ADMIN_SUDO) {
                    return Err(AuthError::TokenInvalid);
                }
                let admin = self
                    .principal(record.subject_user_id)
                    .await?
                    .ok_or(AuthError::TokenInvalid)?;
                if !admin.site_admin {
                    return Err(AuthError::forbidden("Only site admins may use token-sudo."));
                }
                let target = self
                    .inner
                    .db
                    .users()
                    .get_by_username(username)
                    .await?
                    .ok_or(AuthError::NotFound("User"))?;
                let via = AuthVia::Sudo {
                    token_id: record.id,
                    by_user_id: record.subject_user_id,
                };
                let actor = self
                    .actor_for(target.id, via)
                    .await?
                    .ok_or(AuthError::NotFound("User"))?;
                self.log_event(
                    events::ACCESS_TOKEN_SUDO,
                    Some(record.subject_user_id),
                    json!({ "tokenId": record.id, "asUser": target.id }),
                )
                .await;
                actor
            }
        };
        if let Err(e) = tokens.touch_last_used(record.id).await {
            tracing::warn!(error = %e, token_id = %record.id, "updating access token last_used_at");
        }
        Ok(Some(actor))
    }

    /// Creates an access token for `target_user` and returns its id and the
    /// plaintext (shown once). Enforces `auth.accessTokens`: `allow` (`all`:
    /// users create their own tokens, site admins for anyone;
    /// `site-admin-create`: only site admins; `none`: disabled) and
    /// `maxTokenExpiresInDays` (`expires_in = None` means the maximum).
    /// Scopes must include `user:all`; `site-admin:sudo` is for site admins only.
    pub async fn create_access_token(
        &self,
        actor: &Actor,
        target_user: UserId,
        scopes: &[String],
        note: &str,
        expires_in: Option<Duration>,
    ) -> Result<(AccessTokenId, String), AuthError> {
        let policy = self.inner.config.load().value.auth_access_tokens.clone();
        match policy.allow {
            AccessTokenAllow::None => return Err(AuthError::TokensDisabled),
            AccessTokenAllow::SiteAdminCreate if !actor.site_admin => {
                return Err(AuthError::forbidden(
                    "Only site admins may create access tokens.",
                ));
            }
            _ => {}
        }
        if !actor.is_self_or_site_admin(target_user) {
            return Err(AuthError::forbidden(
                "You may only create access tokens for yourself.",
            ));
        }
        let mut scopes: Vec<String> = scopes.to_vec();
        scopes.sort();
        scopes.dedup();
        if let Some(bad) = scopes.iter().find(|s| !KNOWN_SCOPES.contains(&s.as_str())) {
            return Err(AuthError::InvalidInput(format!(
                "Unknown access token scope {bad:?}."
            )));
        }
        if !scopes.iter().any(|s| s == SCOPE_USER_ALL) {
            return Err(AuthError::InvalidInput(format!(
                "Access tokens must include the {SCOPE_USER_ALL} scope."
            )));
        }
        if scopes.iter().any(|s| s == SCOPE_SITE_ADMIN_SUDO) && !actor.site_admin {
            return Err(AuthError::forbidden(format!(
                "Only site admins may create tokens with the {SCOPE_SITE_ADMIN_SUDO} scope."
            )));
        }
        let max_days = policy.max_token_expires_in_days.max(1);
        let max = Duration::from_secs(u64::from(max_days) * 86_400);
        let lifetime = expires_in.unwrap_or(max);
        if lifetime.is_zero() || lifetime > max {
            return Err(AuthError::InvalidInput(format!(
                "Access token expiry must be between 1 second and {max_days} days."
            )));
        }
        let expires_at = chrono::Duration::from_std(lifetime)
            .ok()
            .and_then(|d| Utc::now().checked_add_signed(d))
            .ok_or_else(|| AuthError::InvalidInput("Invalid access token expiry.".into()))?;
        if self
            .inner
            .db
            .users()
            .get_by_id(target_user)
            .await?
            .is_none()
        {
            return Err(AuthError::NotFound("User"));
        }
        let (plaintext, hash) = access_token::generate();
        let prefix = access_token::prefix(&plaintext).unwrap_or_default();
        let id = self
            .inner
            .db
            .access_tokens()
            .create(&NewAccessToken {
                subject_user_id: target_user,
                creator_user_id: actor.user_id,
                hash: &hash,
                prefix,
                scopes: &scopes,
                note,
                expires_at: Some(expires_at),
            })
            .await?;
        self.log_event(
            events::ACCESS_TOKEN_CREATED,
            Some(actor.user_id),
            json!({ "tokenId": id, "subjectUserId": target_user, "scopes": scopes }),
        )
        .await;
        Ok((id, plaintext))
    }

    /// Lists `user`'s tokens (without secrets). Self or site admin only.
    pub async fn list_access_tokens(
        &self,
        actor: &Actor,
        user: UserId,
    ) -> Result<Vec<AccessToken>, AuthError> {
        if !actor.is_self_or_site_admin(user) {
            return Err(AuthError::forbidden(
                "You may only list your own access tokens.",
            ));
        }
        Ok(self.inner.db.access_tokens().list_for_user(user).await?)
    }

    /// Deletes (revokes) a token. Its subject or a site admin only.
    pub async fn delete_access_token(
        &self,
        actor: &Actor,
        id: AccessTokenId,
    ) -> Result<(), AuthError> {
        let tokens = self.inner.db.access_tokens();
        let token = tokens
            .get(id)
            .await?
            .ok_or(AuthError::NotFound("Access token"))?;
        if !actor.is_self_or_site_admin(token.subject_user_id) {
            return Err(AuthError::forbidden(
                "You may only delete your own access tokens.",
            ));
        }
        match tokens.delete(id, Some(token.subject_user_id)).await {
            Ok(()) => {}
            Err(DbError::NotFound(_)) => return Err(AuthError::NotFound("Access token")),
            Err(e) => return Err(e.into()),
        }
        self.log_event(
            events::ACCESS_TOKEN_DELETED,
            Some(actor.user_id),
            json!({ "tokenId": id, "subjectUserId": token.subject_user_id }),
        )
        .await;
        Ok(())
    }

    /// Builds the actor for a live user, using the per-process cache of
    /// username, site-admin flag and permissions (30 s TTL).
    pub async fn actor_for(
        &self,
        user_id: UserId,
        via: AuthVia,
    ) -> Result<Option<Actor>, AuthError> {
        Ok(self.principal(user_id).await?.map(|p| Actor {
            user_id,
            username: p.username,
            site_admin: p.site_admin,
            via,
            permissions: p.permissions,
        }))
    }

    /// Drops cached user info (call after changing a user's site-admin flag,
    /// roles or username, or deleting them).
    pub async fn invalidate_user(&self, user_id: UserId) {
        self.inner.principals.invalidate(&user_id).await;
    }

    async fn principal(&self, user_id: UserId) -> Result<Option<Principal>, AuthError> {
        if let Some(p) = self.inner.principals.get(&user_id).await {
            return Ok(Some(p));
        }
        let Some(user) = self.inner.db.users().get_by_id(user_id).await? else {
            return Ok(None);
        };
        let pairs = self.inner.db.rbac().permissions_for_user(user_id).await?;
        let principal = Principal {
            username: user.username,
            site_admin: user.site_admin,
            permissions: PermissionSet::from_pairs(
                pairs.iter().map(|(n, a)| (n.as_str(), a.as_str())),
            ),
        };
        self.inner
            .principals
            .insert(user_id, principal.clone())
            .await;
        Ok(Some(principal))
    }

    async fn hash(&self, password: &str) -> Result<String, AuthError> {
        let password = password.to_owned();
        let params = self.inner.params;
        tokio::task::spawn_blocking(move || hash_password(&password, &params))
            .await
            .map_err(AuthError::internal)?
            .map_err(Into::into)
    }

    /// Verifies `password` against `phc`, or against a dummy hash when there
    /// is none (so unknown users take as long), returning false in that case.
    async fn verify(&self, password: &str, phc: Option<String>) -> Result<bool, AuthError> {
        let (phc, real) = match phc {
            Some(p) => (p, true),
            None => {
                let dummy = self
                    .inner
                    .dummy_hash
                    .get_or_try_init(|| self.hash("sourcegull-dummy-password"))
                    .await?;
                (dummy.clone(), false)
            }
        };
        let password = password.to_owned();
        let ok = tokio::task::spawn_blocking(move || verify_password(&password, &phc))
            .await
            .map_err(AuthError::internal)?;
        match ok {
            Ok(ok) => Ok(ok && real),
            // A corrupt stored hash must not lock the user out with a 500 forever,
            // nor let them in: treat it as a mismatch.
            Err(e) => {
                tracing::error!(error = %e, "stored password hash is invalid");
                Ok(false)
            }
        }
    }

    /// Records a security event per `log.securityEventLog.location`. Never fails. Sign-ins
    /// and sign-ups are also usage events (`event_logs`, without details) for the admin
    /// analytics.
    pub async fn log_event(&self, name: &str, user_id: Option<UserId>, argument: Value) {
        if user_id.is_some()
            && matches!(name, events::SIGN_IN_SUCCEEDED | events::SIGN_UP_SUCCEEDED)
        {
            let usage = sourcegull_db::NewUsageEvent {
                name: name.to_owned(),
                user_id,
                source: "BACKEND".to_owned(),
                ..Default::default()
            };
            if let Err(e) = self.inner.db.event_logs().insert(&[usage]).await {
                tracing::warn!(error = %e, event = name, "recording usage event");
            }
        }
        let location = self
            .inner
            .config
            .load()
            .value
            .log
            .security_event_log
            .location;
        if matches!(
            location,
            SecurityEventLogLocation::Logs | SecurityEventLogLocation::All
        ) {
            tracing::info!(
                target: "security_event",
                event = name,
                user_id = user_id.map(|u| u.get()),
                argument = %argument,
            );
        }
        if matches!(
            location,
            SecurityEventLogLocation::Database | SecurityEventLogLocation::All
        ) {
            let event = NewSecurityEvent {
                name: name.to_owned(),
                user_id,
                source: "BACKEND".to_owned(),
                argument,
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            };
            if let Err(e) = self.inner.db.security_events().insert(&event).await {
                tracing::warn!(error = %e, event = name, "recording security event");
            }
        }
    }
}
