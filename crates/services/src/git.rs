//! Reading repository contents from the repo node that owns a repo.
//!
//! [`GitReader`] wraps the routed `sourcegull.repo.v1.RepoService` client for one repository and
//! maps gRPC statuses to [`GitCallError`] using the typed error kind the node attaches
//! ([`sourcegull_rpc::ErrorKind`]). It is shared by the HTTP endpoints (raw files,
//! archives, streaming blame), by the GraphQL resolvers, and by the bulk changes reconciler
//! for its two writes ([`GitReader::create_commit_from_patch`], [`GitReader::push`]), and
//! [`GitReader::apply_patch`] (a patch's changed files without committing).
//!
//! The module also holds the URL conventions of repository pages:
//! `/{repo}[@{rev}]/-/{kind}/{path}` ([`RepoUrl`]).

use std::sync::Arc;

use chrono::{FixedOffset, TimeZone, Utc};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::tonic::{Code, Status, Streaming};
use sourcegull_rpc::{ClientError, ErrorKind, RepoClients};
use sourcegull_types::{Repo, RepoId};

/// A failed read, by cause.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitCallError {
    /// The repository is not cloned on its repo node (yet).
    #[error("repository is not cloned yet")]
    RepoNotCloned,
    /// The first clone is running.
    #[error("repository is being cloned")]
    CloneInProgress,
    /// The revision does not resolve to a commit.
    #[error("revision not found: {0}")]
    RevisionNotFound(String),
    /// The path does not exist at the commit.
    #[error("path not found: {0}")]
    PathNotFound(String),
    /// The path has the wrong type (e.g. a directory where a file is needed).
    #[error("{0}")]
    WrongEntryType(String),
    /// A size or count limit was exceeded.
    #[error("{0}")]
    TooLarge(String),
    /// The request was malformed.
    #[error("{0}")]
    InvalidArgument(String),
    /// The patch does not apply to its base commit (git's output in the message).
    #[error("{0}")]
    PatchFailed(String),
    /// The remote rejected the push credentials.
    #[error("{0}")]
    Unauthenticated(String),
    /// The remote refused the push (no write access, protected branch, archived or
    /// missing project); bulk changes mark the changeset read-only.
    #[error("{0}")]
    PermissionDenied(String),
    /// The push is not a fast-forward and was not forced.
    #[error("{0}")]
    NonFastForward(String),
    /// The repo node is unreachable, overloaded, timed out or misconfigured.
    #[error("repo node unavailable: {0}")]
    Unavailable(String),
    /// Anything else (details are logged).
    #[error("internal error")]
    Internal(String),
}

impl GitCallError {
    /// Maps a gRPC status from a repo node.
    pub fn from_status(status: &Status) -> Self {
        let msg = status.message().to_owned();
        match ErrorKind::of(status) {
            Some(ErrorKind::RepoNotFound) => Self::RepoNotCloned,
            Some(ErrorKind::CloneInProgress) => Self::CloneInProgress,
            Some(ErrorKind::RevisionNotFound) => Self::RevisionNotFound(msg),
            Some(ErrorKind::PathNotFound) => Self::PathNotFound(msg),
            Some(ErrorKind::WrongEntryType) => Self::WrongEntryType(msg),
            Some(ErrorKind::TooLarge) => Self::TooLarge(msg),
            Some(ErrorKind::InvalidArgument) => Self::InvalidArgument(msg),
            Some(ErrorKind::Timeout | ErrorKind::Unavailable | ErrorKind::NotAssigned) => {
                Self::Unavailable(msg)
            }
            Some(ErrorKind::CommandFailed) => Self::Internal(msg),
            Some(ErrorKind::PatchFailed) => Self::PatchFailed(msg),
            Some(ErrorKind::Unauthenticated) => Self::Unauthenticated(msg),
            Some(ErrorKind::PermissionDenied) => Self::PermissionDenied(msg),
            Some(ErrorKind::NonFastForward) => Self::NonFastForward(msg),
            None => match status.code() {
                Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
                    Self::Unavailable(msg)
                }
                Code::InvalidArgument => Self::InvalidArgument(msg),
                _ => Self::Internal(format!("{:?}: {msg}", status.code())),
            },
        }
    }

    /// Whether this is a "does not exist" error (revision or path).
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::RevisionNotFound(_) | Self::PathNotFound(_))
    }
}

