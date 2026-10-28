//! Merge requests for bulk changes, implementing [`ChangesetSource`].
//!
//! - **Create**: `POST /projects/:source/merge_requests` (the fork when the head branch is in
//!   one, with `target_project_id`). A 409 (an open MR already exists for the branch) finds it
//!   with `GET /projects/:target/merge_requests?source_branch=…&state=opened` and updates it.
//! - **Drafts** are a title prefix: `Draft: ` on GitLab 14+ (or when the version is unknown),
//!   `WIP: ` before (the version comes from the client's cached `GET /version`).
//!   Undraft strips every known draft marker. Update keeps a draft a draft.
//! - **Close/reopen** are `PUT …/merge_requests/:iid` with `state_event`; **merge** is
//!   `PUT …/merge` (405/406/409/422 → [`CodeHostError::NotMergeable`]); comments are
//!   `POST …/notes`.
//! - **Load (sync)** fetches the MR, its notes, `resource_state_events`, MR pipelines and the
//!   approvals API concurrently. Review state comes from the approvals API where it answers
//!   (with `approvals_left`, Premium) and otherwise from approval system notes (free tier);
//!   "requested changes" notes win over approvals. The check state is the head pipeline's (or
//!   the newest MR pipeline's). Notes, state events and pipelines become the timeline
//!   ([`ChangesetEvent`]s). Before GitLab has `resource_state_events` (13.2), close, reopen
//!   and merge come from system notes.
//! - **State**: `opened` (draft or not), `closed`/`locked`, `merged`; an open or draft MR in
//!   an archived project is [`ChangesetState::ReadOnly`]. A 403 on a write to an archived
//!   project is [`CodeHostError::Archived`].
//! - **Forks** are named `{upstream namespace with / as -}-{project path}` in the target
//!   namespace (the credential user's by default), found with `GET /projects/:path` or
//!   created with `POST /projects/:id/fork`; creation waits until `import_status` is
//!   `finished`.
//! - **Commit via API** ([`ChangesetSource::create_commit`]): `POST
//!   /projects/:id/repository/commits` with one action per file (`create`, `update`,
//!   `delete`, `move`, plus `chmod` for execute flags; content as text, or base64 when it is
//!   not UTF-8 or has a NUL byte), `start_sha` (and `start_project` when committing to a
//!   fork), `force` to replace an existing branch, and the spec's author. GitLab creates the
//!   commit, and signs it where web commit signing is set up (GitLab 15.x+: on GitLab.com
//!   always, on self-managed instances when Gitaly has a signing key).
//! - **Credentials**: a [`Credential::Basic`] with username `oauth2` is an OAuth token
//!   (`Authorization: Bearer`), any other username a personal access token. A credential
//!   needs the `api` scope (checked with `GET /personal_access_tokens/self` where GitLab can
//!   tell).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use sourcegull_codehost::{
    Changeset, ChangesetEvent, ChangesetEventKind, ChangesetSource, ChangesetState, CheckState,
    CodeHostError, CommitAction, CommitFile, CreatedChangeset, Credential, MergeOptions,
    NewChangeset, NewCommit, PushStrategy, RepoMeta, RepoSource, Result, ReviewState,
};

use crate::client::{Auth, GitLabClient, PER_PAGE};
use crate::source::GitLabSource;
use crate::types::Project;

/// Tuning of a [`GitLabChangesetSource`].
#[derive(Debug, Clone)]
pub struct ChangesetOptions {
    /// How often to check whether a new fork has finished importing.
    pub fork_poll_interval: Duration,
    /// How long to wait for a new fork before giving up (with a temporary error).
    pub fork_timeout: Duration,
    /// How often to retry creating a merge request whose just-pushed source branch GitLab does
    /// not see yet (400 `source_branch does not exist`: its branch cache is refreshed after
    /// the push, asynchronously, which takes seconds on a busy instance).
    pub branch_poll_interval: Duration,
    /// How long to retry that before giving up (with a temporary error).
    pub branch_timeout: Duration,
}

impl Default for ChangesetOptions {
    fn default() -> Self {
        Self {
            fork_poll_interval: Duration::from_secs(2),
            fork_timeout: Duration::from_secs(300),
            branch_poll_interval: Duration::from_secs(1),
            branch_timeout: Duration::from_secs(20),
        }
    }
}

