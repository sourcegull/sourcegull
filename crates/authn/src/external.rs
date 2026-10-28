//! Sign-in through an external identity provider (GitLab OAuth): resolving the code host
//! account to a Sourcegull user, creating one when sign-up is allowed, and linking the account.

use serde_json::{Value, json};

use crate::authenticator::events;
use crate::validate::{validate_email, validate_username};
use crate::{AuthError, Authenticator};
use sourcegull_db::{AccountSpec, DbError, ExternalAccountStore, LinkAccount, NewAccount, NewUser};
use sourcegull_types::User;

/// Suffixed usernames tried when the code host username is taken (`alice-2` … `alice-9`).
const USERNAME_SUFFIXES: std::ops::RangeInclusive<u32> = 2..=9;

/// An identity an external provider vouched for.
#[derive(Clone)]
pub struct ExternalIdentity {
    /// `gitlab`.
    pub service_type: String,
    /// The code host instance (normalized base URL).
    pub service_id: String,
    /// The OAuth application that issued `auth_data`.
    pub client_id: String,
    /// The account id on the code host.
    pub account_id: String,
    /// The username on the code host (proposed as the Sourcegull username).
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub email: Option<String>,
    /// The provider confirmed `email` belongs to the account.
    pub email_verified: bool,
    /// Stored with the account (no secrets).
    pub account_data: Value,
    /// Token JSON, stored encrypted.
    pub auth_data: Option<Vec<u8>>,
}

impl std::fmt::Debug for ExternalIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalIdentity")
            .field("service_id", &self.service_id)
            .field("account_id", &self.account_id)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Shown when a first sign-in carries the verified email of an existing user it may not link to.
const EMAIL_IN_USE: &str = "A user with the email address of this account already exists. Sign in \
     as that user another way, or ask a site admin to link the accounts.";

/// The result of [`Authenticator::sign_in_external`].
#[derive(Debug, Clone)]
pub struct ExternalSignIn {
    pub user: User,
    /// A new Sourcegull account was created.
    pub created: bool,
}

impl Authenticator {
    /// Resolves an external identity to a user and links the account (storing its tokens):
    /// the user already linked to the account; else the user with the provider-verified email
    /// (see `link_by_email`); else a new user when `allow_signup` is set or no users exist yet
    /// (the first becomes site admin). Does not start a session.
    ///
    /// `link_by_email` is the provider's `linkByEmail`; without it, a first sign-in whose
    /// verified email belongs to an existing user fails.
    pub async fn sign_in_external(
        &self,
        accounts: &ExternalAccountStore,
        identity: &ExternalIdentity,
        allow_signup: bool,
        link_by_email: bool,
    ) -> Result<ExternalSignIn, AuthError> {
        self.resolve_external(accounts, identity, allow_signup, link_by_email, false)
            .await
    }

    /// Resolves an identity sent on every proxy request without logging a sign-in for each hit.
    /// The proxy is the authority on usernames, so an unlinked identity also matches the user
    /// with the same username.
    pub async fn authenticate_header_identity(
        &self,
        accounts: &ExternalAccountStore,
        identity: &ExternalIdentity,
    ) -> Result<ExternalSignIn, AuthError> {
        self.resolve_external(accounts, identity, true, true, true)
            .await
    }

