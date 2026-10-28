//! Role and permission administration.

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use serde_json::json;
use sourcegull_db::{Role, RolePermission};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::{RoleId, UserId};

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{
    codes, db_error, error, page_size, require_site_admin, require_viewer, services, to_int,
};
use crate::declared;
use crate::pagination::{PageInfo, decode_cursor};
use crate::users::{load_user, parse_user_id};

const ROLE_KIND: &str = "Role";
const PERMISSION_KIND: &str = "Permission";

fn role_id(id: &str) -> Result<RoleId> {
    unmarshal_int_id(ROLE_KIND, id)
        .map(RoleId)
        .ok_or_else(|| error(codes::INVALID_INPUT, "Invalid role ID."))
}

fn permission_ids(ids: &[ID]) -> Result<Vec<i64>> {
    ids.iter()
        .map(|id| {
            unmarshal_int_id(PERMISSION_KIND, id)
                .ok_or_else(|| error(codes::INVALID_INPUT, "Invalid permission ID."))
        })
        .collect()
}

/// A named role.
pub struct RoleNode(pub Role);

#[Object(name = "Role")]
impl RoleNode {
    async fn id(&self) -> ID {
        ID(marshal_id(ROLE_KIND, self.0.id.get()))
    }
    async fn name(&self) -> &str {
        &self.0.name
    }
    async fn system(&self) -> bool {
        self.0.system
    }
    async fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.0.created_at
    }
    /// The resources file entry that manages the role (`roles[<key>]`), or `roles` when the
    /// file manages every custom role; such roles are read-only.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.0.system {
            return Ok(None);
        }
        declared::managed_by(
            services(ctx)?,
            declared::kinds::ROLES,
            &self.0.id.get().to_string(),
        )
        .await
    }
    async fn permissions(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<PermissionConnection> {
        require_site_admin(ctx)?;
        let rows = services(ctx)?
            .db
            .rbac()
            .permissions_for_role(self.0.id)
            .await
            .map_err(db_error)?;
        permission_page(rows, first, None)
    }
}

/// A catalog permission.
pub struct PermissionNode(pub RolePermission);

#[Object(name = "Permission")]
impl PermissionNode {
    async fn id(&self) -> ID {
        ID(marshal_id(PERMISSION_KIND, self.0.id))
    }
    async fn namespace(&self) -> &str {
        &self.0.namespace
    }
    async fn action(&self) -> &str {
        &self.0.action
    }
    async fn display_name(&self) -> String {
        format!("{}#{}", self.0.namespace, self.0.action)
    }
}

#[derive(SimpleObject)]
pub struct RoleConnection {
    pub nodes: Vec<RoleNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

#[derive(SimpleObject)]
pub struct PermissionConnection {
    pub nodes: Vec<PermissionNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

fn page<T>(
    rows: Vec<T>,
    first: Option<i32>,
    after: Option<String>,
) -> Result<(Vec<T>, i32, PageInfo)> {
    let offset = decode_cursor(after.as_deref())?;
    let limit = page_size(first, 50, 1000);
    let total = to_int(i64::try_from(rows.len()).unwrap_or(i64::MAX));
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = usize::try_from(limit).unwrap_or(0);
    let fetched = rows.len().saturating_sub(start).min(take.saturating_add(1));
    let info = PageInfo::new(offset, limit, fetched);
    Ok((
        rows.into_iter().skip(start).take(take).collect(),
        total,
        info,
    ))
}

fn permission_page(
    rows: Vec<RolePermission>,
    first: Option<i32>,
    after: Option<String>,
) -> Result<PermissionConnection> {
    let (rows, total_count, page_info) = page(rows, first, after)?;
    Ok(PermissionConnection {
        nodes: rows.into_iter().map(PermissionNode).collect(),
        total_count,
        page_info,
    })
}

/// Roles assigned to a user (site admins only).
pub async fn user_roles(
    ctx: &Context<'_>,
    user: UserId,
    first: Option<i32>,
) -> Result<RoleConnection> {
    require_site_admin(ctx)?;
    let rows = services(ctx)?
        .db
        .rbac()
        .roles_for_user(user)
        .await
        .map_err(db_error)?;
    let (rows, total_count, page_info) = page(rows, first, None)?;
    Ok(RoleConnection {
        nodes: rows.into_iter().map(RoleNode).collect(),
        total_count,
        page_info,
    })
}

/// Effective permissions of the signed-in user, loaded from the database so the result
/// is fresh even for a request carrying an actor resolved before a mutation.
pub async fn current_permissions(ctx: &Context<'_>) -> Result<PermissionConnection> {
    let actor = require_viewer(ctx)?;
    let rows = services(ctx)?
        .db
        .rbac()
        .list_permissions()
        .await
        .map_err(db_error)?;
    let pairs = services(ctx)?
        .db
        .rbac()
        .permissions_for_user(actor.user_id)
        .await
        .map_err(db_error)?;
    let rows = rows
        .into_iter()
        .filter(|p| {
            pairs
                .iter()
                .any(|(n, a)| n == &p.namespace && a == &p.action)
        })
        .collect();
    permission_page(rows, None, None)
}

#[derive(Default)]
pub struct RbacQuery;

#[Object]
impl RbacQuery {
    async fn roles(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        system: Option<bool>,
    ) -> Result<RoleConnection> {
        require_site_admin(ctx)?;
        let rows = services(ctx)?
            .db
            .rbac()
            .list_roles()
            .await
            .map_err(db_error)?;
        let rows = rows
            .into_iter()
            .filter(|r| system.is_none_or(|s| r.system == s))
            .collect();
        let (rows, total_count, page_info) = page(rows, first, after)?;
        Ok(RoleConnection {
            nodes: rows.into_iter().map(RoleNode).collect(),
            total_count,
            page_info,
        })
    }