/// Merge requests on one GitLab connection, acting with one credential.
pub struct GitLabChangesetSource {
    source: Arc<GitLabSource>,
    client: Arc<GitLabClient>,
    opts: ChangesetOptions,
}

impl std::fmt::Debug for GitLabChangesetSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabChangesetSource")
            .field("service_id", &self.source.service_id())
            .field("auth", self.client.auth())
            .finish_non_exhaustive()
    }
}

/// `MergeRequest` fields Sourcegull reads; the raw object is kept in the metadata.
#[derive(Debug, Clone, Deserialize)]
struct MergeRequest {
    iid: i64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String,
    source_branch: String,
    target_branch: String,
    source_project_id: i64,
    target_project_id: i64,
    #[serde(default)]
    labels: Vec<String>,
    /// GitLab 14+.
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    work_in_progress: Option<bool>,
    web_url: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    head_pipeline: Option<Pipeline>,
}

#[derive(Debug, Clone, Deserialize)]
struct Pipeline {
    id: i64,
    #[serde(default)]
    sha: Option<String>,
    status: String,
    #[serde(default)]
    web_url: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserRef {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    username: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Note {
    id: i64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    author: Option<UserRef>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct StateEvent {
    id: i64,
    #[serde(default)]
    user: Option<UserRef>,
    created_at: DateTime<Utc>,
    state: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Approvals {
    #[serde(default)]
    approvals_left: Option<i64>,
    #[serde(default)]
    approved_by: Vec<ApprovedBy>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApprovedBy {
    user: UserRef,
}

/// The fields of a fork being imported.
#[derive(Debug, Clone, Deserialize)]
struct ImportStatus {
    #[serde(default)]
    import_status: Option<String>,
    #[serde(default)]
    import_error: Option<String>,
}

static DRAFT_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:\[draft\]|\(draft\)|draft\s*:|draft\s+-|\[wip\]|wip\s*:)\s*")
        .expect("draft marker regex")
});

/// `title` without leading draft markers (`Draft:`, `[Draft]`, `(Draft)`, `WIP:`, `[WIP]`).
pub fn strip_draft_prefix(title: &str) -> &str {
    let mut t = title;
    while let Some(m) = DRAFT_MARKER.find(t) {
        if m.end() == 0 {
            break;
        }
        t = &t[m.end()..];
    }
    t
}

pub(crate) fn has_draft_prefix(title: &str) -> bool {
    strip_draft_prefix(title).len() != title.len()
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The check state of a pipeline status.
pub(crate) fn pipeline_state(status: &str) -> CheckState {
    match status {
        "success" => CheckState::Passed,
        "failed" | "canceled" => CheckState::Failed,
        "created"
        | "waiting_for_resource"
        | "preparing"
        | "pending"
        | "running"
        | "scheduled"
        | "waiting_for_callback" => CheckState::Pending,
        _ => CheckState::Unknown,
    }
}

/// The kind of event a note is, by its body (system notes) or as a comment.
fn note_kind(note: &Note, with_state_notes: bool) -> Option<ChangesetEventKind> {
    use ChangesetEventKind as K;
    if !note.system {
        return Some(K::Commented);
    }
    let body = note.body.trim();
    let lower = body.to_ascii_lowercase();
    Some(if lower.starts_with("approved this merge request") {
        K::Approved
    } else if lower.starts_with("unapproved this merge request") {
        K::Unapproved
    } else if lower.starts_with("requested changes") {
        K::ChangesRequested
    } else if lower.starts_with("requested review from") {
        K::ReviewRequested
    } else if lower.starts_with("marked this merge request as **draft**")
        || lower.starts_with("marked as a **work in progress**")
        || lower.starts_with("marked this merge request as a **work in progress**")
    {
        K::MarkedDraft
    } else if lower.starts_with("marked this merge request as **ready**")
        || lower.starts_with("unmarked as a **work in progress**")
        || lower.starts_with("unmarked this merge request as a **work in progress**")
    {
        K::MarkedReady
    } else if with_state_notes && lower == "closed" {
        K::Closed
    } else if with_state_notes && lower == "reopened" {
        K::Reopened
    } else if with_state_notes && lower.starts_with("merged") {
        K::Merged
    } else {
        return None;
    })
}

/// The review state from approval notes: each user's last approval-related note counts.
fn review_from_notes(notes: &[Note]) -> ReviewState {
    let mut last: HashMap<i64, ChangesetEventKind> = HashMap::new();
    for n in notes {
        let kind = note_kind(n, false);
        if let Some(
            k @ (ChangesetEventKind::Approved
            | ChangesetEventKind::Unapproved
            | ChangesetEventKind::ChangesRequested),
        ) = kind
        {
            let user = n.author.as_ref().map_or(0, |a| a.id);
            last.insert(user, k);
        }
    }
    if last
        .values()
        .any(|k| *k == ChangesetEventKind::ChangesRequested)
    {
        ReviewState::ChangesRequested
    } else if last.values().any(|k| *k == ChangesetEventKind::Approved) {
        ReviewState::Approved
    } else {
        ReviewState::Pending
    }
}

impl GitLabChangesetSource {
    /// A source acting with the connection's own token.
    pub fn new(source: Arc<GitLabSource>) -> Self {
        Self {
            client: source.client().clone(),
            source,
            opts: ChangesetOptions::default(),
        }
    }

    /// Replaces the tuning.
    #[must_use]
    pub fn with_options(mut self, opts: ChangesetOptions) -> Self {
        self.opts = opts;
        self
    }

    /// A source acting with `cred` (see the module docs for how it maps to a token).
    /// [`Credential::None`] keeps the current token.
    pub fn for_credential(&self, cred: &Credential) -> Self {
        let client = match cred {
            Credential::None => self.client.clone(),
            Credential::Basic { username, password } => {
                let auth = if username == "oauth2" {
                    Auth::Bearer(password.clone())
                } else {
                    Auth::PrivateToken(password.clone())
                };
                Arc::new(self.source.client().with_auth(auth))
            }
        };
        Self {
            source: self.source.clone(),
            client,
            opts: self.opts.clone(),
        }
    }

    /// The client requests go through.
    pub fn client(&self) -> &Arc<GitLabClient> {
        &self.client
    }

    fn mr_path(repo: &RepoMeta, iid: i64) -> String {
        format!("projects/{}/merge_requests/{iid}", enc(&repo.external.id))
    }

    /// The draft prefix of the instance's version.
    async fn draft_prefix(&self) -> &'static str {
        match self.client.version().await {
            Some(v) if !v.uses_draft_prefix() => "WIP: ",
            _ => "Draft: ",
        }
    }

    async fn title(&self, title: &str, draft: bool) -> String {
        let plain = strip_draft_prefix(title);
        if draft {
            format!("{}{plain}", self.draft_prefix().await)
        } else {
            plain.to_owned()
        }
    }

    async fn get_mr(&self, repo: &RepoMeta, iid: i64) -> Result<(MergeRequest, Value)> {
        let raw: Value = self.client.get(&Self::mr_path(repo, iid)).await?;
        Ok((parse_mr(&raw)?, raw))
    }

    /// Maps a 403 on an archived project to [`CodeHostError::Archived`].
    async fn map_write_error(&self, repo: &RepoMeta, err: CodeHostError) -> CodeHostError {
        let CodeHostError::Forbidden(message) = &err else {
            return err;
        };
        let archived = repo.archived
            || message.to_ascii_lowercase().contains("archived")
            || self
                .client
                .project(&repo.external.id)
                .await
                .is_ok_and(|p| p.archived);
        if archived {
            CodeHostError::Archived(repo.name.clone())
        } else {
            err
        }
    }

    /// `PUT` to a merge request, mapping errors.
    async fn put_mr(&self, repo: &RepoMeta, iid: i64, body: &Value) -> Result<Changeset> {
        match self
            .client
            .put::<Value>(&Self::mr_path(repo, iid), body)
            .await
        {
            Ok(raw) => self.changeset(repo, &raw),
            Err(e) => Err(self.map_write_error(repo, e).await),
        }
    }

    async fn apply(
        &self,
        repo: &RepoMeta,
        iid: i64,
        new: &NewChangeset,
        draft: bool,
    ) -> Result<Changeset> {
        let mut body = json!({
            "title": self.title(&new.title, draft).await,
            "description": new.body,
            "target_branch": new.base_ref,
            "remove_source_branch": new.remove_source_branch,
        });
        if !new.labels.is_empty() {
            body["add_labels"] = new.labels.join(",").into();
        }
        self.put_mr(repo, iid, &body).await
    }

    /// The open MR from `new`'s head branch (and head project) into `repo`.
    async fn find_open(
        &self,
        repo: &RepoMeta,
        new: &NewChangeset,
        source_pid: &str,
    ) -> Result<i64> {
        let url = self.client.api_url(&format!(
            "projects/{}/merge_requests?source_branch={}&state=opened&per_page={PER_PAGE}",
            enc(&repo.external.id),
            enc(&new.head_ref),
        ))?;
        let mrs: Vec<MergeRequest> = self.client.paginate(url).try_collect().await?;
        mrs.into_iter()
            .find(|mr| mr.source_project_id.to_string() == source_pid)
            .map(|mr| mr.iid)
            .ok_or_else(|| {
                CodeHostError::NotFound(format!(
                    "open merge request from {} in {}",
                    new.head_ref, repo.name
                ))
            })
    }

    /// A [`Changeset`] from a raw MR, without review state or timeline.
    fn changeset(&self, repo: &RepoMeta, raw: &Value) -> Result<Changeset> {
        let mr = parse_mr(raw)?;
        Ok(self.to_changeset(repo, &mr, json!({ "mergeRequest": raw })))
    }

    fn to_changeset(&self, repo: &RepoMeta, mr: &MergeRequest, metadata: Value) -> Changeset {
        let draft = mr
            .draft
            .or(mr.work_in_progress)
            .unwrap_or_else(|| has_draft_prefix(&mr.title));
        let mut state = match mr.state.as_str() {
            "merged" => ChangesetState::Merged,
            "closed" | "locked" => ChangesetState::Closed,
            _ if draft => ChangesetState::Draft,
            _ => ChangesetState::Open,
        };
        if repo.archived && matches!(state, ChangesetState::Open | ChangesetState::Draft) {
            state = ChangesetState::ReadOnly;
        }
        Changeset {
            number: mr.iid,
            title: mr.title.clone(),
            body: mr.description.clone().unwrap_or_default(),
            state,
            review_state: None,
            check_state: mr.head_pipeline.as_ref().map(|p| pipeline_state(&p.status)),
            head_ref: mr.source_branch.clone(),
            base_ref: mr.target_branch.clone(),
            head_repo_id: (mr.source_project_id != mr.target_project_id)
                .then(|| mr.source_project_id.to_string()),
            labels: mr.labels.clone(),
            url: mr.web_url.clone(),
            created_at: mr.created_at,
            updated_at: mr.updated_at,
            events: Vec::new(),
            metadata,
        }
    }

    /// A list endpoint of a merge request; `None` when GitLab does not have it (404) or the
    /// feature is unavailable to the token (403, e.g. CI disabled).
    async fn optional_list<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
    ) -> Result<Option<Vec<T>>> {
        let url = self.client.api_url(path)?;
        match self.client.paginate::<T>(url).try_collect().await {
            Ok(items) => Ok(Some(items)),
            Err(CodeHostError::NotFound(_) | CodeHostError::Forbidden(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Waits until a new fork has finished importing.
    async fn wait_for_fork(&self, mut project: Value) -> Result<Project> {
        let deadline = tokio::time::Instant::now() + self.opts.fork_timeout;
        loop {
            let status: ImportStatus = serde_json::from_value(project.clone())
                .map_err(|e| CodeHostError::Decode(e.to_string()))?;
            match status.import_status.as_deref() {
                None | Some("finished" | "none") => {
                    return serde_json::from_value(project)
                        .map_err(|e| CodeHostError::Decode(e.to_string()));
                }
                Some("failed") => {
                    return Err(CodeHostError::Http {
                        status: 500,
                        message: format!(
                            "fork import failed: {}",
                            status.import_error.unwrap_or_default()
                        ),
                    });
                }
                Some(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CodeHostError::Transport(
                    "timed out waiting for the fork to be ready".into(),
                ));
            }
            tokio::time::sleep(self.opts.fork_poll_interval).await;
            let id = project["id"].as_i64().unwrap_or_default();
            project = self.client.get(&format!("projects/{id}")).await?;
        }
    }
}

/// The `actions` of a `POST /projects/:id/repository/commits`.
pub fn commit_actions(files: &[CommitFile]) -> Vec<Value> {
    let mut actions = Vec::with_capacity(files.len());
    for f in files {
        let action = match f.action {
            CommitAction::Create => "create",
            CommitAction::Update => "update",
            CommitAction::Delete => "delete",
            CommitAction::Move => "move",
        };
        // An update that only flips the execute flag is a lone `chmod`.
        let mode_only = f.action == CommitAction::Update && f.content.is_none();
        if !mode_only {
            let mut a = json!({ "action": action, "file_path": f.path });
            if let Some(prev) = &f.previous_path {
                a["previous_path"] = prev.clone().into();
            }
            if let Some(content) = &f.content {
                match std::str::from_utf8(content) {
                    Ok(text) if !text.contains('\0') => a["content"] = text.into(),
                    _ => {
                        a["content"] = base64::engine::general_purpose::STANDARD
                            .encode(content)
                            .into();
                        a["encoding"] = "base64".into();
                    }
                }
            }
            actions.push(a);
        }
        if f.action != CommitAction::Delete
            && let Some(exec) = f.executable
            && (exec || f.action != CommitAction::Create)
        {
            actions.push(json!({
                "action": "chmod",
                "file_path": f.path,
                "execute_filemode": exec,
            }));
        }
    }
    actions
}

/// Whether a 400 from `POST /merge_requests` says the source branch does not exist
/// (`{"message":{"source_branch":["does not exist"]}}`).
fn missing_source_branch(message: &str) -> bool {
    serde_json::from_str::<Value>(message).is_ok_and(|v| {
        v["message"]["source_branch"]
            .as_array()
            .is_some_and(|a| a.iter().any(|m| m == "does not exist"))
    })
}

fn parse_mr(raw: &Value) -> Result<MergeRequest> {
    serde_json::from_value(raw.clone())
        .map_err(|e| CodeHostError::Decode(format!("merge request: {e}")))
}

fn actor(user: Option<&UserRef>) -> Value {
    user.map_or(Value::Null, |u| u.username.clone().into())
}

#[async_trait]
impl ChangesetSource for GitLabChangesetSource {
    fn with_credential(&self, cred: &Credential) -> Result<Box<dyn ChangesetSource>> {
        Ok(Box::new(self.for_credential(cred)))
    }

    async fn create(&self, repo: &RepoMeta, new: &NewChangeset) -> Result<CreatedChangeset> {
        let source_pid = new.head_repo_id.as_deref().unwrap_or(&repo.external.id);
        let mut body = json!({
            "source_branch": new.head_ref,
            "target_branch": new.base_ref,
            "title": self.title(&new.title, new.draft).await,
            "description": new.body,
            "remove_source_branch": new.remove_source_branch,
        });
        if !new.labels.is_empty() {
            body["labels"] = new.labels.join(",").into();
        }
        if source_pid != repo.external.id {
            let target: i64 =
                repo.external.id.parse().map_err(|_| {
                    CodeHostError::Config(format!("project id {:?}", repo.external.id))
                })?;
            body["target_project_id"] = target.into();
        }
        let path = format!("projects/{}/merge_requests", enc(source_pid));
        let deadline = tokio::time::Instant::now() + self.opts.branch_timeout;
        let created = loop {
            match self.client.post::<Value>(&path, &body).await {
                Err(CodeHostError::Http {
                    status: 400,
                    message,
                }) if missing_source_branch(&message) => {
                    if tokio::time::Instant::now() >= deadline {
                        break Err(CodeHostError::Transport(format!(
                            "GitLab does not see the pushed branch {:?} yet: {message}",
                            new.head_ref
                        )));
                    }
                    tokio::time::sleep(self.opts.branch_poll_interval).await;
                }
                r => break r,
            }
        };
        match created {
            Ok(raw) => Ok(CreatedChangeset {
                changeset: self.changeset(repo, &raw)?,
                already_existed: false,
            }),
            Err(CodeHostError::Http { status: 409, .. }) => {
                let iid = self.find_open(repo, new, source_pid).await?;
                Ok(CreatedChangeset {
                    changeset: self.apply(repo, iid, new, new.draft).await?,
                    already_existed: true,
                })
            }
            Err(e) => Err(self.map_write_error(repo, e).await),
        }
    }

    async fn undraft(&self, repo: &RepoMeta, number: i64) -> Result<Changeset> {
        let (mr, raw) = self.get_mr(repo, number).await?;
        let plain = strip_draft_prefix(&mr.title);
        if plain.len() == mr.title.len() && mr.draft != Some(true) {
            return self.changeset(repo, &raw);
        }
        self.put_mr(repo, number, &json!({ "title": plain })).await
    }

    async fn update(&self, repo: &RepoMeta, number: i64, new: &NewChangeset) -> Result<Changeset> {
        let (mr, _) = self.get_mr(repo, number).await?;
        let draft = new.draft
            || mr
                .draft
                .or(mr.work_in_progress)
                .unwrap_or_else(|| has_draft_prefix(&mr.title));
        self.apply(repo, number, new, draft).await
    }

    async fn close(&self, repo: &RepoMeta, number: i64) -> Result<Changeset> {
        self.put_mr(repo, number, &json!({ "state_event": "close" }))
            .await
    }

    async fn reopen(&self, repo: &RepoMeta, number: i64) -> Result<Changeset> {
        self.put_mr(repo, number, &json!({ "state_event": "reopen" }))
            .await
    }

    async fn merge(&self, repo: &RepoMeta, number: i64, opts: MergeOptions) -> Result<Changeset> {
        let mut body = json!({ "squash": opts.squash });
        if opts.remove_source_branch {
            body["should_remove_source_branch"] = true.into();
        }
        let path = format!("{}/merge", Self::mr_path(repo, number));
        match self.client.put::<Value>(&path, &body).await {
            Ok(raw) => self.changeset(repo, &raw),
            Err(CodeHostError::Http {
                status: 405 | 406 | 409 | 422,
                message,
            }) => Err(CodeHostError::NotMergeable(message)),
            Err(e) => Err(self.map_write_error(repo, e).await),
        }
    }

    async fn create_comment(&self, repo: &RepoMeta, number: i64, body: &str) -> Result<()> {
        let path = format!("{}/notes", Self::mr_path(repo, number));
        match self
            .client
            .post::<Value>(&path, &json!({ "body": body }))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(self.map_write_error(repo, e).await),
        }
    }

    async fn load(&self, repo: &RepoMeta, number: i64) -> Result<Changeset> {
        let mr_path = Self::mr_path(repo, number);
        let notes_path =
            format!("{mr_path}/notes?per_page={PER_PAGE}&sort=asc&order_by=created_at");
        let states_path = format!("{mr_path}/resource_state_events?per_page={PER_PAGE}");
        let pipelines_path = format!("{mr_path}/pipelines?per_page={PER_PAGE}");
        let approvals_path = format!("{mr_path}/approvals");
        let (mr, notes, states, pipelines, approvals) = tokio::try_join!(
            self.get_mr(repo, number),
            async {
                let url = self.client.api_url(&notes_path)?;
                self.client
                    .paginate::<Note>(url)
                    .try_collect::<Vec<_>>()
                    .await
            },
            self.optional_list::<StateEvent>(&states_path),
            self.optional_list::<Value>(&pipelines_path),
            async {
                match self.client.get::<Value>(&approvals_path).await {
                    Ok(v) => Ok(Some(v)),
                    Err(CodeHostError::NotFound(_) | CodeHostError::Forbidden(_)) => Ok(None),
                    Err(e) => Err(e),
                }
            },
        )?;
        let (mr, raw_mr) = mr;
        let pipelines = pipelines.unwrap_or_default();

        // Timeline.
        let mut events = Vec::new();
        let state_notes = states.is_none();
        for n in &notes {
            if let Some(kind) = note_kind(n, state_notes) {
                events.push(ChangesetEvent {
                    kind,
                    key: format!("note:{}", n.id),
                    timestamp: n.created_at,
                    metadata: json!({ "noteId": n.id, "author": actor(n.author.as_ref()) }),
                });
            }
        }
        for s in states.iter().flatten() {
            let kind = match s.state.as_str() {
                "closed" => ChangesetEventKind::Closed,
                "reopened" => ChangesetEventKind::Reopened,
                "merged" => ChangesetEventKind::Merged,
                _ => continue,
            };
            events.push(ChangesetEvent {
                kind,
                key: format!("state:{}", s.id),
                timestamp: s.created_at,
                metadata: json!({ "stateEventId": s.id, "author": actor(s.user.as_ref()) }),
            });
        }
        let mut parsed: Vec<Pipeline> = pipelines
            .iter()
            .filter_map(|p| serde_json::from_value(p.clone()).ok())
            .collect();
        if let Some(head) = &mr.head_pipeline
            && !parsed.iter().any(|p| p.id == head.id)
        {
            parsed.push(head.clone());
        }
        for p in &parsed {
            let Some(ts) = p.updated_at.or(p.created_at) else {
                continue;
            };
            events.push(ChangesetEvent {
                kind: ChangesetEventKind::Check,
                key: format!("pipeline:{}", p.id),
                timestamp: ts,
                metadata: json!({
                    "pipelineId": p.id,
                    "status": p.status,
                    "state": pipeline_state(&p.status).as_str(),
                    "sha": p.sha,
                    "webUrl": p.web_url,
                }),
            });
        }
        events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.key.cmp(&b.key)));

        // Check state: the head pipeline, else the newest pipeline (for the head commit when
        // there is one).
        let latest = mr.head_pipeline.clone().or_else(|| {
            let head = parsed
                .iter()
                .filter(|p| mr.sha.is_some() && p.sha == mr.sha)
                .max_by_key(|p| p.id);
            head.or_else(|| parsed.iter().max_by_key(|p| p.id)).cloned()
        });
        let check_state = latest.as_ref().map(|p| pipeline_state(&p.status));

        // Review state.
        let from_notes = review_from_notes(&notes);
        let parsed_approvals = approvals
            .as_ref()
            .and_then(|a| serde_json::from_value::<Approvals>(a.clone()).ok());
        let review_state = match (&parsed_approvals, from_notes) {
            (_, ReviewState::ChangesRequested) => ReviewState::ChangesRequested,
            (Some(a), _) if a.approvals_left.is_some() => {
                if !a.approved_by.is_empty() && a.approvals_left.unwrap_or(0) <= 0 {
                    ReviewState::Approved
                } else {
                    ReviewState::Pending
                }
            }
            (Some(a), _) if !a.approved_by.is_empty() => ReviewState::Approved,
            (_, s) => s,
        };

        let mut cs = self.to_changeset(
            repo,
            &mr,
            json!({
                "mergeRequest": raw_mr,
                "approvals": approvals,
                "approvedBy": parsed_approvals
                    .as_ref()
                    .map(|a| a.approved_by.iter().map(|b| b.user.username.clone()).collect::<Vec<_>>()),
                "headPipeline": latest.as_ref().and_then(|l| {
                    pipelines.iter().find(|p| p["id"].as_i64() == Some(l.id)).cloned()
                }).or_else(|| raw_mr.get("head_pipeline").cloned()),
            }),
        );
        cs.review_state = Some(review_state);
        cs.check_state = check_state;
        cs.events = events;
        Ok(cs)
    }

    async fn get_fork(&self, repo: &RepoMeta, namespace: Option<&str>) -> Result<RepoMeta> {
        let upstream = self.client.project(&repo.external.id).await?;
        let namespace = match namespace {
            Some(ns) => ns.to_owned(),
            None => {
                let me: UserRef = self.client.get("user").await?;
                me.username
            }
        };
        let (up_ns, up_path) = upstream
            .path_with_namespace
            .rsplit_once('/')
            .unwrap_or(("", upstream.path_with_namespace.as_str()));
        let name = if up_ns.is_empty() {
            up_path.to_owned()
        } else {
            format!("{}-{up_path}", up_ns.replace('/', "-"))
        };
        let full = format!("{namespace}/{name}");
        let existing = match self.client.project(&full).await {
            Ok(p) => Some(p),
            Err(CodeHostError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let fork = match existing {
            Some(p) => p,
            None => {
                let ns_key = if self
                    .client
                    .version()
                    .await
                    .is_none_or(|v| v.supports_fork_namespace_path())
                {
                    "namespace_path"
                } else {
                    "namespace"
                };
                let body = json!({ ns_key: namespace, "name": name, "path": name });
                let path = format!("projects/{}/fork", upstream.id);
                match self.client.post::<Value>(&path, &body).await {
                    Ok(created) => self.wait_for_fork(created).await?,
                    // Created concurrently: use it.
                    Err(CodeHostError::Http {
                        status: 400 | 409, ..
                    }) => self.client.project(&full).await?,
                    Err(e) => return Err(self.map_write_error(repo, e).await),
                }
            }
        };
        if fork.forked_from_project.as_ref().map(|f| f.id) != Some(upstream.id) {
            return Err(CodeHostError::Config(format!(
                "project {full} exists but is not a fork of {}",
                upstream.path_with_namespace
            )));
        }
        Ok(self.source.repo_meta(&fork))
    }

    fn push_strategy(&self, repo: &RepoMeta, cred: &Credential) -> Result<PushStrategy> {
        let http = repo.metadata["httpUrlToRepo"]
            .as_str()
            .filter(|u| !u.is_empty());
        let mut target = repo.clone();
        if let (Credential::Basic { .. }, Some(http)) = (cred, http) {
            http.clone_into(&mut target.clone_url);
        }
        let push_url = self.source.clone_url(&target, cred)?;
        let fork_namespace = repo
            .fork
            .then(|| repo.metadata["pathWithNamespace"].as_str())
            .flatten()
            .and_then(|p| p.rsplit_once('/'))
            .map(|(ns, _)| ns.to_owned());
        Ok(PushStrategy {
            push_url,
            fork_namespace,
        })
    }

    fn classify_push_error(&self, repo: &RepoMeta, output: &str) -> Option<CodeHostError> {
        let lower = output.to_ascii_lowercase();
        let denied = lower.contains("not allowed to push")
            || lower.contains("403")
            || lower.contains("permission denied");
        if lower.contains("archived") || (repo.archived && denied) {
            Some(CodeHostError::Archived(repo.name.clone()))
        } else if denied {
            Some(CodeHostError::Forbidden(format!("push to {}", repo.name)))
        } else {
            None
        }
    }

    fn supports_commit_api(&self) -> bool {
        true
    }

    async fn create_commit(&self, repo: &RepoMeta, commit: &NewCommit) -> Result<String> {
        #[derive(Deserialize)]
        struct Created {
            id: String,
        }
        let mut body = json!({
            "branch": commit.branch,
            "commit_message": commit.message,
            "author_name": commit.author_name,
            "author_email": commit.author_email,
            "start_sha": commit.start_sha,
            "actions": commit_actions(&commit.files),
        });
        if commit.force {
            body["force"] = true.into();
        }
        if let Some(start) = commit
            .start_repo_id
            .as_deref()
            .filter(|s| *s != repo.external.id)
        {
            body["start_project"] = start
                .parse::<i64>()
                .map_or_else(|_| Value::from(start), Value::from);
        }
        let path = format!("projects/{}/repository/commits", enc(&repo.external.id));
        match self.client.post::<Created>(&path, &body).await {
            Ok(c) => Ok(c.id),
            Err(e) => Err(self.map_write_error(repo, e).await),
        }
    }

    async fn validate_credential(&self, cred: &Credential) -> Result<()> {
        if matches!(cred, Credential::None) {
            return Err(CodeHostError::Config("a token is required".into()));
        }
        let source = self.for_credential(cred);
        let _: UserRef = source.client.get("user").await?;
        if let Some(scopes) = source.client.token_scopes().await?
            && !scopes.iter().any(|s| s == "api")
        {
            return Err(CodeHostError::Forbidden(format!(
                "the token needs the api scope to open merge requests (it has: {})",
                scopes.join(", ")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_draft_markers() {
        assert_eq!(strip_draft_prefix("Draft: Fix it"), "Fix it");
        assert_eq!(strip_draft_prefix("WIP: Fix it"), "Fix it");
        assert_eq!(strip_draft_prefix("[Draft] (draft) wip: Fix"), "Fix");
        assert_eq!(strip_draft_prefix("Draft - Fix"), "Fix");
        assert_eq!(strip_draft_prefix("Drafting rules"), "Drafting rules");
        assert!(has_draft_prefix("[WIP] x") && !has_draft_prefix("x"));
    }

    #[test]
    fn pipeline_states() {
        assert_eq!(pipeline_state("success"), CheckState::Passed);
        assert_eq!(pipeline_state("canceled"), CheckState::Failed);
        assert_eq!(pipeline_state("running"), CheckState::Pending);
        assert_eq!(pipeline_state("manual"), CheckState::Unknown);
    }

    #[test]
    fn review_state_from_notes() {
        let note = |id, user, body: &str| Note {
            id,
            body: body.into(),
            system: true,
            author: Some(UserRef {
                id: user,
                username: format!("u{user}"),
            }),
            created_at: Utc::now(),
        };
        assert_eq!(review_from_notes(&[]), ReviewState::Pending);
        let notes = [
            note(1, 1, "approved this merge request"),
            note(2, 2, "approved this merge request"),
            note(3, 1, "unapproved this merge request"),
        ];
        assert_eq!(review_from_notes(&notes), ReviewState::Approved);
        let notes = [
            note(1, 1, "approved this merge request"),
            note(2, 1, "unapproved this merge request"),
        ];
        assert_eq!(review_from_notes(&notes), ReviewState::Pending);
        let notes = [
            note(1, 1, "approved this merge request"),
            note(2, 2, "requested changes"),
        ];
        assert_eq!(review_from_notes(&notes), ReviewState::ChangesRequested);
    }
}
