//! [`AuthError`]: authentication errors with stable machine codes.

/// Boxed internal error source.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors from authentication and credential management.
///
/// [`code`](AuthError::code) is a stable machine-readable code for API
/// clients; `Display` is a user-facing message that never leaks internals.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Unknown user, wrong password, no password set, or the account is locked for the client
    /// address after too many failures (the same error, so it reveals nothing).
    #[error("Incorrect username or password, or too many failed attempts. Try again later.")]
    InvalidCredentials,
    /// Self-service sign-up is disabled.
    #[error("Sign-up is disabled. Ask a site admin to create an account for you.")]
    SignupDisabled,
    /// The username is already in use.
    #[error("The username is already taken.")]
    UsernameTaken,
    /// The email address is already in use by another account.
    #[error("The email address is already in use.")]
    EmailTaken,
    /// The password does not satisfy the password policy.
    #[error("{0}")]
    WeakPassword(String),
    /// The username is not valid.
    #[error("{0}")]
    InvalidUsername(String),
    /// The email address is not valid.
    #[error("The email address is not valid.")]
    InvalidEmail,
    /// Invalid request input (e.g. bad scopes or expiry).
    #[error("{0}")]
    InvalidInput(String),
    /// The actor may not perform the operation.
    #[error("{0}")]
    Forbidden(String),
    /// The referenced user or token does not exist.
    #[error("{0} not found.")]
    NotFound(&'static str),
    /// The access token (or `Authorization` header) is malformed, unknown,
    /// expired, deleted or lacks the required scope.
    #[error("Invalid access token.")]
    TokenInvalid,
    /// Access tokens are disabled by `auth.accessTokens.allow`.
    #[error("Access tokens are disabled on this instance.")]
    TokensDisabled,
    /// An unexpected internal failure (database, hashing…).
    #[error("Internal error.")]
    Internal(#[source] BoxError),
}

impl AuthError {
    /// Stable machine code, e.g. `INVALID_CREDENTIALS`.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::InvalidCredentials => "INVALID_CREDENTIALS",
            AuthError::SignupDisabled => "SIGNUP_DISABLED",
            AuthError::UsernameTaken => "USERNAME_TAKEN",
            AuthError::EmailTaken => "EMAIL_TAKEN",
            AuthError::WeakPassword(_) => "WEAK_PASSWORD",
            AuthError::InvalidUsername(_) => "INVALID_USERNAME",
            AuthError::InvalidEmail => "INVALID_EMAIL",
            AuthError::InvalidInput(_) => "INVALID_INPUT",
            AuthError::Forbidden(_) => "FORBIDDEN",
            AuthError::NotFound(_) => "NOT_FOUND",
            AuthError::TokenInvalid => "TOKEN_INVALID",
            AuthError::TokensDisabled => "TOKENS_DISABLED",
            AuthError::Internal(_) => "INTERNAL",
        }
    }

    /// Suggested HTTP status code.
    pub fn http_status(&self) -> u16 {
        match self {
            AuthError::InvalidCredentials | AuthError::TokenInvalid => 401,
            AuthError::UsernameTaken | AuthError::EmailTaken => 409,
            AuthError::WeakPassword(_)
            | AuthError::InvalidUsername(_)
            | AuthError::InvalidEmail
            | AuthError::InvalidInput(_) => 400,
            AuthError::SignupDisabled | AuthError::Forbidden(_) | AuthError::TokensDisabled => 403,
            AuthError::NotFound(_) => 404,
            AuthError::Internal(_) => 500,
        }
    }

    pub(crate) fn forbidden(msg: impl Into<String>) -> Self {
        AuthError::Forbidden(msg.into())
    }

    pub(crate) fn internal(e: impl Into<BoxError>) -> Self {
        AuthError::Internal(e.into())
    }
}

impl From<sourcegull_db::DbError> for AuthError {
    fn from(e: sourcegull_db::DbError) -> Self {
        AuthError::Internal(Box::new(e))
    }
}

impl From<sourcegull_crypto::password::PasswordError> for AuthError {
    fn from(e: sourcegull_crypto::password::PasswordError) -> Self {
        AuthError::Internal(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_messages() {
        assert_eq!(AuthError::InvalidCredentials.code(), "INVALID_CREDENTIALS");
        assert_eq!(AuthError::TokensDisabled.http_status(), 403);
        let e = AuthError::internal("db exploded");
        assert_eq!(e.to_string(), "Internal error.");
        assert_eq!(e.code(), "INTERNAL");
    }
}
