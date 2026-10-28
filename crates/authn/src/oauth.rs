//! OAuth tokens of MCP clients: their format, authenticating an access token at the resource
//! it is bound to, and the grants users list and revoke (Connected apps). The protocol side
//! (discovery, registration, authorization and the token endpoint) is in `sourcegull-oauth-server`.

use crate::authenticator::events;
use crate::{Actor, AuthError, AuthVia, Authenticator};
use serde_json::json;
use sourcegull_crypto::access_token;
use sourcegull_db::{DbError, OAuthGrant};
use sourcegull_types::UserId;

/// Prefix of OAuth access tokens.
pub const ACCESS_TOKEN_PREFIX: &str = "sko_";
/// Prefix of OAuth refresh tokens.
pub const REFRESH_TOKEN_PREFIX: &str = "skr_";
/// The scope of OAuth grants: the MCP endpoint, as the user.
pub const SCOPE_MCP: &str = "mcp";

/// A new access token: the plaintext for the client and the hash to store.
pub fn generate_access_token() -> (String, [u8; 32]) {
    access_token::generate_with_prefix(ACCESS_TOKEN_PREFIX)
}

/// A new refresh token: the plaintext for the client and the hash to store.
pub fn generate_refresh_token() -> (String, [u8; 32]) {
    access_token::generate_with_prefix(REFRESH_TOKEN_PREFIX)
}

/// The stored hash of `token` if it is a well-formed OAuth access or refresh token.
pub fn token_hash(token: &str) -> Option<[u8; 32]> {
    let ok = access_token::parse_with_prefix(token, ACCESS_TOKEN_PREFIX).is_ok()
        || access_token::parse_with_prefix(token, REFRESH_TOKEN_PREFIX).is_ok();
    ok.then(|| access_token::hash_token(token))
}

/// The stored hash of `token` if it is a well-formed refresh token.
pub fn refresh_token_hash(token: &str) -> Option<[u8; 32]> {
    access_token::parse_with_prefix(token, REFRESH_TOKEN_PREFIX)
        .is_ok()
        .then(|| access_token::hash_token(token))
}

/// `resource` in the form grants store and compare: no fragment, no trailing slash, lowercase
/// scheme and host. `None` if it is not an absolute `http(s)` URL.
pub fn canonical_resource(resource: &str) -> Option<String> {
    let mut url = url::Url::parse(resource.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    url.set_fragment(None);
    let s = url.to_string();
    Some(s.trim_end_matches('/').to_owned())
}

impl Authenticator {
    /// Resolves an OAuth access token presented at `resource` (its canonical form, see
    /// [`canonical_resource`]). `Ok(None)` when `token` is not an OAuth access token (another
    /// kind of credential); [`AuthError::TokenInvalid`] when it is malformed, unknown, expired,
    /// revoked, bound to another resource, or when `mcp.oauth.enabled` is off.
    pub async fn authenticate_oauth_token(
        &self,
        token: &str,
        resource: &str,
    ) -> Result<Option<Actor>, AuthError> {
        if !token.starts_with(ACCESS_TOKEN_PREFIX) {
            return Ok(None);
        }
        if !self.config().load().value.mcp_oauth_enabled {
            return Err(AuthError::TokenInvalid);
        }
        access_token::parse_with_prefix(token, ACCESS_TOKEN_PREFIX)
            .map_err(|_| AuthError::TokenInvalid)?;
        let store = self.db().oauth();
        let record = store
            .access_token(&access_token::hash_token(token))
            .await?
            .ok_or(AuthError::TokenInvalid)?;
        if canonical_resource(&record.resource).as_deref() != Some(resource) {
            return Err(AuthError::TokenInvalid);
        }
        let via = AuthVia::OAuth {
            grant_id: record.grant_id,
            client_id: record.client_id,
            scopes: record.scopes,
        };
        let actor = self
            .actor_for(record.user_id, via)
            .await?
            .ok_or(AuthError::TokenInvalid)?;
        if let Err(e) = store.touch_grant(record.grant_id).await {
            tracing::warn!(error = %e, grant = record.grant_id, "updating OAuth grant last_used_at");
        }
        Ok(Some(actor))
    }

    /// Live OAuth grants of `user`, or of every user with `None` (site admins only).
    pub async fn list_oauth_grants(
        &self,
        actor: &Actor,
        user: Option<UserId>,
    ) -> Result<Vec<OAuthGrant>, AuthError> {
        let allowed = match user {
            Some(u) => actor.is_self_or_site_admin(u),
            None => actor.site_admin,
        };
        if !allowed {
            return Err(AuthError::forbidden(
                "You may only list your own connected apps.",
            ));
        }
        Ok(self.db().oauth().list_grants(user).await?)
    }

    /// Revokes an OAuth grant: its tokens stop working at once. Its user or a site admin only.
    pub async fn revoke_oauth_grant(&self, actor: &Actor, id: i64) -> Result<(), AuthError> {
        let store = self.db().oauth();
        let grant = store
            .grant(id)
            .await?
            .ok_or(AuthError::NotFound("Connected app"))?;
        if !actor.is_self_or_site_admin(grant.user_id) {
            return Err(AuthError::forbidden(
                "You may only revoke your own connected apps.",
            ));
        }
        match store.revoke_grant(id).await {
            Ok(()) => {}
            Err(DbError::NotFound(_)) => return Err(AuthError::NotFound("Connected app")),
            Err(e) => return Err(e.into()),
        }
        self.log_event(
            events::OAUTH_GRANT_REVOKED,
            Some(actor.user_id),
            json!({ "grantId": id, "userId": grant.user_id, "clientId": grant.client_id }),
        )
        .await;
        Ok(())
    }

    /// Revokes every OAuth grant of `user`, e.g. when the user is deactivated (`reason` goes
    /// into the security event). Never fails; errors are logged.
    pub async fn revoke_oauth_grants_for_user(&self, user: UserId, reason: &str) {
        match self.db().oauth().revoke_grants_for_user(user).await {
            Ok(0) => {}
            Ok(n) => {
                self.log_event(
                    events::OAUTH_GRANT_REVOKED,
                    Some(user),
                    json!({ "userId": user, "grants": n, "reason": reason }),
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(error = %e, user = user.get(), "revoking OAuth grants of a user failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_kinds() {
        let (access, hash) = generate_access_token();
        let (refresh, rhash) = generate_refresh_token();
        assert!(access.starts_with("sko_") && refresh.starts_with("skr_"));
        assert_eq!(token_hash(&access), Some(hash));
        assert_eq!(token_hash(&refresh), Some(rhash));
        assert_eq!(refresh_token_hash(&refresh), Some(rhash));
        assert_eq!(refresh_token_hash(&access), None);
        let (pat, _) = access_token::generate();
        assert_eq!(token_hash(&pat), None);
        assert_eq!(token_hash("sko_nope"), None);
    }

    #[test]
    fn canonical_resources() {
        assert_eq!(
            canonical_resource("HTTPS://Sourcegull.Example.com/.api/mcp/").as_deref(),
            Some("https://sourcegull.example.com/.api/mcp")
        );
        assert_eq!(
            canonical_resource("https://sourcegull.example.com/.api/mcp#x").as_deref(),
            Some("https://sourcegull.example.com/.api/mcp")
        );
        assert_eq!(
            canonical_resource("https://sourcegull.example.com").as_deref(),
            Some("https://sourcegull.example.com")
        );
        assert_eq!(canonical_resource("/.api/mcp"), None);
        assert_eq!(canonical_resource("ftp://x/y"), None);
    }
}
