//! Event payloads: the bulk change or changeset as JSON (snake_case fields, GraphQL node
//! IDs).
//!
//! Bulk change events carry:
//!
//! ```json
//! {"id": "<BatchChange ID>", "namespace_id": "<User or Org ID>", "name": "hello",
//!  "description": "…", "state": "OPEN", "creator_user_id": "<User ID>",
//!  "last_applier_user_id": "<User ID>",
//!  "url": "https://sourcegull.example.com/users/alice/bulk-changes/hello",
//!  "created_at": "…", "updated_at": "…", "last_applied_at": "…", "closed_at": null}
//! ```
//!
//! Changeset events carry:
//!
//! ```json
//! {"id": "<Changeset ID>", "external_id": "12", "batch_change_ids": ["<BatchChange ID>"],
//!  "owning_batch_change_id": "<BatchChange ID>", "repository_id": "<Repository ID>",
//!  "created_at": "…", "updated_at": "…", "title": "…", "body": "…", "author_name": "…",
//!  "author_email": null, "state": "OPEN", "labels": ["bug"], "external_url": "https://gitlab…",
//!  "fork_namespace": null, "fork_name": null, "review_state": "PENDING",
//!  "check_state": "PASSED", "error": null, "syncer_error": null}
//! ```

use serde_json::{Value, json};
use sourcegull_batches::store::{BatchChange, Changeset, ChangesetSpecStore, PublicationState};
use sourcegull_config::SiteConfig;
use sourcegull_db::{Db, DbError, Namespace};
use sourcegull_services::relay::marshal_id;

/// Node kind of bulk change IDs (as in the GraphQL API).
pub const BATCH_CHANGE_KIND: &str = "BatchChange";
/// Node kind of changeset IDs.
pub const CHANGESET_KIND: &str = "Changeset";

fn user_id(id: sourcegull_types::UserId) -> String {
    marshal_id("User", id.get())
}

/// The bulk change's page: absolute under `externalURL` when set.
async fn batch_change_url(
    db: &Db,
    config: &SiteConfig,
    bc: &BatchChange,
) -> Result<String, DbError> {
    let ns = match bc.namespace {
        Namespace::User(u) => db
            .users()
            .get_by_id(u)
            .await?
            .map(|u| format!("/users/{}", u.username)),
        Namespace::Org(o) => db
            .orgs()
            .get(o)
            .await?
            .map(|o| format!("/organizations/{}", o.name)),
    }
    .unwrap_or_default();
    let base = config
        .external_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    Ok(format!("{base}{ns}/bulk-changes/{}", bc.name))
}

/// The payload of a `batch_change:*` event.
pub async fn batch_change(
    db: &Db,
    config: &SiteConfig,
    bc: &BatchChange,
) -> Result<Value, DbError> {
    let namespace_id = match bc.namespace {
        Namespace::User(u) => user_id(u),
        Namespace::Org(o) => marshal_id("Org", o.get()),
    };
    Ok(json!({
        "id": marshal_id(BATCH_CHANGE_KIND, bc.id),
        "namespace_id": namespace_id,
        "name": bc.name,
        "description": bc.description,
        "state": bc.state().as_str(),
        "creator_user_id": bc.creator_id.map(user_id),
        "last_applier_user_id": bc.last_applier_id.map(user_id),
        "url": batch_change_url(db, config, bc).await?,
        "created_at": bc.created_at,
        "updated_at": bc.updated_at,
        "last_applied_at": bc.last_applied_at,
        "closed_at": bc.closed_at,
    }))
}

/// The raw merge request in the synced metadata.
fn merge_request(metadata: &Value) -> &Value {
    match metadata.get("mergeRequest") {
        Some(mr) if mr.is_object() => mr,
        _ => metadata,
    }
}

/// The payload of a `changeset:*` event. Title and body come from the code host once the
/// changeset is published, else from its spec.
pub async fn changeset(db: &Db, ch: &Changeset) -> Result<Value, DbError> {
    let published = ch.publication_state == PublicationState::Published;
    let (title, body) = if published {
        (ch.title.clone(), ch.body.clone())
    } else {
        match ch.current_spec_id {
            Some(id) => ChangesetSpecStore::new(db)
                .get(id)
                .await?
                .map_or((None, None), |s| (s.title, s.body)),
            None => (None, None),
        }
    };
    let mr = merge_request(&ch.metadata);
    let author_name = published
        .then(|| {
            let author = mr.get("author")?;
            author
                .get("name")
                .or_else(|| author.get("username"))?
                .as_str()
                .map(str::to_owned)
        })
        .flatten();
    let labels: Vec<String> = if published {
        mr.get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| match l {
                        Value::String(s) => Some(s.clone()),
                        Value::Object(o) => o.get("name")?.as_str().map(str::to_owned),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let external_url = (published
        && ch.state != sourcegull_batches::store::ChangesetState::Deleted)
        .then(|| {
            mr.get("web_url")
                .or_else(|| mr.get("url"))
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
                .map(str::to_owned)
        })
        .flatten();
    Ok(json!({
        "id": marshal_id(CHANGESET_KIND, ch.id),
        "external_id": ch.external_id,
        "batch_change_ids": ch
            .batch_change_ids
            .keys()
            .map(|id| marshal_id(BATCH_CHANGE_KIND, *id))
            .collect::<Vec<_>>(),
        "owning_batch_change_id": ch
            .owned_by_batch_change_id
            .map(|id| marshal_id(BATCH_CHANGE_KIND, id)),
        "repository_id": marshal_id("Repository", i64::from(ch.repo_id.get())),
        "created_at": ch.created_at,
        "updated_at": ch.updated_at,
        "title": title,
        "body": body,
        "author_name": author_name,
        "author_email": Value::Null,
        "state": ch.state.as_str(),
        "labels": labels,
        "external_url": external_url,
        "fork_namespace": ch.external_fork_namespace,
        "fork_name": ch.external_fork_name,
        "review_state": published.then(|| {
            ch.external_review_state
                .unwrap_or(sourcegull_batches::store::ReviewState::Pending)
                .as_str()
        }),
        "check_state": ch.external_check_state.map(|s| s.as_str()),
        "error": ch.failure_message,
        "syncer_error": ch.syncer_error,
    }))
}
