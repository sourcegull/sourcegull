//! Executor secrets: `executorSecrets` (global secrets,
//! site admins), `User.executorSecrets` and `Org.executorSecrets` (the namespace's secrets and
//! the global ones it does not overwrite; the user, org members and site admins),
//! `createExecutorSecret`, `updateExecutorSecret` and `deleteExecutorSecret`, and the
//! `ExecutorSecret` and `ExecutorSecretAccessLog` nodes.
//!
//! Values are write-only: no field returns them. Keys are environment variable names
//! (`^[A-Z][A-Z0-9_]*$`); a `DOCKER_AUTH_CONFIG` value must be a Docker config with `auths`
//! only (no credential helpers or stores). Changes are recorded as security events.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::{
    DbError, ExecutorSecret, ExecutorSecretAccessLog, ExecutorSecretScope, ListExecutorSecrets,
    Namespace,
};
use sourcegull_executor_api::secrets::{valid_key, validate_value};
use sourcegull_services::relay::{marshal_id, marshal_string_id, unmarshal_id, unmarshal_int_id};

use crate::context::{
    Services, access_error, codes, db_error, error, page_size, require_viewer, services, to_int,
    viewer,
};
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::ids::parse_namespace_id;
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::{NamespaceRef, UserRef, load_namespace};
use sourcegull_graphql_core::{EmptyResponse, audit, declared};

/// Security event names.
mod events {
    /// An executor secret was created.
    pub const EXECUTOR_SECRET_CREATED: &str = "ExecutorSecretCreated";
    /// An executor secret's value was replaced.
    pub const EXECUTOR_SECRET_UPDATED: &str = "ExecutorSecretUpdated";
    /// An executor secret was deleted.
    pub const EXECUTOR_SECRET_DELETED: &str = "ExecutorSecretDeleted";
}

/// Node kind of executor secret IDs (`ExecutorSecret:"BATCHES:12"`).
pub const EXECUTOR_SECRET_KIND: &str = "ExecutorSecret";
/// Node kind of access log IDs.
pub const ACCESS_LOG_KIND: &str = "ExecutorSecretAccessLog";

const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;

/// `ExecutorSecretScope`.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "ExecutorSecretScope")]
pub enum Scope {
    /// Batch Changes executions.
    #[graphql(name = "BATCHES")]
    Batches,
    /// Auto-indexing.
    #[graphql(name = "CODEINTEL")]
    CodeIntel,
}

impl Scope {
    fn db(self) -> ExecutorSecretScope {
        match self {
            Self::Batches => ExecutorSecretScope::Batches,
            Self::CodeIntel => ExecutorSecretScope::CodeIntel,
        }
    }

    fn from_db(s: ExecutorSecretScope) -> Self {
        match s {
            ExecutorSecretScope::Batches => Self::Batches,
            ExecutorSecretScope::CodeIntel => Self::CodeIntel,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Batches => "BATCHES",
            Self::CodeIntel => "CODEINTEL",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "BATCHES" => Some(Self::Batches),
            "CODEINTEL" => Some(Self::CodeIntel),
            _ => None,
        }
    }
}

/// The ID of a secret.
pub fn secret_id(secret: &ExecutorSecret) -> String {
    marshal_string_id(
        EXECUTOR_SECRET_KIND,
        &format!("{}:{}", Scope::from_db(secret.scope).name(), secret.id),
    )
}

/// Parses a secret ID into its scope and row id.
pub fn parse_secret_id(id: &str) -> Option<(Scope, i64)> {
    let (kind, spec) = unmarshal_id(id)?;
    if kind != EXECUTOR_SECRET_KIND {
        return None;
    }
    let spec: String = serde_json::from_str(&spec).ok()?;
    let (scope, id) = spec.split_once(':')?;
    Some((Scope::parse(scope)?, id.parse().ok()?))
}

fn invalid_id(id: &str) -> async_graphql::Error {
    error(
        codes::INVALID_INPUT,
        format!("Invalid executor secret ID {id:?}."),
    )
}

/// Fails unless the viewer may manage the secrets of `ns` (global: `None`).
async fn require_namespace_access(ctx: &Context<'_>, ns: Option<Namespace>) -> Result<()> {
    sourcegull_services::executor_secrets::require_can_manage(&services(ctx)?.db, viewer(ctx), ns)
        .await
        .map(|_| ())
        .map_err(access_error)
}

/// An executor secret (without its value).
#[derive(Clone)]
pub struct ExecutorSecretNode(pub ExecutorSecret);

