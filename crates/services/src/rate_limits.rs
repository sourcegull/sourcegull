//! Request limits on the endpoints anyone can call (`auth.rateLimits`), per client address and
//! per account, counted through [`AppState::rate_limits`].

use std::net::IpAddr;
use std::time::Duration;

use sourcegull_config::{AuthRateLimits, RequestLimit};
use sourcegull_ratelimit::Limit;
pub use sourcegull_ratelimit::Limited;

use crate::AppState;

/// One of the `auth.rateLimits` limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimit {
    /// Password sign-in attempts per client address.
    SignInPerAddress,
    /// Failed password sign-ins per account.
    FailedSignInsPerAccount,
    /// Password reset requests per client address.
    PasswordResetPerAddress,
    /// Password reset requests per email address.
    PasswordResetPerEmail,
    /// Access requests per client address.
    AccessRequestPerAddress,
    /// Access requests per email address.
    AccessRequestPerEmail,
    /// `/.oauth/authorize` requests per client address.
    OAuthAuthorizePerAddress,
    /// Rejected incoming webhook deliveries per client address.
    FailedWebhooksPerAddress,
}

impl RateLimit {
    fn name(self) -> &'static str {
        match self {
            Self::SignInPerAddress => "sign-in:addr",
            Self::FailedSignInsPerAccount => "sign-in-failed:account",
            Self::PasswordResetPerAddress => "password-reset:addr",
            Self::PasswordResetPerEmail => "password-reset:email",
            Self::AccessRequestPerAddress => "access-request:addr",
            Self::AccessRequestPerEmail => "access-request:email",
            Self::OAuthAuthorizePerAddress => "oauth-authorize:addr",
            Self::FailedWebhooksPerAddress => "webhook-failed:addr",
        }
    }

    fn configured(self, c: &AuthRateLimits) -> RequestLimit {
        match self {
            Self::SignInPerAddress => c.sign_in_per_address,
            Self::FailedSignInsPerAccount => c.failed_sign_ins_per_account,
            Self::PasswordResetPerAddress => c.password_reset_per_address,
            Self::PasswordResetPerEmail => c.password_reset_per_email,
            Self::AccessRequestPerAddress => c.access_request_per_address,
            Self::AccessRequestPerEmail => c.access_request_per_email,
            Self::OAuthAuthorizePerAddress => c.oauth_authorize_per_address,
            Self::FailedWebhooksPerAddress => c.failed_webhooks_per_address,
        }
    }
}

/// The subject of a per-address limit.
pub fn address(ip: Option<IpAddr>) -> String {
    sourcegull_authn::client_address(ip)
}

/// The subject of a per-account or per-email limit: case and surrounding space do not count.
pub fn account(login: &str) -> String {
    login.trim().to_lowercase()
}

impl AppState {
    fn rate_limit_of(&self, limit: RateLimit, subject: &str) -> Option<(String, Limit)> {
        let config = self.config.load();
        let c = &config.value.auth_rate_limits;
        if !c.enabled {
            return None;
        }
        let l = limit.configured(c);
        Some((
            format!("{}:{subject}", limit.name()),
            Limit::new(l.requests, Duration::from_secs(l.period.max(1))),
        ))
    }

    /// Counts one request of `subject` against `limit`; fails when it is over the limit.
    pub async fn rate_limit_hit(&self, limit: RateLimit, subject: &str) -> Result<(), Limited> {
        match self.rate_limit_of(limit, subject) {
            Some((key, l)) => self.rate_limits.hit(&key, l).await,
            None => Ok(()),
        }
    }

    /// Fails when `subject` already used up `limit`, without counting a request.
    pub async fn rate_limit_check(&self, limit: RateLimit, subject: &str) -> Result<(), Limited> {
        match self.rate_limit_of(limit, subject) {
            Some((key, l)) => self.rate_limits.check(&key, l).await,
            None => Ok(()),
        }
    }
}
