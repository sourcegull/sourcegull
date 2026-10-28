//! Changesets: `ExternalChangeset` (in a repository the viewer may read) and
//! `HiddenExternalChangeset` (id, state and dates only), their events, and listings.

use std::sync::Arc;

use async_graphql::{Context, ID, Interface, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sourcegull_batches::store::{
    Changeset, ChangesetSpecStore, ChangesetSpecType, ChangesetStore, ListChangesets,
    PublicationState, ReconcilerState,
};
use sourcegull_config::RolloutWindows;
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::{Repo, RepoAccess};

use super::batch_change::{self, BatchChangeConnection, BatchChangesArgs};
use super::batch_spec::VisibleChangesetSpecNode;
use super::{
    BatchChangeStateGql, CHANGESET_EVENT_KIND, CHANGESET_KIND, ChangesetCheckStateGql,
    ChangesetReviewStateGql, ChangesetStateGql, batch_change_id, changeset_id, check_first,
    parse_cursor, readable_repo, readable_repos,
};
use crate::context::{db_error, repo_access, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::codehost::{ExternalLink, ExternalServiceKindGql};
use sourcegull_graphql_core::diff::DiffStat;
use sourcegull_graphql_core::shared::{PersonData, PersonRef, RepositoryRef};

/// What both changeset types share.
struct Data {
    c: Changeset,
}

impl Data {
    fn id(&self) -> ID {
        changeset_id(self.c.id)
    }

    async fn batch_changes(
        &self,
        ctx: &Context<'_>,
        args: BatchChangesArgs,
    ) -> Result<BatchChangeConnection> {
        batch_change::list_for_changeset(ctx, self.c.id, args).await
    }
}

/// A changeset in a repository the viewer may read.
pub struct ExternalChangesetNode {
    d: Data,
    repo: Arc<Repo>,
}

/// A changeset in a repository the viewer may not read.
pub struct HiddenExternalChangesetNode {
    d: Data,
}

/// A changeset (`Changeset` interface).
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(
    name = "Changeset",
    field(name = "id", ty = "ID", desc = "The opaque node ID."),
    field(
        name = "batch_changes",
        ty = "BatchChangeConnection",
        desc = "The bulk changes the changeset belongs to.",
        arg(name = "first", ty = "Option<i32>", default_with = "Some(50)"),
        arg(name = "after", ty = "Option<String>"),
        arg(name = "state", ty = "Option<BatchChangeStateGql>"),
        arg(name = "states", ty = "Option<Vec<BatchChangeStateGql>>"),
        arg(name = "viewer_can_administer", ty = "Option<bool>")
    ),
    field(
        name = "state",
        ty = "ChangesetStateGql",
        desc = "The state shown in the UI."
    ),
    field(name = "created_at", ty = "DateTime<Utc>"),
    field(name = "updated_at", ty = "DateTime<Utc>"),
    field(
        name = "next_sync_at",
        ty = "Option<DateTime<Utc>>",
        desc = "When the changeset is synced with the code host next."
    )
)]
pub enum ChangesetNode {
    ExternalChangeset(ExternalChangesetNode),
    HiddenExternalChangeset(HiddenExternalChangesetNode),
}

impl ChangesetNode {
    /// `c` as seen by a viewer who may read `repo` (else hidden).
    pub fn new(c: Changeset, repo: Option<Repo>) -> Self {
        let d = Data { c };
        match repo {
            Some(repo) => Self::ExternalChangeset(ExternalChangesetNode {
                d,
                repo: Arc::new(repo),
            }),
            None => Self::HiddenExternalChangeset(HiddenExternalChangesetNode { d }),
        }
    }
}

/// The raw merge request in the synced metadata (`{"mergeRequest": {...}}` from the GitLab
/// source; a bare MR object is accepted too).
fn merge_request(metadata: &Value) -> &Value {
    match metadata.get("mergeRequest") {
        Some(mr) if mr.is_object() => mr,
        _ => metadata,
    }
}

/// A label on a changeset.
#[derive(SimpleObject)]
#[graphql(name = "ChangesetLabel")]
pub struct ChangesetLabel {
    pub text: String,
    /// The color as hex without `#`; empty when the code host did not report one.
    pub color: String,
    pub description: Option<String>,
}

#[Object(name = "HiddenExternalChangeset")]
impl HiddenExternalChangesetNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        self.d.id()
    }

    /// The bulk changes the changeset belongs to.
    async fn batch_changes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        state: Option<BatchChangeStateGql>,
        states: Option<Vec<BatchChangeStateGql>>,
        viewer_can_administer: Option<bool>,
    ) -> Result<BatchChangeConnection> {
        let args = BatchChangesArgs {
            first,
            after,
            state,
            states,
            viewer_can_administer,
        };
        self.d.batch_changes(ctx, args).await
    }

    async fn state(&self) -> ChangesetStateGql {
        self.d.c.state.into()
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.d.c.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.d.c.updated_at
    }

    async fn next_sync_at(&self) -> Option<DateTime<Utc>> {
        self.d.c.next_sync_at
    }
}