impl From<Status> for GitCallError {
    fn from(s: Status) -> Self {
        Self::from_status(&s)
    }
}

impl From<ClientError> for GitCallError {
    fn from(e: ClientError) -> Self {
        Self::Unavailable(e.to_string())
    }
}

/// Result of a [`GitReader`] call.
pub type GitResult<T> = Result<T, GitCallError>;

/// Access to one repository through its repo node: reads, plus the bulk changes writes.
#[derive(Clone)]
pub struct GitReader {
    clients: Arc<RepoClients>,
    repo: pb::RepoRef,
}

impl std::fmt::Debug for GitReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitReader")
            .field("repo", &self.repo.name)
            .finish()
    }
}

/// Options of [`GitReader::list_tree`].
#[derive(Debug, Clone, Default)]
pub struct TreeOptions {
    /// List the whole subtree instead of one level.
    pub recursive: bool,
    /// Include blob sizes.
    pub include_sizes: bool,
    /// Maximum entries (0 = the node's default).
    pub limit: u32,
}

impl GitReader {
    /// A reader for `repo`.
    pub fn new(clients: Arc<RepoClients>, repo: &Repo) -> Self {
        Self::from_parts(clients, repo.id, &repo.name)
    }

    /// A reader for the repo with `id` and `name`.
    pub fn from_parts(clients: Arc<RepoClients>, id: RepoId, name: &str) -> Self {
        Self {
            clients,
            repo: pb::RepoRef {
                id: id.get(),
                name: name.to_owned(),
            },
        }
    }

    /// The repo id.
    pub fn repo_id(&self) -> RepoId {
        RepoId(self.repo.id)
    }

    /// The repo name.
    pub fn repo_name(&self) -> &str {
        &self.repo.name
    }

    fn client(
        &self,
    ) -> GitResult<
        pb::repo_service_client::RepoServiceClient<sourcegull_rpc::tonic::transport::Channel>,
    > {
        Ok(self.clients.for_repo(self.repo_id())?)
    }

    fn r(&self) -> Option<pb::RepoRef> {
        Some(self.repo.clone())
    }

    /// Resolves `rev` (empty = HEAD) to a commit oid.
    pub async fn resolve_revision(&self, rev: &str) -> GitResult<String> {
        let resp = self
            .client()?
            .resolve_revision(pb::ResolveRevisionRequest {
                repo: self.r(),
                rev: rev.to_owned(),
            })
            .await?;
        Ok(resp.into_inner().commit_oid)
    }

    /// Lists branches and/or tags.
    pub async fn list_refs(
        &self,
        branches: bool,
        tags: bool,
        prefix: &str,
    ) -> GitResult<pb::ListRefsResponse> {
        let resp = self
            .client()?
            .list_refs(pb::ListRefsRequest {
                repo: self.r(),
                branches,
                tags,
                prefix: prefix.to_owned(),
            })
            .await?;
        Ok(resp.into_inner())
    }

