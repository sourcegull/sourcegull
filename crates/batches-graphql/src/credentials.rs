//! Credentials for publishing changesets (`BatchChangesCredential`) and the code hosts they
//! are for (`BatchChangesCodeHost`).
//!
//! A credential is stored for a code host connection's base URL (the repos' service id), after
//! the code host accepted it (`ChangesetSource::validate_credential`: GitLab `GET /user` and
//! the `api` scope). A user's credential is used before the site's.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_batches::store::{
    BatchesCredential, BatchesCredentialStore, CredentialKind, CredentialSecret,
};
use sourcegull_codehost::{CodeHostError, Credential};
use sourcegull_db::DbError;
use sourcegull_services::batches::access::{self, CredentialAction};
use sourcegull_services::relay::marshal_string_id;
use sourcegull_types::{ExternalServiceKind, UserId};

use super::{
    CREDENTIAL_KIND, audit, batch_codes, check_first, invalid, invalid_id, not_found,
    parse_string_id, require_enabled,
};
use crate::context::{access_error, codes, db_error, error, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::codehost::ExternalServiceKindGql;

/// How long the code host has to accept a credential.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Security event names.
const CREDENTIAL_CREATED: &str = "BatchChangesCredentialCreated";
const CREDENTIAL_DELETED: &str = "BatchChangesCredentialDeleted";

/// A credential (without its secret).
#[derive(Clone)]
pub struct CredentialNode(pub BatchesCredential);

/// The ID of a credential (`user:<id>` or `site:<id>`).
pub fn credential_id(c: &BatchesCredential) -> ID {
    let prefix = if c.is_site_credential() {
        "site"
    } else {
        "user"
    };
    ID(marshal_string_id(
        CREDENTIAL_KIND,
        &format!("{prefix}:{}", c.id),
    ))
}

/// Parses a credential ID into (is site credential, row id).
fn parse_credential_id(id: &str) -> Option<(bool, i64)> {
    let spec = parse_string_id(CREDENTIAL_KIND, id)?;
    let (kind, key) = spec.split_once(':')?;
    let site = match kind.to_ascii_lowercase().as_str() {
        "site" => true,
        "user" => false,
        _ => return None,
    };
    Some((site, key.parse().ok().filter(|n| *n > 0)?))
}

/// The connection kind of a credential's `external_service_type` (GitLab is the only one).
fn kind_of(_service_type: &str) -> ExternalServiceKindGql {
    ExternalServiceKindGql::GitLab
}

#[Object(name = "BatchChangesCredential")]
impl CredentialNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        credential_id(&self.0)
    }

    async fn external_service_kind(&self) -> ExternalServiceKindGql {
        kind_of(&self.0.external_service_type)
    }

    /// The code host's base URL.
    #[graphql(name = "externalServiceURL")]
    async fn external_service_url(&self) -> &str {
        &self.0.external_service_id
    }

    /// The public key to add to the code host for pushing over SSH, if the credential has a
    /// keypair.
    #[graphql(name = "sshPublicKey")]
    async fn ssh_public_key(&self) -> Option<&str> {
        self.0.ssh_public_key.as_deref()
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// Whether this is the site's credential (used when a user has none).
    async fn is_site_credential(&self) -> bool {
        self.0.is_site_credential()
    }
}

/// A code host changesets can be published to.
pub struct CodeHostNode {
    kind: ExternalServiceKind,
    url: String,
    has_webhooks: bool,
    credential: Option<BatchesCredential>,
}

#[Object(name = "BatchChangesCodeHost")]
impl CodeHostNode {
    async fn external_service_kind(&self) -> ExternalServiceKindGql {
        self.kind.into()
    }

    /// The code host's base URL.
    #[graphql(name = "externalServiceURL")]
    async fn external_service_url(&self) -> &str {
        &self.url
    }

    /// The credential changesets would be published with.
    async fn credential(&self) -> Option<CredentialNode> {
        self.credential.clone().map(CredentialNode)
    }

    /// Whether pushing needs an SSH key (pushes go over HTTPS with the token: false).
    #[graphql(name = "requiresSSH")]
    async fn requires_ssh(&self) -> bool {
        false
    }

    /// Whether a username is needed besides the token (not for GitLab).
    async fn requires_username(&self) -> bool {
        false
    }

    /// Whether commits can be signed by a GitHub App (GitHub only).
    async fn supports_commit_signing(&self) -> bool {
        false
    }

    /// Whether the code host connection receives webhooks.
    async fn has_webhooks(&self) -> bool {
        self.has_webhooks
    }
}

