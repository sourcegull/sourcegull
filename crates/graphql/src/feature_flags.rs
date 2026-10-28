//! Feature flags: `featureFlags`, `featureFlag`,
//! `evaluateFeatureFlag`, `evaluatedFeatureFlags`, `organizationFeatureFlagValue`,
//! `organizationFeatureFlagOverrides`, `createFeatureFlag`, `updateFeatureFlag`,
//! `deleteFeatureFlag`, `createFeatureFlagOverride`, `updateFeatureFlagOverride`,
//! `deleteFeatureFlagOverride`. Managing flags and overrides is for site admins; evaluating is
//! for every signed-in user (for themself and their orgs).
//!
//! Flags evaluate with [`sourcegull_config::evaluate`]: a user override, then org overrides (`true`
//! wins), then the flag's value or rollout (a stable hash of flag name and user id).

use std::collections::HashMap;

use async_graphql::{Context, ID, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_config::{FULL_ROLLOUT, FeatureFlag as Flag, FlagKind, FlagOverrides};
use sourcegull_db::{
    DbError, FeatureFlagValue, Namespace, OverrideNamespace, StoredFeatureFlag, StoredFlagOverride,
};
use sourcegull_services::relay::{marshal_string_id, unmarshal_id};
use sourcegull_types::{OrgId, UserId};

use crate::EmptyResponse;
use crate::context::{codes, db_error, error, require_site_admin, require_viewer, services};
use crate::declared;
use crate::namespace::{NamespaceNode, is_member, load_namespace, parse_namespace_id};

/// Node kind of override IDs.
pub const OVERRIDE_KIND: &str = "FeatureFlagOverride";

fn store_error(e: DbError) -> async_graphql::Error {
    match e {
        DbError::Invalid(m) => error(codes::INVALID_INPUT, m),
        DbError::AlreadyExists { .. } => error(codes::ALREADY_EXISTS, "It already exists."),
        e => db_error(e),
    }
}

fn not_found(name: &str) -> async_graphql::Error {
    error(
        codes::NOT_FOUND,
        format!("Feature flag {name:?} not found."),
    )
}

/// The value from `value` / `rolloutBasisPoints` (exactly one must be set).
fn flag_value(value: Option<bool>, rollout: Option<i32>) -> Result<FeatureFlagValue> {
    match (value, rollout) {
        (Some(v), None) => Ok(FeatureFlagValue::Bool(v)),
        (None, Some(r)) if (0..=10_000).contains(&r) => Ok(FeatureFlagValue::Rollout(r)),
        (None, Some(_)) => Err(error(
            codes::INVALID_INPUT,
            "rolloutBasisPoints must be between 0 and 10000.",
        )),
        _ => Err(error(
            codes::INVALID_INPUT,
            "Set exactly one of value and rolloutBasisPoints.",
        )),
    }
}

fn to_flag(f: &StoredFeatureFlag) -> Flag {
    Flag {
        name: f.name.clone(),
        kind: match f.value {
            FeatureFlagValue::Bool(v) => FlagKind::Bool(v),
            FeatureFlagValue::Rollout(r) => FlagKind::Rollout {
                basis_points: u32::try_from(r).unwrap_or(0).min(FULL_ROLLOUT),
            },
        },
    }
}

/// The key inside an override ID.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct OverrideKey {
    #[serde(rename = "flagName")]
    flag_name: String,
    #[serde(rename = "userID", skip_serializing_if = "Option::is_none")]
    user_id: Option<i64>,
    #[serde(rename = "orgID", skip_serializing_if = "Option::is_none")]
    org_id: Option<i64>,
}

fn override_id(o: &StoredFlagOverride) -> ID {
    let (user_id, org_id) = match o.namespace {
        OverrideNamespace::User(u) => (Some(u), None),
        OverrideNamespace::Org(g) => (None, Some(g)),
    };
    let key = serde_json::to_string(&OverrideKey {
        flag_name: o.flag_name.clone(),
        user_id,
        org_id,
    })
    .unwrap_or_default();
    ID(marshal_string_id(OVERRIDE_KIND, &key))
}

fn parse_override_id(id: &str) -> Result<(String, OverrideNamespace)> {
    let invalid = || error(codes::INVALID_INPUT, format!("Invalid override ID {id:?}."));
    let (kind, spec) = unmarshal_id(id).ok_or_else(invalid)?;
    if kind != OVERRIDE_KIND {
        return Err(invalid());
    }
    let key: String = serde_json::from_str(&spec).map_err(|_| invalid())?;
    let key: OverrideKey = serde_json::from_str(&key).map_err(|_| invalid())?;
    let ns = match (key.user_id, key.org_id) {
        (Some(u), None) => OverrideNamespace::User(u),
        (None, Some(o)) => OverrideNamespace::Org(o),
        _ => return Err(invalid()),
    };
    Ok((key.flag_name, ns))
}