#[Object(name = "ExecutorSecret")]
impl ExecutorSecretNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(secret_id(&self.0))
    }

    /// The environment variable the secret is injected as.
    async fn key(&self) -> &str {
        &self.0.key
    }

    /// The jobs the secret is usable in.
    async fn scope(&self) -> Scope {
        Scope::from_db(self.0.scope)
    }

    /// Whether this namespace's secret overwrites a global secret with the same key.
    async fn overwrites_global_secret(&self) -> bool {
        self.0.overwrites_global_secret
    }

    /// The owning user or organization; null for a global secret.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<Option<NamespaceRef>> {
        match self.0.namespace {
            Some(ns) => Ok(load_namespace(ctx, ns).await?.map(NamespaceRef)),
            None => Ok(None),
        }
    }

    /// The creator; null once deleted.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user_node(ctx, self.0.creator_id).await
    }

    /// The resources file entry that manages this global secret (`executorSecrets[<key>]`), or
    /// `executorSecrets` when the file manages every global secret; such secrets are read-only.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.0.namespace.is_some() {
            return Ok(None);
        }
        declared::managed_by(
            services(ctx)?,
            declared::kinds::EXECUTOR_SECRETS,
            &declared_id(&self.0),
        )
        .await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }

    /// Every decryption of the value, newest first.
    async fn access_logs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<ExecutorSecretAccessLogConnection> {
        let limit = page_size(Some(first), DEFAULT_PAGE, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let store = services(ctx)?.db.executor_secrets(None);
        let mut logs = store
            .access_logs(self.0.id, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let total = store.count_access_logs(self.0.id).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, logs.len());
        logs.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(ExecutorSecretAccessLogConnection {
            nodes: logs
                .into_iter()
                .map(|log| ExecutorSecretAccessLogNode {
                    log,
                    secret: self.0.clone(),
                })
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }
}

async fn user_node(
    ctx: &Context<'_>,
    id: Option<sourcegull_types::UserId>,
) -> Result<Option<UserRef>> {
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(services(ctx)?
        .db
        .users()
        .get_by_id(id)
        .await
        .map_err(db_error)?
        .map(UserRef))
}

/// One decryption of a secret value.
pub struct ExecutorSecretAccessLogNode {
    log: ExecutorSecretAccessLog,
    secret: ExecutorSecret,
}

#[Object(name = "ExecutorSecretAccessLog")]
impl ExecutorSecretAccessLogNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(ACCESS_LOG_KIND, self.log.id))
    }

    async fn executor_secret(&self) -> ExecutorSecretNode {
        ExecutorSecretNode(self.secret.clone())
    }

    /// The user in whose name the secret was read; null for internal procedures and deleted
    /// users.
    async fn user(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user_node(ctx, self.log.user_id).await
    }

    /// The internal procedure that read the secret (e.g. `codeintel-autoindexing`), else
    /// empty.
    async fn machine_user(&self) -> &str {
        &self.log.machine_user
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.log.created_at
    }
}

/// A page of executor secrets.
#[derive(SimpleObject)]
pub struct ExecutorSecretConnection {
    nodes: Vec<ExecutorSecretNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// A page of access logs.
#[derive(SimpleObject)]
pub struct ExecutorSecretAccessLogConnection {
    nodes: Vec<ExecutorSecretAccessLogNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// The secrets usable in `ns` (global when `None`), after checking access.
pub async fn list(
    ctx: &Context<'_>,
    ns: Option<Namespace>,
    scope: Scope,
    first: i32,
    after: Option<String>,
) -> Result<ExecutorSecretConnection> {
    require_namespace_access(ctx, ns).await?;
    let limit = page_size(Some(first), DEFAULT_PAGE, MAX_PAGE);
    let offset = decode_cursor(after.as_deref())?;
    let store = services(ctx)?.db.executor_secrets(None);
    let opts = ListExecutorSecrets {
        namespace: ns,
        keys: None,
        limit: limit + 1,
        offset,
    };
    let mut rows = store.list(scope.db(), &opts).await.map_err(db_error)?;
    let total = store.count(scope.db(), &opts).await.map_err(db_error)?;
    let page_info = PageInfo::new(offset, limit, rows.len());
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(ExecutorSecretConnection {
        nodes: rows.into_iter().map(ExecutorSecretNode).collect(),
        total_count: to_int(total),
        page_info,
    })
}

/// A node of this domain.
pub enum ExecutorsNode {
    /// An executor secret.
    Secret(ExecutorSecretNode),
    /// An access log entry.
    AccessLog(ExecutorSecretAccessLogNode),
}

/// The `node(id:)` loader of secrets and access log entries.
pub fn node_loaders<N: From<ExecutorsNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<ExecutorsNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            Ok(if kind == EXECUTOR_SECRET_KIND {
                load_node(ctx, id).await?.map(ExecutorsNode::Secret)
            } else {
                load_access_log_node(ctx, id)
                    .await?
                    .map(ExecutorsNode::AccessLog)
            }
            .map(N::from))
        })
    }
    [NodeLoader {
        kinds: &[EXECUTOR_SECRET_KIND, ACCESS_LOG_KIND],
        load: load::<N>,
    }]
}