/// A page of code hosts.
#[derive(SimpleObject)]
pub struct BatchChangesCodeHostConnection {
    pub nodes: Vec<CodeHostNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// The distinct code hosts of the live connections (kind, base URL, whether any connection
/// for it has webhooks), by URL.
async fn connection_hosts(
    ctx: &Context<'_>,
) -> Result<BTreeMap<String, (ExternalServiceKind, bool)>> {
    let svc = services(ctx)?;
    let mut hosts = BTreeMap::new();
    for kind in [ExternalServiceKind::GitLab] {
        let conns = svc
            .db
            .external_services(svc.keyring.clone())
            .list_with_config(kind)
            .await
            .map_err(db_error)?;
        for (_, config) in conns {
            let Ok(url) = svc.registry.service_id(kind, &config) else {
                continue;
            };
            let webhooks = svc.registry.has_webhooks(kind, &config);
            let entry = hosts.entry(url).or_insert((kind, false));
            entry.1 |= webhooks;
        }
    }
    Ok(hosts)
}

/// `batchChangesCodeHosts` (site credentials) and `User.batchChangesCodeHosts` (the user's,
/// else the site's; the user themself or site admins only).
pub async fn code_hosts(
    ctx: &Context<'_>,
    user: Option<UserId>,
    first: Option<i32>,
    after: Option<String>,
) -> Result<BatchChangesCodeHostConnection> {
    let actor = require_enabled(ctx)?;
    if let Some(u) = user {
        access::require_user_credentials(actor, Some(u), CredentialAction::ListCodeHosts)
            .map_err(access_error)?;
    }
    let limit = usize::try_from(check_first(first)?).unwrap_or(0);
    let offset = match after.as_deref() {
        None | Some("") => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| invalid(format!("Invalid cursor {c:?}.")))?,
    };
    let hosts = connection_hosts(ctx).await?;
    let store = BatchesCredentialStore::new(&services(ctx)?.db, None);
    let mut creds: HashMap<(String, String), BatchesCredential> = HashMap::new();
    for c in store.site_credentials().await.map_err(db_error)? {
        creds.insert(
            (
                c.external_service_type.clone(),
                c.external_service_id.clone(),
            ),
            c,
        );
    }
    if let Some(u) = user {
        for c in store.user_credentials(u).await.map_err(db_error)? {
            creds.insert(
                (
                    c.external_service_type.clone(),
                    c.external_service_id.clone(),
                ),
                c,
            );
        }
    }
    let total = hosts.len();
    let end = offset.saturating_add(limit).min(total);
    let has_next_page = end < total;
    let nodes = hosts
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(url, (kind, has_webhooks))| CodeHostNode {
            credential: creds.remove(&(service_type(kind).to_owned(), url.clone())),
            kind,
            url,
            has_webhooks,
        })
        .collect();
    Ok(BatchChangesCodeHostConnection {
        nodes,
        total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        page_info: PageInfo {
            has_next_page,
            end_cursor: has_next_page.then(|| end.to_string()),
        },
    })
}

/// The repos' `external_service_type` of a connection kind.
fn service_type(kind: ExternalServiceKind) -> &'static str {
    kind.service_type()
}

/// A credential by ID for `node(id:)`: the owner or site admins; site credentials for site
/// admins only.
pub(super) async fn load_node(ctx: &Context<'_>, id: &str) -> Result<Option<CredentialNode>> {
    let actor = require_enabled(ctx)?;
    let Some((site, key)) = parse_credential_id(id) else {
        return Ok(None);
    };
    let store = BatchesCredentialStore::new(&services(ctx)?.db, None);
    let cred = if site {
        access::require_site_credentials(actor, CredentialAction::View).map_err(access_error)?;
        store.site_credential_by_id(key).await
    } else {
        store.user_credential_by_id(key).await
    }
    .map_err(db_error)?;
    let Some(c) = cred else {
        return Ok(None);
    };
    if let Some(u) = c.user_id {
        access::require_user_credentials(actor, Some(u), CredentialAction::View)
            .map_err(access_error)?;
    }
    Ok(Some(CredentialNode(c)))
}