impl HiddenExternalChangesetNode {
    pub(super) fn new(c: Changeset) -> Self {
        Self { d: Data { c } }
    }
}

impl ExternalChangesetNode {
    /// `c` in `repo`, which the viewer may read.
    pub(super) fn new(c: Changeset, repo: Repo) -> Self {
        Self {
            d: Data { c },
            repo: Arc::new(repo),
        }
    }

    fn published(&self) -> bool {
        self.d.c.publication_state == PublicationState::Published
    }

    async fn spec(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<sourcegull_batches::store::ChangesetSpec>> {
        let Some(id) = self.d.c.current_spec_id else {
            return Ok(None);
        };
        ChangesetSpecStore::new(&services(ctx)?.db)
            .get(id)
            .await
            .map_err(db_error)
    }

    /// The branch spec's field when unpublished (titles and bodies of published changesets
    /// come from the code host).
    async fn spec_field(
        &self,
        ctx: &Context<'_>,
        field: fn(&sourcegull_batches::store::ChangesetSpec) -> Option<String>,
    ) -> Result<Option<String>> {
        Ok(self
            .spec(ctx)
            .await?
            .filter(|s| s.kind == ChangesetSpecType::Branch)
            .and_then(|s| field(&s)))
    }
}

#[Object(name = "ExternalChangeset")]
impl ExternalChangesetNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        self.d.id()
    }

    /// The merge request's id on the code host (its `iid`), once published.
    #[graphql(name = "externalID")]
    async fn external_id(&self) -> Option<&str> {
        self.d.c.external_id.as_deref()
    }

    /// The repository the changeset targets.
    async fn repository(&self) -> RepositoryRef {
        RepositoryRef((*self.repo).clone())
    }

    /// The bulk changes the changeset belongs to.
    async fn batch_changes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        state: Option<BatchChangeStateGql>,
        states: Option<Vec<BatchChangeStateGql>>,
        viewer_can_administer: Option<bool>,
    ) -> Result<BatchChangeConnection> {
        let args = BatchChangesArgs {
            first,
            after,
            state,
            states,
            viewer_can_administer,
        };
        self.d.batch_changes(ctx, args).await
    }

    /// The bulk change that created the changeset; null when it was imported.
    async fn owned_by_batch_change(&self) -> Option<ID> {
        self.d.c.owned_by_batch_change_id.map(batch_change_id)
    }

    /// The timeline (reviews, pipelines, state changes), oldest first.
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<ChangesetEventConnection> {
        let limit = usize::try_from(check_first(first)?).unwrap_or(0);
        let offset = usize::try_from(parse_cursor(after.as_deref())?.unwrap_or(0)).unwrap_or(0);
        let events = ChangesetStore::new(&services(ctx)?.db)
            .events(&[self.d.c.id], &[])
            .await
            .map_err(db_error)?;
        let total = events.len();
        let end = offset.saturating_add(limit).min(total);
        let has_next_page = end < total;
        Ok(ChangesetEventConnection {
            nodes: events
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|e| ChangesetEventNode {
                    id: e.id,
                    created_at: e.created_at,
                    changeset_id: self.d.c.id,
                })
                .collect(),
            total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
            page_info: PageInfo {
                has_next_page,
                end_cursor: has_next_page.then(|| end.to_string()),
            },
        })
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.d.c.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.d.c.updated_at
    }

    async fn next_sync_at(&self) -> Option<DateTime<Utc>> {
        self.d.c.next_sync_at
    }

    /// When a changeset waiting for a rollout window (`SCHEDULED`) is expected to be
    /// published: its place in the schedule queue played through the configured windows.
    /// Null when it is not scheduled or no window would publish it within about a year.
    async fn schedule_estimate_at(&self, ctx: &Context<'_>) -> Result<Option<DateTime<Utc>>> {
        if self.d.c.reconciler_state != ReconcilerState::Scheduled {
            return Ok(None);
        }
        let s = services(ctx)?;
        let windows = RolloutWindows::lenient(&s.config.load().value.batch_changes_rollout_windows);
        if windows.is_empty() {
            return Ok(None);
        }
        let Some(place) = ChangesetStore::new(&s.db)
            .place_in_schedule(self.d.c.id)
            .await
            .map_err(db_error)?
        else {
            return Ok(None);
        };
        Ok(windows.estimate(Utc::now(), u64::try_from(place).unwrap_or(0)))
    }

    /// The title: from the code host once published, else from the spec.
    async fn title(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.published() {
            return Ok(self.d.c.title.clone());
        }
        self.spec_field(ctx, |s| s.title.clone()).await
    }

    /// The body: from the code host once published, else from the spec.
    async fn body(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.published() {
            return Ok(self.d.c.body.clone());
        }
        self.spec_field(ctx, |s| s.body.clone()).await
    }

    /// The merge request's author on the code host (GitLab: the name only).
    async fn author(&self) -> Option<PersonRef> {
        if !self.published() {
            return None;
        }
        let author = merge_request(&self.d.c.metadata).get("author")?;
        let name = author
            .get("name")
            .or_else(|| author.get("username"))?
            .as_str()?
            .to_owned();
        Some(PersonRef(PersonData {
            display_name: name.clone(),
            name,
            email: String::new(),
            avatar_url: author
                .get("avatar_url")
                .and_then(Value::as_str)
                .map(str::to_owned),
            user: None,
        }))
    }

    async fn state(&self) -> ChangesetStateGql {
        self.d.c.state.into()
    }

    /// The labels on the code host.
    async fn labels(&self) -> Vec<ChangesetLabel> {
        if !self.published() {
            return Vec::new();
        }
        let Some(labels) = merge_request(&self.d.c.metadata)
            .get("labels")
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        labels
            .iter()
            .filter_map(|l| match l {
                Value::String(s) => Some(ChangesetLabel {
                    text: s.clone(),
                    color: String::new(),
                    description: None,
                }),
                Value::Object(o) => Some(ChangesetLabel {
                    text: o.get("name")?.as_str()?.to_owned(),
                    color: o
                        .get("color")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .to_owned(),
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
                _ => None,
            })
            .collect()
    }

    /// The merge request's page; null until published and once deleted.
    #[graphql(name = "externalURL")]
    async fn external_url(&self) -> Option<ExternalLink> {
        if !self.published() || self.d.c.state == sourcegull_batches::store::ChangesetState::Deleted
        {
            return None;
        }
        let mr = merge_request(&self.d.c.metadata);
        let url = mr
            .get("web_url")
            .or_else(|| mr.get("url"))
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())?;
        Some(ExternalLink {
            url: url.to_owned(),
            service_kind: (self.d.c.external_service_type == "gitlab")
                .then_some(ExternalServiceKindGql::GitLab),
            service_type: Some(self.d.c.external_service_type.clone()),
        })
    }

    /// The namespace of the fork the changeset was opened from.
    async fn fork_namespace(&self) -> Option<&str> {
        self.d.c.external_fork_namespace.as_deref()
    }

    /// The name of the fork the changeset was opened from.
    async fn fork_name(&self) -> Option<&str> {
        self.d.c.external_fork_name.as_deref()
    }

    /// The review state, once published.
    async fn review_state(&self) -> Option<ChangesetReviewStateGql> {
        if !self.published() {
            return None;
        }
        Some(
            self.d
                .c
                .external_review_state
                .unwrap_or(sourcegull_batches::store::ReviewState::Pending)
                .into(),
        )
    }

    /// Lines added and deleted.
    async fn diff_stat(&self) -> Option<DiffStat> {
        let (added, deleted) = (self.d.c.diff_stat_added?, self.d.c.diff_stat_deleted?);
        Some(DiffStat {
            added,
            deleted,
            changed: 0,
        })
    }

    /// The CI state, once published; null without checks.
    async fn check_state(&self) -> Option<ChangesetCheckStateGql> {
        if !self.published() {
            return None;
        }
        ChangesetCheckStateGql::from_db(self.d.c.external_check_state?)
    }

    /// The last error publishing or updating the changeset.
    async fn error(&self) -> Option<&str> {
        self.d.c.failure_message.as_deref()
    }

    /// The error of the last sync with the code host.
    async fn syncer_error(&self) -> Option<&str> {
        self.d.c.syncer_error.as_deref()
    }

    /// The spec the changeset was last applied from; null for imported changesets.
    async fn current_spec(&self, ctx: &Context<'_>) -> Result<Option<VisibleChangesetSpecNode>> {
        Ok(self
            .spec(ctx)
            .await?
            .map(|s| VisibleChangesetSpecNode::new(s, (*self.repo).clone())))
    }
}

