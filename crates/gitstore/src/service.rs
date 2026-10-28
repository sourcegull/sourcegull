//! The repo node gRPC service (`sourcegull.repo.v1.RepoService`) on top of
//! [`GitStore`].
//!
//! The service is generic over a [`RemoteResolver`] that turns a repo into a
//! [`Remote`] (URL + credentials). The repo role implements it with a database
//! lookup; tests use a closure. Fetch credentials therefore never travel over
//! RPC. Pushes (bulk changes) are the exception: they act as a user, so
//! `Push` and `CreateCommitFromPatch` carry that user's token in the request,
//! and the node only uses it for that one git process.
//!
//! Reads run on the blocking pool. Streaming RPCs produce batches as they are
//! computed, with backpressure from the gRPC stream.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use gix::ObjectId;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::repo::v1::repo_service_server::{RepoService, RepoServiceServer};
use sourcegull_types::RepoId;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::archive::{ArchiveFormat, archive_stream};
use crate::blame::{BlameHunk, BlameOptions, blame_stream};
use crate::diff::{self, ChangeStatus, DiffOptions, FileDiff};
use crate::error::{GitError, Result};
use crate::janitor::JanitorTask;
use crate::patch::{ApplyOptions, ChangedFile, FileAction, FileKind, Identity, PatchCommit};
use crate::push::PushSpec;
use crate::read::{self, CommitInfo, EntryKind, ListTreeOptions, LogOptions, Signature, TreeEntry};
use crate::refs::{self, ListRefsFilter, RefKind, RefUpdate};
use crate::remote::{Remote, RemoteAuth, Secret};
use crate::store::GitStore;
use crate::sync::SyncOutcome;

/// A repository as named in requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    /// Repo id (`repo.id`).
    pub id: RepoId,
    /// Repo name (`gitlab.com/group/project`).
    pub name: String,
}

/// Resolves the remote (URL and credentials) of a repo for clone and fetch, and says whether
/// the node may write the repo.
pub trait RemoteResolver: Send + Sync + 'static {
    /// Returns the remote of `repo`.
    fn resolve(&self, repo: &RepoRef) -> impl Future<Output = Result<Remote>> + Send;

    /// Checks that `repo` is assigned to this node before a write (clone, fetch, commit,
    /// push): [`GitError::NotAssigned`] otherwise. Reads are not checked. The default allows
    /// every repo.
    fn check_assigned(&self, _repo: &RepoRef) -> impl Future<Output = Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}

impl<F> RemoteResolver for F
where
    F: Fn(&RepoRef) -> Result<Remote> + Send + Sync + 'static,
{
    fn resolve(&self, repo: &RepoRef) -> impl Future<Output = Result<Remote>> + Send {
        std::future::ready(self(repo))
    }
}

/// Server-side limits and batch sizes.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Entries per ListTree message.
    pub tree_batch: usize,
    /// Commits per CommitLog message.
    pub log_batch: usize,
    /// Default and maximum commits per CommitLog call.
    pub max_log_commits: usize,
    /// Files per Diff message.
    pub diff_batch: usize,
    /// Hunks per Blame message (incremental blame sends them as found).
    pub blame_batch: usize,
    /// Default and maximum commits in a Compare.
    pub max_compare_commits: usize,
    /// Allow pushes to local paths and `file://` URLs (tests and development only).
    pub allow_local_push: bool,
    /// Free-space watermarks, reported by DiskInfo.
    pub disk: crate::janitor::DiskWatermarks,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            tree_batch: 1_000,
            log_batch: 100,
            max_log_commits: 1_000,
            diff_batch: 20,
            blame_batch: 50,
            max_compare_commits: 250,
            allow_local_push: false,
            disk: crate::janitor::DiskWatermarks::default(),
        }
    }
}

/// The repo node service.
pub struct RepoNodeService<R> {
    store: Arc<GitStore>,
    resolver: Arc<R>,
    config: ServiceConfig,
}

impl<R: RemoteResolver> RepoNodeService<R> {
    /// Creates the service.
    pub fn new(store: Arc<GitStore>, resolver: R, config: ServiceConfig) -> Self {
        Self {
            store,
            resolver: Arc::new(resolver),
            config,
        }
    }

    /// Wraps the service in the tonic server type, with message size limits
    /// suitable for file contents and patches.
    pub fn into_server(self) -> RepoServiceServer<Self> {
        let patch = self.store.config().max_patch_bytes;
        RepoServiceServer::new(self)
            .max_decoding_message_size((16 * 1024 * 1024).max(patch + 1024 * 1024))
            .max_encoding_message_size(64 * 1024 * 1024)
    }

    async fn remote(&self, repo: &RepoRef) -> Result<Remote> {
        self.resolver.resolve(repo).await
    }