/// `createBatchChangesCredential`.
pub(super) async fn create(
    ctx: &Context<'_>,
    user: Option<&str>,
    kind: ExternalServiceKindGql,
    url: &str,
    token: &str,
) -> Result<CredentialNode> {
    let actor = require_enabled(ctx)?;
    let user = user
        .map(sourcegull_graphql_core::ids::parse_user_id)
        .transpose()?;
    match user {
        Some(u) => access::require_user_credentials(actor, Some(u), CredentialAction::Add),
        None => access::require_site_credentials(actor, CredentialAction::Add),
    }
    .map_err(access_error)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(invalid("empty credential not allowed"));
    }
    let kind = ExternalServiceKind::from(kind);
    // Changesets are published only to GitLab, so a token for another host has no use.
    if kind != ExternalServiceKind::GitLab {
        return Err(invalid(
            "Bulk changes publish changesets to GitLab only; credentials for other code hosts \
             are not supported.",
        ));
    }
    let svc = services(ctx)?;
    // The connection whose base URL matches, normalized the way repos record it.
    let mut source = None;
    let mut service_id = None;
    for (es, config) in svc
        .db
        .external_services(svc.keyring.clone())
        .list_with_config(kind)
        .await
        .map_err(db_error)?
    {
        let Ok(id) = svc.registry.service_id(kind, &config) else {
            continue;
        };
        if same_url(&id, url) {
            source = Some(
                svc.registry
                    .for_connection(es.id.get())
                    .changeset_source(kind, &config)
                    .map_err(|e| error(codes::INTERNAL, e.to_string()))?,
            );
            service_id = Some(id);
            break;
        }
    }
    let (Some(source), Some(service_id)) = (source, service_id) else {
        return Err(invalid(format!(
            "There is no code host connection for {url:?}."
        )));
    };
    let stype = service_type(kind);
    let store = BatchesCredentialStore::new(&svc.db, svc.keyring.clone());
    let existing = match user {
        Some(u) => store.user_credential(u, stype, &service_id).await,
        None => store.site_credential(stype, &service_id).await,
    }
    .map_err(db_error)?;
    if existing.is_some() {
        return Err(error(
            batch_codes::DUPLICATE_CREDENTIAL,
            "a credential for this code host already exists",
        ));
    }
    let cred = Credential::Basic {
        username: "git".into(),
        password: token.to_owned(),
    };
    let timed_out =
        match tokio::time::timeout(VERIFY_TIMEOUT, source.validate_credential(&cred)).await {
            Ok(Ok(())) => false,
            Ok(Err(e)) => return Err(verify_failed(&e)),
            Err(_) => true,
        };
    let secret = CredentialSecret {
        kind: CredentialKind::PersonalAccessToken,
        token: token.to_owned(),
        ssh: None,
    };
    let saved = match user {
        Some(u) => {
            store
                .set_user_credential(u, stype, &service_id, &secret)
                .await
        }
        None => store.set_site_credential(stype, &service_id, &secret).await,
    }
    .map_err(|e| match e {
        DbError::Invalid(m) => error(codes::UNAVAILABLE, m),
        e => db_error(e),
    })?;
    audit(
        ctx,
        CREDENTIAL_CREATED,
        actor,
        json!({
            "id": saved.id,
            "site": saved.is_site_credential(),
            "userID": saved.user_id.map(|u| u.get()),
            "externalServiceURL": saved.external_service_id,
        }),
    )
    .await;
    if timed_out {
        // Saved, but the caller learns it could not be checked.
        return Err(error(
            batch_codes::VERIFY_CREDENTIAL_TIMEOUT,
            "Could not verify credentials within 10 seconds. Credentials have been saved, but \
             might not be valid.",
        ));
    }
    Ok(CredentialNode(saved))
}

fn verify_failed(e: &CodeHostError) -> async_graphql::Error {
    error(
        batch_codes::VERIFY_CREDENTIAL_FAILED,
        format!("Failed to verify the credential:\n```\n{e}\n```\n"),
    )
}

/// Whether two code host URLs are the same base URL (scheme and host case-insensitive,
/// trailing slashes ignored).
fn same_url(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// `deleteBatchChangesCredential`.
pub(super) async fn delete(ctx: &Context<'_>, id: &str) -> Result<EmptyResponse> {
    let actor = require_enabled(ctx)?;
    let (site, key) = parse_credential_id(id).ok_or_else(|| invalid_id(id))?;
    let store = BatchesCredentialStore::new(&services(ctx)?.db, None);
    if site {
        access::require_site_credentials(actor, CredentialAction::Delete).map_err(access_error)?;
        if !store.delete_site_credential(key).await.map_err(db_error)? {
            return Err(not_found("Credential"));
        }
    } else {
        let cred = store
            .user_credential_by_id(key)
            .await
            .map_err(db_error)?
            .ok_or_else(|| not_found("Credential"))?;
        access::require_user_credentials(actor, cred.user_id, CredentialAction::Delete)
            .map_err(access_error)?;
        store.delete_user_credential(key).await.map_err(db_error)?;
    }
    audit(
        ctx,
        CREDENTIAL_DELETED,
        actor,
        json!({ "id": key, "site": site }),
    )
    .await;
    Ok(EmptyResponse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let c = BatchesCredential {
            id: 7,
            user_id: None,
            external_service_type: "gitlab".into(),
            external_service_id: "https://gitlab.com/".into(),
            ssh_public_key: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(parse_credential_id(&credential_id(&c)), Some((true, 7)));
        let u = BatchesCredential {
            user_id: Some(UserId(1)),
            ..c
        };
        assert_eq!(parse_credential_id(&credential_id(&u)), Some((false, 7)));
        assert_eq!(parse_credential_id("bogus"), None);
    }

    #[test]
    fn urls() {
        assert!(same_url("https://gitlab.com/", "https://GitLab.com"));
        assert!(!same_url(
            "https://gitlab.com/",
            "https://gitlab.example.com/"
        ));
    }
}