/// A page of changesets.
#[derive(SimpleObject)]
pub struct ChangesetConnection {
    pub nodes: Vec<ChangesetNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// An event in a changeset's timeline.
pub struct ChangesetEventNode {
    id: i64,
    changeset_id: i64,
    created_at: DateTime<Utc>,
}

#[Object(name = "ChangesetEvent")]
impl ChangesetEventNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(CHANGESET_EVENT_KIND, self.id))
    }

    /// The changeset (in a repository the viewer may read; events are listed from it).
    async fn changeset(&self, ctx: &Context<'_>) -> Result<ExternalChangesetNode> {
        let c = ChangesetStore::new(&services(ctx)?.db)
            .get(self.changeset_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| super::not_found("Changeset"))?;
        let repo = readable_repo(ctx, c.repo_id)
            .await?
            .ok_or_else(|| super::not_found("Changeset"))?;
        Ok(ExternalChangesetNode {
            d: Data { c },
            repo: Arc::new(repo),
        })
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

/// A page of changeset events.
#[derive(SimpleObject)]
pub struct ChangesetEventConnection {
    pub nodes: Vec<ChangesetEventNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// A page of changesets matching `opts`. When `safe`, changesets in repositories the viewer
/// may not read are included (hidden); otherwise they are left out of nodes and count.
pub(super) async fn connection(
    ctx: &Context<'_>,
    mut opts: ListChangesets,
    safe: bool,
    limit: i64,
    after: Option<i64>,
) -> Result<ChangesetConnection> {
    let viewer_access = repo_access(ctx).await?;
    // Hidden changesets are shown as such when the filter cannot reveal anything about them.
    let access = if safe { RepoAccess::All } else { viewer_access };
    let store = ChangesetStore::new(&services(ctx)?.db);
    let total = store.count(&opts, &access).await.map_err(db_error)?;
    if limit == 0 {
        return Ok(ChangesetConnection {
            nodes: Vec::new(),
            total_count: to_int(total),
            page_info: PageInfo {
                has_next_page: total > 0,
                end_cursor: None,
            },
        });
    }
    opts.after_id = after;
    opts.limit = limit + 1;
    let mut rows = store.list(&opts, &access).await.map_err(db_error)?;
    let has_next_page = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    let repo_ids: Vec<_> = rows.iter().map(|c| c.repo_id).collect();
    let repos = readable_repos(ctx, &repo_ids).await?;
    let end_cursor = has_next_page
        .then(|| rows.last().map(|c| c.id.to_string()))
        .flatten();
    let nodes = rows
        .into_iter()
        .map(|c| {
            let repo = repos.get(&c.repo_id).cloned();
            ChangesetNode::new(c, repo)
        })
        .collect();
    Ok(ChangesetConnection {
        nodes,
        total_count: to_int(total),
        page_info: PageInfo {
            has_next_page,
            end_cursor,
        },
    })
}

/// A changeset by ID for `node(id:)`: hidden when its repository is not readable.
pub(super) async fn load_node(ctx: &Context<'_>, id: &str) -> Result<Option<ChangesetNode>> {
    let Some(key) = unmarshal_int_id(CHANGESET_KIND, id) else {
        return Ok(None);
    };
    let Some(c) = ChangesetStore::new(&services(ctx)?.db)
        .get(key)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    let repo = readable_repo(ctx, c.repo_id).await?;
    Ok(Some(ChangesetNode::new(c, repo)))
}

/// One CSV field: quoted when needed, and with a leading `'` when a spreadsheet would read it
/// as a formula.
fn csv_field(v: &str) -> String {
    let v = if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{v}")
    } else {
        v.to_owned()
    };
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

/// The CSV export of the changesets matching `opts` in repositories the viewer may read.
pub(super) async fn csv(ctx: &Context<'_>, opts: &ListChangesets) -> Result<String> {
    let access = repo_access(ctx).await?;
    let db = &services(ctx)?.db;
    let rows = ChangesetStore::new(db)
        .list(opts, &access)
        .await
        .map_err(db_error)?;
    let repo_ids: Vec<_> = rows.iter().map(|c| c.repo_id).collect();
    let repos = readable_repos(ctx, &repo_ids).await?;
    let mut out = String::from(
        "id,repository,title,state,reviewState,checkState,externalID,externalURL,added,deleted,createdAt,updatedAt\r\n",
    );
    for c in rows {
        let Some(repo) = repos.get(&c.repo_id) else {
            continue;
        };
        let published = c.publication_state == PublicationState::Published;
        let title = if published {
            c.title.clone()
        } else {
            match c.current_spec_id {
                Some(id) => ChangesetSpecStore::new(db)
                    .get(id)
                    .await
                    .map_err(db_error)?
                    .and_then(|s| s.title),
                None => None,
            }
        };
        let url = if published {
            let mr = merge_request(&c.metadata);
            mr.get("web_url")
                .or_else(|| mr.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        } else {
            String::new()
        };
        let review = if published {
            c.external_review_state
                .unwrap_or(sourcegull_batches::store::ReviewState::Pending)
                .as_str()
        } else {
            ""
        };
        let check = match c.external_check_state {
            Some(s) if published && s != sourcegull_batches::store::CheckState::Unknown => {
                s.as_str()
            }
            _ => "",
        };
        let fields = [
            changeset_id(c.id).0,
            repo.name.clone(),
            title.unwrap_or_default(),
            c.state.as_str().to_owned(),
            review.to_owned(),
            check.to_owned(),
            c.external_id.clone().unwrap_or_default(),
            url,
            c.diff_stat_added.map(|n| n.to_string()).unwrap_or_default(),
            c.diff_stat_deleted
                .map(|n| n.to_string())
                .unwrap_or_default(),
            c.created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            c.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ];
        let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::csv_field;

    #[test]
    fn csv_fields() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(csv_field("-1,2"), "\"'-1,2\"");
        assert_eq!(csv_field(""), "");
    }
}
