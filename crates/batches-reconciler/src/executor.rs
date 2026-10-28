//! The executor of a [`Plan`]: [`Reconciler::reconcile`] plans one
//! changeset and carries the operations out against the code host ([`ChangesetSource`]) and
//! the repository's repo node (`CreateCommitFromPatch` with a combined push, or `ApplyPatch`
//! and the code host's commit API).
//!
//! - **Credentials**: writes act with the user credential of the bulk change's last applier,
//!   else the site credential; without either they fail with
//!   [`ReconcileError::MissingCredentials`]. Reads (sync, import) fall back to the
//!   connection's own token.
//! - **Push**: the spec's diff is committed on its base commit and force-pushed to the head
//!   branch, on the target repository or a fork (the template's `fork`, or
//!   `batchChanges.enforceForks` for changesets not yet published; a published changeset
//!   keeps the fork it was opened from). A patch that does not apply fails at once; an
//!   archived repository or a refused push makes the changeset `READONLY` and fails it.
//! - **Commit via API** (`batchChanges.commitViaAPI`, implied by
//!   `batchChanges.rejectUnverifiedCommit`): the repo node only applies the patch
//!   (`ApplyPatch`) and the code host creates (and signs) the commit from the changed files
//!   ([`ChangesetSource::create_commit`], with `force` like the push). Changes the API cannot
//!   carry (beyond `batchChanges.commitViaAPIMaxBytes`/`MaxFiles`, symlinks, submodules, a
//!   host without the API) are pushed with git instead, or fail the changeset with
//!   [`ReconcileError::UnverifiedCommitRejected`] under `rejectUnverifiedCommit`.
//! - **Merge request**: create (as a draft, with `remove_source_branch` from
//!   `batchChanges.autoDeleteBranch` and a link back to the bulk change in the body),
//!   update, undraft, close, reopen and load. An update of an archived project only marks the
//!   changeset `READONLY`; a sync of a deleted merge request marks it `DELETED`.
//! - **State**: what the code host returned (external, review and check state, title, body,
//!   the raw merge request), diff stats, the publication state and the next sync time are
//!   written with [`ChangesetStore::store_sync`](sourcegull_batches::store::ChangesetStore), with
//!   the timeline, in one transaction. They are written even when a later operation fails, so
//!   a `READONLY` or a published merge request is never forgotten.
//! - Detach and archive requests update `batch_change_ids`.
//! - **Rollout windows**: with `batchChanges.rolloutWindows` configured, a plan that
//!   publishes moves the changeset to `scheduled` instead of running, unless a window (or a
//!   "publish now") released it ([`crate::scheduler`]).
//! - A full sync replaces the timeline events webhooks stored (keys starting with
//!   [`HOOK_EVENT_PREFIX`]) and the next sync is later when the connection has webhooks.
//!
//! Transient failures are [retryable](ReconcileError::is_retryable): the queue retries them
//! with backoff and fails the changeset after its `max_failures`.
//!
//! Outbound webhook events ([`webhook_events`]) are emitted after each run:
//! `changeset:publish`, `changeset:update` and `changeset:close` for what was done,
//! `changeset:publish_error` or `changeset:update_error` (on every failed attempt, with the
//! error in the payload) for what failed.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sourcegull_batches::store::{
    BatchChangeStore, BatchesCredentialStore, Changeset, ChangesetSpec, ChangesetSpecStore,
    ChangesetSpecType, ChangesetStore, ChangesetSync, CheckState, CredentialSecret, ExternalState,
    NewChangesetEvent, PublicationState, ReconcilerState, ReviewState, compute_changeset_state,
};
use sourcegull_codehost::{
    ChangesetSource, CodeHostError, CommitAction, CommitFile, Credential, HOOK_EVENT_PREFIX,
    MergeOptions, NewChangeset as HostChangeset, NewCommit, PushStrategy, RepoMeta,
};
use sourcegull_config::{RolloutWindows, SiteConfigHandle};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, Fence, Namespace};
use sourcegull_outbound_webhooks::{self as webhooks, Events};
use sourcegull_rpc::RepoClients;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::git::{GitCallError, GitReader, is_full_oid};
use sourcegull_types::{Repo, UserId};

use crate::error::{ReconcileError, Result};
use crate::plan::{DesiredPublication, Operation, Plan, plan, supports_draft};
use crate::source::{SourceProvider, credential};
use crate::syncer::next_sync;

/// How long [`Operation::Sleep`] waits by default.
pub const DEFAULT_SYNC_DELAY: Duration = Duration::from_secs(3);

/// The commit author when the spec names none.
pub const DEFAULT_AUTHOR_NAME: &str = "Sourcegull";
/// The commit author email when the spec names none.
pub const DEFAULT_AUTHOR_EMAIL: &str = "batch-changes@sourcegull.invalid";

/// The `fork_namespace` of a changeset spec that forks into the credential user's namespace.
pub const FORK_USER_NAMESPACE: &str = "<user>";

/// Reconciles changesets: plans and executes their operations, and syncs them.
#[derive(Clone)]
pub struct Reconciler {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    sources: Arc<dyn SourceProvider>,
    clients: Option<Arc<RepoClients>>,
    config: SiteConfigHandle,
    sync_delay: Duration,
    events: Events,
}

impl std::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("sync_delay", &self.sync_delay)
            .finish_non_exhaustive()
    }
}

impl Reconciler {
    /// `keyring` decrypts bulk changes credentials; `clients` reach the repo nodes (without
    /// them, pushes fail and are retried).
    pub fn new(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        sources: Arc<dyn SourceProvider>,
        clients: Option<Arc<RepoClients>>,
        config: SiteConfigHandle,
    ) -> Self {
        let events = Events::new(db.clone(), keyring.clone(), config.clone());
        Self {
            db,
            keyring,
            sources,
            clients,
            config,
            sync_delay: DEFAULT_SYNC_DELAY,
            events,
        }
    }

    /// Replaces how long [`Operation::Sleep`] waits.
    #[must_use]
    pub fn with_sync_delay(mut self, delay: Duration) -> Self {
        self.sync_delay = delay;
        self
    }

