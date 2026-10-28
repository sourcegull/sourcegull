//! `orgs`, `org_members` and `org_invitations` store.

use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{Org, OrgId, OrgInvitationId, OrgMember, OrgRole, UserId};
use sqlx::PgPool;

const ORG_COLUMNS: &str = "id, name, display_name, created_at, updated_at, deleted_at";

const INVITATION_COLUMNS: &str = "id, org_id, sender_user_id, recipient_user_id, \
     recipient_email, created_at, notified_at, expires_at, responded_at, response_accepted, \
     revoked_at";

/// Pending invitations: not answered, not revoked, not expired.
const PENDING: &str = "responded_at IS NULL AND revoked_at IS NULL \
     AND (expires_at IS NULL OR expires_at > now())";

/// An invitation to join an org, sent to a user or an email address.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OrgInvitation {
    pub id: OrgInvitationId,
    pub org_id: OrgId,
    pub sender_user_id: UserId,
    pub recipient_user_id: Option<UserId>,
    pub recipient_email: Option<String>,
    pub created_at: DateTime<Utc>,
    pub notified_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub responded_at: Option<DateTime<Utc>>,
    /// `Some(true)` accepted, `Some(false)` declined, `None` no response yet.
    pub response_accepted: Option<bool>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl OrgInvitation {
    /// Whether the invitation can still be answered.
    pub fn is_pending(&self) -> bool {
        self.responded_at.is_none()
            && self.revoked_at.is_none()
            && self.expires_at.is_none_or(|e| e > Utc::now())
    }
}

/// The fields of a new invitation.
#[derive(Debug, Clone)]
pub struct NewOrgInvitation<'a> {
    pub org_id: OrgId,
    pub sender: UserId,
    pub recipient_user: Option<UserId>,
    pub recipient_email: Option<&'a str>,
    /// SHA-256 of the invitation link's token.
    pub token_sha256: &'a [u8],
    pub expires_at: Option<DateTime<Utc>>,
}

/// Reads and writes orgs and their members.
#[derive(Debug, Clone)]
pub struct OrgStore {
    pool: PgPool,
}

