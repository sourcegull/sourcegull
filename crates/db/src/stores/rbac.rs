//! RBAC store: roles, their permissions and user assignments.

use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{RoleId, UserId};
use sqlx::PgPool;

/// A role (`roles` table).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    /// System roles (`USER`, `SITE_ADMINISTRATOR`) cannot be renamed or deleted.
    pub system: bool,
    pub created_at: DateTime<Utc>,
}

/// Serializes site-admin changes through `setRoles`, so that two demotions cannot remove the
/// last site admin together.
const SITE_ADMIN_CHANGE_LOCK: i64 = 0x5345_454b_4552_0002; // "SOURCEGULL" + 2

/// A permission in the catalog.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RolePermission {
    pub id: i64,
    pub namespace: String,
    pub action: String,
}

/// Reads roles and permissions and manages user–role assignments.
#[derive(Debug, Clone)]
pub struct RbacStore {
    pool: PgPool,
}

impl RbacStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// All defined permissions in display order.
    pub async fn list_permissions(&self) -> Result<Vec<RolePermission>> {
        Ok(sqlx::query_as(
            "SELECT id, namespace, action FROM permissions ORDER BY namespace, action",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Permissions granted to one live role.
    pub async fn permissions_for_role(&self, role: RoleId) -> Result<Vec<RolePermission>> {
        Ok(sqlx::query_as(
            "SELECT p.id, p.namespace, p.action FROM permissions p
            JOIN role_permissions rp ON rp.permission_id = p.id
            JOIN roles r ON r.id = rp.role_id AND r.deleted_at IS NULL
            WHERE r.id = $1 ORDER BY p.namespace, p.action",
        )
        .bind(role)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Creates a custom role with a validated set of permissions.
    pub async fn create_role(&self, name: &str, permissions: &[i64]) -> Result<Role> {
        let mut tx = self.pool.begin().await?;
        validate_permissions(&mut tx, permissions).await?;
        let role: Role = sqlx::query_as(
            "INSERT INTO roles (name) VALUES ($1)
            RETURNING id, name, system, created_at",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        insert_permissions(&mut tx, role.id, permissions).await?;
        tx.commit().await?;
        Ok(role)
    }

    /// Replaces a role's permissions and returns users whose cached actor must be dropped.
    pub async fn set_permissions(&self, role: RoleId, permissions: &[i64]) -> Result<Vec<UserId>> {
        let mut tx = self.pool.begin().await?;
        let exists: Option<RoleId> = sqlx::query_scalar(
            "SELECT id FROM roles WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(role)
        .fetch_optional(&mut *tx)
        .await?;
        if exists.is_none() {
            return Err(DbError::NotFound("role"));
        }
        validate_permissions(&mut tx, permissions).await?;
        let users = sqlx::query_scalar("SELECT user_id FROM user_roles WHERE role_id = $1")
            .bind(role)
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
            .bind(role)
            .execute(&mut *tx)
            .await?;
        insert_permissions(&mut tx, role, permissions).await?;
        tx.commit().await?;
        Ok(users)
    }

    /// Renames a custom role. Fails with [`DbError::AlreadyExists`] when the name is taken.
    pub async fn rename_role(&self, role: RoleId, name: &str) -> Result<()> {
        let r = sqlx::query(
            "UPDATE roles SET name = $2 WHERE id = $1 AND NOT system AND deleted_at IS NULL",
        )
        .bind(role)
        .bind(name)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("role"));
        }
        Ok(())
    }

    /// Soft deletes a custom role and returns its former users.
    pub async fn delete_role(&self, role: RoleId) -> Result<Vec<UserId>> {
        let mut tx = self.pool.begin().await?;
        let system: Option<bool> = sqlx::query_scalar(
            "SELECT system FROM roles WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(role)
        .fetch_optional(&mut *tx)
        .await?;
        match system {
            None => return Err(DbError::NotFound("role")),
            Some(true) => return Err(DbError::Invalid("system roles cannot be deleted")),
            Some(false) => {}
        }
        let users = sqlx::query_scalar("SELECT user_id FROM user_roles WHERE role_id = $1")
            .bind(role)
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM user_roles WHERE role_id = $1")
            .bind(role)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE roles SET deleted_at = now() WHERE id = $1")
            .bind(role)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(users)
    }

    /// Replaces a user's roles. USER is mandatory; SITE_ADMINISTRATOR updates
    /// `users.site_admin` in the same transaction.
    pub async fn set_roles(&self, user: UserId, roles: &[RoleId]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let admin: Option<bool> = sqlx::query_scalar(
            "SELECT site_admin FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let was_admin = admin.ok_or(DbError::NotFound("user"))?;
        let found: Vec<(RoleId, String, bool)> = sqlx::query_as(
            "SELECT id, name, system FROM roles WHERE id = ANY($1) AND deleted_at IS NULL FOR SHARE",
        )
        .bind(roles)
        .fetch_all(&mut *tx)
        .await?;
        if found.len() != roles.iter().collect::<std::collections::HashSet<_>>().len() {
            return Err(DbError::NotFound("role"));
        }
        let user_role: RoleId =
            sqlx::query_scalar("SELECT id FROM roles WHERE name = 'USER' AND system")
                .fetch_one(&mut *tx)
                .await?;
        let admin_role: RoleId =
            sqlx::query_scalar("SELECT id FROM roles WHERE name = 'SITE_ADMINISTRATOR' AND system")
                .fetch_one(&mut *tx)
                .await?;
        let admin = roles.contains(&admin_role);
        if was_admin != admin {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(SITE_ADMIN_CHANGE_LOCK)
                .execute(&mut *tx)
                .await?;
            if was_admin && !admin {
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM users WHERE site_admin AND deleted_at IS NULL",
                )
                .fetch_one(&mut *tx)
                .await?;
                if count <= 1 {
                    return Err(DbError::Invalid("cannot revoke the last site admin"));
                }
            }
            sqlx::query("UPDATE users SET site_admin = $2, updated_at = now() WHERE id = $1")
                .bind(user)
                .bind(admin)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM user_roles WHERE user_id = $1 AND role_id NOT IN ($2, $3)")
            .bind(user)
            .bind(user_role)
            .bind(admin_role)
            .execute(&mut *tx)
            .await?;
        for (role, _, system) in found {
            if !system {
                sqlx::query("INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(user).bind(role).execute(&mut *tx).await?;
            }
        }
        sqlx::query(
            "INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(user_role)
        .execute(&mut *tx)
        .await?;
        if admin {
            sqlx::query(
                "INSERT INTO user_roles (user_id, role_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(user)
            .bind(admin_role)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM user_roles WHERE user_id = $1 AND role_id = $2")
                .bind(user)
                .bind(admin_role)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The distinct `(namespace, action)` permissions granted to a user
    /// through their (non-deleted) roles, sorted.
    pub async fn permissions_for_user(&self, user: UserId) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT DISTINCT p.namespace, p.action
             FROM user_roles ur
             JOIN roles r ON r.id = ur.role_id AND r.deleted_at IS NULL
             JOIN role_permissions rp ON rp.role_id = r.id
             JOIN permissions p ON p.id = rp.permission_id
             WHERE ur.user_id = $1
             ORDER BY p.namespace, p.action",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// All non-deleted roles, system roles first, then by name.
    pub async fn list_roles(&self) -> Result<Vec<Role>> {
        Ok(sqlx::query_as(
            "SELECT id, name, system, created_at FROM roles
             WHERE deleted_at IS NULL ORDER BY system DESC, name",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The non-deleted roles assigned to a user, by name.
    pub async fn roles_for_user(&self, user: UserId) -> Result<Vec<Role>> {
        Ok(sqlx::query_as(
            "SELECT r.id, r.name, r.system, r.created_at
             FROM user_roles ur JOIN roles r ON r.id = ur.role_id
             WHERE ur.user_id = $1 AND r.deleted_at IS NULL ORDER BY r.name",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Assigns a role to a user (idempotent). Fails with
    /// [`DbError::NotFound`] if the role does not exist.
    pub async fn assign(&self, user: UserId, role: RoleId) -> Result<()> {
        let r = sqlx::query(
            "INSERT INTO user_roles (user_id, role_id)
             SELECT $1, id FROM roles WHERE id = $2 AND deleted_at IS NULL
             ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(role)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM roles WHERE id = $1 AND deleted_at IS NULL)",
            )
            .bind(role)
            .fetch_one(&self.pool)
            .await?;
            if !exists {
                return Err(DbError::NotFound("role"));
            }
        }
        Ok(())
    }

    /// Removes a role from a user. Returns whether it was assigned.
    pub async fn unassign(&self, user: UserId, role: RoleId) -> Result<bool> {
        let r = sqlx::query("DELETE FROM user_roles WHERE user_id = $1 AND role_id = $2")
            .bind(user)
            .bind(role)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }
}

async fn validate_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ids: &[i64],
) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM permissions WHERE id = ANY($1)")
        .bind(ids)
        .fetch_one(&mut **tx)
        .await?;
    if count
        != i64::try_from(ids.iter().collect::<std::collections::HashSet<_>>().len())
            .unwrap_or(i64::MAX)
    {
        return Err(DbError::NotFound("permission"));
    }
    Ok(())
}

async fn insert_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    role: RoleId,
    ids: &[i64],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission_id)
        SELECT $1, unnest($2::bigint[]) ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