    /// The database.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Plans `ch` and executes the plan. Returns the plan.
    pub async fn reconcile(&self, ch: &Changeset) -> Result<Plan> {
        let specs = ChangesetSpecStore::new(&self.db);
        let current = match ch.current_spec_id {
            Some(id) => specs.get(id).await?,
            None => None,
        };
        let previous = match ch.previous_spec_id {
            Some(id) => specs.get(id).await?,
            None => None,
        };
        let mut plan = plan(previous.as_ref(), current.as_ref(), ch);
        if plan.is_empty() {
            return Ok(plan);
        }
        if plan.publishes() && !ch.window_released && self.has_rollout_windows() {
            ChangesetStore::new(&self.db).schedule(&[ch.id]).await?;
            tracing::info!(changeset = ch.id, operations = %plan, "publication waits for a rollout window");
            plan.scheduled = true;
            return Ok(plan);
        }
        tracing::info!(changeset = ch.id, operations = %plan, "reconciling changeset");
        let mut run = self.run(ch, current).await?;
        let result = run.execute(&plan).await;
        let persisted = run.persist().await;
        let error = match (&result, &persisted) {
            (Err(e), _) | (Ok(()), Err(e)) => Some(e),
            _ => None,
        };
        self.emit_events(ch.id, &plan, error).await;
        result?;
        persisted?;
        Ok(plan)
    }

    /// Emits the outbound webhook events of an executed plan, with the changeset as stored and
    /// in the state the queue is about to give it (completed, or retrying or failed with
    /// `error`).
    async fn emit_events(&self, id: i64, plan: &Plan, error: Option<&ReconcileError>) {
        let events = webhook_events(plan, error.is_none());
        if events.is_empty() {
            return;
        }
        let mut ch = match ChangesetStore::new(&self.db).get(id).await {
            Ok(Some(ch)) => ch,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(changeset = id, error = %e, "loading for webhooks failed");
                return;
            }
        };
        ch.reconciler_state = match error {
            None => ReconcilerState::Completed,
            Some(e) if e.is_retryable() => ReconcilerState::Errored,
            Some(_) => ReconcilerState::Failed,
        };
        if let Some(e) = error {
            ch.failure_message = Some(e.to_string());
        }
        ch.state =
            compute_changeset_state(ch.reconciler_state, ch.publication_state, ch.external_state);
        for event in events {
            self.events.changeset(event, &ch).await;
        }
    }

    /// Loads `ch` from the code host and stores its state (the periodic syncer). A merge
    /// request that is gone marks the changeset `DELETED`. The state is written only while
    /// `fence` (the syncer's lease) is current.
    pub async fn sync(&self, ch: &Changeset, fence: &Fence) -> Result<()> {
        let mut run = self.run(ch, None).await?;
        run.fence = fence.clone();
        let result = run.sync_op().await;
        let persisted = run.persist().await;
        result?;
        persisted
    }

    /// Comments on `ch`'s merge request with `user`'s credential (else the site's), for a bulk
    /// operation `user` started.
    pub async fn comment_as(&self, ch: &Changeset, user: UserId, body: &str) -> Result<()> {
        let mut run = self.run(ch, None).await?;
        run.actor = Some(user);
        let result = run.comment(body).await;
        let persisted = run.persist().await;
        result?;
        persisted
    }

    /// Merges `ch`'s merge request (squashed with `squash`) with `user`'s credential (else
    /// the site's), for a bulk operation, and stores the result.
    pub async fn merge_as(&self, ch: &Changeset, user: UserId, squash: bool) -> Result<()> {
        let mut run = self.run(ch, None).await?;
        run.actor = Some(user);
        let result = run.merge(squash).await;
        let persisted = run.persist().await;
        result?;
        persisted
    }

    /// Closes `ch`'s merge request (when open or a draft) with `user`'s credential (else the
    /// site's), for a bulk operation, and stores the result.
    pub async fn close_as(&self, ch: &Changeset, user: UserId) -> Result<()> {
        let mut run = self.run(ch, None).await?;
        run.actor = Some(user);
        let result = run.close().await;
        let persisted = run.persist().await;
        result?;
        persisted
    }

    /// Whether `batchChanges.rolloutWindows` gates publication.
    pub fn has_rollout_windows(&self) -> bool {
        !RolloutWindows::lenient(&self.config.load().value.batch_changes_rollout_windows).is_empty()
    }

    async fn run(&self, ch: &Changeset, spec: Option<ChangesetSpec>) -> Result<Run<'_>> {
        // Internal: the reconciler acts for the bulk change, not for a viewer.
        #[allow(clippy::disallowed_methods)]
        let repo = self.db.repos().get(ch.repo_id).await?.ok_or_else(|| {
            ReconcileError::Invalid(format!("repository {} not found", ch.repo_id))
        })?;
        Ok(Run {
            r: self,
            sync: sync_of(ch),
            ch: ch.clone(),
            spec,
            repo,
            conn: None,
            actor: None,
            remote: None,
            events: None,
            dirty: false,
            clear_closing: false,
            fence: Fence::none(crate::SYNCER),
        })
    }

    /// The user whose credential changes `ch`: the last applier of its owning bulk change,
    /// else of the first bulk change tracking it.
    async fn applier(&self, ch: &Changeset) -> Result<Option<UserId>> {
        let id = ch
            .owned_by_batch_change_id
            .or_else(|| ch.batch_change_ids.keys().next().copied());
        Ok(match id {
            Some(id) => BatchChangeStore::new(&self.db)
                .get(id)
                .await?
                .and_then(|b| b.last_applier_id),
            None => None,
        })
    }

    /// The credential secret for `user` (or the site's) on `repo`'s code host.
    async fn resolve_credential(
        &self,
        user: Option<UserId>,
        repo: &Repo,
    ) -> Result<Option<CredentialSecret>> {
        let creds = BatchesCredentialStore::new(&self.db, self.keyring.clone());
        let (kind, id) = (&repo.external_service_type, &repo.external_service_id);
        Ok(match user {
            Some(u) => creds.resolve(u, kind, id).await?.map(|(_, s)| s),
            None => match creds.site_credential(kind, id).await? {
                Some(c) => Some(creds.secret(&c).await?),
                None => None,
            },
        })
    }
}