    /// Metadata of `path` at `commit` (`""` is the root tree).
    pub async fn stat(&self, commit: &str, path: &str) -> GitResult<pb::TreeEntry> {
        let resp = self
            .client()?
            .stat(pb::StatRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                path: path.to_owned(),
            })
            .await?;
        resp.into_inner()
            .entry
            .ok_or_else(|| GitCallError::Internal("stat returned no entry".into()))
    }

    /// Reads a file, cut at `max_bytes` (0 = the node's default).
    pub async fn read_file(
        &self,
        commit: &str,
        path: &str,
        max_bytes: u64,
    ) -> GitResult<pb::ReadFileResponse> {
        let resp = self
            .client()?
            .read_file(pb::ReadFileRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                path: path.to_owned(),
                max_bytes,
            })
            .await?;
        Ok(resp.into_inner())
    }

    /// Lists a directory; returns the entries and whether the limit cut the listing short.
    pub async fn list_tree(
        &self,
        commit: &str,
        path: &str,
        opts: &TreeOptions,
    ) -> GitResult<(Vec<pb::TreeEntry>, bool)> {
        let mut stream = self
            .client()?
            .list_tree(pb::ListTreeRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                path: path.to_owned(),
                recursive: opts.recursive,
                include_sizes: opts.include_sizes,
                limit: opts.limit,
            })
            .await?
            .into_inner();
        let mut entries = Vec::new();
        let mut truncated = false;
        while let Some(msg) = stream.message().await? {
            entries.extend(msg.entries);
            truncated |= msg.truncated;
        }
        Ok((entries, truncated))
    }

    /// Loads one commit.
    pub async fn get_commit(&self, oid: &str) -> GitResult<pb::Commit> {
        let resp = self
            .client()?
            .get_commit(pb::GetCommitRequest {
                repo: self.r(),
                commit_oid: oid.to_owned(),
            })
            .await?;
        resp.into_inner()
            .commit
            .ok_or_else(|| GitCallError::Internal("GetCommit returned no commit".into()))
    }

    /// History of `commit` (optionally limited to `path`); returns the commits and whether
    /// more exist.
    pub async fn commit_log(
        &self,
        commit: &str,
        path: &str,
        skip: u32,
        limit: u32,
    ) -> GitResult<(Vec<pb::Commit>, bool)> {
        self.commit_log_hiding(commit, &[], path, skip, limit).await
    }

    /// Like [`commit_log`](Self::commit_log), leaving out `hide` and their ancestors
    /// (`commit ^hide…`).
    pub async fn commit_log_hiding(
        &self,
        commit: &str,
        hide: &[String],
        path: &str,
        skip: u32,
        limit: u32,
    ) -> GitResult<(Vec<pb::Commit>, bool)> {
        let mut stream = self
            .client()?
            .commit_log(pb::CommitLogRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                path: path.to_owned(),
                skip,
                limit,
                first_parent: false,
                hide_oids: hide.to_vec(),
                before: None,
                after: None,
            })
            .await?
            .into_inner();
        let mut commits = Vec::new();
        let mut has_more = false;
        while let Some(msg) = stream.message().await? {
            commits.extend(msg.commits);
            has_more |= msg.has_more;
        }
        Ok((commits, has_more))
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors.
    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> GitResult<bool> {
        let resp = self
            .client()?
            .is_ancestor(pb::IsAncestorRequest {
                repo: self.r(),
                ancestor: ancestor.to_owned(),
                descendant: descendant.to_owned(),
            })
            .await?;
        Ok(resp.into_inner().is_ancestor)
    }

    /// The best common ancestor of two commits, if they have one.
    pub async fn merge_base(&self, a: &str, b: &str) -> GitResult<Option<String>> {
        let resp = self
            .client()?
            .merge_base(pb::MergeBaseRequest {
                repo: self.r(),
                a: a.to_owned(),
                b: b.to_owned(),
            })
            .await?;
        let oid = resp.into_inner().oid;
        Ok((!oid.is_empty()).then_some(oid))
    }

    /// File diffs between two commits (`old` empty = first parent of `new`).
    pub async fn diff(
        &self,
        old: &str,
        new: &str,
        paths: Vec<String>,
        stats_only: bool,
    ) -> GitResult<Vec<pb::FileDiff>> {
        let mut stream = self
            .client()?
            .diff(pb::DiffRequest {
                repo: self.r(),
                old_commit_oid: old.to_owned(),
                new_commit_oid: new.to_owned(),
                paths,
                stats_only,
                context_lines: 3,
                detect_renames: true,
            })
            .await?
            .into_inner();
        let mut files = Vec::new();
        while let Some(msg) = stream.message().await? {
            files.extend(msg.files);
        }
        Ok(files)
    }

    /// Commits in `base..head` and the changed files of `merge-base..head`.
    pub async fn compare(
        &self,
        base: &str,
        head: &str,
        max_commits: u32,
    ) -> GitResult<pb::CompareResponse> {
        let resp = self
            .client()?
            .compare(pb::CompareRequest {
                repo: self.r(),
                base_commit_oid: base.to_owned(),
                head_commit_oid: head.to_owned(),
                max_commits,
            })
            .await?;
        Ok(resp.into_inner())
    }

    /// Streams blame hunks of `path` at `commit` (1-based inclusive lines; 0 = unbounded).
    pub async fn blame(
        &self,
        commit: &str,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> GitResult<Streaming<pb::BlameResponse>> {
        let resp = self
            .client()?
            .blame(pb::BlameRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                path: path.to_owned(),
                start_line,
                end_line,
                ignore_whitespace: false,
            })
            .await?;
        Ok(resp.into_inner())
    }

    /// Collects all blame hunks.
    pub async fn blame_all(
        &self,
        commit: &str,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> GitResult<Vec<pb::BlameHunk>> {
        let mut stream = self.blame(commit, path, start_line, end_line).await?;
        let mut hunks = Vec::new();
        while let Some(msg) = stream.message().await? {
            hunks.extend(msg.hunks);
        }
        hunks.sort_by_key(|h| h.start_line);
        Ok(hunks)
    }

    /// Streams an archive of `paths` (empty = whole tree) at `commit`.
    pub async fn archive(
        &self,
        commit: &str,
        format: pb::ArchiveFormat,
        paths: Vec<String>,
    ) -> GitResult<Streaming<pb::ArchiveResponse>> {
        let resp = self
            .client()?
            .archive(pb::ArchiveRequest {
                repo: self.r(),
                commit_oid: commit.to_owned(),
                format: format as i32,
                paths,
            })
            .await?;
        Ok(resp.into_inner())
    }

    /// One exchange of git's smart HTTP protocol: the ref advertisement, or the
    /// response to a negotiation `body`.
    pub async fn upload_pack(
        &self,
        advertise_refs: bool,
        body: bytes::Bytes,
        git_protocol: &str,
    ) -> GitResult<Streaming<pb::UploadPackResponse>> {
        let resp = self
            .client()?
            .upload_pack(pb::UploadPackRequest {
                repo: self.r(),
                advertise_refs,
                body,
                git_protocol: git_protocol.to_owned(),
            })
            .await?;
        Ok(resp.into_inner())
    }
}