/// A flag with the same value for everyone (overrides aside).
pub struct FeatureFlagBooleanNode(StoredFeatureFlag);

#[Object(name = "FeatureFlagBoolean")]
impl FeatureFlagBooleanNode {
    /// The name of the feature flag.
    async fn name(&self) -> &str {
        &self.0.name
    }

    /// The static value of the feature flag.
    async fn value(&self) -> bool {
        matches!(self.0.value, FeatureFlagValue::Bool(true))
    }

    /// Overrides that apply to the feature flag (site admins only).
    async fn overrides(&self, ctx: &Context<'_>) -> Result<Vec<FeatureFlagOverrideNode>> {
        overrides(ctx, &self.0.name).await
    }

    /// The resources file entry that manages the flag (`featureFlags[<key>]`), or
    /// `featureFlags` when the file manages every flag; such flags are read-only (overrides
    /// stay editable).
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        declared::managed_by(services(ctx)?, declared::kinds::FEATURE_FLAGS, &self.0.name).await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

/// A flag on for a stable share of users.
pub struct FeatureFlagRolloutNode(StoredFeatureFlag);

#[Object(name = "FeatureFlagRollout")]
impl FeatureFlagRolloutNode {
    /// The name of the feature flag.
    async fn name(&self) -> &str {
        &self.0.name
    }

    /// The share of users the flag is on for, in basis points (0.01%).
    async fn rollout_basis_points(&self) -> i32 {
        match self.0.value {
            FeatureFlagValue::Rollout(r) => r,
            FeatureFlagValue::Bool(v) => {
                if v {
                    10_000
                } else {
                    0
                }
            }
        }
    }

    /// Overrides that apply to the feature flag (site admins only).
    async fn overrides(&self, ctx: &Context<'_>) -> Result<Vec<FeatureFlagOverrideNode>> {
        overrides(ctx, &self.0.name).await
    }

    /// The resources file entry that manages the flag (`featureFlags[<key>]`), or
    /// `featureFlags` when the file manages every flag; such flags are read-only (overrides
    /// stay editable).
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        declared::managed_by(services(ctx)?, declared::kinds::FEATURE_FLAGS, &self.0.name).await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

/// A boolean or rollout feature flag.
#[derive(Union)]
#[graphql(name = "FeatureFlag")]
pub enum FeatureFlagNode {
    Boolean(FeatureFlagBooleanNode),
    Rollout(FeatureFlagRolloutNode),
}

impl From<StoredFeatureFlag> for FeatureFlagNode {
    fn from(f: StoredFeatureFlag) -> Self {
        match f.value {
            FeatureFlagValue::Bool(_) => Self::Boolean(FeatureFlagBooleanNode(f)),
            FeatureFlagValue::Rollout(_) => Self::Rollout(FeatureFlagRolloutNode(f)),
        }
    }
}

async fn overrides(ctx: &Context<'_>, flag: &str) -> Result<Vec<FeatureFlagOverrideNode>> {
    require_site_admin(ctx)?;
    Ok(services(ctx)?
        .db
        .feature_flags()
        .overrides(flag)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(FeatureFlagOverrideNode)
        .collect())
}

/// An override of a flag's value for a user or an org.
pub struct FeatureFlagOverrideNode(StoredFlagOverride);

#[Object(name = "FeatureFlagOverride")]
impl FeatureFlagOverrideNode {
    /// A unique ID for this override.
    async fn id(&self) -> ID {
        override_id(&self.0)
    }

    /// The user or org the override applies to.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<NamespaceNode> {
        let ns = match self.0.namespace {
            OverrideNamespace::User(u) => Namespace::User(UserId(u)),
            OverrideNamespace::Org(o) => Namespace::Org(OrgId(o)),
        };
        load_namespace(ctx, ns)
            .await?
            .ok_or_else(|| error(codes::NOT_FOUND, "The namespace no longer exists."))
    }

    /// The flag being overridden.
    async fn target_flag(&self, ctx: &Context<'_>) -> Result<FeatureFlagNode> {
        services(ctx)?
            .db
            .feature_flags()
            .get(&self.0.flag_name)
            .await
            .map_err(db_error)?
            .map(FeatureFlagNode::from)
            .ok_or_else(|| not_found(&self.0.flag_name))
    }

    /// The overridden value.
    async fn value(&self) -> bool {
        self.0.value
    }
}

/// A flag evaluated for the viewer.
#[derive(SimpleObject)]
pub struct EvaluatedFeatureFlag {
    name: String,
    value: bool,
}

/// The viewer's org ids.
async fn viewer_orgs(ctx: &Context<'_>, user: UserId) -> Result<Vec<i64>> {
    Ok(services(ctx)?
        .db
        .orgs()
        .list_for_user(user)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|o| o.id.get())
        .collect())
}