impl OrgStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates an org. Fails with [`DbError::AlreadyExists`] if the name is taken.
    pub async fn create(&self, name: &str, display_name: Option<&str>) -> Result<Org> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO orgs (name, display_name) VALUES ($1, $2) RETURNING {ORG_COLUMNS}"
        ))
        .bind(name)
        .bind(display_name)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Looks up a live org by id.
    pub async fn get(&self, id: OrgId) -> Result<Option<Org>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {ORG_COLUMNS} FROM orgs WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks up a live org by name, case-insensitively.
    pub async fn get_by_name(&self, name: &str) -> Result<Option<Org>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {ORG_COLUMNS} FROM orgs WHERE lower(name) = lower($1) AND deleted_at IS NULL"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Adds a member. Fails with [`DbError::AlreadyExists`] if already a member.
    pub async fn add_member(&self, org: OrgId, user: UserId, role: OrgRole) -> Result<OrgMember> {
        Ok(sqlx::query_as(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)
             RETURNING org_id, user_id, role, created_at",
        )
        .bind(org)
        .bind(user)
        .bind(role)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Removes a member. Fails with [`DbError::NotFound`] if not a member.
    pub async fn remove_member(&self, org: OrgId, user: UserId) -> Result<()> {
        let r = sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(user)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("org member"));
        }
        Ok(())
    }

    /// Whether the user is a member of the live org.
    pub async fn is_member(&self, org: OrgId, user: UserId) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM org_members m JOIN orgs o ON o.id = m.org_id
             WHERE m.org_id = $1 AND m.user_id = $2 AND o.deleted_at IS NULL)",
        )
        .bind(org)
        .bind(user)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Members of an org who are live users, oldest membership first.
    pub async fn list_members(&self, org: OrgId) -> Result<Vec<OrgMember>> {
        Ok(sqlx::query_as(
            "SELECT m.org_id, m.user_id, m.role, m.created_at
             FROM org_members m JOIN users u ON u.id = m.user_id
             WHERE m.org_id = $1 AND u.deleted_at IS NULL
             ORDER BY m.created_at, m.user_id",
        )
        .bind(org)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Orgs the user belongs to, by name.
    pub async fn list_for_user(&self, user: UserId) -> Result<Vec<Org>> {
        Ok(sqlx::query_as(
            "SELECT o.id, o.name, o.display_name, o.created_at, o.updated_at, o.deleted_at
             FROM orgs o JOIN org_members m ON m.org_id = o.id
             WHERE m.user_id = $1 AND o.deleted_at IS NULL ORDER BY o.name",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Live orgs whose name or display name contains `query` (case-insensitively), by name.
    pub async fn list(&self, query: Option<&str>, limit: i64, offset: i64) -> Result<Vec<Org>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {ORG_COLUMNS} FROM orgs WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR strpos(lower(name), lower($1)) > 0 \
                    OR strpos(lower(coalesce(display_name, '')), lower($1)) > 0) \
             ORDER BY lower(name), id LIMIT $2 OFFSET $3"
        ))
        .bind(query.filter(|q| !q.is_empty()))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of orgs [`list`](Self::list) matches.
    pub async fn count(&self, query: Option<&str>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM orgs WHERE deleted_at IS NULL \
               AND ($1::text IS NULL OR strpos(lower(name), lower($1)) > 0 \
                    OR strpos(lower(coalesce(display_name, '')), lower($1)) > 0)",
        )
        .bind(query.filter(|q| !q.is_empty()))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Sets the display name (`None` clears it). Fails with [`DbError::NotFound`] for a
    /// missing or deleted org.
    pub async fn update(&self, id: OrgId, display_name: Option<&str>) -> Result<Org> {
        sqlx::query_as(&format!(
            "UPDATE orgs SET display_name = $2, updated_at = now() \
             WHERE id = $1 AND deleted_at IS NULL RETURNING {ORG_COLUMNS}"
        ))
        .bind(id)
        .bind(display_name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DbError::NotFound("org"))
    }

    /// Soft-deletes an org; its name becomes free. Pending invitations are revoked.
    pub async fn soft_delete(&self, id: OrgId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE orgs SET deleted_at = now(), updated_at = now() \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("org"));
        }
        sqlx::query(&format!(
            "UPDATE org_invitations SET revoked_at = now() WHERE org_id = $1 AND {PENDING}"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The membership of `user` in the live org, if any.
    pub async fn get_member(&self, org: OrgId, user: UserId) -> Result<Option<OrgMember>> {
        Ok(sqlx::query_as(
            "SELECT m.org_id, m.user_id, m.role, m.created_at \
             FROM org_members m JOIN orgs o ON o.id = m.org_id \
             WHERE m.org_id = $1 AND m.user_id = $2 AND o.deleted_at IS NULL",
        )
        .bind(org)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// A page of the members who are live users, matching `query` against usernames and
    /// display names, oldest membership first.
    pub async fn search_members(
        &self,
        org: OrgId,
        query: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrgMember>> {
        Ok(sqlx::query_as(
            "SELECT m.org_id, m.user_id, m.role, m.created_at \
             FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND u.deleted_at IS NULL \
               AND ($2::text IS NULL OR strpos(lower(u.username), lower($2)) > 0 \
                    OR strpos(lower(coalesce(u.display_name, '')), lower($2)) > 0) \
             ORDER BY m.created_at, m.user_id LIMIT $3 OFFSET $4",
        )
        .bind(org)
        .bind(query.filter(|q| !q.is_empty()))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of members [`search_members`](Self::search_members) matches.
    pub async fn count_members(&self, org: OrgId, query: Option<&str>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND u.deleted_at IS NULL \
               AND ($2::text IS NULL OR strpos(lower(u.username), lower($2)) > 0 \
                    OR strpos(lower(coalesce(u.display_name, '')), lower($2)) > 0)",
        )
        .bind(org)
        .bind(query.filter(|q| !q.is_empty()))
        .fetch_one(&self.pool)
        .await?)
    }

    // ------------------------------------------------------------ invitations

    /// Creates an invitation.
    pub async fn create_invitation(&self, new: &NewOrgInvitation<'_>) -> Result<OrgInvitation> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO org_invitations (org_id, sender_user_id, recipient_user_id, \
                 recipient_email, token_sha256, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING {INVITATION_COLUMNS}"
        ))
        .bind(new.org_id)
        .bind(new.sender)
        .bind(new.recipient_user)
        .bind(new.recipient_email)
        .bind(new.token_sha256)
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// An invitation of a live org by id.
    pub async fn get_invitation(&self, id: OrgInvitationId) -> Result<Option<OrgInvitation>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM org_invitations i \
             WHERE id = $1 AND EXISTS (SELECT 1 FROM orgs o \
                 WHERE o.id = i.org_id AND o.deleted_at IS NULL)"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// An invitation of a live org by the SHA-256 of its link token.
    pub async fn get_invitation_by_token(&self, sha256: &[u8]) -> Result<Option<OrgInvitation>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM org_invitations i \
             WHERE token_sha256 = $1 AND EXISTS (SELECT 1 FROM orgs o \
                 WHERE o.id = i.org_id AND o.deleted_at IS NULL)"
        ))
        .bind(sha256)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The org's pending invitations, newest first.
    pub async fn pending_invitations(&self, org: OrgId) -> Result<Vec<OrgInvitation>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM org_invitations \
             WHERE org_id = $1 AND {PENDING} ORDER BY created_at DESC, id DESC"
        ))
        .bind(org)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Pending invitations of live orgs addressed to `user` or to one of `emails` (the user's
    /// verified addresses), newest first.
    pub async fn pending_invitations_for_user(
        &self,
        user: UserId,
        emails: &[String],
    ) -> Result<Vec<OrgInvitation>> {
        let emails: Vec<String> = emails.iter().map(|e| e.to_lowercase()).collect();
        Ok(sqlx::query_as(&format!(
            "SELECT {INVITATION_COLUMNS} FROM org_invitations i \
             WHERE (recipient_user_id = $1 OR lower(recipient_email) = ANY($2)) AND {PENDING} \
               AND EXISTS (SELECT 1 FROM orgs o WHERE o.id = i.org_id AND o.deleted_at IS NULL) \
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(user)
        .bind(&emails)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Replaces the link token of an invitation (resending makes a new link).
    pub async fn set_invitation_token(&self, id: OrgInvitationId, sha256: &[u8]) -> Result<()> {
        sqlx::query("UPDATE org_invitations SET token_sha256 = $2 WHERE id = $1")
            .bind(id)
            .bind(sha256)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records that the recipient was notified (email sent).
    pub async fn mark_notified(&self, id: OrgInvitationId) -> Result<()> {
        sqlx::query("UPDATE org_invitations SET notified_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Answers a pending invitation as `user`; accepting adds them as a member (no-op if they
    /// already are). Fails with [`DbError::NotFound`] if the invitation is not pending.
    pub async fn respond_to_invitation(
        &self,
        id: OrgInvitationId,
        user: UserId,
        accept: bool,
    ) -> Result<OrgInvitation> {
        let mut tx = self.pool.begin().await?;
        let inv: OrgInvitation = sqlx::query_as(&format!(
            "UPDATE org_invitations SET responded_at = now(), response_accepted = $2, \
                 recipient_user_id = coalesce(recipient_user_id, $3) \
             WHERE id = $1 AND {PENDING} RETURNING {INVITATION_COLUMNS}"
        ))
        .bind(id)
        .bind(accept)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DbError::NotFound("pending org invitation"))?;
        if accept {
            sqlx::query(
                "INSERT INTO org_members (org_id, user_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(inv.org_id)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(inv)
    }

    /// Revokes a pending invitation. Fails with [`DbError::NotFound`] if it is not pending.
    pub async fn revoke_invitation(&self, id: OrgInvitationId) -> Result<()> {
        let r = sqlx::query(&format!(
            "UPDATE org_invitations SET revoked_at = now() WHERE id = $1 AND {PENDING}"
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("pending org invitation"));
        }
        Ok(())
    }
}
