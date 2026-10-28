//! Access requests: `accessRequests`,
//! `accessRequestsCount` and `setAccessRequestStatus`. Site admins only. Visitors submit
//! requests at `POST /.api/request-access`.
//!
//! Approving creates the user (named after the email's local part, suffixed when taken, with
//! the email unverified) and returns a single-use account setup link valid for
//! [`sourcegull_db::INVITE_TTL_HOURS`] hours. SMTP-configured sites also mail the link.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::{AccessRequest, AccessRequestStatus as Status};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

use crate::audit::{self, events};
use crate::context::{
    auth_error, codes, db_error, error, page_size, require_site_admin, services, to_int,
};

const KIND: &str = "AccessRequest";

/// The state of an access request.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccessRequestStatus {
    Pending,
    Approved,
    Rejected,
}

impl From<AccessRequestStatus> for Status {
    fn from(s: AccessRequestStatus) -> Self {
        match s {
            AccessRequestStatus::Pending => Self::Pending,
            AccessRequestStatus::Approved => Self::Approved,
            AccessRequestStatus::Rejected => Self::Rejected,
        }
    }
}

impl From<Status> for AccessRequestStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::Pending => Self::Pending,
            Status::Approved => Self::Approved,
            Status::Rejected => Self::Rejected,
        }
    }
}

/// A visitor's request for an account.
pub struct AccessRequestNode(AccessRequest);

#[Object(name = "AccessRequest")]
impl AccessRequestNode {
    async fn id(&self) -> ID {
        ID(marshal_id(KIND, self.0.id))
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn email(&self) -> &str {
        &self.0.email
    }

    async fn additional_info(&self) -> &str {
        &self.0.additional_info
    }

    async fn status(&self) -> AccessRequestStatus {
        self.0.status.into()
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

/// A page of access requests.
#[derive(SimpleObject)]
pub struct AccessRequestConnection {
    nodes: Vec<AccessRequestNode>,
    total_count: i32,
}

/// The result of deciding a request.
#[derive(SimpleObject)]
pub struct AccessRequestDecision {
    request: AccessRequestNode,
    /// For an approval: the new user's single-use account setup URL. Deliver it to the
    /// applicant privately.
    reset_url: Option<String>,
}

fn parse_id(id: &str) -> Result<i64> {
    unmarshal_int_id(KIND, id).ok_or_else(|| {
        error(
            codes::INVALID_INPUT,
            format!("Invalid access request ID {id:?}."),
        )
    })
}

/// Access request queries.
#[derive(Default)]
pub struct AccessRequestsQuery;

#[Object]
impl AccessRequestsQuery {
    /// Access requests, oldest first, optionally in one state (site admins only). `after` is
    /// the ID of the last request of the previous page.
    async fn access_requests(
        &self,
        ctx: &Context<'_>,
        status: Option<AccessRequestStatus>,
        first: Option<i32>,
        after: Option<ID>,
    ) -> Result<AccessRequestConnection> {
        require_site_admin(ctx)?;
        let store = services(ctx)?.db.access_requests();
        let after = after.map(|id| parse_id(&id)).transpose()?.unwrap_or(0);
        let status = status.map(Status::from);
        let rows = store
            .list(status, after, page_size(first, 50, 100))
            .await
            .map_err(db_error)?;
        let total = store.count(status).await.map_err(db_error)?;
        Ok(AccessRequestConnection {
            nodes: rows.into_iter().map(AccessRequestNode).collect(),
            total_count: to_int(total),
        })
    }

    /// The number of access requests, optionally in one state (site admins only).
    async fn access_requests_count(
        &self,
        ctx: &Context<'_>,
        status: Option<AccessRequestStatus>,
    ) -> Result<i32> {
        require_site_admin(ctx)?;
        let n = services(ctx)?
            .db
            .access_requests()
            .count(status.map(Status::from))
            .await
            .map_err(db_error)?;
        Ok(to_int(n))
    }
}

/// Access request mutations.
#[derive(Default)]
pub struct AccessRequestsMutation;

#[Object]
impl AccessRequestsMutation {
    /// Approves or rejects a pending request (site admins only).
    async fn set_access_request_status(
        &self,
        ctx: &Context<'_>,
        id: ID,
        status: AccessRequestStatus,
    ) -> Result<AccessRequestDecision> {
        let admin = require_site_admin(ctx)?;
        if status == AccessRequestStatus::Pending {
            return Err(error(
                codes::INVALID_INPUT,
                "A decision must approve or reject the request.",
            ));
        }
        let svc = services(ctx)?;
        let store = svc.db.access_requests();
        let id = parse_id(&id)?;
        // Claim the request first, so that two admins cannot both decide it.
        let Some(request) = store
            .transition(id, Status::Pending, status.into(), Some(admin.user_id))
            .await
            .map_err(db_error)?
        else {
            return Err(match store.get(id).await.map_err(db_error)? {
                Some(_) => error(codes::CONFLICT, "The access request was already decided."),
                None => error(codes::NOT_FOUND, "Access request not found."),
            });
        };
        let mut reset_url = None;
        if status == AccessRequestStatus::Approved {
            let created = svc
                .auth
                .create_user_unique(
                    &username_from_email(&request.email),
                    Some(request.name.clone()),
                    None,
                    Some(&request.email),
                    false,
                    false,
                )
                .await;
            let user = match created {
                Ok(u) => u,
                Err(e) => {
                    // Undo the claim so that the request can be decided again.
                    if let Err(e) = store
                        .transition(id, status.into(), Status::Pending, None)
                        .await
                    {
                        tracing::warn!(error = %e, "reopening access request failed");
                    }
                    return Err(auth_error(e));
                }
            };
            let token = generate_session_id();
            store
                .create_invite(user.id, &hash_session_id(&token))
                .await
                .map_err(db_error)?;
            let origin = svc
                .config
                .load()
                .value
                .external_url
                .clone()
                .unwrap_or_default();
            reset_url = Some(format!(
                "{}/set-password?token={token}",
                origin.trim_end_matches('/')
            ));
            if let (Some(app), Some(url)) = (&svc.app, &reset_url) {
                app.send_email(sourcegull_services::email::link_message(
                    &request.email,
                    "Set up your account",
                    "Your access request was approved. Set your password with this link:",
                    url,
                ))
                .await;
            }
        }
        audit::record(
            svc,
            events::ACCESS_REQUEST_DECIDED,
            Some(admin.user_id),
            json!({ "accessRequest": id, "status": Status::from(status).as_str() }),
        )
        .await;
        Ok(AccessRequestDecision {
            request: AccessRequestNode(request),
            reset_url,
        })
    }
}

/// A valid username from the email's local part (`user` when nothing is left).
fn username_from_email(email: &str) -> String {
    let local = email.split('@').next().unwrap_or_default();
    let mut name = String::new();
    for c in local.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name: String = name.trim_matches('-').chars().take(200).collect();
    if sourcegull_authn::validate::validate_username(&name).is_ok() {
        name
    } else {
        "user".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_from_emails() {
        assert_eq!(username_from_email("jane.doe+x@example.com"), "jane-doe-x");
        assert_eq!(username_from_email("__@example.com"), "__");
        assert_eq!(username_from_email("...@example.com"), "user");
    }
}