/// The outbound webhook events of executing `plan`: on success `changeset:publish`
/// (published, possibly as a draft), `changeset:update` (pushed to or updated an open merge
/// request, undrafted or reopened it) and `changeset:close`; on failure
/// `changeset:publish_error` if it was publishing, else `changeset:update_error` if it was
/// updating.
pub fn webhook_events(plan: &Plan, ok: bool) -> Vec<&'static str> {
    let has = |ops: &[Operation]| plan.ops.iter().any(|op| ops.contains(op));
    let publish = has(&[Operation::Publish, Operation::PublishDraft]);
    let update = !publish
        && has(&[
            Operation::Push,
            Operation::Update,
            Operation::Undraft,
            Operation::Reopen,
        ]);
    let close = has(&[Operation::Close]);
    let mut events = Vec::new();
    if ok {
        if publish {
            events.push(webhooks::CHANGESET_PUBLISH);
        }
        if update {
            events.push(webhooks::CHANGESET_UPDATE);
        }
        if close {
            events.push(webhooks::CHANGESET_CLOSE);
        }
    } else if publish {
        events.push(webhooks::CHANGESET_PUBLISH_ERROR);
    } else if update {
        events.push(webhooks::CHANGESET_UPDATE_ERROR);
    }
    events
}

/// The source of a changeset's repository with the credential it acts with.
struct Conn {
    source: Arc<dyn ChangesetSource>,
    target: RepoMeta,
    /// `None`: no bulk changes credential, the source acts with the connection's token.
    credential: Option<Credential>,
    applier: Option<UserId>,
}

/// One reconciliation (or sync) of one changeset.
struct Run<'a> {
    r: &'a Reconciler,
    ch: Changeset,
    spec: Option<ChangesetSpec>,
    repo: Repo,
    conn: Option<Conn>,
    /// Acts with this user's credential (a bulk operation's initiator) instead of the batch
    /// change's last applier's.
    actor: Option<UserId>,
    /// Where branches are pushed: the target repository or a fork.
    remote: Option<RepoMeta>,
    /// The state to write back.
    sync: ChangesetSync,
    /// The timeline, when the merge request was loaded.
    events: Option<Vec<NewChangesetEvent>>,
    /// `sync` changed.
    dirty: bool,
    /// A close was carried out (or is moot): clear `closing`.
    clear_closing: bool,
    /// `sync` and `events` are written only while this is current.
    fence: Fence,
}

/// The changeset's current state as a [`ChangesetSync`] to update.
fn sync_of(ch: &Changeset) -> ChangesetSync {
    ChangesetSync {
        external_id: ch.external_id.clone(),
        external_branch: ch.external_branch.clone(),
        external_fork_namespace: ch.external_fork_namespace.clone(),
        external_fork_name: ch.external_fork_name.clone(),
        external_state: ch.external_state,
        external_review_state: ch.external_review_state,
        external_check_state: ch.external_check_state,
        external_updated_at: ch.external_updated_at,
        external_deleted_at: ch.external_deleted_at,
        diff_stat_added: ch.diff_stat_added,
        diff_stat_deleted: ch.diff_stat_deleted,
        metadata: ch.metadata.clone(),
        title: ch.title.clone(),
        body: ch.body.clone(),
        sync_state: ch.sync_state.clone(),
        publication_state: None,
        next_sync_at: ch.next_sync_at,
    }
}

/// `refs/heads/<name>` for a branch name or full ref.
pub fn full_ref(r: &str) -> String {
    if r.starts_with("refs/") {
        r.to_owned()
    } else {
        format!("refs/heads/{r}")
    }
}

/// The branch name of a full ref (`refs/heads/x` → `x`).
pub fn short_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/").unwrap_or(r)
}