impl GitReader {
    /// Applies a patch on top of `req.base_commit_oid`, commits it to `req.target_ref` and,
    /// with `req.push` set, pushes it there. `req.repo` is set to this repository.
    /// A patch that does not apply is [`GitCallError::PatchFailed`]; push rejections are
    /// [`GitCallError::Unauthenticated`], [`GitCallError::PermissionDenied`] and
    /// [`GitCallError::NonFastForward`].
    pub async fn create_commit_from_patch(
        &self,
        mut req: pb::CreateCommitFromPatchRequest,
    ) -> GitResult<pb::CreateCommitFromPatchResponse> {
        req.repo = self.r();
        Ok(self
            .client()?
            .create_commit_from_patch(req)
            .await?
            .into_inner())
    }

    /// Applies a patch on top of `req.base_commit_oid` without committing and returns the
    /// changed files with their new contents (commit-via-API mode). `req.repo` is set
    /// to this repository. A patch that does not apply is [`GitCallError::PatchFailed`];
    /// changes beyond the request's limits are [`GitCallError::TooLarge`].
    pub async fn apply_patch(
        &self,
        mut req: pb::ApplyPatchRequest,
    ) -> GitResult<pb::ApplyPatchResponse> {
        req.repo = self.r();
        Ok(self.client()?.apply_patch(req).await?.into_inner())
    }