    async fn permissions(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<PermissionConnection> {
        require_site_admin(ctx)?;
        permission_page(
            services(ctx)?
                .db
                .rbac()
                .list_permissions()
                .await
                .map_err(db_error)?,
            first,
            after,
        )
    }
}

#[derive(Default)]
pub struct RbacMutation;

#[Object]
impl RbacMutation {
    async fn create_role(
        &self,
        ctx: &Context<'_>,
        name: String,
        permissions: Vec<ID>,
    ) -> Result<RoleNode> {
        let actor = require_site_admin(ctx)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(error(codes::VALIDATION, "Role name is required."));
        }
        let ids = permission_ids(&permissions)?;
        let svc = services(ctx)?;
        declared::ensure_creatable(svc, declared::kinds::ROLES).await?;
        let role = svc
            .db
            .rbac()
            .create_role(name, &ids)
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::ROLE_CREATED,
            Some(actor.user_id),
            json!({"role": role.id, "name": name, "permissions": ids}),
        )
        .await;
        Ok(RoleNode(role))
    }

    async fn delete_role(&self, ctx: &Context<'_>, role: ID) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = role_id(&role)?;
        declared::ensure_not_declared(svc, declared::kinds::ROLES, &id.get().to_string()).await?;
        let users = svc.db.rbac().delete_role(id).await.map_err(role_error)?;
        for user in users {
            svc.auth.invalidate_user(user).await;
        }
        audit::record(
            svc,
            events::ROLE_DELETED,
            Some(actor.user_id),
            json!({"role": id}),
        )
        .await;
        Ok(EmptyResponse)
    }

    async fn set_permissions(
        &self,
        ctx: &Context<'_>,
        role: ID,
        permissions: Vec<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = role_id(&role)?;
        declared::ensure_not_declared(svc, declared::kinds::ROLES, &id.get().to_string()).await?;
        let ids = permission_ids(&permissions)?;
        let users = svc
            .db
            .rbac()
            .set_permissions(id, &ids)
            .await
            .map_err(db_error)?;
        for user in users {
            svc.auth.invalidate_user(user).await;
        }
        audit::record(
            svc,
            events::ROLE_PERMISSIONS_CHANGED,
            Some(actor.user_id),
            json!({"role": id, "permissions": ids}),
        )
        .await;
        Ok(EmptyResponse)
    }

    async fn set_roles(
        &self,
        ctx: &Context<'_>,
        user: ID,
        roles: Vec<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let user = parse_user_id(&user)?;
        load_user(ctx, user).await?;
        let before = svc.db.rbac().roles_for_user(user).await.map_err(db_error)?;
        let ids = roles
            .iter()
            .map(|id| role_id(id))
            .collect::<Result<Vec<_>>>()?;
        let all = svc.db.rbac().list_roles().await.map_err(db_error)?;
        let is_admin_role = |id: &sourcegull_types::RoleId| {
            all.iter()
                .any(|r| r.id == *id && r.system && r.name == "SITE_ADMINISTRATOR")
        };
        let was_admin = before.iter().any(|r| is_admin_role(&r.id));
        let will_be_admin = ids.iter().any(is_admin_role);
        if was_admin != will_be_admin {
            declared::ensure_site_admin_change_allowed(svc, user, will_be_admin).await?;
        }
        svc.db
            .rbac()
            .set_roles(user, &ids)
            .await
            .map_err(role_error)?;
        svc.auth.invalidate_user(user).await;
        let after = svc.db.rbac().roles_for_user(user).await.map_err(db_error)?;
        for r in &after {
            if !before.iter().any(|old| old.id == r.id) {
                audit::record(
                    svc,
                    events::ROLE_CHANGE_GRANTED,
                    Some(actor.user_id),
                    json!({"userID": user, "role": r.id}),
                )
                .await;
            }
        }
        for r in &before {
            if !after.iter().any(|new| new.id == r.id) {
                audit::record(
                    svc,
                    events::ROLE_CHANGE_DENIED,
                    Some(actor.user_id),
                    json!({"userID": user, "role": r.id}),
                )
                .await;
            }
        }
        Ok(EmptyResponse)
    }
}

fn role_error(err: sourcegull_db::DbError) -> async_graphql::Error {
    match err {
        sourcegull_db::DbError::Invalid(message) => error(codes::FORBIDDEN, message),
        other => db_error(other),
    }
}