/// A secret by ID, if the viewer may see its namespace.
async fn load_node(ctx: &Context<'_>, id: &str) -> Result<Option<ExecutorSecretNode>> {
    let Some((scope, id)) = parse_secret_id(id) else {
        return Ok(None);
    };
    let secret = services(ctx)?
        .db
        .executor_secrets(None)
        .get(scope.db(), id)
        .await
        .map_err(db_error)?;
    let Some(secret) = secret else {
        return Ok(None);
    };
    require_namespace_access(ctx, secret.namespace).await?;
    Ok(Some(ExecutorSecretNode(secret)))
}

/// An access log entry by ID, if the viewer may see its secret.
async fn load_access_log_node(
    ctx: &Context<'_>,
    id: &str,
) -> Result<Option<ExecutorSecretAccessLogNode>> {
    let Some(id) = unmarshal_int_id(ACCESS_LOG_KIND, id) else {
        return Ok(None);
    };
    let store = services(ctx)?.db.executor_secrets(None);
    let Some(log) = store.access_log(id).await.map_err(db_error)? else {
        return Ok(None);
    };
    let mut secret = None;
    for scope in [ExecutorSecretScope::Batches, ExecutorSecretScope::CodeIntel] {
        if let Some(s) = store
            .get(scope, log.executor_secret_id)
            .await
            .map_err(db_error)?
        {
            secret = Some(s);
        }
    }
    let Some(secret) = secret else {
        return Ok(None);
    };
    require_namespace_access(ctx, secret.namespace).await?;
    Ok(Some(ExecutorSecretAccessLogNode { log, secret }))
}

fn invalid(message: impl Into<String>) -> async_graphql::Error {
    error(codes::INVALID_INPUT, message)
}

/// The security event argument describing a secret.
fn event_argument(secret: &ExecutorSecret) -> serde_json::Value {
    let (user, org) = match secret.namespace {
        Some(Namespace::User(u)) => (Some(u.get()), None),
        Some(Namespace::Org(o)) => (None, Some(o.get())),
        None => (None, None),
    };
    json!({
        "id": secret.id,
        "key": secret.key,
        "scope": Scope::from_db(secret.scope).name(),
        "namespaceUserID": user,
        "namespaceOrgID": org,
    })
}

/// Executor secret queries.
#[derive(Default)]
pub struct ExecutorSecretsQuery;

#[Object]
impl ExecutorSecretsQuery {
    /// The global executor secrets of `scope` (site admins only).
    async fn executor_secrets(
        &self,
        ctx: &Context<'_>,
        scope: Scope,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<ExecutorSecretConnection> {
        list(ctx, None, scope, first, after).await
    }
}

/// Executor secret mutations.
#[derive(Default)]
pub struct ExecutorSecretsMutation;

#[Object]
impl ExecutorSecretsMutation {
    /// Creates a secret: global without `namespace` (site admins only), else in a user's
    /// namespace (the user or site admins) or an organization's (its members or site admins).
    async fn create_executor_secret(
        &self,
        ctx: &Context<'_>,
        scope: Scope,
        key: String,
        value: String,
        namespace: Option<ID>,
    ) -> Result<ExecutorSecretNode> {
        let actor = require_viewer(ctx)?;
        let ns = namespace.map(|id| parse_namespace_id(&id)).transpose()?;
        require_namespace_access(ctx, ns).await?;
        if key.is_empty() {
            return Err(invalid("key cannot be empty string"));
        }
        if !valid_key(&key) {
            return Err(invalid(
                "invalid key format, should be a valid env var name",
            ));
        }
        validate_value(&key, &value).map_err(invalid)?;
        if let Some(Namespace::User(u)) = ns {
            services(ctx)?
                .db
                .users()
                .get_by_id(u)
                .await
                .map_err(db_error)?
                .ok_or_else(|| error(codes::NOT_FOUND, "User not found."))?;
        }
        let services = services(ctx)?;
        if ns.is_none() {
            declared::ensure_creatable(services, declared::kinds::EXECUTOR_SECRETS).await?;
        }
        let secret = services
            .db
            .executor_secrets(services.keyring.clone())
            .create(scope.db(), &key, &value, ns, Some(actor.user_id))
            .await
            .map_err(|e| match e {
                DbError::AlreadyExists { .. } => error(
                    "ErrDuplicateExecutorSecret",
                    "multiple secrets with the same key in the same namespace not allowed",
                ),
                DbError::Invalid(m) => error(codes::UNAVAILABLE, m),
                e => db_error(e),
            })?;
        audit::record(
            services,
            events::EXECUTOR_SECRET_CREATED,
            Some(actor.user_id),
            event_argument(&secret),
        )
        .await;
        Ok(ExecutorSecretNode(secret))
    }