    /// Pushes `source` (a ref or commit of this repository) to `destination_ref` on
    /// `target`'s remote (empty URL: the repository's own) with `target`'s credentials.
    /// Returns the pushed commit.
    pub async fn push(
        &self,
        source: &str,
        destination_ref: &str,
        target: pb::PushTarget,
    ) -> GitResult<String> {
        let resp = self
            .client()?
            .push(pb::PushRequest {
                repo: self.r(),
                source: source.to_owned(),
                destination_ref: destination_ref.to_owned(),
                target: Some(target),
            })
            .await?;
        Ok(resp.into_inner().commit_oid)
    }
}

/// Whether `s` is a full 40-character lowercase hex object id.
pub fn is_full_oid(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The first line of a commit message.
pub fn commit_subject(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

/// The message after the subject and the blank line(s) that follow it.
pub fn commit_body(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once('\n')?;
    let body = rest.trim_matches(|c| c == '\n' || c == '\r').trim_end();
    (!body.is_empty()).then_some(body)
}

/// Formats a git signature time as RFC 3339 in its own offset.
pub fn signature_date(time: i64, tz_offset_seconds: i32) -> String {
    let utc = Utc.timestamp_opt(time, 0).single().unwrap_or_default();
    match FixedOffset::east_opt(tz_offset_seconds) {
        Some(tz) => utc.with_timezone(&tz).to_rfc3339(),
        None => utc.to_rfc3339(),
    }
}

/// The page kinds under `/{repo}[@{rev}]/-/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoUrlKind {
    /// `/-/raw/{path}`: file contents or directory archives.
    Raw,
    /// `/-/tree/{path}`.
    Tree,
    /// `/-/blob/{path}`.
    Blob,
    /// `/-/commit/{oid}`.
    Commit,
    /// `/-/commits[/{path}]`.
    Commits,
    /// `/-/compare/{base}...{head}`.
    Compare,
    /// `/-/branches`.
    Branches,
    /// `/-/tags`.
    Tags,
    /// `/-/stream/{path}` (only below `/.api/blame/`).
    Stream,
}

impl RepoUrlKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "raw" => Self::Raw,
            "tree" => Self::Tree,
            "blob" => Self::Blob,
            "commit" => Self::Commit,
            "commits" => Self::Commits,
            "compare" => Self::Compare,
            "branches" => Self::Branches,
            "tags" => Self::Tags,
            "stream" => Self::Stream,
            _ => return None,
        })
    }
}

/// A parsed repository URL path: `/{repo}[@{rev}]/-/{kind}[/{rest}]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoUrl {
    /// The repository name (percent-decoded).
    pub repo: String,
    /// The revision after `@` (percent-decoded), if any.
    pub rev: Option<String>,
    /// The page kind.
    pub kind: RepoUrlKind,
    /// Everything after `/-/{kind}/` (percent-decoded, no leading or trailing slash).
    pub rest: String,
}

impl RepoUrl {
    /// Parses a URL path (with or without the leading slash). Returns `None` when the path has
    /// no `/-/{kind}` part or the repo name is empty.
    ///
    /// Repository names never contain a `-` path segment, so the first `/-/` separates the
    /// repository (and revision) from the page.
    pub fn parse(path: &str) -> Option<Self> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let (repo_rev, page) = path.split_once("/-/")?;
        let (kind, rest) = page.split_once('/').unwrap_or((page, ""));
        let kind = RepoUrlKind::parse(kind)?;
        let (repo, rev) = split_repo_rev(repo_rev);
        let repo = decode(repo);
        if repo.is_empty() {
            return None;
        }
        Some(Self {
            repo,
            rev: rev.map(decode).filter(|r| !r.is_empty()),
            kind,
            rest: decode(rest.trim_matches('/')),
        })
    }
}