    /// The remote of `repo` for a clone or fetch, once [`RemoteResolver::check_assigned`]
    /// passed.
    async fn remote_for_write(&self, repo: &RepoRef) -> Result<Remote> {
        self.resolver.check_assigned(repo).await?;
        self.remote(repo).await
    }

    /// The remote a push of `repo` goes to, with the request's credentials.
    ///
    /// Defense in depth: the caller chose the URL and credentials, but
    /// the node only pushes to the code host the repo was cloned from (same
    /// host as its remote; forks live there too), and to local paths only when
    /// configured for tests.
    async fn push_remote(&self, repo: &RepoRef, target: &pb::PushTarget) -> Result<Remote> {
        let auth = match &target.credentials {
            Some(c) if !c.token.is_empty() => RemoteAuth::Basic {
                username: if c.username.is_empty() {
                    "oauth2".to_owned()
                } else {
                    c.username.clone()
                },
                password: Secret::new(c.token.clone()),
            },
            _ => RemoteAuth::None,
        };
        let own = self.remote(repo).await?;
        let url = if target.remote_url.is_empty() {
            own.url().to_owned()
        } else {
            target.remote_url.clone()
        };
        let remote = Remote::new(&url, auth);
        let local_ok = self.config.allow_local_push && crate::push::is_local_url(remote.url());
        crate::push::check_push_url(remote.url(), self.config.allow_local_push)?;
        if !local_ok {
            let host = crate::push::url_host(remote.url());
            if host.is_none() || host != crate::push::url_host(own.url()) {
                return Err(GitError::InvalidArgument(format!(
                    "push remote {} is not on the code host of {}",
                    remote.url(),
                    repo.name
                )));
            }
        }
        Ok(remote)
    }
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send>>;

fn repo_ref(r: Option<pb::RepoRef>) -> std::result::Result<RepoRef, Status> {
    let r = r.ok_or_else(|| GitError::InvalidArgument("repo is required".into()))?;
    if r.id <= 0 {
        return Err(GitError::InvalidArgument(format!("invalid repo id {}", r.id)).into());
    }
    Ok(RepoRef {
        id: RepoId(r.id),
        name: r.name,
    })
}

fn oid(hex: &str) -> std::result::Result<ObjectId, Status> {
    Ok(read::parse_oid(hex)?)
}

/// An identity, or `None` when unset or empty.
fn identity(i: Option<pb::CommitIdentity>) -> Option<Identity> {
    i.filter(|i| !(i.name.is_empty() && i.email.is_empty()))
        .map(|i| Identity {
            name: i.name,
            email: i.email,
        })
}

/// The most bytes of file contents one ApplyPatch answer carries (the server encodes
/// messages up to 64 MiB).
pub const MAX_APPLY_CONTENT_BYTES: u64 = 48 * 1024 * 1024;

fn apply_options(o: Option<pb::PatchApplyOptions>) -> ApplyOptions {
    let o = o.unwrap_or_default();
    ApplyOptions {
        strip: o.strip,
        ignore_whitespace: o.ignore_whitespace,
        recount: o.recount,
        unidiff_zero: o.unidiff_zero,
        directory: o.directory,
        allow_empty: o.allow_empty,
    }
}

fn to_pb_file_kind(k: FileKind) -> pb::changed_file::Kind {
    use pb::changed_file::Kind;
    match k {
        FileKind::Regular => Kind::Regular,
        FileKind::Executable => Kind::Executable,
        FileKind::Symlink => Kind::Symlink,
        FileKind::Submodule => Kind::Submodule,
    }
}

fn to_pb_changed_file(f: ChangedFile) -> pb::ChangedFile {
    use pb::changed_file::Action;
    let action = match f.action {
        FileAction::Create => Action::Create,
        FileAction::Update => Action::Update,
        FileAction::Delete => Action::Delete,
        FileAction::Move => Action::Move,
    };
    pb::ChangedFile {
        action: action.into(),
        path: f.path,
        previous_path: f.previous_path.unwrap_or_default(),
        kind: to_pb_file_kind(f.kind).into(),
        previous_kind: f
            .previous_kind
            .map_or(pb::changed_file::Kind::Unspecified, to_pb_file_kind)
            .into(),
        content: f.content,
        content_changed: f.content_changed,
    }
}

fn hex(o: Option<ObjectId>) -> String {
    o.map(|o| o.to_string()).unwrap_or_default()
}

fn to_pb_sync(o: SyncOutcome) -> pb::SyncResult {
    pb::SyncResult {
        ref_updates: o.ref_updates.into_iter().map(to_pb_ref_update).collect(),
        head_ref: o.head_ref.unwrap_or_default(),
        head_oid: hex(o.head_oid),
        size_bytes: o.size_bytes,
        duration_ms: o.duration.as_millis() as u64,
        progress_tail: o.progress_tail,
        cloned: o.cloned,
    }
}

fn to_pb_ref_update(u: RefUpdate) -> pb::RefUpdate {
    pb::RefUpdate {
        name: u.name,
        old_oid: hex(u.old),
        new_oid: hex(u.new),
    }
}

fn to_pb_entry(e: TreeEntry) -> pb::TreeEntry {
    let kind = match e.kind {
        EntryKind::File => pb::EntryKind::File,
        EntryKind::Executable => pb::EntryKind::Executable,
        EntryKind::Symlink => pb::EntryKind::Symlink,
        EntryKind::Directory => pb::EntryKind::Directory,
        EntryKind::Submodule => pb::EntryKind::Submodule,
    };
    pb::TreeEntry {
        path: e.path,
        name: e.name,
        kind: kind as i32,
        mode: e.mode,
        oid: e.oid.to_string(),
        size: e.size,
    }
}

fn to_pb_sig(s: Signature) -> pb::Signature {
    pb::Signature {
        name: s.name,
        email: s.email,
        time: s.time,
        tz_offset_seconds: s.tz_offset_seconds,
    }
}

fn to_pb_commit(c: CommitInfo) -> pb::Commit {
    pb::Commit {
        oid: c.oid.to_string(),
        parents: c.parents.iter().map(ToString::to_string).collect(),
        author: Some(to_pb_sig(c.author)),
        committer: Some(to_pb_sig(c.committer)),
        message: c.message,
        tree_oid: c.tree.to_string(),
    }
}

fn to_pb_file(f: FileDiff) -> pb::FileDiff {
    let status = match f.status {
        ChangeStatus::Added => pb::ChangeStatus::Added,
        ChangeStatus::Deleted => pb::ChangeStatus::Deleted,
        ChangeStatus::Modified => pb::ChangeStatus::Modified,
        ChangeStatus::Renamed => pb::ChangeStatus::Renamed,
        ChangeStatus::Copied => pb::ChangeStatus::Copied,
        ChangeStatus::TypeChanged => pb::ChangeStatus::TypeChanged,
    };
    pb::FileDiff {
        status: status as i32,
        old_path: f.old_path.unwrap_or_default(),
        new_path: f.new_path.unwrap_or_default(),
        old_oid: hex(f.old_oid),
        new_oid: hex(f.new_oid),
        old_mode: f.old_mode,
        new_mode: f.new_mode,
        additions: f.additions,
        deletions: f.deletions,
        binary: f.binary,
        too_large: f.too_large,
        hunks: f
            .hunks
            .into_iter()
            .map(|h| pb::Hunk {
                old_start: h.old_start,
                old_lines: h.old_lines,
                new_start: h.new_start,
                new_lines: h.new_lines,
                body: h.body,
                section: h.section,
            })
            .collect(),
    }
}

fn to_pb_blame(h: BlameHunk) -> pb::BlameHunk {
    pb::BlameHunk {
        commit_oid: h.commit.to_string(),
        start_line: h.start_line,
        line_count: h.line_count,
        orig_start_line: h.orig_start_line,
        orig_path: h.orig_path,
        author: Some(to_pb_sig(h.author)),
        summary: h.summary,
    }
}

fn clamp_limit(requested: u32, max: usize) -> usize {
    if requested == 0 {
        max
    } else {
        (requested as usize).min(max)
    }
}

/// Runs `produce` on the blocking pool, feeding items it sends into a gRPC
/// stream. `produce` gets a sender that blocks when the client is slow and
/// returns `false` once the client has gone away.
fn blocking_stream<T, F>(store: Arc<GitStore>, repo: RepoId, produce: F) -> BoxStream<T>
where
    T: Send + 'static,
    F: FnOnce(&gix::Repository, &dyn Fn(T) -> bool) -> Result<()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<std::result::Result<T, Status>>(4);
    tokio::task::spawn_blocking(move || {
        let send = |item: T| tx.blocking_send(Ok(item)).is_ok();
        let result = store.repo(repo).and_then(|r| produce(&r, &send));
        if let Err(e) = result {
            let _ = tx.blocking_send(Err(e.into()));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

#[tonic::async_trait]
impl<R: RemoteResolver> RepoService for RepoNodeService<R> {
    async fn ensure_repo(
        &self,
        req: Request<pb::EnsureRepoRequest>,
    ) -> std::result::Result<Response<pb::EnsureRepoResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let already_cloned = self.store.is_cloned(repo.id);
        if already_cloned && !req.fetch_if_cloned {
            return Ok(Response::new(pb::EnsureRepoResponse {
                result: None,
                already_cloned,
            }));
        }
        let remote = self.remote_for_write(&repo).await?;
        let out = self
            .store
            .ensure(repo.id, &remote, req.fetch_if_cloned)
            .await?;
        Ok(Response::new(pb::EnsureRepoResponse {
            already_cloned: out.as_ref().is_none_or(|o| !o.cloned),
            result: out.map(to_pb_sync),
        }))
    }

    async fn fetch_repo(
        &self,
        req: Request<pb::FetchRepoRequest>,
    ) -> std::result::Result<Response<pb::FetchRepoResponse>, Status> {
        let repo = repo_ref(req.into_inner().repo)?;
        let remote = self.remote_for_write(&repo).await?;
        let out = self.store.fetch(repo.id, &remote).await?;
        Ok(Response::new(pb::FetchRepoResponse {
            result: Some(to_pb_sync(out)),
        }))
    }

    async fn clone_repo(
        &self,
        req: Request<pb::CloneRepoRequest>,
    ) -> std::result::Result<Response<pb::CloneRepoResponse>, Status> {
        let repo = repo_ref(req.into_inner().repo)?;
        let remote = self.remote_for_write(&repo).await?;
        let out = self.store.clone_repo(repo.id, &remote).await?;
        Ok(Response::new(pb::CloneRepoResponse {
            result: Some(to_pb_sync(out)),
        }))
    }

    async fn delete_repo(
        &self,
        req: Request<pb::DeleteRepoRequest>,
    ) -> std::result::Result<Response<pb::DeleteRepoResponse>, Status> {
        let repo = repo_ref(req.into_inner().repo)?;
        let existed = self.store.delete(repo.id).await?;
        Ok(Response::new(pb::DeleteRepoResponse { existed }))
    }

    async fn repo_status(
        &self,
        req: Request<pb::RepoStatusRequest>,
    ) -> std::result::Result<Response<pb::RepoStatusResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let s = self.store.status(repo.id, req.include_size).await?;
        Ok(Response::new(pb::RepoStatusResponse {
            cloned: s.cloned,
            clone_in_progress: s.clone_in_progress,
            head_ref: s.head_ref.unwrap_or_default(),
            head_oid: hex(s.head_oid),
            size_bytes: s.size_bytes,
            pack_count: s.fragmentation.packs,
            loose_object_count: s.fragmentation.loose_objects,
            path: s.path.display().to_string(),
        }))
    }

    async fn run_janitor(
        &self,
        req: Request<pb::RunJanitorRequest>,
    ) -> std::result::Result<Response<pb::RunJanitorResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let task = match pb::JanitorTask::try_from(req.task) {
            Ok(pb::JanitorTask::Maintenance) => JanitorTask::Maintenance,
            Ok(pb::JanitorTask::Gc) => JanitorTask::Gc,
            Ok(pb::JanitorTask::Repack) => JanitorTask::Repack,
            Ok(pb::JanitorTask::Prune) => JanitorTask::Prune,
            Ok(pb::JanitorTask::CommitGraph) => JanitorTask::CommitGraph,
            Ok(pb::JanitorTask::Fsck) => JanitorTask::Fsck,
            _ => return Err(GitError::InvalidArgument("unknown janitor task".into()).into()),
        };
        let out = self.store.run_janitor(repo.id, task).await?;
        Ok(Response::new(pb::RunJanitorResponse {
            corrupt: out.corrupt,
            output: out.output,
            duration_ms: out.duration.as_millis() as u64,
            size_bytes_after: out.size_bytes_after,
        }))
    }

    async fn disk_info(
        &self,
        _req: Request<pb::DiskInfoRequest>,
    ) -> std::result::Result<Response<pb::DiskInfoResponse>, Status> {
        let store = self.store.clone();
        let disk = self.config.disk;
        let resp = tokio::task::spawn_blocking(move || -> std::io::Result<pb::DiskInfoResponse> {
            let layout = store.layout();
            let usage = crate::janitor::disk_usage(layout.root())?;
            // The index directory holds a few files per repository at most, so walking it is
            // cheap (unlike the repositories, whose sizes come from `gitserver_repos`).
            Ok(pb::DiskInfoResponse {
                total_bytes: usage.total,
                free_bytes: usage.available,
                repos_on_disk: layout.list_repos()?.len() as u64,
                index_bytes: crate::sync::dir_size(&layout.index_dir())?,
                path: layout.root().display().to_string(),
                pause_clones_below_bytes: disk.pause_clones_threshold(usage.total),
                clones_paused: crate::janitor::assess_disk(usage, &disk).pause_clones,
            })
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(|e| Status::internal(format!("reading disk usage: {e}")))?;
        Ok(Response::new(resp))
    }

    async fn resolve_revision(
        &self,
        req: Request<pb::ResolveRevisionRequest>,
    ) -> std::result::Result<Response<pb::ResolveRevisionResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let rev = req.rev;
        let id = self
            .store
            .read(repo.id, move |r| read::resolve_revision(r, &rev))
            .await?;
        Ok(Response::new(pb::ResolveRevisionResponse {
            commit_oid: id.to_string(),
        }))
    }

    async fn list_refs(
        &self,
        req: Request<pb::ListRefsRequest>,
    ) -> std::result::Result<Response<pb::ListRefsResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let (list, head) = self
            .store
            .read(repo.id, move |r| {
                let filter = ListRefsFilter {
                    branches: req.branches,
                    tags: req.tags,
                    prefix: &req.prefix,
                };
                Ok((refs::list_refs(r, filter)?, refs::head(r)?.0))
            })
            .await?;
        Ok(Response::new(pb::ListRefsResponse {
            refs: list
                .into_iter()
                .map(|r| pb::RefInfo {
                    name: r.name,
                    short_name: r.short_name,
                    kind: match r.kind {
                        RefKind::Branch => pb::RefKind::Branch,
                        RefKind::Tag => pb::RefKind::Tag,
                        RefKind::Other => pb::RefKind::Other,
                    } as i32,
                    target_oid: r.target.to_string(),
                    commit_oid: hex(r.commit),
                    commit_time: r.commit_time.unwrap_or_default(),
                    annotated: r.annotated,
                })
                .collect(),
            head_ref: head.unwrap_or_default(),
        }))
    }

    async fn stat(
        &self,
        req: Request<pb::StatRequest>,
    ) -> std::result::Result<Response<pb::StatResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let entry = self
            .store
            .read(repo.id, move |r| read::stat(r, commit, &req.path))
            .await?;
        Ok(Response::new(pb::StatResponse {
            entry: Some(to_pb_entry(entry)),
        }))
    }