    /// `header`: a trusted-proxy identity (see [`Self::authenticate_header_identity`]).
    async fn resolve_external(
        &self,
        accounts: &ExternalAccountStore,
        identity: &ExternalIdentity,
        allow_signup: bool,
        link_by_email: bool,
        header: bool,
    ) -> Result<ExternalSignIn, AuthError> {
        let log_signin = !header;
        let db = self.db();
        let spec = AccountSpec {
            service_type: &identity.service_type,
            service_id: &identity.service_id,
            account_id: &identity.account_id,
        };
        let mut user_id = accounts.user_for(spec).await?;
        if !log_signin && let Some(id) = user_id {
            let user = db
                .users()
                .get_by_id(id)
                .await?
                .ok_or(AuthError::NotFound("User"))?;
            return Ok(ExternalSignIn {
                user,
                created: false,
            });
        }
        if user_id.is_none() && header {
            user_id = db
                .users()
                .get_by_username(&identity.username)
                .await?
                .map(|u| u.id);
        }
        if user_id.is_none()
            && link_by_email
            && identity.email_verified
            && let Some(email) = &identity.email
        {
            user_id = db.user_emails().user_by_verified_email(email).await?;
        }
        let (user, created) = match user_id {
            Some(id) => (
                db.users()
                    .get_by_id(id)
                    .await?
                    .ok_or(AuthError::NotFound("User"))?,
                false,
            ),
            None => {
                if !allow_signup && !self.may_create_first_admin().await? {
                    return Err(AuthError::SignupDisabled);
                }
                let user = match self.create_external_user(identity, !allow_signup).await {
                    Err(AuthError::EmailTaken) => return Err(AuthError::forbidden(EMAIL_IN_USE)),
                    r => r?,
                };
                (user, true)
            }
        };
        let link = LinkAccount {
            spec,
            client_id: &identity.client_id,
            account_data: identity.account_data.clone(),
            auth_data: identity.auth_data.as_deref(),
        };
        match accounts.link(user.id, &link).await {
            Ok(_) => {}
            Err(DbError::AlreadyExists { .. }) => {
                return Err(AuthError::Forbidden(
                    "The user is linked to a different account on this code host.".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        }
        let provider =
            json!({ "provider": identity.service_type, "serviceID": identity.service_id });
        if created {
            self.log_event(events::SIGN_UP_SUCCEEDED, Some(user.id), provider.clone())
                .await;
        }
        if log_signin {
            self.log_event(events::SIGN_IN_SUCCEEDED, Some(user.id), provider)
                .await;
        }
        Ok(ExternalSignIn { user, created })
    }

    /// Creates the account of a first external sign-in, with the code host username (or a
    /// suffixed variant when taken) and the provider's email.
    /// `only_if_first`: sign-up is closed, so only the instance's first admin may be created
    /// (see [`NewAccount::only_if_first`]).
    async fn create_external_user(
        &self,
        identity: &ExternalIdentity,
        only_if_first: bool,
    ) -> Result<User, AuthError> {
        self.create_user_unique(
            &identity.username,
            identity.display_name.clone(),
            identity.avatar_url.clone(),
            identity.email.as_deref(),
            identity.email_verified,
            only_if_first,
        )
        .await
    }

    /// Creates a user named `username`, or `username-2` … `username-9` when taken, with
    /// `email` (dropped when invalid). Fails with [`AuthError::EmailTaken`] when another user
    /// has the email verified, and with [`AuthError::SignupDisabled`] when `only_if_first` is
    /// set and the user would not be the instance's first admin.
    pub async fn create_user_unique(
        &self,
        username: &str,
        display_name: Option<String>,
        avatar_url: Option<String>,
        email: Option<&str>,
        email_verified: bool,
        only_if_first: bool,
    ) -> Result<User, AuthError> {
        let base = username.trim();
        validate_username(base)?;
        let email = email.map(str::trim).filter(|e| validate_email(e).is_ok());
        let candidates = std::iter::once(base.to_owned())
            .chain(USERNAME_SUFFIXES.map(|n| format!("{base}-{n}")));
        for username in candidates {
            let new = NewAccount {
                user: NewUser {
                    username,
                    display_name: display_name.clone(),
                    avatar_url: avatar_url.clone(),
                    ..Default::default()
                },
                email: email.map(str::to_owned),
                email_verified,
                only_if_first,
            };
            match self.db().accounts().create(&new).await {
                Ok(c) => return Ok(c.user),
                Err(DbError::Invalid(_)) => return Err(AuthError::SignupDisabled),
                Err(DbError::AlreadyExists { constraint }) if constraint.contains("email") => {
                    return Err(AuthError::EmailTaken);
                }
                Err(DbError::AlreadyExists { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(AuthError::UsernameTaken)
    }
}