/// Overrides grouped by flag.
fn group(overrides: Vec<StoredFlagOverride>) -> HashMap<String, FlagOverrides> {
    let mut out: HashMap<String, FlagOverrides> = HashMap::new();
    for o in overrides {
        let e = out.entry(o.flag_name).or_default();
        match o.namespace {
            OverrideNamespace::User(u) => {
                e.users.insert(u, o.value);
            }
            OverrideNamespace::Org(g) => {
                e.orgs.insert(g, o.value);
            }
        }
    }
    out
}

/// Every flag evaluated for `user` in `orgs`.
async fn evaluate_all(
    ctx: &Context<'_>,
    user: Option<i64>,
    orgs: &[i64],
) -> Result<Vec<EvaluatedFeatureFlag>> {
    let store = services(ctx)?.db.feature_flags();
    let flags = store.list().await.map_err(db_error)?;
    let overrides = group(store.overrides_for(user, orgs).await.map_err(db_error)?);
    let empty = FlagOverrides::default();
    Ok(flags
        .iter()
        .map(|f| EvaluatedFeatureFlag {
            name: f.name.clone(),
            value: sourcegull_config::evaluate(
                &to_flag(f),
                user,
                orgs,
                overrides.get(&f.name).unwrap_or(&empty),
            ),
        })
        .collect())
}

/// Feature flag queries.
#[derive(Default)]
pub struct FeatureFlagsQuery;

#[Object]
impl FeatureFlagsQuery {
    /// The feature flags, by name (site admins only).
    async fn feature_flags(&self, ctx: &Context<'_>) -> Result<Vec<FeatureFlagNode>> {
        require_site_admin(ctx)?;
        Ok(services(ctx)?
            .db
            .feature_flags()
            .list()
            .await
            .map_err(db_error)?
            .into_iter()
            .map(FeatureFlagNode::from)
            .collect())
    }

    /// One feature flag (site admins only).
    async fn feature_flag(&self, ctx: &Context<'_>, name: String) -> Result<FeatureFlagNode> {
        require_site_admin(ctx)?;
        services(ctx)?
            .db
            .feature_flags()
            .get(&name)
            .await
            .map_err(db_error)?
            .map(FeatureFlagNode::from)
            .ok_or_else(|| not_found(&name))
    }

    /// The flag evaluated for the viewer; null if it does not exist.
    async fn evaluate_feature_flag(
        &self,
        ctx: &Context<'_>,
        flag_name: String,
    ) -> Result<Option<bool>> {
        let actor = require_viewer(ctx)?;
        let orgs = viewer_orgs(ctx, actor.user_id).await?;
        Ok(evaluate_all(ctx, Some(actor.user_id.get()), &orgs)
            .await?
            .into_iter()
            .find(|f| f.name == flag_name)
            .map(|f| f.value))
    }

    /// Every flag evaluated for the viewer.
    async fn evaluated_feature_flags(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<EvaluatedFeatureFlag>> {
        let actor = require_viewer(ctx)?;
        let orgs = viewer_orgs(ctx, actor.user_id).await?;
        evaluate_all(ctx, Some(actor.user_id.get()), &orgs).await
    }

    /// The flag's value for an org (its override, else the flag's value; a rollout flag counts
    /// as on only at 100%). Members of the org and site admins only.
    #[graphql(name = "organizationFeatureFlagValue")]
    async fn organization_feature_flag_value(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "orgID")] org_id: ID,
        flag_name: String,
    ) -> Result<bool> {
        let actor = require_viewer(ctx)?;
        let Namespace::Org(org) = parse_namespace_id(&org_id)? else {
            return Err(error(
                codes::INVALID_INPUT,
                "orgID must be an organization.",
            ));
        };
        if !actor.site_admin && !is_member(ctx, org, actor.user_id).await? {
            return Err(error(
                codes::FORBIDDEN,
                "You are not a member of the organization.",
            ));
        }
        Ok(evaluate_all(ctx, None, &[org.get()])
            .await?
            .into_iter()
            .find(|f| f.name == flag_name)
            .is_some_and(|f| f.value))
    }

    /// The overrides of the viewer's orgs.
    async fn organization_feature_flag_overrides(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<FeatureFlagOverrideNode>> {
        let actor = require_viewer(ctx)?;
        let orgs = viewer_orgs(ctx, actor.user_id).await?;
        Ok(services(ctx)?
            .db
            .feature_flags()
            .overrides_for(None, &orgs)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(FeatureFlagOverrideNode)
            .collect())
    }
}

