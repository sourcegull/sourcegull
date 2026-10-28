//! Applying the resources file: each section's entries are compared with the objects they own
//! (`declared_resources`) and the differences written through the same stores the UI uses.
//!
//! Per entry:
//! - unchanged (same resolved hash, object still there): nothing;
//! - changed: the object is updated (or replaced, when an identity field such as a connection's
//!   kind changed);
//! - new key: an existing object with the same identity that no entry owns yet (a connection
//!   to the same code host URL, a context or role with the same name, …) is taken over and
//!   updated, so moving a live instance to a file does not recreate anything; otherwise the
//!   object is created;
//! - key removed from its section: the object is deleted.
//!
//! A section removed from the file releases its objects to the UI unchanged. Site admins and
//! role members are matched against verified email addresses; only what the file granted is
//! taken back when an address is removed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sourcegull_codeintel_policies::store::{
    ListPolicies, PolicyFields, PolicyStore, PolicyWriteError,
};
use sourcegull_config::resources::{
    CodeIntelPolicyDecl, ConnectionDecl, Declared, Entry, ExecutorSecretDecl, FeatureFlagDecl,
    OutboundWebhookDecl, ResourceKind, Resources, RoleDecl, SearchContextDecl, scope_str,
};
use sourcegull_crypto::Keyring;
use sourcegull_db::{
    ContextAccess, Db, DbError, DeclaredObject, ExecutorSecretScope, ExternalServiceUpdate,
    FeatureFlagValue, ListExecutorSecrets, NewExternalService, ROLE_GRANT, SITE_ADMIN_GRANT,
    SearchContextInput,
};
use sourcegull_outbound_webhooks::store::{
    ListOutboundWebhooks, NewOutboundWebhook, OutboundWebhookStore, OutboundWebhookUpdate,
    ScopedEventType,
};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalServiceId, RepoAccess, RepoId, RoleId, UserId};

use crate::check::{connection_config_text, connection_kind};