    async fn read_file(
        &self,
        req: Request<pb::ReadFileRequest>,
    ) -> std::result::Result<Response<pb::ReadFileResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let max = self.store.config().max_file_bytes;
        let limit = if req.max_bytes == 0 {
            max
        } else {
            req.max_bytes.min(max)
        };
        let blob = self
            .store
            .read(repo.id, move |r| {
                read::read_blob(r, commit, &req.path, limit)
            })
            .await?;
        Ok(Response::new(pb::ReadFileResponse {
            content: blob.content.into(),
            oid: blob.oid.to_string(),
            size: blob.size,
            binary: blob.binary,
            truncated: blob.truncated,
        }))
    }

    type ListTreeStream = BoxStream<pb::ListTreeResponse>;

    async fn list_tree(
        &self,
        req: Request<pb::ListTreeRequest>,
    ) -> std::result::Result<Response<Self::ListTreeStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let opts = ListTreeOptions {
            path: req.path,
            recursive: req.recursive,
            include_sizes: req.include_sizes,
            limit: clamp_limit(req.limit, self.store.config().max_tree_entries),
        };
        let batch = self.config.tree_batch.max(1);
        Ok(Response::new(blocking_stream(
            self.store.clone(),
            repo.id,
            move |r, send| {
                let mut buf = Vec::with_capacity(batch);
                let mut gone = false;
                let truncated = read::list_tree(r, commit, &opts, |e| {
                    buf.push(to_pb_entry(e));
                    if buf.len() == batch {
                        let entries = std::mem::take(&mut buf);
                        if !send(pb::ListTreeResponse {
                            entries,
                            truncated: false,
                        }) {
                            gone = true;
                            return std::ops::ControlFlow::Break(());
                        }
                    }
                    std::ops::ControlFlow::Continue(())
                })?;
                if !gone && (!buf.is_empty() || truncated) {
                    send(pb::ListTreeResponse {
                        entries: buf,
                        truncated,
                    });
                }
                Ok(())
            },
        )))
    }

    async fn get_commit(
        &self,
        req: Request<pb::GetCommitRequest>,
    ) -> std::result::Result<Response<pb::GetCommitResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let info = self
            .store
            .read(repo.id, move |r| read::commit_info(r, commit))
            .await?;
        Ok(Response::new(pb::GetCommitResponse {
            commit: Some(to_pb_commit(info)),
        }))
    }

    type CommitLogStream = BoxStream<pb::CommitLogResponse>;

    async fn commit_log(
        &self,
        req: Request<pb::CommitLogRequest>,
    ) -> std::result::Result<Response<Self::CommitLogStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let opts = LogOptions {
            path: req.path,
            skip: req.skip as usize,
            limit: clamp_limit(req.limit, self.config.max_log_commits),
            first_parent: req.first_parent,
            before: req.before,
            after: req.after,
            hide: req
                .hide_oids
                .iter()
                .map(|h| oid(h))
                .collect::<std::result::Result<_, _>>()?,
        };
        let batch = self.config.log_batch.max(1);
        Ok(Response::new(blocking_stream(
            self.store.clone(),
            repo.id,
            move |r, send| {
                let mut buf = Vec::with_capacity(batch);
                let mut gone = false;
                let has_more = read::log_each(r, commit, &opts, |c| {
                    buf.push(to_pb_commit(c));
                    if buf.len() == batch {
                        let commits = std::mem::take(&mut buf);
                        if !send(pb::CommitLogResponse {
                            commits,
                            has_more: false,
                        }) {
                            gone = true;
                            return std::ops::ControlFlow::Break(());
                        }
                    }
                    std::ops::ControlFlow::Continue(())
                })?;
                if !gone && (!buf.is_empty() || has_more) {
                    send(pb::CommitLogResponse {
                        commits: buf,
                        has_more,
                    });
                }
                Ok(())
            },
        )))
    }

    type RecentContributionsStream = BoxStream<pb::RecentContributionsResponse>;

    async fn recent_contributions(
        &self,
        req: Request<pb::RecentContributionsRequest>,
    ) -> std::result::Result<Response<Self::RecentContributionsStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let since = req.since;
        let batch = self.config.log_batch.max(1);
        let store = self.store.clone();
        Ok(Response::new(blocking_stream(
            self.store.clone(),
            repo.id,
            move |r, send| {
                let Ok(head) = r.head_id() else {
                    return Ok(()); // empty repository
                };
                let Some((log, changes)) = store.change_log(repo.id, r)? else {
                    return Err(GitError::Corrupt(
                        "the commit log has not been built yet".to_owned(),
                    ));
                };
                let Some(start) = log.lookup(&head.detach()) else {
                    return Err(GitError::Corrupt(
                        "HEAD is not in the commit log yet".to_owned(),
                    ));
                };
                let mut buf = Vec::with_capacity(batch);
                // Newest first by committer time: stop at the first commit
                // committed before `since` (authored no later).
                for c in log.walk(start) {
                    if c.committer_time() < since {
                        break;
                    }
                    let author = c.author();
                    if c.is_merge() || author.time < since {
                        continue;
                    }
                    buf.push(pb::RecentContribution {
                        oid: c.oid().to_string(),
                        author_name: author.name().to_owned(),
                        author_email: author.email().to_owned(),
                        author_time: author.time,
                        paths: changes.changed_paths(c.pos()).map(str::to_owned).collect(),
                    });
                    if buf.len() == batch {
                        let commits = std::mem::take(&mut buf);
                        if !send(pb::RecentContributionsResponse { commits }) {
                            return Ok(());
                        }
                    }
                }
                if !buf.is_empty() {
                    send(pb::RecentContributionsResponse { commits: buf });
                }
                Ok(())
            },
        )))
    }

    type DiffStream = BoxStream<pb::DiffResponse>;

    async fn diff(
        &self,
        req: Request<pb::DiffRequest>,
    ) -> std::result::Result<Response<Self::DiffStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let new = oid(&req.new_commit_oid)?;
        let old = if req.old_commit_oid.is_empty() {
            None
        } else {
            Some(oid(&req.old_commit_oid)?)
        };
        let opts = DiffOptions {
            paths: req.paths,
            stats_only: req.stats_only,
            context_lines: if req.context_lines == 0 {
                3
            } else {
                req.context_lines.min(1000)
            },
            detect_renames: req.detect_renames,
            ..Default::default()
        };
        let batch = self.config.diff_batch.max(1);
        Ok(Response::new(blocking_stream(
            self.store.clone(),
            repo.id,
            move |r, send| {
                let mut buf = Vec::with_capacity(batch);
                let mut gone = false;
                diff::diff_commits(r, old, new, &opts, |f| {
                    buf.push(to_pb_file(f));
                    if buf.len() == batch
                        && !send(pb::DiffResponse {
                            files: std::mem::take(&mut buf),
                        })
                    {
                        gone = true;
                        return std::ops::ControlFlow::Break(());
                    }
                    std::ops::ControlFlow::Continue(())
                })?;
                if !gone && !buf.is_empty() {
                    send(pb::DiffResponse { files: buf });
                }
                Ok(())
            },
        )))
    }

    async fn compare(
        &self,
        req: Request<pb::CompareRequest>,
    ) -> std::result::Result<Response<pb::CompareResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let base = oid(&req.base_commit_oid)?;
        let head = oid(&req.head_commit_oid)?;
        let max = clamp_limit(req.max_commits, self.config.max_compare_commits);
        let c = self
            .store
            .read(repo.id, move |r| diff::compare(r, base, head, max))
            .await?;
        Ok(Response::new(pb::CompareResponse {
            merge_base_oid: hex(c.merge_base),
            commits: c.commits.into_iter().map(to_pb_commit).collect(),
            commits_truncated: c.commits_truncated,
            files: c.files.into_iter().map(to_pb_file).collect(),
        }))
    }

    async fn merge_base(
        &self,
        req: Request<pb::MergeBaseRequest>,
    ) -> std::result::Result<Response<pb::MergeBaseResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let (a, b) = (oid(&req.a)?, oid(&req.b)?);
        let mb = self
            .store
            .read(repo.id, move |r| crate::graph::merge_base(r, a, b))
            .await?;
        Ok(Response::new(pb::MergeBaseResponse { oid: hex(mb) }))
    }

    async fn is_ancestor(
        &self,
        req: Request<pb::IsAncestorRequest>,
    ) -> std::result::Result<Response<pb::IsAncestorResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let (a, d) = (oid(&req.ancestor)?, oid(&req.descendant)?);
        let is_ancestor = self
            .store
            .read(repo.id, move |r| crate::graph::is_ancestor(r, a, d))
            .await?;
        Ok(Response::new(pb::IsAncestorResponse { is_ancestor }))
    }

    type BlameStream = BoxStream<pb::BlameResponse>;

    async fn blame(
        &self,
        req: Request<pb::BlameRequest>,
    ) -> std::result::Result<Response<Self::BlameStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        // Validate up front so a missing repo/commit/path is a status, not a stream error.
        let path = req.path.clone();
        self.store
            .read(repo.id, move |r| {
                let e = read::stat(r, commit, &path)?;
                if !e.kind.is_blob() {
                    return Err(GitError::WrongEntryType {
                        path: e.path,
                        expected: "file",
                    });
                }
                Ok(())
            })
            .await?;
        let opts = BlameOptions {
            start_line: req.start_line,
            end_line: req.end_line,
            ignore_whitespace: req.ignore_whitespace,
            timeout: self.store.config().blame_timeout,
        };
        let hunks = blame_stream(
            self.store.git_bin(),
            &self.store.repo_dir(repo.id),
            commit,
            &req.path,
            &opts,
        )?;
        let batch = self.config.blame_batch.max(1);
        // Send whatever is ready (up to `batch`) as one message, so the first
        // hunks are not held back waiting for a full batch.
        let stream = hunks.ready_chunks(batch).map(|chunk| {
            let mut out = Vec::with_capacity(chunk.len());
            for h in chunk {
                out.push(to_pb_blame(h.map_err(Status::from)?));
            }
            Ok(pb::BlameResponse { hunks: out })
        });
        Ok(Response::new(Box::pin(stream)))
    }

    type ArchiveStream = BoxStream<pb::ArchiveResponse>;

    async fn archive(
        &self,
        req: Request<pb::ArchiveRequest>,
    ) -> std::result::Result<Response<Self::ArchiveStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let commit = oid(&req.commit_oid)?;
        let format = match pb::ArchiveFormat::try_from(req.format) {
            Ok(pb::ArchiveFormat::Zip) => ArchiveFormat::Zip,
            Ok(pb::ArchiveFormat::Tar) | Ok(pb::ArchiveFormat::Unspecified) => ArchiveFormat::Tar,
            Err(_) => return Err(GitError::InvalidArgument("unknown archive format".into()).into()),
        };
        let paths = req.paths.clone();
        self.store
            .read(repo.id, move |r| {
                read::find_commit(r, commit)?;
                for p in &paths {
                    read::stat(r, commit, p)?;
                }
                Ok(())
            })
            .await?;
        let chunks = archive_stream(
            self.store.git_bin(),
            &self.store.repo_dir(repo.id),
            commit,
            format,
            &req.paths,
            self.store.config().archive_timeout,
        )?;
        Ok(Response::new(Box::pin(chunks.map(|c| {
            c.map(|chunk| pb::ArchiveResponse { chunk })
                .map_err(Status::from)
        }))))
    }

    type UploadPackStream = BoxStream<pb::UploadPackResponse>;

    async fn upload_pack(
        &self,
        req: Request<pb::UploadPackRequest>,
    ) -> std::result::Result<Response<Self::UploadPackStream>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        if req.body.len() > crate::upload_pack::MAX_REQUEST {
            return Err(GitError::TooLarge {
                what: "upload-pack request".into(),
                limit: crate::upload_pack::MAX_REQUEST as u64,
            }
            .into());
        }
        // Fails with RepoNotFound when the repo is not cloned.
        self.store.read(repo.id, |_| Ok(())).await?;
        let chunks = crate::upload_pack::upload_pack_stream(
            self.store.git_bin(),
            &self.store.repo_dir(repo.id),
            req.advertise_refs,
            req.body,
            &req.git_protocol,
            self.store.config().archive_timeout,
        )?;
        Ok(Response::new(Box::pin(chunks.map(|c| {
            c.map(|chunk| pb::UploadPackResponse { chunk })
                .map_err(Status::from)
        }))))
    }

    type CopyRepoStream = BoxStream<pb::CopyRepoResponse>;

    async fn copy_repo(
        &self,
        req: Request<pb::CopyRepoRequest>,
    ) -> std::result::Result<Response<Self::CopyRepoStream>, Status> {
        use pb::copy_repo_response::Part;
        use sha2::Digest as _;

        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let haves = req
            .have
            .iter()
            .map(|h| oid(h))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let source = self.store.copy_out(repo.id, haves).await?;
        let header = pb::CopyRepoHeader {
            refs: source
                .header
                .refs
                .iter()
                .map(|(name, id)| pb::CopyRef {
                    name: name.clone(),
                    oid: id.to_string(),
                })
                .collect(),
            head_ref: source.header.head_ref.unwrap_or_default(),
            origin_url: source.header.origin_url.unwrap_or_default(),
            has_pack: source.pack.is_some(),
        };
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let msg = |part| pb::CopyRepoResponse { part: Some(part) };
            if tx.send(Ok(msg(Part::Header(header)))).await.is_err() {
                return;
            }
            let mut hasher = sha2::Sha256::new();
            let mut sent: u64 = 0;
            if let Some(mut pack) = source.pack {
                while let Some(chunk) = pack.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(Status::from(e))).await;
                            return;
                        }
                    };
                    hasher.update(&chunk);
                    sent += chunk.len() as u64;
                    if tx.send(Ok(msg(Part::Chunk(chunk)))).await.is_err() {
                        return;
                    }
                }
            }
            let trailer = pb::CopyRepoTrailer {
                sha256: hasher.finalize().to_vec(),
                pack_bytes: sent,
            };
            let _ = tx.send(Ok(msg(Part::Trailer(trailer)))).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn create_commit_from_patch(
        &self,
        req: Request<pb::CreateCommitFromPatchRequest>,
    ) -> std::result::Result<Response<pb::CreateCommitFromPatchResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        self.resolver.check_assigned(&repo).await?;
        let target_ref = crate::patch::normalize_ref(&req.target_ref)?;
        let commit = PatchCommit {
            base: oid(&req.base_commit_oid)?,
            target_ref: target_ref.clone(),
            patch: req.patch,
            message: req.message,
            author: identity(req.author)
                .ok_or_else(|| GitError::InvalidArgument("author is required".into()))?,
            committer: identity(req.committer),
            date: req.date,
            apply: apply_options(req.apply),
        };
        // Check the push target before creating anything.
        let push = match &req.push {
            Some(target) => Some((self.push_remote(&repo, target).await?, target.force)),
            None => None,
        };
        let created = self
            .store
            .create_commit_from_patch(repo.id, &commit)
            .await?;
        let pushed = if let Some((remote, force)) = push {
            let spec = PushSpec {
                remote,
                commit: created.commit,
                destination_ref: target_ref,
                force,
            };
            self.store
                .push(repo.id, &spec, self.config.allow_local_push)
                .await?;
            true
        } else {
            false
        };
        Ok(Response::new(pb::CreateCommitFromPatchResponse {
            commit_oid: created.commit.to_string(),
            tree_oid: created.tree.to_string(),
            pushed,
        }))
    }

    async fn apply_patch(
        &self,
        req: Request<pb::ApplyPatchRequest>,
    ) -> std::result::Result<Response<pb::ApplyPatchResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        let max_bytes = match req.max_content_bytes {
            0 => MAX_APPLY_CONTENT_BYTES,
            n => n.min(MAX_APPLY_CONTENT_BYTES),
        };
        let max_files = (req.max_files > 0).then_some(req.max_files as usize);
        let (tree, files) = self
            .store
            .changed_files(
                repo.id,
                oid(&req.base_commit_oid)?,
                &req.patch,
                &apply_options(req.apply),
                max_bytes,
                max_files,
            )
            .await?;
        Ok(Response::new(pb::ApplyPatchResponse {
            tree_oid: tree.to_string(),
            files: files.into_iter().map(to_pb_changed_file).collect(),
        }))
    }

    async fn push(
        &self,
        req: Request<pb::PushRequest>,
    ) -> std::result::Result<Response<pb::PushResponse>, Status> {
        let req = req.into_inner();
        let repo = repo_ref(req.repo)?;
        if req.source.is_empty() {
            return Err(GitError::InvalidArgument("source is required".into()).into());
        }
        let destination_ref = crate::patch::normalize_ref(&req.destination_ref)?;
        self.resolver.check_assigned(&repo).await?;
        let target = req.target.unwrap_or_default();
        let remote = self.push_remote(&repo, &target).await?;
        let source = req.source;
        let commit = self
            .store
            .read(repo.id, move |r| read::resolve_revision(r, &source))
            .await?;
        let spec = PushSpec {
            remote,
            commit,
            destination_ref,
            force: target.force,
        };
        self.store
            .push(repo.id, &spec, self.config.allow_local_push)
            .await?;
        Ok(Response::new(pb::PushResponse {
            commit_oid: commit.to_string(),
        }))
    }
}