/// Puts the bulk change link into a changeset body: in place of every
/// `${{ batch_change_link }}`, else appended after a blank line.
pub fn insert_batch_change_link(body: &str, link: &str) -> String {
    let mut out = String::with_capacity(body.len() + link.len());
    let mut rest = body;
    let mut replaced = false;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else { break };
        out.push_str(&rest[..start]);
        if after[..end].trim() == "batch_change_link" {
            out.push_str(link);
            replaced = true;
        } else {
            out.push_str(&rest[start..start + 3 + end + 2]);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    if replaced {
        out
    } else if body.is_empty() {
        link.to_owned()
    } else {
        format!("{body}\n\n{link}")
    }
}

impl Run<'_> {
    async fn execute(&mut self, plan: &Plan) -> Result<()> {
        for &op in &plan.ops {
            match op {
                Operation::Push => self.push().await?,
                Operation::Publish => self.publish(false).await?,
                Operation::PublishDraft => self.publish(true).await?,
                Operation::Update => self.update().await?,
                Operation::Undraft => self.undraft().await?,
                Operation::Close => self.close().await?,
                Operation::Reopen => self.reopen().await?,
                Operation::Sync => self.sync_op().await?,
                Operation::Import => self.import().await?,
                Operation::Sleep => tokio::time::sleep(self.r.sync_delay).await,
                Operation::Detach => self.detach().await?,
                Operation::Archive => self.archive().await?,
                // `detached_at` is cleared when the apply attaches the changeset again.
                Operation::Reattach => {}
            }
        }
        Ok(())
    }

    /// Writes the state learned from the code host.
    async fn persist(&mut self) -> Result<()> {
        if !self.dirty && !self.clear_closing {
            return Ok(());
        }
        let now = Utc::now();
        let latest_event = self.events.iter().flatten().map(|e| e.created_at).max();
        let last_change = self.sync.external_updated_at.max(latest_event);
        let webhooks = self.r.sources.has_webhooks(&self.repo).await;
        self.sync.next_sync_at = Some(next_sync(now, last_change, webhooks));
        if self.clear_closing && self.sync.publication_state.is_none() {
            self.sync.publication_state = Some(self.ch.publication_state);
        }
        let store = ChangesetStore::new(&self.r.db);
        store
            .store_sync(
                self.ch.id,
                &self.sync,
                self.events.as_deref(),
                HOOK_EVENT_PREFIX,
                &self.fence,
            )
            .await?;
        Ok(())
    }

    fn branch_spec(&self) -> Result<&ChangesetSpec> {
        self.spec
            .as_ref()
            .filter(|s| s.kind == ChangesetSpecType::Branch)
            .ok_or_else(|| ReconcileError::InvalidSpec("the changeset has no branch spec".into()))
    }

    /// The source, resolving the credential once. `write` requires a bulk changes
    /// credential.
    async fn conn(&mut self, write: bool) -> Result<&Conn> {
        if self.conn.is_none() {
            let (base, target) = self.r.sources.source(&self.repo).await?;
            let applier = match self.actor {
                Some(user) => Some(user),
                None => self.r.applier(&self.ch).await?,
            };
            let secret = self.r.resolve_credential(applier, &self.repo).await?;
            let (source, credential) = match secret {
                Some(secret) => {
                    let cred = credential(&secret);
                    let source: Arc<dyn ChangesetSource> = base
                        .with_credential(&cred)
                        .map_err(|e| ReconcileError::code_host("using the credential", e))?
                        .into();
                    (source, Some(cred))
                }
                None => (base, None),
            };
            self.conn = Some(Conn {
                source,
                target,
                credential,
                applier,
            });
        }
        let conn = self.conn.as_ref().expect("set above");
        if write && conn.credential.is_none() {
            return Err(ReconcileError::MissingCredentials {
                repo: self.repo.name.clone(),
                who: if self.actor.is_some() {
                    "the user who started the bulk operation has".into()
                } else if conn.applier.is_some() {
                    "the user who last applied the bulk change has".into()
                } else {
                    "the changeset has no bulk change applier, and has".into()
                },
                service_type: self.repo.external_service_type.clone(),
                service_id: self.repo.external_service_id.clone(),
            });
        }
        Ok(conn)
    }

    /// The repository to push to: the target, or the fork the changeset uses.
    async fn remote(&mut self) -> Result<RepoMeta> {
        if let Some(r) = &self.remote {
            return Ok(r.clone());
        }
        let enforce = self.r.config.load().value.batch_changes_enforce_forks;
        let spec_ns = self.spec.as_ref().and_then(|s| s.fork_namespace.clone());
        // `Some(None)`: a fork in the credential user's namespace.
        let fork: Option<Option<String>> = if let Some(ns) = &self.ch.external_fork_namespace {
            Some(Some(ns.clone()))
        } else if let Some(ns) = spec_ns {
            Some((!ns.is_empty() && ns != FORK_USER_NAMESPACE).then_some(ns))
        } else if enforce && self.ch.external_id.is_none() {
            Some(None)
        } else {
            None
        };
        let conn = self.conn(true).await?;
        let remote = match fork {
            None => conn.target.clone(),
            Some(ns) => conn
                .source
                .get_fork(&conn.target, ns.as_deref())
                .await
                .map_err(|e| ReconcileError::code_host("finding the fork to push to", e))?,
        };
        self.remote = Some(remote.clone());
        Ok(remote)
    }

    fn is_fork(&self, remote: &RepoMeta) -> bool {
        self.conn
            .as_ref()
            .is_some_and(|c| c.target.external.id != remote.external.id)
    }

    fn mark_read_only(&mut self) {
        self.sync.external_state = Some(ExternalState::ReadOnly);
        self.dirty = true;
    }

    fn mark_deleted(&mut self) {
        self.sync.external_state = Some(ExternalState::Deleted);
        self.sync.external_deleted_at = Some(Utc::now());
        self.dirty = true;
    }

    /// A failed write: an archived project marks the changeset read-only.
    fn write_error(&mut self, context: &'static str, e: CodeHostError) -> ReconcileError {
        if let CodeHostError::Archived(what) = e {
            self.mark_read_only();
            return ReconcileError::ReadOnly(what);
        }
        ReconcileError::code_host(context, e)
    }

    fn number(&self) -> Result<i64> {
        let id = self
            .sync
            .external_id
            .as_deref()
            .or_else(|| self.spec.as_ref().and_then(|s| s.external_id.as_deref()))
            .ok_or_else(|| ReconcileError::Invalid("the changeset has no merge request".into()))?;
        id.trim_start_matches(['!', '#'])
            .parse()
            .map_err(|_| ReconcileError::Invalid(format!("invalid merge request number {id:?}")))
    }

    /// Stores what the code host returned. `loaded` results carry review and check state and
    /// the timeline; write results do not.
    fn apply_remote(&mut self, c: &sourcegull_codehost::Changeset, loaded: bool) {
        let s = &mut self.sync;
        s.external_id = Some(c.number.to_string());
        s.external_branch = Some(full_ref(&c.head_ref));
        s.external_state = ExternalState::parse(c.state.as_str());
        if loaded || c.review_state.is_some() {
            s.external_review_state = c.review_state.and_then(|r| ReviewState::parse(r.as_str()));
        }
        if loaded || c.check_state.is_some() {
            s.external_check_state = c.check_state.and_then(|r| CheckState::parse(r.as_str()));
        }
        s.external_updated_at = Some(c.updated_at);
        s.external_deleted_at = None;
        s.title = Some(c.title.clone());
        s.body = Some(c.body.clone());
        if !c.metadata.is_null() {
            s.metadata = c.metadata.clone();
        }
        if loaded {
            self.events = Some(
                c.events
                    .iter()
                    .map(|e| NewChangesetEvent {
                        kind: e.kind.as_str().to_owned(),
                        key: e.key.clone(),
                        created_at: e.timestamp,
                        metadata: e.metadata.clone(),
                    })
                    .collect(),
            );
        }
        self.dirty = true;
    }

    async fn push(&mut self) -> Result<()> {
        let spec = self.branch_spec()?.clone();
        let head_ref = full_ref(
            spec.head_ref
                .as_deref()
                .ok_or_else(|| ReconcileError::InvalidSpec("no head branch".into()))?,
        );
        let diff = spec
            .diff
            .clone()
            .ok_or_else(|| ReconcileError::InvalidSpec("no diff".into()))?;
        let base = spec
            .base_rev
            .clone()
            .ok_or_else(|| ReconcileError::InvalidSpec("no base revision".into()))?;
        if ChangesetStore::new(&self.r.db)
            .other_on_branch(
                self.ch.id,
                self.ch.repo_id,
                &self.ch.external_service_type,
                &head_ref,
            )
            .await?
            .is_some()
        {
            return Err(ReconcileError::SameBranch);
        }
        let remote = self.remote().await?;
        if remote.archived {
            self.mark_read_only();
            return Err(ReconcileError::ReadOnly(remote.name));
        }
        let conn = self.conn(true).await?;
        let cred = conn.credential.clone().unwrap_or(Credential::None);
        let source = conn.source.clone();
        let target_id = conn.target.external.id.clone();
        let clients = self.r.clients.clone().ok_or(ReconcileError::NoRepoNodes)?;
        let git = GitReader::from_parts(clients, self.repo.id, &self.repo.name);
        let base = if is_full_oid(&base) {
            base
        } else {
            git.resolve_revision(&base)
                .await
                .map_err(|source| ReconcileError::Git {
                    context: "resolving the base revision",
                    source,
                })?
        };
        let message = spec
            .commit_message
            .clone()
            .or_else(|| spec.title.clone())
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "Bulk change".into());
        let author = pb::CommitIdentity {
            name: spec
                .commit_author_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| DEFAULT_AUTHOR_NAME.into()),
            email: spec
                .commit_author_email
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| DEFAULT_AUTHOR_EMAIL.into()),
        };

        let config = self.r.config.load();
        let reject_unverified = config.value.batch_changes_reject_unverified_commit;
        let limits = ApiLimits {
            max_bytes: config.value.batch_changes_commit_via_api_max_bytes,
            max_files: config.value.batch_changes_commit_via_api_max_files,
        };
        let via_api = reject_unverified || config.value.batch_changes_commit_via_api;
        drop(config);
        let mut fork_namespace = None;
        let mut committed = false;
        if via_api {
            let commit = ApiCommit {
                repo: &self.repo.name,
                git: &git,
                source: source.as_ref(),
                remote: &remote,
                target_id: &target_id,
                base: &base,
                branch: short_ref(&head_ref),
                diff: &diff,
                message: &message,
                author: &author,
                limits,
            };
            match commit.create().await {
                Ok(Ok(sha)) => {
                    tracing::info!(changeset = self.ch.id, repo = %remote.name, commit = %sha,
                        "created the changeset commit through the code host API");
                    committed = true;
                }
                Ok(Err(reason)) if reject_unverified => {
                    return Err(ReconcileError::UnverifiedCommitRejected {
                        repo: remote.name.clone(),
                        reason,
                    });
                }
                Ok(Err(reason)) => {
                    tracing::info!(changeset = self.ch.id, repo = %remote.name, %reason,
                        "cannot create the commit through the code host API; pushing it");
                }
                Err(ApiCommitError::CodeHost(e)) => {
                    return Err(
                        self.write_error("creating the commit through the code host API", e)
                    );
                }
                Err(ApiCommitError::Reconcile(e)) => return Err(e),
            }
        }
        if !committed {
            let strategy = source
                .push_strategy(&remote, &cred)
                .map_err(|e| ReconcileError::code_host("finding where to push", e))?;
            fork_namespace = strategy.fork_namespace.clone();
            self.push_commit(
                &git,
                source.as_ref(),
                &remote,
                &strategy,
                &cred,
                pb::CreateCommitFromPatchRequest {
                    base_commit_oid: base,
                    target_ref: head_ref.clone(),
                    apply: Some(apply_options(&diff)),
                    patch: diff.into(),
                    message,
                    author: Some(author),
                    ..Default::default()
                },
            )
            .await?;
        }
        let fork = self.is_fork(&remote);
        let s = &mut self.sync;
        s.external_branch = Some(head_ref);
        s.diff_stat_added = spec.diff_stat_added;
        s.diff_stat_deleted = spec.diff_stat_deleted;
        if fork {
            let path = remote.metadata["pathWithNamespace"]
                .as_str()
                .unwrap_or(&remote.uri)
                .to_owned();
            let (ns, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            s.external_fork_namespace = Some(fork_namespace.unwrap_or_else(|| ns.to_owned()));
            s.external_fork_name = Some(name.to_owned());
        }
        self.dirty = true;
        Ok(())
    }

    /// Commits `req`'s patch on the repo node and force-pushes it to `strategy`'s remote.
    async fn push_commit(
        &mut self,
        git: &GitReader,
        source: &dyn ChangesetSource,
        remote: &RepoMeta,
        strategy: &PushStrategy,
        cred: &Credential,
        mut req: pb::CreateCommitFromPatchRequest,
    ) -> Result<()> {
        // Credentials go separately (to git's askpass), never in the URL.
        let mut url = strategy.push_url.clone();
        let _ = url.set_username("");
        let _ = url.set_password(None);
        let credentials = match cred {
            Credential::Basic { username, password } => Some(pb::PushCredentials {
                username: username.clone(),
                token: password.clone(),
            }),
            Credential::None => None,
        };
        req.push = Some(pb::PushTarget {
            remote_url: url.to_string(),
            credentials,
            force: true,
        });
        match git.create_commit_from_patch(req).await {
            Ok(_) => Ok(()),
            Err(GitCallError::PatchFailed(message)) => Err(ReconcileError::PatchFailed {
                repo: self.repo.name.clone(),
                message,
            }),
            Err(GitCallError::Unauthenticated(message)) => {
                Err(ReconcileError::PushUnauthenticated {
                    repo: remote.name.clone(),
                    message,
                })
            }
            Err(GitCallError::PermissionDenied(message)) => {
                let kind = source.classify_push_error(remote, &message);
                tracing::info!(changeset = self.ch.id, repo = %remote.name, error = %message,
                    archived = matches!(kind, Some(CodeHostError::Archived(_))),
                    "push refused; marking the changeset read-only");
                self.mark_read_only();
                Err(ReconcileError::ReadOnly(format!(
                    "{} ({message})",
                    remote.name
                )))
            }
            Err(source) => Err(ReconcileError::Git {
                context: "creating and pushing the changeset commit",
                source,
            }),
        }
    }

    /// The merge request to open or update from the spec.
    async fn host_changeset(&mut self, draft: bool) -> Result<HostChangeset> {
        let spec = self.branch_spec()?.clone();
        let remote = self.remote().await?;
        let fork = self.is_fork(&remote);
        let body = self
            .decorate(spec.body.as_deref().unwrap_or_default())
            .await?;
        Ok(HostChangeset {
            title: spec.title.clone().unwrap_or_default(),
            body,
            head_ref: short_ref(spec.head_ref.as_deref().unwrap_or_default()).to_owned(),
            base_ref: short_ref(spec.base_ref.as_deref().unwrap_or_default()).to_owned(),
            draft,
            labels: Vec::new(),
            remove_source_branch: self.r.config.load().value.batch_changes_auto_delete_branch,
            head_repo_id: fork.then(|| remote.external.id.clone()),
        })
    }

    /// The body with a link back to the owning bulk change.
    async fn decorate(&self, body: &str) -> Result<String> {
        let Some(id) = self.ch.owned_by_batch_change_id else {
            return Ok(body.to_owned());
        };
        let Some(bc) = BatchChangeStore::new(&self.r.db).get(id).await? else {
            return Ok(body.to_owned());
        };
        let (kind, ns) = match bc.namespace {
            Namespace::User(u) => (
                "users",
                self.r.db.users().get_by_id(u).await?.map(|u| u.username),
            ),
            Namespace::Org(o) => (
                "organizations",
                self.r.db.orgs().get(o).await?.map(|o| o.name),
            ),
        };
        let ns = ns.unwrap_or_else(|| "unknown".into());
        let text = format!("_Created by Sourcegull bulk change `{ns}/{}`._", bc.name);
        let link = match &self.r.config.load().value.external_url {
            Some(url) => format!(
                "[{text}]({}/{kind}/{ns}/bulk-changes/{})",
                url.trim_end_matches('/'),
                bc.name
            ),
            None => text,
        };
        Ok(insert_batch_change_link(body, &link))
    }

    async fn publish(&mut self, draft: bool) -> Result<()> {
        let new = self.host_changeset(draft).await?;
        let conn = self.conn(true).await?;
        let created = conn.source.create(&conn.target, &new).await;
        let created = created.map_err(|e| self.write_error("creating the merge request", e))?;
        self.apply_remote(&created.changeset, false);
        self.sync.publication_state = Some(PublicationState::Published);
        Ok(())
    }

    async fn update(&mut self) -> Result<()> {
        let spec = self.branch_spec()?;
        let draft = supports_draft(&self.ch.external_service_type)
            && DesiredPublication::of(spec.published, self.ch.ui_publication_state)
                == DesiredPublication::Draft;
        let new = self.host_changeset(draft).await?;
        let number = self.number()?;
        let conn = self.conn(true).await?;
        match conn.source.update(&conn.target, number, &new).await {
            Ok(c) => self.apply_remote(&c, false),
            // An update of an archived project only marks the changeset read-only.
            Err(CodeHostError::Archived(_)) => self.mark_read_only(),
            Err(e) => return Err(ReconcileError::code_host("updating the merge request", e)),
        }
        Ok(())
    }

    async fn undraft(&mut self) -> Result<()> {
        let number = self.number()?;
        let conn = self.conn(true).await?;
        let c = conn.source.undraft(&conn.target, number).await;
        let c = c.map_err(|e| self.write_error("marking the merge request ready", e))?;
        self.apply_remote(&c, false);
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.clear_closing = true;
        if !matches!(
            self.sync.external_state,
            Some(ExternalState::Open | ExternalState::Draft)
        ) {
            return Ok(());
        }
        let number = self.number()?;
        let conn = self.conn(true).await?;
        let c = conn.source.close(&conn.target, number).await;
        let c = c.map_err(|e| self.write_error("closing the merge request", e))?;
        self.apply_remote(&c, false);
        Ok(())
    }

    async fn reopen(&mut self) -> Result<()> {
        let number = self.number()?;
        let conn = self.conn(true).await?;
        let c = conn.source.reopen(&conn.target, number).await;
        let c = c.map_err(|e| self.write_error("reopening the merge request", e))?;
        self.apply_remote(&c, false);
        Ok(())
    }

    async fn comment(&mut self, body: &str) -> Result<()> {
        let number = self.number()?;
        let conn = self.conn(true).await?;
        let res = conn.source.create_comment(&conn.target, number, body).await;
        res.map_err(|e| self.write_error("commenting on the merge request", e))
    }

    async fn merge(&mut self, squash: bool) -> Result<()> {
        let number = self.number()?;
        let opts = MergeOptions {
            squash,
            remove_source_branch: self.r.config.load().value.batch_changes_auto_delete_branch,
        };
        let conn = self.conn(true).await?;
        let c = conn.source.merge(&conn.target, number, opts).await;
        let c = c.map_err(|e| self.write_error("merging the merge request", e))?;
        self.apply_remote(&c, false);
        Ok(())
    }

    async fn sync_op(&mut self) -> Result<()> {
        let number = self.number()?;
        let conn = self.conn(false).await?;
        match conn.source.load(&conn.target, number).await {
            Ok(c) => self.apply_remote(&c, true),
            Err(CodeHostError::NotFound(_)) => self.mark_deleted(),
            Err(CodeHostError::Archived(_)) => self.mark_read_only(),
            Err(e) => return Err(ReconcileError::code_host("loading the merge request", e)),
        }
        Ok(())
    }

    async fn import(&mut self) -> Result<()> {
        let number = self.number()?;
        let conn = self.conn(false).await?;
        match conn.source.load(&conn.target, number).await {
            Ok(c) => self.apply_remote(&c, true),
            Err(CodeHostError::NotFound(_)) => {
                return Err(ReconcileError::Invalid(format!(
                    "merge request !{number} not found in {}",
                    self.repo.name
                )));
            }
            Err(e) => return Err(ReconcileError::code_host("importing the merge request", e)),
        }
        self.sync.publication_state = Some(PublicationState::Published);
        Ok(())
    }

    async fn detach(&mut self) -> Result<()> {
        let store = ChangesetStore::new(&self.r.db);
        let ids: Vec<i64> = self
            .ch
            .batch_change_ids
            .iter()
            .filter(|(_, a)| a.detach)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            store.remove_from_batch_change(self.ch.id, id).await?;
            self.ch.batch_change_ids.remove(&id);
        }
        Ok(())
    }

    async fn archive(&mut self) -> Result<()> {
        let store = ChangesetStore::new(&self.r.db);
        let ids: Vec<i64> = self
            .ch
            .batch_change_ids
            .iter()
            .filter(|(_, a)| a.archive && !a.is_archived)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            store.mark_archived(self.ch.id, id).await?;
        }
        Ok(())
    }
}