/// Splits `repo@rev` at the first `@` (repository names contain no `@`).
pub fn split_repo_rev(s: &str) -> (&str, Option<&str>) {
    match s.split_once('@') {
        Some((repo, rev)) => (repo, Some(rev)),
        None => (s, None),
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Characters escaped in path segments of generated URLs (`/` is kept as the separator).
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Escapes a repo name, revision or file path for use in a URL path.
pub fn escape_path(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

/// `/{repo}[@{rev}]`.
pub fn repo_url(repo: &str, rev: Option<&str>) -> String {
    match rev {
        Some(rev) if !rev.is_empty() => format!("/{}@{}", escape_path(repo), escape_path(rev)),
        _ => format!("/{}", escape_path(repo)),
    }
}

/// `/{repo}[@{rev}]/-/{kind}[/{path}]`.
pub fn repo_page_url(repo: &str, rev: Option<&str>, kind: &str, path: &str) -> String {
    let base = repo_url(repo, rev);
    if path.is_empty() {
        format!("{base}/-/{kind}")
    } else {
        format!("{base}/-/{kind}/{}", escape_path(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_error_kinds_map_to_typed_errors() {
        let e = |k: ErrorKind| GitCallError::from_status(&k.status("m"));
        assert_eq!(
            e(ErrorKind::PatchFailed),
            GitCallError::PatchFailed("m".into())
        );
        assert_eq!(
            e(ErrorKind::Unauthenticated),
            GitCallError::Unauthenticated("m".into())
        );
        assert_eq!(
            e(ErrorKind::PermissionDenied),
            GitCallError::PermissionDenied("m".into())
        );
        assert_eq!(
            e(ErrorKind::NonFastForward),
            GitCallError::NonFastForward("m".into())
        );
        // A write that reached a node the repository moved away from: retried later.
        assert_eq!(
            e(ErrorKind::NotAssigned),
            GitCallError::Unavailable("m".into())
        );
    }

    #[test]
    fn parses_repo_urls() {
        let u = RepoUrl::parse("/gitlab.com/g/p@main/-/raw/src/a%20b.rs").unwrap();
        assert_eq!(u.repo, "gitlab.com/g/p");
        assert_eq!(u.rev.as_deref(), Some("main"));
        assert_eq!(u.kind, RepoUrlKind::Raw);
        assert_eq!(u.rest, "src/a b.rs");

        let u = RepoUrl::parse("gitlab.com/g/p@feature/x/-/blob/README.md").unwrap();
        assert_eq!(u.rev.as_deref(), Some("feature/x"));
        assert_eq!(u.kind, RepoUrlKind::Blob);

        let u = RepoUrl::parse("/gitlab.com/g/p/-/raw").unwrap();
        assert_eq!((u.rev, u.rest.as_str()), (None, ""));
        let u = RepoUrl::parse("/gitlab.com/g/p/-/raw/dir/").unwrap();
        assert_eq!(u.rest, "dir");
        // Only the first `/-/` separates; later ones are part of the path.
        let u = RepoUrl::parse("/r/-/blob/a/-/b").unwrap();
        assert_eq!(u.rest, "a/-/b");

        assert_eq!(RepoUrl::parse("/gitlab.com/g/p"), None);
        assert_eq!(RepoUrl::parse("/gitlab.com/g/p/-/nope/x"), None);
        assert_eq!(RepoUrl::parse("/-/raw/x"), None);
        assert_eq!(RepoUrl::parse("/@main/-/raw/x"), None);
    }

    #[test]
    fn builds_urls() {
        assert_eq!(repo_url("g.com/a/b", None), "/g.com/a/b");
        assert_eq!(repo_url("g.com/a/b", Some("v1 x")), "/g.com/a/b@v1%20x");
        assert_eq!(
            repo_page_url("r", Some("main"), "blob", "a#b/c?.rs"),
            "/r@main/-/blob/a%23b/c%3F.rs"
        );
        assert_eq!(repo_page_url("r", None, "commits", ""), "/r/-/commits");
    }

    #[test]
    fn commit_message_parts() {
        assert_eq!(commit_subject("fix: a\n\nbody\nmore\n"), "fix: a");
        assert_eq!(commit_body("fix: a\n\nbody\nmore\n"), Some("body\nmore"));
        assert_eq!(commit_body("fix: a\n"), None);
        assert_eq!(commit_body("fix: a"), None);
        assert!(is_full_oid("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_oid("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_full_oid("main"));
    }
}