/// What applying a file changed, and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// `(kind, key, action)` per change; actions are `created`, `updated`, `adopted`,
    /// `deleted` and `released`.
    pub changes: Vec<(String, String, &'static str)>,
    /// Grants and revocations of site admin and roles, as `(kind, key, user, action)`.
    pub grants: Vec<(String, String, UserId, &'static str)>,
    /// Entries that could not be applied; the others were.
    pub errors: Vec<String>,
    /// Non-fatal findings, such as context repositories that are not synced yet.
    pub warnings: Vec<String>,
}

impl ApplyReport {
    fn change(&mut self, kind: ResourceKind, key: &str, action: &'static str) {
        self.changes
            .push((kind.as_str().to_owned(), key.to_owned(), action));
    }
}

/// Why one entry could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<PolicyWriteError> for ApplyError {
    fn from(e: PolicyWriteError) -> Self {
        match e {
            PolicyWriteError::Db(e) => Self::Db(e),
            other => Self::Invalid(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, ApplyError>;

/// Applies resources files to one database.
#[derive(Debug, Clone)]
pub struct Applier {
    pub(crate) db: Db,
    pub(crate) keyring: Option<Arc<Keyring>>,
    pub(crate) registry: Registry,
}

impl Applier {
    /// An applier writing to `db`. Connections, webhooks and executor secrets need `keyring`.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>, registry: Registry) -> Self {
        Self {
            db,
            keyring,
            registry,
        }
    }

    /// Applies every section of `resources` and releases the kinds it no longer manages.
    pub async fn apply(&self, resources: &Resources) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for kind in ResourceKind::ALL {
            if kind == ResourceKind::SiteAdmins {
                continue;
            }
            match resources.section(kind) {
                Some(entries) => self.apply_section(kind, entries, &mut report).await?,
                None => {
                    let released = self.db.declared().release_kind(kind.as_str()).await?;
                    if released > 0 {
                        report.change(kind, "*", "released");
                        if kind == ResourceKind::Roles {
                            self.db.declared().remove_grants_of_kind(ROLE_GRANT).await?;
                        }
                    }
                }
            }
        }
        if let Some(entries) = resources.section(ResourceKind::Roles) {
            for d in entries {
                if let Err(e) = self.apply_role_members(d, &mut report).await {
                    report.errors.push(format!("roles[{}]: {e}", d.key));
                }
            }
        }
        self.apply_site_admins(
            resources.site_admins.as_deref(),
            resources.exclusive,
            &mut report,
        )
        .await?;
        Ok(report)
    }

    /// Releases everything the file managed (it is no longer configured): owned objects become
    /// editable in the UI, declared site admins no longer apply, and grants are forgotten
    /// (the users keep what they have).
    pub async fn release_all(&self) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        let declared = self.db.declared();
        for kind in declared.kinds_with_objects().await? {
            declared.release_kind(&kind).await?;
            report.changes.push((kind, "*".to_owned(), "released"));
        }
        declared.set_site_admins(None).await?;
        declared.remove_grants_of_kind(SITE_ADMIN_GRANT).await?;
        declared.remove_grants_of_kind(ROLE_GRANT).await?;
        Ok(report)
    }

    async fn apply_section(
        &self,
        kind: ResourceKind,
        entries: &[Declared],
        report: &mut ApplyReport,
    ) -> Result<()> {
        let declared = self.db.declared();
        let owned: HashMap<String, DeclaredObject> = declared
            .objects(kind.as_str())
            .await?
            .into_iter()
            .map(|o| (o.key.clone(), o))
            .collect();
        let mut owned_ids: HashSet<String> = owned.values().map(|o| o.object_id.clone()).collect();
        let wanted: HashSet<&str> = entries.iter().map(|d| d.key.as_str()).collect();
        // Deletions first, so a renamed key can take over its old object's name.
        for o in owned.values().filter(|o| !wanted.contains(o.key.as_str())) {
            match self.delete(kind, &o.object_id).await {
                Ok(()) => {
                    declared.remove_object(kind.as_str(), &o.key).await?;
                    owned_ids.remove(&o.object_id);
                    if kind == ResourceKind::Roles {
                        declared.remove_grants(ROLE_GRANT, &o.key).await?;
                    }
                    report.change(kind, &o.key, "deleted");
                }
                Err(e) => report
                    .errors
                    .push(format!("{kind}[{}]: deleting: {e}", o.key)),
            }
        }
        for d in entries {
            let (hash, warnings) = match self.effective_hash(d).await {
                Ok(h) => h,
                Err(e) => {
                    report.errors.push(format!("{kind}[{}]: {e}", d.key));
                    continue;
                }
            };
            report.warnings.extend(
                warnings
                    .into_iter()
                    .map(|w| format!("{kind}[{}]: {w}", d.key)),
            );
            let current = owned.get(&d.key);
            let result = match current {
                Some(o) if self.exists(kind, &o.object_id).await? => {
                    if o.applied_sha256 == hash {
                        continue;
                    }
                    self.update(d, &o.object_id).await.map(|id| (id, "updated"))
                }
                _ => {
                    let candidate = self.find_existing(d).await?;
                    match candidate.filter(|id| !owned_ids.contains(id)) {
                        Some(id) => self.update(d, &id).await.map(|id| (id, "adopted")),
                        None => self.create(d).await.map(|id| (id, "created")),
                    }
                }
            };
            match result {
                Ok((id, action)) => {
                    if let Some(o) = current
                        && o.object_id != id
                    {
                        owned_ids.remove(&o.object_id);
                    }
                    declared
                        .set_object(kind.as_str(), &d.key, &id, &hash)
                        .await?;
                    owned_ids.insert(id);
                    report.change(kind, &d.key, action);
                }
                Err(e) => report.errors.push(format!("{kind}[{}]: {e}", d.key)),
            }
        }
        Ok(())
    }

    /// The entry's hash, mixed with what it resolves to in the database (a context's
    /// repository ids), so newly synced repositories count as a change.
    async fn effective_hash(&self, d: &Declared) -> Result<([u8; 32], Vec<String>)> {
        let Entry::SearchContext(c) = &d.entry else {
            return Ok((d.hash, Vec::new()));
        };
        let (repos, missing) = self.context_repos(c).await?;
        let mut h = Sha256::new();
        h.update(d.hash);
        for (id, revs) in &repos {
            h.update(id.0.to_be_bytes());
            for r in revs {
                h.update(r.as_bytes());
                h.update([0]);
            }
            h.update([1]);
        }
        let warnings = missing
            .into_iter()
            .map(|r| format!("repository {r} is not synced yet; it is added once it is"))
            .collect();
        Ok((h.finalize().into(), warnings))
    }

    async fn exists(&self, kind: ResourceKind, id: &str) -> Result<bool> {
        let db = &self.db;
        Ok(match kind {
            ResourceKind::Connections => db
                .external_services(None)
                .get(ExternalServiceId(int(id)?))
                .await?
                .is_some(),
            ResourceKind::SearchContexts => db
                .search_contexts()
                .get(int(id)?, ContextAccess::All)
                .await?
                .is_some(),
            ResourceKind::Roles => db
                .rbac()
                .list_roles()
                .await?
                .iter()
                .any(|r| r.id.0 == int(id).unwrap_or(-1)),
            ResourceKind::OutboundWebhooks => OutboundWebhookStore::new(db, self.keyring.clone())
                .get(int(id)?)
                .await?
                .is_some(),
            ResourceKind::ExecutorSecrets => {
                let (scope, id) = secret_id(id)?;
                db.executor_secrets(None).get(scope, id).await?.is_some()
            }
            ResourceKind::CodeIntelPolicies => PolicyStore::new(db)
                .get(i32::try_from(int(id)?).unwrap_or(-1))
                .await?
                .is_some(),
            ResourceKind::FeatureFlags => db.feature_flags().get(id).await?.is_some(),
            ResourceKind::SiteAdmins => false,
        })
    }

    /// An existing object with the entry's identity, owned or not.
    async fn find_existing(&self, d: &Declared) -> Result<Option<String>> {
        let db = &self.db;
        Ok(match &d.entry {
            Entry::Connection(c) => {
                let kind = connection_kind(c.kind);
                let Ok(wanted) = self
                    .registry
                    .service_id(kind, &connection_config_text(&c.config))
                else {
                    return Ok(None);
                };
                if self.keyring.is_none() {
                    return Ok(None);
                }
                let store = db.external_services(self.keyring.clone());
                let mut found = None;
                for svc in store.list(Some(kind), 10_000, 0).await? {
                    let Ok(text) = store.config(svc.id).await else {
                        continue;
                    };
                    if self
                        .registry
                        .service_id(kind, &text)
                        .is_ok_and(|id| id == wanted)
                    {
                        found = Some(svc.id.0.to_string());
                        break;
                    }
                }
                found
            }
            Entry::SearchContext(c) => db
                .search_contexts()
                .get_by_spec(&c.name, ContextAccess::All)
                .await?
                .filter(|ctx| ctx.namespace.is_none())
                .map(|ctx| ctx.id.to_string()),
            Entry::Role(r) => db
                .rbac()
                .list_roles()
                .await?
                .into_iter()
                .find(|role| !role.system && role.name == r.name)
                .map(|role| role.id.0.to_string()),
            Entry::OutboundWebhook(w) => OutboundWebhookStore::new(db, self.keyring.clone())
                .list(&ListOutboundWebhooks::default())
                .await?
                .into_iter()
                .find(|hook| hook.url == w.url)
                .map(|hook| hook.id.to_string()),
            Entry::ExecutorSecret(s) => {
                let scope = secret_scope(s);
                db.executor_secrets(None)
                    .list(
                        scope,
                        &ListExecutorSecrets {
                            namespace: None,
                            keys: Some(vec![s.name.clone()]),
                            limit: 0,
                            offset: 0,
                        },
                    )
                    .await?
                    .into_iter()
                    .find(|secret| secret.namespace.is_none())
                    .map(|secret| format!("{}/{}", scope.as_str(), secret.id))
            }
            Entry::CodeIntelPolicy(p) => PolicyStore::new(db)
                .list(&ListPolicies::default())
                .await?
                .into_iter()
                .find(|policy| {
                    policy.repo_id.is_none() && !policy.protected && policy.name == p.name
                })
                .map(|policy| policy.id.to_string()),
            Entry::FeatureFlag(f) => db.feature_flags().get(&f.name).await?.map(|flag| flag.name),
        })
    }

    async fn create(&self, d: &Declared) -> Result<String> {
        match &d.entry {
            Entry::Connection(c) => self.create_connection(c).await,
            Entry::SearchContext(c) => {
                let input = self.context_input(c).await?;
                let ctx = self
                    .db
                    .search_contexts()
                    .create(None, &input)
                    .await
                    .map_err(|e| taken(e, "a search context with this name already exists"))?;
                Ok(ctx.id.to_string())
            }
            Entry::Role(r) => {
                let permissions = self.permission_ids(r).await?;
                let role = self
                    .db
                    .rbac()
                    .create_role(&r.name, &permissions)
                    .await
                    .map_err(|e| taken(e, "a role with this name already exists"))?;
                Ok(role.id.0.to_string())
            }
            Entry::OutboundWebhook(w) => {
                let hook = OutboundWebhookStore::new(&self.db, self.keyring.clone())
                    .create(&NewOutboundWebhook {
                        url: w.url.clone(),
                        secret: w.secret.clone(),
                        event_types: event_types(w),
                        created_by: None,
                    })
                    .await?;
                Ok(hook.id.to_string())
            }
            Entry::ExecutorSecret(s) => {
                let scope = secret_scope(s);
                let secret = self
                    .db
                    .executor_secrets(self.keyring.clone())
                    .create(scope, &s.name, &s.value, None, None)
                    .await
                    .map_err(|e| taken(e, "a global secret with this name already exists"))?;
                Ok(format!("{}/{}", scope.as_str(), secret.id))
            }
            Entry::CodeIntelPolicy(p) => {
                let policy = PolicyStore::new(&self.db)
                    .create(None, &policy_fields(p)?)
                    .await?;
                Ok(policy.id.to_string())
            }
            Entry::FeatureFlag(f) => {
                let flag = self
                    .db
                    .feature_flags()
                    .create(&f.name, flag_value(f))
                    .await
                    .map_err(|e| taken(e, "a feature flag with this name already exists"))?;
                Ok(flag.name)
            }
        }
    }

    /// Writes the entry to the object `id`; returns the object's id afterwards (a new one when
    /// it had to be replaced).
    async fn update(&self, d: &Declared, id: &str) -> Result<String> {
        let db = &self.db;
        match &d.entry {
            Entry::Connection(c) => self.update_connection(c, id).await,
            Entry::SearchContext(c) => {
                let input = self.context_input(c).await?;
                db.search_contexts()
                    .update(int(id)?, &input)
                    .await
                    .map_err(|e| taken(e, "a search context with this name already exists"))?;
                Ok(id.to_owned())
            }
            Entry::Role(r) => {
                let role = RoleId(int(id)?);
                let current = db
                    .rbac()
                    .list_roles()
                    .await?
                    .into_iter()
                    .find(|x| x.id == role)
                    .ok_or(DbError::NotFound("role"))?;
                if current.name != r.name {
                    db.rbac()
                        .rename_role(role, &r.name)
                        .await
                        .map_err(|e| taken(e, "a role with this name already exists"))?;
                }
                let permissions = self.permission_ids(r).await?;
                db.rbac().set_permissions(role, &permissions).await?;
                Ok(id.to_owned())
            }
            Entry::OutboundWebhook(w) => {
                OutboundWebhookStore::new(db, self.keyring.clone())
                    .update(
                        int(id)?,
                        &OutboundWebhookUpdate {
                            url: w.url.clone(),
                            secret: Some(w.secret.clone()),
                            event_types: event_types(w),
                            updated_by: None,
                        },
                    )
                    .await?
                    .ok_or(DbError::NotFound("outbound webhook"))?;
                Ok(id.to_owned())
            }
            Entry::ExecutorSecret(s) => {
                let (scope, secret) = secret_id(id)?;
                let store = db.executor_secrets(self.keyring.clone());
                let current = store
                    .get(scope, secret)
                    .await?
                    .ok_or(DbError::NotFound("executor secret"))?;
                if scope != secret_scope(s) || current.key != s.name {
                    store.delete(scope, secret).await?;
                    return self.create(d).await;
                }
                store.update_value(scope, secret, &s.value).await?;
                Ok(id.to_owned())
            }
            Entry::CodeIntelPolicy(p) => {
                PolicyStore::new(db)
                    .update(int32(id)?, &policy_fields(p)?)
                    .await?;
                Ok(id.to_owned())
            }
            Entry::FeatureFlag(f) => {
                if id != f.name {
                    db.feature_flags().delete(id).await?;
                    return self.create(d).await;
                }
                db.feature_flags().update(&f.name, flag_value(f)).await?;
                Ok(id.to_owned())
            }
        }
    }

    async fn delete(&self, kind: ResourceKind, id: &str) -> Result<()> {
        let db = &self.db;
        let gone = |e: DbError| match e {
            DbError::NotFound(_) => Ok(()),
            other => Err(ApplyError::Db(other)),
        };
        match kind {
            ResourceKind::Connections => {
                db.external_services(None)
                    .soft_delete(ExternalServiceId(int(id)?))
                    .await
                    .map(drop)
                    .or_else(gone)?;
            }
            ResourceKind::SearchContexts => {
                db.search_contexts().delete(int(id)?).await.or_else(gone)?;
            }
            ResourceKind::Roles => {
                db.rbac()
                    .delete_role(RoleId(int(id)?))
                    .await
                    .map(drop)
                    .or_else(gone)?;
            }
            ResourceKind::OutboundWebhooks => {
                OutboundWebhookStore::new(db, None).delete(int(id)?).await?;
            }
            ResourceKind::ExecutorSecrets => {
                let (scope, id) = secret_id(id)?;
                db.executor_secrets(None).delete(scope, id).await?;
            }
            ResourceKind::CodeIntelPolicies => {
                match PolicyStore::new(db).delete(int32(id)?).await {
                    Ok(()) | Err(PolicyWriteError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            ResourceKind::FeatureFlags => {
                db.feature_flags().delete(id).await.or_else(gone)?;
            }
            ResourceKind::SiteAdmins => {}
        }
        Ok(())
    }

    fn require_keyring(&self) -> Result<()> {
        if self.keyring.is_none() {
            return Err(ApplyError::Invalid(
                "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn create_connection(&self, c: &ConnectionDecl) -> Result<String> {
        self.require_keyring()?;
        let kind = connection_kind(c.kind);
        let text = connection_config_text(&c.config);
        let store = self.db.external_services(self.keyring.clone());
        let svc = store
            .create(&NewExternalService {
                kind,
                display_name: c.display_name.trim().to_owned(),
                config: text.clone(),
            })
            .await?;
        store
            .set_unrestricted(svc.id, self.registry.unrestricted(kind, &text))
            .await?;
        Ok(svc.id.0.to_string())
    }

    async fn update_connection(&self, c: &ConnectionDecl, id: &str) -> Result<String> {
        self.require_keyring()?;
        let kind = connection_kind(c.kind);
        let id = ExternalServiceId(int(id)?);
        let store = self.db.external_services(self.keyring.clone());
        let current = store
            .get(id)
            .await?
            .ok_or(DbError::NotFound("external service"))?;
        if current.kind != kind {
            store.soft_delete(id).await?;
            return self.create_connection(c).await;
        }
        let text = connection_config_text(&c.config);
        // Locked from read to write, like the UI's update.
        let locked = store.lock_config(id).await?;
        let config = (locked.config != text).then(|| text.clone());
        locked
            .update(&ExternalServiceUpdate {
                display_name: Some(c.display_name.trim().to_owned()),
                config,
                config_keeps_sync: false,
            })
            .await?;
        store
            .set_unrestricted(id, self.registry.unrestricted(kind, &text))
            .await?;
        Ok(id.0.to_string())
    }

    /// The context's repositories that exist, and the names that do not (yet).
    async fn context_repos(
        &self,
        c: &SearchContextDecl,
    ) -> Result<(Vec<(RepoId, Vec<String>)>, Vec<String>)> {
        let mut repos = Vec::new();
        let mut missing = Vec::new();
        for r in c.repositories.as_deref().unwrap_or_default() {
            // Instance-level contexts are written as the site, which reads every repository.
            let repo = self
                .db
                .repos()
                .get_by_name_authorized(&r.repository, &RepoAccess::All)
                .await?;
            match repo {
                Some(repo) => {
                    // `HEAD` (as the API shows it) and no revision are the default branch.
                    let revisions = if r.revisions.is_empty() {
                        vec![String::new()]
                    } else {
                        r.revisions
                            .iter()
                            .map(|rev| {
                                if rev == "HEAD" {
                                    String::new()
                                } else {
                                    rev.clone()
                                }
                            })
                            .collect()
                    };
                    repos.push((repo.id, revisions));
                }
                None => missing.push(r.repository.clone()),
            }
        }
        Ok((repos, missing))
    }

    async fn context_input(&self, c: &SearchContextDecl) -> Result<SearchContextInput> {
        let (repos, _) = self.context_repos(c).await?;
        Ok(SearchContextInput {
            name: c.name.clone(),
            description: c.description.clone(),
            public: c.public,
            query: c.query.clone(),
            repos,
        })
    }

    async fn permission_ids(&self, r: &RoleDecl) -> Result<Vec<i64>> {
        let catalog = self.db.rbac().list_permissions().await?;
        r.permissions
            .iter()
            .map(|p| {
                let (ns, action) = p.split_once('#').unwrap_or((p, ""));
                catalog
                    .iter()
                    .find(|c| c.namespace == ns && c.action == action)
                    .map(|c| c.id)
                    .ok_or_else(|| {
                        let known: Vec<String> = catalog
                            .iter()
                            .map(|c| format!("{}#{}", c.namespace, c.action))
                            .collect();
                        ApplyError::Invalid(format!(
                            "unknown permission {p:?}; known: {}",
                            known.join(", ")
                        ))
                    })
            })
            .collect()
    }

    /// Gives the role to the users the entry lists (again, when it was taken away in the UI) and
    /// takes it back from those the file gave it to and no longer lists.
    async fn apply_role_members(&self, d: &Declared, report: &mut ApplyReport) -> Result<()> {
        let Entry::Role(r) = &d.entry else {
            return Ok(());
        };
        let declared = self.db.declared();
        let Some(owned) = declared
            .objects(ResourceKind::Roles.as_str())
            .await?
            .into_iter()
            .find(|o| o.key == d.key)
        else {
            return Ok(());
        };
        let role = RoleId(int(&owned.object_id)?);
        let wanted: HashSet<UserId> = declared
            .users_with_verified_emails(&r.users)
            .await?
            .into_iter()
            .collect();
        let granted: HashSet<UserId> = declared
            .grants(ROLE_GRANT, &d.key)
            .await?
            .into_iter()
            .collect();
        let rbac = self.db.rbac();
        // Every listed user, granted before or not: a role taken away in the UI comes back.
        for user in &wanted {
            let has = rbac
                .roles_for_user(*user)
                .await?
                .iter()
                .any(|x| x.id == role);
            if !has {
                rbac.assign(*user, role).await?;
                declared.add_grant(ROLE_GRANT, &d.key, *user).await?;
                report
                    .grants
                    .push(("roles".into(), d.key.clone(), *user, "granted"));
            }
        }
        for user in granted.difference(&wanted) {
            rbac.unassign(*user, role).await?;
            declared.remove_grant(ROLE_GRANT, &d.key, *user).await?;
            report
                .grants
                .push(("roles".into(), d.key.clone(), *user, "revoked"));
        }
        Ok(())
    }

    /// Makes the users with declared, verified emails site admins, and takes it back from those
    /// the file made admins and no longer lists. With `exclusive`, other admins are demoted
    /// too, but only once a declared admin is site admin, so the instance keeps one.
    async fn apply_site_admins(
        &self,
        emails: Option<&[String]>,
        exclusive: bool,
        report: &mut ApplyReport,
    ) -> Result<()> {
        let declared = self.db.declared();
        declared.set_site_admins(emails).await?;
        let Some(emails) = emails else {
            declared.remove_grants_of_kind(SITE_ADMIN_GRANT).await?;
            return Ok(());
        };
        let wanted: HashSet<UserId> = declared
            .users_with_verified_emails(emails)
            .await?
            .into_iter()
            .collect();
        let admins: HashSet<UserId> = declared.site_admins().await?.into_iter().collect();
        let granted: HashSet<UserId> = declared
            .grants(SITE_ADMIN_GRANT, "")
            .await?
            .into_iter()
            .collect();
        let users = self.db.users();
        for user in wanted.difference(&admins) {
            users.set_site_admin(*user, true).await?;
            declared.add_grant(SITE_ADMIN_GRANT, "", *user).await?;
            report
                .grants
                .push(("siteAdmins".into(), String::new(), *user, "granted"));
        }
        let mut demote: Vec<UserId> = granted.difference(&wanted).copied().collect();
        if exclusive && !wanted.is_empty() {
            demote.extend(admins.difference(&wanted).filter(|u| !granted.contains(u)));
        }
        let mut remaining = admins.union(&wanted).count();
        for user in demote {
            if !admins.contains(&user) {
                declared.remove_grant(SITE_ADMIN_GRANT, "", user).await?;
                continue;
            }
            if remaining <= 1 {
                report.errors.push(format!(
                    "siteAdmins: not demoting user {} (the last site admin)",
                    user.0
                ));
                continue;
            }
            users.set_site_admin(user, false).await?;
            declared.remove_grant(SITE_ADMIN_GRANT, "", user).await?;
            remaining -= 1;
            report
                .grants
                .push(("siteAdmins".into(), String::new(), user, "revoked"));
        }
        Ok(())
    }
}

fn int(id: &str) -> Result<i64> {
    id.parse()
        .map_err(|_| ApplyError::Invalid(format!("stored object id {id:?} is not a number")))
}

fn int32(id: &str) -> Result<i32> {
    i32::try_from(int(id)?)
        .map_err(|_| ApplyError::Invalid(format!("stored object id {id:?} is out of range")))
}

/// Executor secret object ids are `<scope>/<id>`.
fn secret_id(id: &str) -> Result<(ExecutorSecretScope, i64)> {
    let (scope, n) = id
        .split_once('/')
        .ok_or_else(|| ApplyError::Invalid(format!("stored object id {id:?} has no scope")))?;
    let scope = match scope {
        "batches" => ExecutorSecretScope::Batches,
        "codeintel" => ExecutorSecretScope::CodeIntel,
        other => {
            return Err(ApplyError::Invalid(format!(
                "unknown secret scope {other:?}"
            )));
        }
    };
    Ok((scope, int(n)?))
}

fn secret_scope(s: &ExecutorSecretDecl) -> ExecutorSecretScope {
    match scope_str(s.scope) {
        "codeintel" => ExecutorSecretScope::CodeIntel,
        _ => ExecutorSecretScope::Batches,
    }
}

fn event_types(w: &OutboundWebhookDecl) -> Vec<ScopedEventType> {
    w.event_types
        .iter()
        .map(|e| {
            let (event_type, scope) = e.parts();
            ScopedEventType {
                event_type: event_type.to_owned(),
                scope: scope.map(str::to_owned),
            }
        })
        .collect()
}

fn policy_fields(p: &CodeIntelPolicyDecl) -> Result<PolicyFields> {
    crate::check::policy_fields(p).map_err(|e| ApplyError::Invalid(e.join("; ")))
}

fn flag_value(f: &FeatureFlagDecl) -> FeatureFlagValue {
    match (f.value, f.rollout_basis_points) {
        (Some(v), _) => FeatureFlagValue::Bool(v),
        (None, Some(r)) => FeatureFlagValue::Rollout(r),
        (None, None) => FeatureFlagValue::Bool(false),
    }
}

/// A name conflict as a readable error.
fn taken(e: DbError, message: &str) -> ApplyError {
    match e {
        DbError::AlreadyExists { .. } => ApplyError::Invalid(format!(
            "{message} and belongs to another entry or was created in the UI with a different identity"
        )),
        other => ApplyError::Db(other),
    }
}