/// The limits of commit-via-API mode (`batchChanges.commitViaAPIMax*`).
#[derive(Debug, Clone, Copy)]
struct ApiLimits {
    max_bytes: u64,
    max_files: u32,
}

/// Why [`ApiCommit::create`] failed (as opposed to the API path not being usable).
enum ApiCommitError {
    /// The code host refused the commit.
    CodeHost(CodeHostError),
    Reconcile(ReconcileError),
}

/// A changeset commit created through the code host's API (commit-via-API mode):
/// the repo node applies the patch on the base commit without committing
/// ([`GitReader::apply_patch`]) and the changed files go to
/// [`ChangesetSource::create_commit`], so the code host creates (and signs) the commit.
struct ApiCommit<'a> {
    /// The repository's name (for errors).
    repo: &'a str,
    git: &'a GitReader,
    source: &'a dyn ChangesetSource,
    /// Where the commit goes: the target repository or a fork.
    remote: &'a RepoMeta,
    /// The target repository's external id (holding `base` when `remote` is a fork).
    target_id: &'a str,
    base: &'a str,
    branch: &'a str,
    diff: &'a [u8],
    message: &'a str,
    author: &'a pb::CommitIdentity,
    limits: ApiLimits,
}

impl ApiCommit<'_> {
    /// Creates the commit and returns its id, or `Ok(Err(reason))` when the API cannot carry
    /// it (no commit API, too large, symlinks or submodules) and nothing was created.
    async fn create(
        &self,
    ) -> std::result::Result<std::result::Result<String, String>, ApiCommitError> {
        if !self.source.supports_commit_api() {
            return Ok(Err("the code host has no API to create commits".into()));
        }
        let req = pb::ApplyPatchRequest {
            base_commit_oid: self.base.to_owned(),
            apply: Some(apply_options(self.diff)),
            patch: self.diff.to_vec().into(),
            max_content_bytes: self.limits.max_bytes,
            max_files: self.limits.max_files,
            ..Default::default()
        };
        let applied = match self.git.apply_patch(req).await {
            Ok(a) => a,
            Err(GitCallError::TooLarge(message)) => {
                return Ok(Err(format!(
                    "the change is too large for the code host API ({message}; see \
                     batchChanges.commitViaAPIMaxBytes and batchChanges.commitViaAPIMaxFiles)"
                )));
            }
            Err(GitCallError::PatchFailed(message)) => {
                return Err(ApiCommitError::Reconcile(ReconcileError::PatchFailed {
                    repo: self.repo.to_owned(),
                    message,
                }));
            }
            Err(source) => {
                return Err(ApiCommitError::Reconcile(ReconcileError::Git {
                    context: "applying the changeset patch",
                    source,
                }));
            }
        };
        let files = match commit_files(applied.files) {
            Ok(f) => f,
            Err(reason) => return Ok(Err(reason)),
        };
        let commit = NewCommit {
            branch: self.branch.to_owned(),
            message: self.message.to_owned(),
            author_name: self.author.name.clone(),
            author_email: self.author.email.clone(),
            start_sha: self.base.to_owned(),
            start_repo_id: (self.remote.external.id != self.target_id)
                .then(|| self.target_id.to_owned()),
            // Like the force push: the branch is replaced with one commit on the base.
            force: true,
            files,
        };
        self.source
            .create_commit(self.remote, &commit)
            .await
            .map(Ok)
            .map_err(ApiCommitError::CodeHost)
    }
}