    /// Replaces the value of a secret (same access rules as creating it).
    async fn update_executor_secret(
        &self,
        ctx: &Context<'_>,
        scope: Scope,
        id: ID,
        value: String,
    ) -> Result<ExecutorSecretNode> {
        let actor = require_viewer(ctx)?;
        let (id_scope, row_id) = parse_secret_id(&id).ok_or_else(|| invalid_id(&id))?;
        if id_scope != scope {
            return Err(invalid("scope mismatch"));
        }
        let services = services(ctx)?;
        let store = services.db.executor_secrets(services.keyring.clone());
        let secret = store
            .get(scope.db(), row_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Executor secret not found."))?;
        require_namespace_access(ctx, secret.namespace).await?;
        ensure_not_declared(services, &secret).await?;
        validate_value(&secret.key, &value).map_err(invalid)?;
        let secret = store
            .update_value(scope.db(), row_id, &value)
            .await
            .map_err(|e| match e {
                DbError::Invalid(m) => error(codes::UNAVAILABLE, m),
                e => db_error(e),
            })?
            .ok_or_else(|| error(codes::NOT_FOUND, "Executor secret not found."))?;
        audit::record(
            services,
            events::EXECUTOR_SECRET_UPDATED,
            Some(actor.user_id),
            event_argument(&secret),
        )
        .await;
        Ok(ExecutorSecretNode(secret))
    }

    /// Deletes a secret (same access rules as creating it).
    async fn delete_executor_secret(
        &self,
        ctx: &Context<'_>,
        scope: Scope,
        id: ID,
    ) -> Result<Option<EmptyResponse>> {
        let actor = require_viewer(ctx)?;
        let (id_scope, row_id) = parse_secret_id(&id).ok_or_else(|| invalid_id(&id))?;
        if id_scope != scope {
            return Err(invalid("scope mismatch"));
        }
        let services = services(ctx)?;
        let store = services.db.executor_secrets(None);
        let secret = store
            .get(scope.db(), row_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Executor secret not found."))?;
        require_namespace_access(ctx, secret.namespace).await?;
        ensure_not_declared(services, &secret).await?;
        store.delete(scope.db(), row_id).await.map_err(db_error)?;
        audit::record(
            services,
            events::EXECUTOR_SECRET_DELETED,
            Some(actor.user_id),
            event_argument(&secret),
        )
        .await;
        Ok(Some(EmptyResponse))
    }
}

/// The object id the resources file records for a secret: `<scope>/<id>`.
fn declared_id(secret: &ExecutorSecret) -> String {
    format!("{}/{}", secret.scope.as_str(), secret.id)
}

/// Global secrets the resources file manages are read-only.
async fn ensure_not_declared(services: &Services, secret: &ExecutorSecret) -> Result<()> {
    if secret.namespace.is_some() {
        return Ok(());
    }
    declared::ensure_not_declared(
        services,
        declared::kinds::EXECUTOR_SECRETS,
        &declared_id(secret),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let secret = ExecutorSecret {
            id: 12,
            scope: ExecutorSecretScope::CodeIntel,
            key: "K".into(),
            namespace: None,
            creator_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            overwrites_global_secret: false,
        };
        let id = secret_id(&secret);
        assert_eq!(parse_secret_id(&id), Some((Scope::CodeIntel, 12)));
        assert_eq!(parse_secret_id(&marshal_id("User", 1)), None);
    }
}