/// Feature flag mutations (site admins only).
#[derive(Default)]
pub struct FeatureFlagsMutation;

#[Object]
impl FeatureFlagsMutation {
    /// Creates a flag: a boolean one with `value`, or a rollout one with
    /// `rolloutBasisPoints`.
    async fn create_feature_flag(
        &self,
        ctx: &Context<'_>,
        name: String,
        value: Option<bool>,
        rollout_basis_points: Option<i32>,
    ) -> Result<FeatureFlagNode> {
        require_site_admin(ctx)?;
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(error(codes::INVALID_INPUT, "The flag name is empty."));
        }
        declared::ensure_creatable(services(ctx)?, declared::kinds::FEATURE_FLAGS).await?;
        let value = flag_value(value, rollout_basis_points)?;
        Ok(services(ctx)?
            .db
            .feature_flags()
            .create(&name, value)
            .await
            .map_err(store_error)?
            .into())
    }

    /// Deletes a flag and its overrides.
    async fn delete_feature_flag(&self, ctx: &Context<'_>, name: String) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        declared::ensure_not_declared(services(ctx)?, declared::kinds::FEATURE_FLAGS, &name)
            .await?;
        services(ctx)?
            .db
            .feature_flags()
            .delete(&name)
            .await
            .map_err(|e| match e {
                DbError::NotFound(_) => not_found(&name),
                e => store_error(e),
            })?;
        Ok(EmptyResponse)
    }

    /// Changes a flag's value (and possibly its type).
    async fn update_feature_flag(
        &self,
        ctx: &Context<'_>,
        name: String,
        value: Option<bool>,
        rollout_basis_points: Option<i32>,
    ) -> Result<FeatureFlagNode> {
        require_site_admin(ctx)?;
        let value = flag_value(value, rollout_basis_points)?;
        declared::ensure_not_declared(services(ctx)?, declared::kinds::FEATURE_FLAGS, &name)
            .await?;
        Ok(services(ctx)?
            .db
            .feature_flags()
            .update(&name, value)
            .await
            .map_err(|e| match e {
                DbError::NotFound(_) => not_found(&name),
                e => store_error(e),
            })?
            .into())
    }

    /// Overrides a flag for a user or an org (`namespace` is a user or org ID).
    async fn create_feature_flag_override(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        flag_name: String,
        value: bool,
    ) -> Result<FeatureFlagOverrideNode> {
        require_site_admin(ctx)?;
        let ns = parse_namespace_id(&namespace)?;
        if load_namespace(ctx, ns).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        let ns = match ns {
            Namespace::User(u) => OverrideNamespace::User(u.get()),
            Namespace::Org(o) => OverrideNamespace::Org(o.get()),
        };
        services(ctx)?
            .db
            .feature_flags()
            .create_override(&flag_name, ns, value)
            .await
            .map(FeatureFlagOverrideNode)
            .map_err(|e| match e {
                DbError::NotFound(_) => not_found(&flag_name),
                e => store_error(e),
            })
    }

    /// Deletes an override.
    async fn delete_feature_flag_override(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let (flag, ns) = parse_override_id(&id)?;
        services(ctx)?
            .db
            .feature_flags()
            .delete_override(&flag, ns)
            .await
            .map_err(store_error)?;
        Ok(EmptyResponse)
    }

    /// Changes an override's value.
    async fn update_feature_flag_override(
        &self,
        ctx: &Context<'_>,
        id: ID,
        value: bool,
    ) -> Result<FeatureFlagOverrideNode> {
        require_site_admin(ctx)?;
        let (flag, ns) = parse_override_id(&id)?;
        services(ctx)?
            .db
            .feature_flags()
            .update_override(&flag, ns, value)
            .await
            .map(FeatureFlagOverrideNode)
            .map_err(store_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_ids_round_trip() {
        for ns in [OverrideNamespace::User(7), OverrideNamespace::Org(9)] {
            let o = StoredFlagOverride {
                flag_name: "a:b\"c".into(),
                namespace: ns,
                value: true,
            };
            let id = override_id(&o);
            assert_eq!(parse_override_id(&id).unwrap(), ("a:b\"c".to_owned(), ns));
        }
        assert!(parse_override_id("bogus").is_err());
    }

    #[test]
    fn flag_values() {
        assert_eq!(
            flag_value(Some(true), None).unwrap(),
            FeatureFlagValue::Bool(true)
        );
        assert_eq!(
            flag_value(None, Some(250)).unwrap(),
            FeatureFlagValue::Rollout(250)
        );
        assert!(flag_value(None, None).is_err());
        assert!(flag_value(Some(true), Some(1)).is_err());
        assert!(flag_value(None, Some(10_001)).is_err());
    }
}