/// The repo node's changed files as code host commit actions, or why the API cannot carry
/// them (symlinks and submodules).
fn commit_files(files: Vec<pb::ChangedFile>) -> std::result::Result<Vec<CommitFile>, String> {
    use pb::changed_file::{Action, Kind};
    let special = |k: Kind| matches!(k, Kind::Symlink | Kind::Submodule);
    files
        .into_iter()
        .map(|f| {
            let action = f.action();
            let (kind, previous) = (f.kind(), f.previous_kind());
            let allowed = match action {
                // A deleted symlink is a plain delete.
                Action::Delete => kind != Kind::Submodule,
                _ => !special(kind) && !special(previous),
            };
            if !allowed {
                return Err(format!(
                    "{} is a symbolic link or submodule, which the code host API cannot commit",
                    f.path
                ));
            }
            let executable = match action {
                Action::Create => Some(kind == Kind::Executable),
                Action::Update | Action::Move if previous != kind => Some(kind == Kind::Executable),
                _ => None,
            };
            let action = match action {
                Action::Create => CommitAction::Create,
                Action::Update => CommitAction::Update,
                Action::Delete => CommitAction::Delete,
                Action::Move => CommitAction::Move,
                Action::Unspecified => {
                    return Err(format!("unknown change of {}", f.path));
                }
            };
            let content = (action != CommitAction::Delete
                && (f.content_changed || action == CommitAction::Create))
                .then_some(f.content);
            Ok(CommitFile {
                action,
                previous_path: (!f.previous_path.is_empty()).then_some(f.previous_path),
                path: f.path,
                content,
                executable,
            })
        })
        .collect()
}

/// `git apply` options for a changeset spec's diff. Batch steps (the step runner and
/// `gull bulk`) write diffs with `git diff --no-prefix`, which are applied with `-p0`; diffs uploaded with `a/` and `b/` prefixes (plain `git diff`) are stripped with `-p1`.
pub(crate) fn apply_options(diff: &[u8]) -> pb::PatchApplyOptions {
    pb::PatchApplyOptions {
        strip: Some(if has_git_prefixes(diff) { 1 } else { 0 }),
        ..Default::default()
    }
}

/// Whether every file header of `diff` names its paths with `git diff`'s `a/` and `b/`
/// prefixes (`/dev/null` for added and deleted files). Only header lines count: `---` and
/// `+++` lines are read between a `diff --git` line (or the start) and the first hunk.
fn has_git_prefixes(diff: &[u8]) -> bool {
    let mut headers = 0;
    let mut in_header = true;
    for line in diff.split(|&b| b == b'\n') {
        let ok = if let Some(rest) = line.strip_prefix(b"diff --git ") {
            in_header = true;
            rest.starts_with(b"a/") && rest.windows(3).any(|w| w == b" b/")
        } else if line.starts_with(b"@@") {
            in_header = false;
            continue;
        } else if let Some(path) = line
            .strip_prefix(b"--- ")
            .or_else(|| line.strip_prefix(b"+++ "))
            .filter(|_| in_header)
        {
            path.starts_with(b"a/") || path.starts_with(b"b/") || path.starts_with(b"/dev/null")
        } else {
            continue;
        };
        if !ok {
            return false;
        }
        headers += 1;
    }
    headers > 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn apply_options_follow_the_diff_prefixes() {
        let strip = |d: &str| super::apply_options(d.as_bytes()).strip;
        // `git diff --no-prefix`, as batch steps write it.
        let bare = "diff --git config.txt config.txt\nindex 1..2 100644\n--- config.txt\n\
                    +++ config.txt\n@@ -1 +1 @@\n-version = 1\n+version = 2\n";
        assert_eq!(strip(bare), Some(0));
        // Plain `git diff`, with a removed line that looks like a header inside the hunk.
        let prefixed = "diff --git a/x.sql b/x.sql\n--- a/x.sql\n+++ b/x.sql\n@@ -1,2 +1 @@\n\
                        --- old comment\n select 1;\ndiff --git a/new b/new\nnew file mode 100644\n\
                        --- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+x\n";
        assert_eq!(strip(prefixed), Some(1));
        // A file in a directory named `a` without prefixes.
        assert_eq!(strip("diff --git a/x a/x\n--- a/x\n+++ a/x\n"), Some(0));
        // A unified diff without git headers.
        assert_eq!(
            strip("--- README.md\n+++ README.md\n@@ -1 +1 @@\n-a\n+b\n"),
            Some(0)
        );
        assert_eq!(strip(""), Some(0));
    }

    use super::*;

    #[test]
    fn webhook_events_of_plans() {
        use Operation::*;
        let events = |ops: &[Operation], ok: bool| {
            webhook_events(
                &Plan {
                    ops: ops.to_vec(),
                    ..Default::default()
                },
                ok,
            )
        };
        assert_eq!(events(&[Push, Publish], true), vec!["changeset:publish"]);
        assert_eq!(
            events(&[Push, PublishDraft], true),
            vec!["changeset:publish"]
        );
        assert_eq!(
            events(&[Push, Publish], false),
            vec!["changeset:publish_error"]
        );
        assert_eq!(events(&[Update], true), vec!["changeset:update"]);
        assert_eq!(events(&[Undraft], true), vec!["changeset:update"]);
        assert_eq!(events(&[Reopen], true), vec!["changeset:update"]);
        assert_eq!(events(&[Push, Sleep, Sync], true), vec!["changeset:update"]);
        assert_eq!(events(&[Update], false), vec!["changeset:update_error"]);
        assert_eq!(events(&[Close], true), vec!["changeset:close"]);
        assert_eq!(events(&[Close], false), Vec::<&str>::new());
        assert_eq!(events(&[Sync], true), Vec::<&str>::new());
        assert_eq!(events(&[Import], true), Vec::<&str>::new());
        assert_eq!(events(&[Detach, Archive], true), Vec::<&str>::new());
    }

    #[test]
    fn batch_change_links() {
        let link = "[x](u)";
        assert_eq!(insert_batch_change_link("Body", link), "Body\n\n[x](u)");
        assert_eq!(insert_batch_change_link("", link), "[x](u)");
        assert_eq!(
            insert_batch_change_link("See ${{ batch_change_link }}.", link),
            "See [x](u)."
        );
        assert_eq!(
            insert_batch_change_link("${{batch_change_link}} and ${{ other }}", link),
            "[x](u) and ${{ other }}"
        );
        assert_eq!(
            insert_batch_change_link("${{ other }}", link),
            "${{ other }}\n\n[x](u)"
        );
        assert_eq!(
            insert_batch_change_link("open ${{ batch_change_link", link),
            "open ${{ batch_change_link\n\n[x](u)"
        );
    }

    #[test]
    fn refs() {
        assert_eq!(full_ref("fix"), "refs/heads/fix");
        assert_eq!(full_ref("refs/heads/fix"), "refs/heads/fix");
        assert_eq!(short_ref("refs/heads/a/b"), "a/b");
        assert_eq!(short_ref("main"), "main");
    }

    #[test]
    fn changed_files_become_commit_files() {
        use pb::changed_file::{Action, Kind};
        let f = |action: Action, path: &str, kind: Kind, previous: Kind, changed: bool| {
            pb::ChangedFile {
                action: action.into(),
                path: path.into(),
                previous_path: if action == Action::Move {
                    format!("old/{path}")
                } else {
                    String::new()
                },
                kind: kind.into(),
                previous_kind: previous.into(),
                content: if changed { b"x".to_vec() } else { Vec::new() },
                content_changed: changed,
            }
        };
        let files = commit_files(vec![
            f(
                Action::Create,
                "a.sh",
                Kind::Executable,
                Kind::Unspecified,
                true,
            ),
            f(Action::Create, "b", Kind::Regular, Kind::Unspecified, true),
            f(Action::Update, "c", Kind::Regular, Kind::Regular, true),
            f(Action::Update, "d", Kind::Executable, Kind::Regular, false),
            f(Action::Move, "e", Kind::Regular, Kind::Regular, false),
            f(Action::Move, "g", Kind::Regular, Kind::Executable, true),
            f(Action::Delete, "h", Kind::Symlink, Kind::Symlink, false),
        ])
        .unwrap();
        let summary: Vec<_> = files
            .iter()
            .map(|c| {
                (
                    c.action,
                    c.path.as_str(),
                    c.previous_path.as_deref(),
                    c.content.is_some(),
                    c.executable,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (CommitAction::Create, "a.sh", None, true, Some(true)),
                (CommitAction::Create, "b", None, true, Some(false)),
                (CommitAction::Update, "c", None, true, None),
                (CommitAction::Update, "d", None, false, Some(true)),
                (CommitAction::Move, "e", Some("old/e"), false, None),
                (CommitAction::Move, "g", Some("old/g"), true, Some(false)),
                (CommitAction::Delete, "h", None, false, None),
            ]
        );
        for bad in [
            f(
                Action::Create,
                "link",
                Kind::Symlink,
                Kind::Unspecified,
                true,
            ),
            f(
                Action::Update,
                "was-link",
                Kind::Regular,
                Kind::Symlink,
                true,
            ),
            f(
                Action::Update,
                "sub",
                Kind::Submodule,
                Kind::Submodule,
                true,
            ),
            f(
                Action::Delete,
                "sub",
                Kind::Submodule,
                Kind::Submodule,
                false,
            ),
        ] {
            let path = bad.path.clone();
            let err = commit_files(vec![bad]).unwrap_err();
            assert!(err.contains(&path), "{err}");
        }
    }
}
