//! Code browsing: commits, trees, blobs, highlighting, blame,
//! refs and comparisons, read from the repo node that owns the repository.
//!
//! Types: `GitCommit`, `GitTree`, `GitBlob`, `TreeEntry`, `GitRef`, `RepositoryComparison`,
//! `FileDiff`, `Hunk`, …. Notes:
//!
//! - `GitBlob.highlight` returns the compact occurrence format of `sourcegull-highlight`
//!   (`packed`: four `u32` per range, `[lineDelta, startCol, length, kind]` with UTF-16
//!   columns) instead of SCIP JSON; `html` is still available.
//! - `GitCommit.ancestors` pages with an opaque cursor (`afterCursor`, or `after` as an alias);
//!   date filters are not supported.
//!
//! Each GraphQL request carries a [`RequestCache`] so that a commit referenced many times
//! (blame hunks, parents) is fetched once. Highlighting results are cached process-wide by
//! blob OID, language and [`sourcegull_highlight::VERSION`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_graphql::parser::types::Field;
use async_graphql::registry::Registry;
use async_graphql::{
    Context, ContextSelectionSet, Enum, ID, Interface, Object, OutputType, Positioned, Result,
    ServerResult, SimpleObject, Value,
};
use sourcegull_highlight::{Engine, Highlighted, PlainReason};
use sourcegull_own_graphql::ownership::{self, OwnedPath};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::git::{
    GitCallError, GitReader, TreeOptions, commit_body, commit_subject, repo_page_url, repo_url,
    signature_date,
};
use sourcegull_services::relay::marshal_string_id;
use sourcegull_types::Repo;
use tokio::sync::OnceCell;

use crate::context::{codes, error, services};
use crate::file2::File2;
use crate::pagination::{PageInfo, decode_cursor, encode_cursor};
use crate::repos::RepositoryNode;

pub use sourcegull_graphql_core::git::BLOB_CONTENT_MAX_BYTES;
/// Default and maximum commits per `ancestors` page.
pub const MAX_ANCESTORS: i64 = 1000;
/// Default and maximum commits in `RepositoryComparison.commits`.
pub const MAX_COMPARE_COMMITS: u32 = 250;
/// Default and maximum refs per page.
pub const MAX_REFS: i64 = 1000;
/// Default and maximum file diffs per page.
pub const MAX_FILE_DIFFS: i64 = 1000;

pub use sourcegull_graphql_core::git::git_error;

/// `Ok(None)` for "does not exist" errors, the error otherwise.
fn none_if_not_found<T>(r: Result<T, GitCallError>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_not_found() || matches!(e, GitCallError::WrongEntryType(_)) => Ok(None),
        Err(e) => Err(git_error(e)),
    }
}

/// Per-request memo of loaded commits (request data, installed by [`crate::execute`]).
#[derive(Debug, Default)]
pub struct RequestCache {
    commits: Mutex<HashMap<(i32, String), Arc<pb::Commit>>>,
}

/// A repository plus its reader; cheap to clone.
#[derive(Clone)]
pub struct RepoHandle {
    pub repo: Arc<Repo>,
    pub git: GitReader,
}

impl RepoHandle {
    /// A handle for `repo` using the schema's repo node clients.
    pub fn new(ctx: &Context<'_>, repo: &Repo) -> Result<Self> {
        let clients = services(ctx)?.repo_clients.clone();
        Ok(Self {
            git: GitReader::new(clients, repo),
            repo: Arc::new(repo.clone()),
        })
    }

    fn name(&self) -> &str {
        &self.repo.name
    }

    fn node(&self) -> RepositoryNode {
        RepositoryNode {
            repo: (*self.repo).clone(),
            gitserver: None,
        }
    }

    /// Loads a commit, memoized per request.
    pub async fn commit(&self, ctx: &Context<'_>, oid: &str) -> Result<Arc<pb::Commit>> {
        let key = (self.repo.id.get(), oid.to_owned());
        let cache = ctx.data_opt::<RequestCache>();
        if let Some(c) = cache.and_then(|c| {
            c.commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .cloned()
        }) {
            return Ok(c);
        }
        let commit = Arc::new(self.git.get_commit(oid).await.map_err(git_error)?);
        if let Some(c) = cache {
            c.commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, commit.clone());
        }
        Ok(commit)
    }

    /// Resolves `rev` to a commit; `None` when it does not exist.
    pub async fn resolve_commit(
        &self,
        ctx: &Context<'_>,
        rev: &str,
    ) -> Result<Option<GitCommitNode>> {
        let Some(oid) = none_if_not_found(self.git.resolve_revision(rev).await)? else {
            return Ok(None);
        };
        let commit = self.commit(ctx, &oid).await?;
        let input = (!rev.is_empty() && rev != oid).then(|| rev.to_owned());
        Ok(Some(GitCommitNode::new(self.clone(), commit, input)))
    }
}

/// Abbreviates an oid to 7 characters.
fn abbrev(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Converts a count to a GraphQL `Int`, saturating.
fn int(n: impl TryInto<i32>) -> i32 {
    n.try_into().unwrap_or(i32::MAX)
}

// ------------------------------------------------------------------ commits

/// A person in a signature, or an owner.
#[derive(SimpleObject, Clone)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub display_name: String,
    #[graphql(name = "avatarURL")]
    pub avatar_url: Option<String>,
    /// The user account of this person, when known (owners resolved to users).
    pub user: Option<crate::users::UserNode>,
}

/// An author or committer signature.
#[derive(SimpleObject, Clone)]
pub struct Signature {
    pub person: Person,
    /// RFC 3339 with the signature's offset.
    pub date: String,
}

fn signature(s: Option<&pb::Signature>) -> Signature {
    let s = s.cloned().unwrap_or_default();
    Signature {
        date: signature_date(s.time, s.tz_offset_seconds),
        person: Person {
            display_name: s.name.clone(),
            name: s.name,
            email: s.email,
            avatar_url: None,
            user: None,
        },
    }
}

/// A commit.
#[derive(Clone)]
pub struct GitCommitNode {
    h: RepoHandle,
    c: Arc<pb::Commit>,
    /// The revision the commit was looked up by (used in tree/blob URLs).
    input_rev: Option<String>,
}

impl GitCommitNode {
    /// Wraps a loaded commit.
    pub fn new(h: RepoHandle, c: Arc<pb::Commit>, input_rev: Option<String>) -> Self {
        Self { h, c, input_rev }
    }

    fn url_rev(&self) -> &str {
        self.input_rev.as_deref().unwrap_or(&self.c.oid)
    }

    fn entry(&self, path: String, e: Option<&pb::TreeEntry>) -> EntryBase {
        EntryBase {
            h: self.h.clone(),
            commit_oid: self.c.oid.clone(),
            rev: self.input_rev.clone(),
            path,
            oid: e.map(|e| e.oid.clone()),
            kind: e.map_or(pb::EntryKind::Directory as i32, |e| e.kind),
            size: e.and_then(|e| e.size),
        }
    }

    async fn find_blob(&self, path: String) -> Result<Option<GitBlobNode>> {
        let path = path.trim_matches('/').to_owned();
        Ok(self.stat(&path).await?.and_then(|e| {
            is_blob_kind(e.kind).then(|| GitBlobNode::new(self.entry(path, Some(&e))))
        }))
    }

    async fn stat(&self, path: &str) -> Result<Option<pb::TreeEntry>> {
        none_if_not_found(self.h.git.stat(&self.c.oid, path).await)
    }
}

/// A page of commits.
#[derive(SimpleObject)]
pub struct GitCommitConnection {
    pub nodes: Vec<GitCommitNode>,
    pub page_info: PageInfo,
    /// Set when the server-side limit cut the list (comparisons).
    pub truncated: bool,
}

#[Object(name = "GitCommit")]
impl GitCommitNode {
    /// An opaque ID.
    async fn id(&self) -> ID {
        ID(marshal_string_id(
            "GitCommit",
            &format!("{}:{}", self.h.repo.id.get(), self.c.oid),
        ))
    }

    /// The full commit oid.
    async fn oid(&self) -> &str {
        &self.c.oid
    }

    /// The first 7 characters of the oid.
    #[graphql(name = "abbreviatedOID")]
    async fn abbreviated_oid(&self) -> &str {
        abbrev(&self.c.oid)
    }

    /// The full message.
    async fn message(&self) -> &str {
        &self.c.message
    }

    /// The first line of the message.
    async fn subject(&self) -> &str {
        commit_subject(&self.c.message)
    }

    /// The message after the subject line, if any.
    async fn body(&self) -> Option<&str> {
        commit_body(&self.c.message)
    }

    /// The author.
    async fn author(&self) -> Signature {
        signature(self.c.author.as_ref())
    }

    /// The committer.
    async fn committer(&self) -> Option<Signature> {
        Some(signature(self.c.committer.as_ref()))
    }

    /// Parent commits.
    async fn parents(&self, ctx: &Context<'_>) -> Result<Vec<GitCommitNode>> {
        let mut out = Vec::with_capacity(self.c.parents.len());
        for p in &self.c.parents {
            let c = self.h.commit(ctx, p).await?;
            out.push(GitCommitNode::new(self.h.clone(), c, None));
        }
        Ok(out)
    }

    /// The commit page.
    async fn url(&self) -> String {
        repo_page_url(self.h.name(), None, "commit", &self.c.oid)
    }

    /// The commit page (same as `url`).
    #[graphql(name = "canonicalURL")]
    async fn canonical_url(&self) -> String {
        repo_page_url(self.h.name(), None, "commit", &self.c.oid)
    }

    /// The repository.
    async fn repository(&self) -> RepositoryNode {
        self.h.node()
    }

    /// The tree at `path` (root by default); null if it does not exist or is not a directory.
    async fn tree(
        &self,
        #[graphql(default)] path: String,
        #[graphql(default, desc = "Ignored; pass `recursive` to `entries`.")] recursive: bool,
    ) -> Result<Option<GitTreeNode>> {
        let _ = recursive;
        let path = path.trim_matches('/').to_owned();
        if path.is_empty() {
            return Ok(Some(GitTreeNode(self.entry(path, None))));
        }
        Ok(self.stat(&path).await?.and_then(|e| {
            (e.kind == pb::EntryKind::Directory as i32)
                .then(|| GitTreeNode(self.entry(path, Some(&e))))
        }))
    }

    /// The repository's owners at this commit.
    async fn ownership(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<Ownerships> {
        let root = self.entry(String::new(), None);
        ownership::ownership(ctx, &root.owned_path(), true, first, after, None)
            .await
            .map(Ownerships)
    }

    /// The file at `path`; null if it does not exist or is a directory.
    async fn blob(&self, path: String) -> Result<Option<GitBlobNode>> {
        self.find_blob(path).await
    }

    /// Alias of `blob` (`File2`).
    async fn file(&self, path: String) -> Result<Option<GitBlobNode>> {
        self.find_blob(path).await
    }

    /// The tree or file at `path`.
    async fn path(&self, path: String) -> Result<Option<TreeEntry>> {
        let path = path.trim_matches('/').to_owned();
        if path.is_empty() {
            return Ok(Some(TreeEntry::GitTree(GitTreeNode(
                self.entry(path, None),
            ))));
        }
        Ok(self
            .stat(&path)
            .await?
            .map(|e| tree_entry(self.entry(path, Some(&e)))))
    }

    /// History: this commit and its ancestors, newest first, optionally limited to the
    /// commits that changed `path`.
    async fn ancestors(
        &self,
        first: Option<i32>,
        path: Option<String>,
        #[graphql(desc = "Cursor from `pageInfo.endCursor`.")] after_cursor: Option<String>,
        #[graphql(desc = "Alias of `afterCursor`.")] after: Option<String>,
    ) -> Result<GitCommitConnection> {
        let limit = first.map_or(50, |n| i64::from(n).clamp(1, MAX_ANCESTORS));
        let offset = decode_cursor(after_cursor.as_deref().or(after.as_deref()))?;
        let path = path.unwrap_or_default();
        let (commits, has_more) = self
            .h
            .git
            .commit_log(
                &self.c.oid,
                path.trim_matches('/'),
                u32::try_from(offset).unwrap_or(u32::MAX),
                u32::try_from(limit).unwrap_or(u32::MAX),
            )
            .await
            .map_err(git_error)?;
        Ok(GitCommitConnection {
            nodes: commits
                .into_iter()
                .map(|c| GitCommitNode::new(self.h.clone(), Arc::new(c), None))
                .collect(),
            page_info: PageInfo {
                has_next_page: has_more,
                end_cursor: has_more.then(|| encode_cursor(offset + limit)),
            },
            truncated: false,
        })
    }

    /// The changes of this commit against `base` (default: its first parent).
    async fn diff(
        &self,
        ctx: &Context<'_>,
        base: Option<String>,
    ) -> Result<RepositoryComparisonNode> {
        let (base_oid, base_spec) = match base.filter(|b| !b.is_empty()) {
            Some(b) => {
                let oid = self.h.git.resolve_revision(&b).await.map_err(git_error)?;
                (Some(oid), b)
            }
            None => {
                let p = self.c.parents.first().cloned();
                let spec = p.clone().unwrap_or_default();
                (p, spec)
            }
        };
        let _ = ctx;
        Ok(RepositoryComparisonNode::new(
            self.h.clone(),
            base_spec,
            base_oid,
            self.url_rev().to_owned(),
            self.c.oid.clone(),
            false,
        ))
    }
}

// ------------------------------------------------------------------ trees and blobs

fn is_blob_kind(kind: i32) -> bool {
    matches!(
        pb::EntryKind::try_from(kind),
        Ok(pb::EntryKind::File | pb::EntryKind::Executable | pb::EntryKind::Symlink)
    )
}

/// What a tree entry is.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum TreeEntryKind {
    File,
    Executable,
    Symlink,
    Directory,
    Submodule,
}

fn entry_kind(kind: i32) -> TreeEntryKind {
    match pb::EntryKind::try_from(kind) {
        Ok(pb::EntryKind::Executable) => TreeEntryKind::Executable,
        Ok(pb::EntryKind::Symlink) => TreeEntryKind::Symlink,
        Ok(pb::EntryKind::Directory) => TreeEntryKind::Directory,
        Ok(pb::EntryKind::Submodule) => TreeEntryKind::Submodule,
        _ => TreeEntryKind::File,
    }
}

/// Shared data of trees and blobs.
#[derive(Clone)]
pub struct EntryBase {
    h: RepoHandle,
    commit_oid: String,
    rev: Option<String>,
    path: String,
    /// Object id, when known.
    oid: Option<String>,
    kind: i32,
    size: Option<u64>,
}

impl EntryBase {
    fn name(&self) -> String {
        self.path.rsplit('/').next().unwrap_or("").to_owned()
    }

    fn url(&self, kind: &str) -> String {
        repo_page_url(
            self.h.name(),
            Some(self.rev.as_deref().unwrap_or(&self.commit_oid)),
            kind,
            &self.path,
        )
    }

    fn canonical_url(&self, kind: &str) -> String {
        repo_page_url(self.h.name(), Some(&self.commit_oid), kind, &self.path)
    }

    async fn commit(&self, ctx: &Context<'_>) -> Result<GitCommitNode> {
        let c = self.h.commit(ctx, &self.commit_oid).await?;
        Ok(GitCommitNode::new(self.h.clone(), c, self.rev.clone()))
    }

    /// The entry as the owners of a path are computed for.
    fn owned_path(&self) -> OwnedPath<'_> {
        OwnedPath {
            repo: &self.h.repo,
            git: &self.h.git,
            commit_oid: &self.commit_oid,
            rev: self.rev.as_deref(),
            path: &self.path,
        }
    }
}

/// The `OwnershipConnection` of the `ownership` fields. Registering it registers the `File2`
/// interface first, before `TreeEntry` and `Ownable`, so `GitBlob` lists `File2` as its first
/// interface.
pub struct Ownerships(ownership::OwnershipConnection);

impl OutputType for Ownerships {
    fn type_name() -> Cow<'static, str> {
        <ownership::OwnershipConnection as OutputType>::type_name()
    }

    fn create_type_info(registry: &mut Registry) -> String {
        <File2 as OutputType>::create_type_info(registry);
        <ownership::OwnershipConnection as OutputType>::create_type_info(registry)
    }

    async fn resolve(
        &self,
        ctx: &ContextSelectionSet<'_>,
        field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        OutputType::resolve(&self.0, ctx, field).await
    }
}

/// A resource with ownership information (`Ownable`; files only).
// The field's arguments repeat `ty = ...`, which clippy mistakes for duplicate attributes.
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(field(
    name = "ownership",
    ty = "Ownerships",
    desc = "Ownership summary for this entity.",
    arg(name = "first", ty = "Option<i32>"),
    arg(name = "after", ty = "Option<String>"),
    arg(name = "reasons", ty = "Option<Vec<ownership::OwnershipReasonType>>")
))]
pub enum Ownable {
    GitBlob(GitBlobNode),
}

/// A file or directory in a tree.
// The interface fields repeat `ty = "String"`, which clippy mistakes for duplicate attributes.
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(
    field(name = "path", ty = "String", desc = "Path from the repository root."),
    field(name = "name", ty = "String", desc = "The last path component."),
    field(
        name = "is_directory",
        ty = "bool",
        desc = "Whether this is a directory."
    ),
    field(
        name = "url",
        ty = "String",
        desc = "The page of the entry at the input revision."
    ),
    field(
        name = "canonicalURL",
        method = "canonical_url",
        ty = "String",
        desc = "The page of the entry at the commit oid."
    ),
    field(name = "kind", ty = "TreeEntryKind", desc = "What the entry is.")
)]
pub enum TreeEntry {
    /// A directory.
    GitTree(GitTreeNode),
    /// A file, symlink or submodule.
    GitBlob(GitBlobNode),
}

fn tree_entry(base: EntryBase) -> TreeEntry {
    if base.kind == pb::EntryKind::Directory as i32 {
        TreeEntry::GitTree(GitTreeNode(base))
    } else {
        TreeEntry::GitBlob(GitBlobNode::new(base))
    }
}

/// A directory at a commit.
#[derive(Clone)]
pub struct GitTreeNode(EntryBase);

#[Object(name = "GitTree")]
impl GitTreeNode {
    /// Path from the repository root (`""` for the root).
    async fn path(&self) -> String {
        self.0.path.clone()
    }

    /// The last path component.
    async fn name(&self) -> String {
        self.0.name()
    }

    /// Always true.
    async fn is_directory(&self) -> bool {
        true
    }

    /// Whether this is the root tree.
    async fn is_root(&self) -> bool {
        self.0.path.is_empty()
    }

    /// What the entry is (always `DIRECTORY`).
    async fn kind(&self) -> TreeEntryKind {
        TreeEntryKind::Directory
    }

    /// The tree page at the input revision.
    async fn url(&self) -> String {
        self.0.url("tree")
    }

    /// The tree page at the commit oid.
    #[graphql(name = "canonicalURL")]
    async fn canonical_url(&self) -> String {
        self.0.canonical_url("tree")
    }

    /// The commit.
    async fn commit(&self, ctx: &Context<'_>) -> Result<GitCommitNode> {
        self.0.commit(ctx).await
    }

    /// The repository.
    async fn repository(&self) -> RepositoryNode {
        self.0.h.node()
    }

    /// Entries (directories first, then by name); `recursive` lists the whole subtree.
    async fn entries(
        &self,
        first: Option<i32>,
        #[graphql(default)] recursive: bool,
    ) -> Result<Vec<TreeEntry>> {
        let opts = TreeOptions {
            recursive,
            include_sizes: false,
            limit: first.map_or(0, |n| u32::try_from(n.max(1)).unwrap_or(1)),
        };
        let (mut entries, _) = self
            .0
            .h
            .git
            .list_tree(&self.0.commit_oid, &self.0.path, &opts)
            .await
            .map_err(git_error)?;
        if !recursive {
            entries.sort_by(|a, b| {
                let dir = |e: &pb::TreeEntry| e.kind != pb::EntryKind::Directory as i32;
                dir(a).cmp(&dir(b)).then_with(|| a.name.cmp(&b.name))
            });
        }
        Ok(entries
            .into_iter()
            .map(|e| {
                tree_entry(EntryBase {
                    h: self.0.h.clone(),
                    commit_oid: self.0.commit_oid.clone(),
                    rev: self.0.rev.clone(),
                    path: e.path,
                    oid: Some(e.oid),
                    kind: e.kind,
                    size: e.size,
                })
            })
            .collect())
    }

    /// The directory's owners: CODEOWNERS entries that own it and owners
    /// assigned to it or a parent, strongest first.
    async fn ownership(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<Ownerships> {
        ownership::ownership(ctx, &self.0.owned_path(), true, first, after, None)
            .await
            .map(Ownerships)
    }

    /// Ownership coverage of this directory on the default branch, computed in the
    /// background when the `analytics` signal is enabled.
    async fn ownership_stats(&self, ctx: &Context<'_>) -> Result<ownership::OwnershipStats> {
        ownership::tree_stats(ctx, self.0.h.repo.id, &self.0.path).await
    }

    /// An archive of this directory (`format`: zip or tar).
    #[graphql(name = "archiveURL")]
    async fn archive_url(
        &self,
        #[graphql(default_with = "\"zip\".to_owned()")] format: String,
    ) -> String {
        format!(
            "{}?format={}",
            repo_page_url(
                self.0.h.name(),
                Some(&self.0.commit_oid),
                "raw",
                &self.0.path
            ),
            if format.eq_ignore_ascii_case("tar") {
                "tar"
            } else {
                "zip"
            }
        )
    }
}

/// A file at a commit.
#[derive(Clone)]
pub struct GitBlobNode {
    base: EntryBase,
    file: Arc<OnceCell<Arc<pb::ReadFileResponse>>>,
}

impl GitBlobNode {
    fn new(base: EntryBase) -> Self {
        Self {
            base,
            file: Arc::new(OnceCell::new()),
        }
    }

    /// The file at `path` in commit `commit_oid` (not checked to exist), reached by `rev`
    /// (the commit when unset).
    pub(crate) fn at(h: RepoHandle, commit_oid: &str, rev: Option<&str>, path: &str) -> Self {
        Self::new(EntryBase {
            h,
            commit_oid: commit_oid.to_owned(),
            rev: rev.map(str::to_owned),
            path: path.to_owned(),
            oid: None,
            kind: pb::EntryKind::File as i32,
            size: None,
        })
    }

    async fn read(&self) -> Result<Arc<pb::ReadFileResponse>> {
        self.file
            .get_or_try_init(|| async {
                self.base
                    .h
                    .git
                    .read_file(
                        &self.base.commit_oid,
                        &self.base.path,
                        BLOB_CONTENT_MAX_BYTES,
                    )
                    .await
                    .map(Arc::new)
                    .map_err(git_error)
            })
            .await
            .cloned()
    }
}

/// Highlights `file` at `path` through the shared cache.
async fn highlight_cached(
    path: &str,
    file: Arc<pb::ReadFileResponse>,
    disable_timeout: bool,
) -> Result<Arc<Highlighted>> {
    sourcegull_codenav::highlight::highlight_cached(path, file, disable_timeout)
        .await
        .map_err(|e| error(codes::INTERNAL, format!("highlighting failed: {e}")))
}

/// Requested highlighting output (accepted for compatibility; the compact form is always
/// available in `packed`).
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum HighlightResponseFormat {
    HtmlHighlight,
    HtmlPlaintext,
    JsonScip,
}

/// A highlighted file.
pub struct HighlightedFile {
    h: Arc<Highlighted>,
    file: Arc<pb::ReadFileResponse>,
}

#[Object]
impl HighlightedFile {
    /// Whether highlighting was skipped because of a limit (size, lines, timeout).
    async fn aborted(&self) -> bool {
        matches!(
            self.h.plain_reason,
            Some(PlainReason::TooLarge | PlainReason::TooManyLines | PlainReason::Timeout)
        )
    }

    /// The compact occurrences: four integers per range, `[lineDelta, startCol, length, kind]`;
    /// `startCol` is relative to the previous range's start when on the same line. Columns
    /// count UTF-16 code units. Kinds are `sourcegull_highlight::SyntaxKind` values.
    async fn packed(&self) -> Vec<i32> {
        self.h
            .packed()
            .into_iter()
            .map(|v| i32::try_from(v).unwrap_or(i32::MAX))
            .collect()
    }

    /// The highlighter version (part of cache keys).
    async fn version(&self) -> i32 {
        int(sourcegull_highlight::VERSION)
    }

    /// The detected language id.
    async fn language(&self) -> Option<&str> {
        self.h.language.as_deref()
    }

    /// `treeSitter`, `syntect` or `plainText`.
    async fn engine(&self) -> &'static str {
        match self.h.engine {
            Engine::TreeSitter => "treeSitter",
            Engine::Syntect => "syntect",
            Engine::PlainText => "plainText",
        }
    }

    /// Why the file is plain text, if it is.
    async fn plain_reason(&self) -> Option<&'static str> {
        self.h.plain_reason.map(|r| match r {
            PlainReason::TooLarge => "tooLarge",
            PlainReason::TooManyLines => "tooManyLines",
            PlainReason::Timeout => "timeout",
            PlainReason::Binary => "binary",
            PlainReason::NoGrammar => "noGrammar",
            PlainReason::Failed => "failed",
        })
    }

    /// The file as an HTML table with `hl-*` classes (for API compatibility).
    async fn html(&self) -> String {
        sourcegull_highlight::to_html(
            &String::from_utf8_lossy(&self.file.content),
            &self.h.occurrences,
        )
    }
}

/// A blame hunk.
pub struct HunkNode {
    h: RepoHandle,
    hunk: pb::BlameHunk,
    path: String,
}

#[Object(name = "Hunk")]
impl HunkNode {
    /// First line (1-based).
    async fn start_line(&self) -> i32 {
        int(self.hunk.start_line)
    }

    /// Line after the last one (exclusive).
    async fn end_line(&self) -> i32 {
        int(self.hunk.start_line + self.hunk.line_count)
    }

    /// The commit oid that last changed the lines.
    async fn rev(&self) -> &str {
        &self.hunk.commit_oid
    }

    /// The commit's author.
    async fn author(&self) -> Signature {
        signature(self.hunk.author.as_ref())
    }

    /// The commit subject.
    async fn message(&self) -> &str {
        &self.hunk.summary
    }

    /// The path in that commit.
    async fn filename(&self) -> &str {
        if self.hunk.orig_path.is_empty() {
            &self.path
        } else {
            &self.hunk.orig_path
        }
    }

    /// The commit.
    async fn commit(&self, ctx: &Context<'_>) -> Result<GitCommitNode> {
        let c = self.h.commit(ctx, &self.hunk.commit_oid).await?;
        Ok(GitCommitNode::new(self.h.clone(), c, None))
    }
}

#[Object(name = "GitBlob")]
impl GitBlobNode {
    /// Path from the repository root.
    pub(crate) async fn path(&self) -> String {
        self.base.path.clone()
    }

    /// The file name.
    pub(crate) async fn name(&self) -> String {
        self.base.name()
    }

    /// Always false.
    pub(crate) async fn is_directory(&self) -> bool {
        false
    }

    /// What the entry is.
    async fn kind(&self) -> TreeEntryKind {
        entry_kind(self.base.kind)
    }

    /// The file page at the input revision.
    pub(crate) async fn url(&self) -> String {
        self.base.url("blob")
    }

    /// The file page at the commit oid (a permalink).
    #[graphql(name = "canonicalURL")]
    pub(crate) async fn canonical_url(&self) -> String {
        self.base.canonical_url("blob")
    }

    /// The raw file at the commit oid.
    #[graphql(name = "rawURL")]
    async fn raw_url(&self) -> String {
        repo_page_url(
            self.base.h.name(),
            Some(&self.base.commit_oid),
            "raw",
            &self.base.path,
        )
    }

    /// The commit.
    async fn commit(&self, ctx: &Context<'_>) -> Result<GitCommitNode> {
        self.base.commit(ctx).await
    }

    /// The repository.
    async fn repository(&self) -> RepositoryNode {
        self.base.h.node()
    }

    /// Blob object id.
    async fn oid(&self) -> Result<String> {
        match &self.base.oid {
            Some(o) => Ok(o.clone()),
            None => Ok(self.read().await?.oid.clone()),
        }
    }

    /// Size in bytes.
    async fn byte_size(&self) -> Result<i32> {
        match self.base.size {
            Some(s) => Ok(int(s)),
            None => Ok(int(self.read().await?.size)),
        }
    }

    /// Whether the content looks binary.
    async fn binary(&self) -> Result<bool> {
        Ok(self.read().await?.binary)
    }

    /// Whether `content` was cut at the size limit.
    async fn content_truncated(&self) -> Result<bool> {
        Ok(self.read().await?.truncated)
    }

    /// The text content (empty for binary files), optionally lines `startLine..=endLine`
    /// (1-based).
    async fn content(&self, start_line: Option<i32>, end_line: Option<i32>) -> Result<String> {
        let f = self.read().await?;
        if f.binary {
            return Ok(String::new());
        }
        let text = String::from_utf8_lossy(&f.content);
        if start_line.is_none() && end_line.is_none() {
            return Ok(text.into_owned());
        }
        let start = usize::try_from(start_line.unwrap_or(1).max(1) - 1).unwrap_or(0);
        let end = end_line.map_or(usize::MAX, |e| usize::try_from(e.max(0)).unwrap_or(0));
        Ok(text
            .split_inclusive('\n')
            .skip(start)
            .take(end.saturating_sub(start))
            .collect())
    }

    /// Number of lines.
    async fn total_lines(&self) -> Result<i32> {
        let f = self.read().await?;
        if f.binary || f.content.is_empty() {
            return Ok(0);
        }
        let newlines = f.content.iter().filter(|&&b| b == b'\n').count();
        let unterminated = usize::from(f.content.last() != Some(&b'\n'));
        Ok(int(newlines + unterminated))
    }

    /// Detected language names.
    async fn languages(&self) -> Result<Vec<String>> {
        let f = self.read().await?;
        let head = &f.content[..f.content.len().min(16 * 1024)];
        Ok(sourcegull_languages::detect(&self.base.path, Some(head))
            .map(|l| vec![l.name.to_owned()])
            .unwrap_or_default())
    }

    /// Syntax highlighting in the compact occurrence format.
    async fn highlight(
        &self,
        #[graphql(default)] disable_timeout: bool,
        #[graphql(desc = "Accepted for compatibility.")] format: Option<HighlightResponseFormat>,
    ) -> Result<HighlightedFile> {
        let _ = format;
        let file = self.read().await?;
        let h = highlight_cached(&self.base.path, file.clone(), disable_timeout).await?;
        Ok(HighlightedFile { h, file })
    }

    /// Code graph data of the file: precise uploads, else syntactic (locals) data. One
    /// entry per source.
    async fn code_graph_data(
        &self,
        ctx: &Context<'_>,
        filter: Option<sourcegull_codeintel_graphql::codenav::CodeGraphDataFilter>,
    ) -> Result<Option<Vec<sourcegull_codeintel_graphql::codenav::CodeGraphData>>> {
        let file = sourcegull_codenav::FileAt {
            repo: (*self.base.h.repo).clone(),
            commit: self.base.commit_oid.clone(),
            path: self.base.path.clone(),
        };
        sourcegull_codeintel_graphql::codenav::code_graph_data(ctx, file, filter).await
    }

    /// Hover and definition of the symbol at (`line`, `character`), 0-based with UTF-16
    /// characters; null when there is no identifier there.
    async fn symbol_info(
        &self,
        ctx: &Context<'_>,
        line: i32,
        character: i32,
    ) -> Result<Option<sourcegull_codeintel_graphql::codenav::SymbolInfo>> {
        let file = sourcegull_codenav::FileAt {
            repo: (*self.base.h.repo).clone(),
            commit: self.base.commit_oid.clone(),
            path: self.base.path.clone(),
        };
        sourcegull_codeintel_graphql::codenav::symbol_info(ctx, file, line, character).await
    }

    /// The file's owners: CODEOWNERS entries, assigned owners, then recent
    /// contributors and viewers, strongest first; `reasons` keeps only those kinds of reasons.
    async fn ownership(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        reasons: Option<Vec<ownership::OwnershipReasonType>>,
    ) -> Result<Ownerships> {
        ownership::ownership(ctx, &self.base.owned_path(), false, first, after, reasons)
            .await
            .map(Ownerships)
    }

    /// Blame hunks covering lines `startLine..=endLine` (1-based; 0 = unbounded), ordered by line.
    async fn blame(
        &self,
        #[graphql(default)] start_line: i32,
        #[graphql(default)] end_line: i32,
    ) -> Result<Vec<HunkNode>> {
        let hunks = self
            .base
            .h
            .git
            .blame_all(
                &self.base.commit_oid,
                &self.base.path,
                u32::try_from(start_line.max(0)).unwrap_or(0),
                u32::try_from(end_line.max(0)).unwrap_or(0),
            )
            .await
            .map_err(git_error)?;
        Ok(hunks
            .into_iter()
            .map(|hunk| HunkNode {
                h: self.base.h.clone(),
                hunk,
                path: self.base.path.clone(),
            })
            .collect())
    }
}

// ------------------------------------------------------------------ refs

/// The kind of a ref.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum GitRefType {
    GitBranch,
    GitTag,
    GitRefOther,
}

/// Sort order of refs.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GitRefOrder {
    /// Most recently committed first.
    #[default]
    AuthoredOrCommittedAt,
    /// By name.
    Alphabetical,
}

/// A git object (here always a commit).
pub struct GitObjectNode {
    h: RepoHandle,
    oid: String,
}

impl GitObjectNode {
    /// The commit `oid` of `h`.
    pub fn new(h: RepoHandle, oid: String) -> Self {
        Self { h, oid }
    }
}

#[Object(name = "GitObject")]
impl GitObjectNode {
    /// The object id.
    async fn oid(&self) -> &str {
        &self.oid
    }

    /// The first 7 characters of the oid.
    #[graphql(name = "abbreviatedOID")]
    async fn abbreviated_oid(&self) -> &str {
        abbrev(&self.oid)
    }

    /// The commit, if the object is one.
    async fn commit(&self, ctx: &Context<'_>) -> Result<Option<GitCommitNode>> {
        if self.oid.is_empty() {
            return Ok(None);
        }
        let c = self.h.commit(ctx, &self.oid).await?;
        Ok(Some(GitCommitNode::new(self.h.clone(), c, None)))
    }
}

/// A branch, tag or other ref.
pub struct GitRefNode {
    h: RepoHandle,
    r: pb::RefInfo,
}

impl GitRefNode {
    /// A ref of `h` as the repo node listed it.
    pub fn from_ref(h: RepoHandle, r: pb::RefInfo) -> Self {
        Self { h, r }
    }

    /// The branch `name` (`refs/heads/…`, or a short name) of `h` at `commit_oid`, without
    /// asking the repo node (e.g. a resolved bulk change spec workspace).
    pub fn branch(h: RepoHandle, name: &str, commit_oid: &str) -> Self {
        let short = name.strip_prefix("refs/heads/").unwrap_or(name);
        Self {
            h,
            r: pb::RefInfo {
                name: format!("refs/heads/{short}"),
                short_name: short.to_owned(),
                kind: pb::RefKind::Branch as i32,
                target_oid: commit_oid.to_owned(),
                commit_oid: commit_oid.to_owned(),
                ..Default::default()
            },
        }
    }
}

#[Object(name = "GitRef")]
impl GitRefNode {
    /// An opaque ID.
    async fn id(&self) -> ID {
        ID(marshal_string_id(
            "GitRef",
            &format!("{}:{}", self.h.repo.id.get(), self.r.name),
        ))
    }

    /// The full name (`refs/heads/main`).
    async fn name(&self) -> &str {
        &self.r.name
    }

    /// The short name (`main`).
    async fn display_name(&self) -> &str {
        &self.r.short_name
    }

    /// The short name (`main`).
    async fn abbrev_name(&self) -> &str {
        &self.r.short_name
    }

    /// The kind.
    #[graphql(name = "type")]
    async fn ref_type(&self) -> GitRefType {
        ref_type(self.r.kind)
    }

    /// The name prefix (`refs/heads/`).
    async fn prefix(&self) -> String {
        self.r
            .name
            .strip_suffix(&self.r.short_name)
            .unwrap_or("")
            .to_owned()
    }

    /// Whether the tag is annotated.
    async fn annotated(&self) -> bool {
        self.r.annotated
    }

    /// The commit the ref points to.
    async fn target(&self) -> GitObjectNode {
        GitObjectNode {
            h: self.h.clone(),
            oid: self.r.commit_oid.clone(),
        }
    }

    /// The repository.
    async fn repository(&self) -> RepositoryNode {
        self.h.node()
    }

    /// The repository page at this ref.
    async fn url(&self) -> String {
        repo_url(self.h.name(), Some(&self.r.short_name))
    }
}

fn ref_type(kind: i32) -> GitRefType {
    match pb::RefKind::try_from(kind) {
        Ok(pb::RefKind::Branch) => GitRefType::GitBranch,
        Ok(pb::RefKind::Tag) => GitRefType::GitTag,
        _ => GitRefType::GitRefOther,
    }
}

/// A page of refs.
#[derive(SimpleObject)]
pub struct GitRefConnection {
    pub nodes: Vec<GitRefNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Lists refs of `ty` (`None` = branches and tags).
pub async fn list_refs(
    h: &RepoHandle,
    ty: Option<GitRefType>,
    first: Option<i32>,
    after: Option<&str>,
    query: Option<&str>,
    order: GitRefOrder,
) -> Result<GitRefConnection> {
    let (branches, tags) = match ty {
        Some(GitRefType::GitBranch) => (true, false),
        Some(GitRefType::GitTag) => (false, true),
        _ => (true, true),
    };
    let resp = h
        .git
        .list_refs(branches, tags, "")
        .await
        .map_err(git_error)?;
    let q = query.map(str::to_lowercase).filter(|q| !q.is_empty());
    let mut refs: Vec<pb::RefInfo> = resp
        .refs
        .into_iter()
        .filter(|r| ty.is_none_or(|t| ref_type(r.kind) == t))
        .filter(|r| {
            q.as_deref()
                .is_none_or(|q| r.short_name.to_lowercase().contains(q))
        })
        .collect();
    match order {
        GitRefOrder::AuthoredOrCommittedAt => refs.sort_by(|a, b| {
            b.commit_time
                .cmp(&a.commit_time)
                .then_with(|| a.short_name.cmp(&b.short_name))
        }),
        GitRefOrder::Alphabetical => refs.sort_by(|a, b| a.short_name.cmp(&b.short_name)),
    }
    let total = refs.len();
    let limit = first.map_or(MAX_REFS, |n| i64::from(n).clamp(0, MAX_REFS));
    let offset = decode_cursor(after)?;
    let page: Vec<pb::RefInfo> = refs
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit + 1).unwrap_or(usize::MAX))
        .collect();
    let page_info = PageInfo::new(offset, limit, page.len());
    Ok(GitRefConnection {
        nodes: page
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(|r| GitRefNode { h: h.clone(), r })
            .collect(),
        total_count: int(total),
        page_info,
    })
}

/// The ref HEAD points to.
pub async fn default_branch(h: &RepoHandle) -> Result<Option<GitRefNode>> {
    let resp = match h.git.list_refs(true, false, "").await {
        Ok(r) => r,
        Err(GitCallError::RepoNotCloned | GitCallError::CloneInProgress) => return Ok(None),
        Err(e) => return Err(git_error(e)),
    };
    let head = resp.head_ref;
    Ok(resp
        .refs
        .into_iter()
        .find(|r| r.name == head)
        .map(|r| GitRefNode { h: h.clone(), r }))
}

// ------------------------------------------------------------------ comparisons

/// A revision expression and the object it resolved to.
pub struct GitRevSpecExpr {
    h: RepoHandle,
    expr: String,
    oid: Option<String>,
}

#[Object]
impl GitRevSpecExpr {
    /// The expression as given.
    async fn expr(&self) -> &str {
        &self.expr
    }

    /// The commit it resolved to (null for the empty tree of a root commit).
    async fn object(&self) -> Option<GitObjectNode> {
        self.oid.as_ref().map(|oid| GitObjectNode {
            h: self.h.clone(),
            oid: oid.clone(),
        })
    }
}

/// The `base...head` (or `base..head`) range of a comparison.
pub struct GitRevisionRange {
    cmp: Arc<ComparisonData>,
}

#[Object]
impl GitRevisionRange {
    /// `base...head` for comparisons, `base..head` for a commit's diff.
    async fn expr(&self) -> String {
        let sep = if self.cmp.three_dot { "..." } else { ".." };
        format!("{}{sep}{}", self.cmp.base_spec, self.cmp.head_spec)
    }

    /// The base revision.
    async fn base_rev_spec(&self) -> GitRevSpecExpr {
        GitRevSpecExpr {
            h: self.cmp.h.clone(),
            expr: self.cmp.base_spec.clone(),
            oid: self.cmp.base_oid.clone(),
        }
    }

    /// The head revision.
    async fn head_rev_spec(&self) -> GitRevSpecExpr {
        GitRevSpecExpr {
            h: self.cmp.h.clone(),
            expr: self.cmp.head_spec.clone(),
            oid: Some(self.cmp.head_oid.clone()),
        }
    }

    /// The merge base of base and head (three-dot comparisons).
    async fn merge_base(&self) -> Result<Option<GitObjectNode>> {
        Ok(self.cmp.old_side().await?.map(|oid| GitObjectNode {
            h: self.cmp.h.clone(),
            oid,
        }))
    }
}

struct ComparisonData {
    h: RepoHandle,
    base_spec: String,
    base_oid: Option<String>,
    head_spec: String,
    head_oid: String,
    /// `base...head` (diff from the merge base) vs. `base..head` (diff from base).
    three_dot: bool,
    compare: OnceCell<Arc<pb::CompareResponse>>,
    diffs: OnceCell<Arc<Vec<pb::FileDiff>>>,
}

impl ComparisonData {
    async fn compare(&self) -> Result<Arc<pb::CompareResponse>> {
        self.compare
            .get_or_try_init(|| async {
                let base = self.base_oid.as_deref().unwrap_or("");
                self.h
                    .git
                    .compare(base, &self.head_oid, MAX_COMPARE_COMMITS)
                    .await
                    .map(Arc::new)
                    .map_err(git_error)
            })
            .await
            .cloned()
    }

    /// The old side of the diff: the merge base (three-dot) or the base (two-dot).
    async fn old_side(&self) -> Result<Option<String>> {
        match (&self.base_oid, self.three_dot) {
            (None, _) => Ok(None),
            (Some(b), false) => Ok(Some(b.clone())),
            (Some(_), true) => {
                let mb = self.compare().await?.merge_base_oid.clone();
                Ok((!mb.is_empty()).then_some(mb))
            }
        }
    }

    async fn diffs(&self, paths: Vec<String>) -> Result<Arc<Vec<pb::FileDiff>>> {
        let load = || async {
            let old = self.old_side().await?.unwrap_or_default();
            if old.is_empty() && self.base_oid.is_some() {
                // Unrelated histories: nothing sensible to diff against.
                return Ok(Arc::new(Vec::new()));
            }
            self.h
                .git
                .diff(&old, &self.head_oid, paths.clone(), false)
                .await
                .map(Arc::new)
                .map_err(git_error)
        };
        if paths.is_empty() {
            self.diffs.get_or_try_init(load).await.cloned()
        } else {
            load().await
        }
    }
}

/// A comparison of two revisions.
pub struct RepositoryComparisonNode(Arc<ComparisonData>);

impl RepositoryComparisonNode {
    /// A comparison of `base` and `head`. `base_oid: None` means "against the first parent or
    /// the empty tree" (a commit's own diff).
    pub fn new(
        h: RepoHandle,
        base_spec: String,
        base_oid: Option<String>,
        head_spec: String,
        head_oid: String,
        three_dot: bool,
    ) -> Self {
        Self(Arc::new(ComparisonData {
            h,
            base_spec,
            base_oid,
            head_spec,
            head_oid,
            three_dot,
            compare: OnceCell::new(),
            diffs: OnceCell::new(),
        }))
    }
}

pub use sourcegull_graphql_core::diff::{DiffStat, FileDiffHunk, FileDiffHunkRange};

/// The changes to one file.
pub struct FileDiffNode {
    cmp: Arc<ComparisonData>,
    f: pb::FileDiff,
    old_commit: Option<String>,
}

impl FileDiffNode {
    fn blob(&self, commit: &str, path: &str, oid: &str) -> GitBlobNode {
        GitBlobNode::new(EntryBase {
            h: self.cmp.h.clone(),
            commit_oid: commit.to_owned(),
            rev: None,
            path: path.to_owned(),
            oid: (!oid.is_empty()).then(|| oid.to_owned()),
            kind: pb::EntryKind::File as i32,
            size: None,
        })
    }
}

#[Object(name = "FileDiff")]
impl FileDiffNode {
    /// Path before the change (null for added files).
    async fn old_path(&self) -> Option<&str> {
        (!self.f.old_path.is_empty()).then_some(self.f.old_path.as_str())
    }

    /// Path after the change (null for deleted files).
    async fn new_path(&self) -> Option<&str> {
        (!self.f.new_path.is_empty()).then_some(self.f.new_path.as_str())
    }

    /// `ADDED`, `DELETED`, `MODIFIED`, `RENAMED`, `COPIED` or `TYPE_CHANGED`.
    async fn status(&self) -> &'static str {
        match pb::ChangeStatus::try_from(self.f.status) {
            Ok(pb::ChangeStatus::Added) => "ADDED",
            Ok(pb::ChangeStatus::Deleted) => "DELETED",
            Ok(pb::ChangeStatus::Renamed) => "RENAMED",
            Ok(pb::ChangeStatus::Copied) => "COPIED",
            Ok(pb::ChangeStatus::TypeChanged) => "TYPE_CHANGED",
            _ => "MODIFIED",
        }
    }

    /// Whether either side is binary.
    async fn binary(&self) -> bool {
        self.f.binary
    }

    /// Whether the patch was skipped because a side is too large.
    async fn too_large(&self) -> bool {
        self.f.too_large
    }

    /// The file before the change.
    async fn old_file(&self) -> Option<GitBlobNode> {
        let commit = self.old_commit.as_deref()?;
        (!self.f.old_path.is_empty()).then(|| self.blob(commit, &self.f.old_path, &self.f.old_oid))
    }

    /// The file after the change.
    async fn new_file(&self) -> Option<GitBlobNode> {
        (!self.f.new_path.is_empty())
            .then(|| self.blob(&self.cmp.head_oid, &self.f.new_path, &self.f.new_oid))
    }

    /// The new file, or the old one for deletions.
    async fn most_relevant_file(&self) -> Option<GitBlobNode> {
        if self.f.new_path.is_empty() {
            let commit = self.old_commit.as_deref()?;
            Some(self.blob(commit, &self.f.old_path, &self.f.old_oid))
        } else {
            Some(self.blob(&self.cmp.head_oid, &self.f.new_path, &self.f.new_oid))
        }
    }

    /// The hunks.
    async fn hunks(&self) -> Vec<FileDiffHunk> {
        self.f.hunks.iter().cloned().map(FileDiffHunk).collect()
    }

    /// Line counts.
    async fn stat(&self) -> DiffStat {
        DiffStat {
            added: int(self.f.additions),
            deleted: int(self.f.deletions),
            changed: 0,
        }
    }

    /// A stable id within the comparison.
    #[graphql(name = "internalID")]
    async fn internal_id(&self) -> String {
        format!(
            "{}:{}",
            self.f.old_path,
            if self.f.new_path.is_empty() {
                &self.f.old_path
            } else {
                &self.f.new_path
            }
        )
    }
}

/// A page of file diffs.
#[derive(SimpleObject)]
pub struct FileDiffConnection {
    pub nodes: Vec<FileDiffNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
    /// Totals over all files of the comparison.
    pub diff_stat: DiffStat,
}

#[Object(name = "RepositoryComparison")]
impl RepositoryComparisonNode {
    /// The base repository.
    async fn base_repository(&self) -> RepositoryNode {
        self.0.h.node()
    }

    /// The head repository.
    async fn head_repository(&self) -> RepositoryNode {
        self.0.h.node()
    }

    /// The compared range.
    async fn range(&self) -> GitRevisionRange {
        GitRevisionRange {
            cmp: self.0.clone(),
        }
    }

    /// Commits in `base..head`, newest first (just the head commit for a commit's own diff).
    async fn commits(&self, ctx: &Context<'_>, first: Option<i32>) -> Result<GitCommitConnection> {
        let limit = first.map_or(MAX_COMPARE_COMMITS as usize, |n| {
            usize::try_from(n.max(0)).unwrap_or(0)
        });
        let cmp = &self.0;
        if cmp.base_oid.is_none() {
            let c = cmp.h.commit(ctx, &cmp.head_oid).await?;
            return Ok(GitCommitConnection {
                nodes: vec![GitCommitNode::new(cmp.h.clone(), c, None)],
                page_info: PageInfo {
                    has_next_page: false,
                    end_cursor: None,
                },
                truncated: false,
            });
        }
        let resp = cmp.compare().await?;
        let more = resp.commits.len() > limit;
        Ok(GitCommitConnection {
            nodes: resp
                .commits
                .iter()
                .take(limit)
                .cloned()
                .map(|c| GitCommitNode::new(cmp.h.clone(), Arc::new(c), None))
                .collect(),
            page_info: PageInfo {
                has_next_page: more || resp.commits_truncated,
                end_cursor: None,
            },
            truncated: resp.commits_truncated,
        })
    }

    /// The changed files with hunks. `paths` restricts the diff (prefix match on directories).
    async fn file_diffs(
        &self,
        first: Option<i32>,
        after: Option<String>,
        paths: Option<Vec<String>>,
    ) -> Result<FileDiffConnection> {
        let all = self.0.diffs(paths.unwrap_or_default()).await?;
        let old_commit = match self.0.old_side().await? {
            Some(o) => Some(o),
            None => {
                // A commit's own diff: the old side is its first parent (if any).
                let c = self
                    .0
                    .h
                    .git
                    .get_commit(&self.0.head_oid)
                    .await
                    .map_err(git_error)?;
                c.parents.first().cloned()
            }
        };
        let mut stat = DiffStat::default();
        for f in all.iter() {
            stat.added = stat.added.saturating_add(int(f.additions));
            stat.deleted = stat.deleted.saturating_add(int(f.deletions));
        }
        let limit = first.map_or(MAX_FILE_DIFFS, |n| i64::from(n).clamp(0, MAX_FILE_DIFFS));
        let offset = decode_cursor(after.as_deref())?;
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(limit).unwrap_or(0);
        let fetched = all.len().saturating_sub(skip).min(take + 1);
        Ok(FileDiffConnection {
            nodes: all
                .iter()
                .skip(skip)
                .take(take)
                .cloned()
                .map(|f| FileDiffNode {
                    cmp: self.0.clone(),
                    f,
                    old_commit: old_commit.clone(),
                })
                .collect(),
            total_count: int(all.len()),
            page_info: PageInfo::new(offset, limit, fetched),
            diff_stat: stat,
        })
    }
}

/// `Repository.comparison(base, head)`: three-dot comparison; empty specs mean HEAD.
pub async fn comparison(
    h: &RepoHandle,
    base: Option<String>,
    head: Option<String>,
) -> Result<RepositoryComparisonNode> {
    let base = base.unwrap_or_default();
    let head = head.unwrap_or_default();
    let base_oid = h.git.resolve_revision(&base).await.map_err(git_error)?;
    let head_oid = h.git.resolve_revision(&head).await.map_err(git_error)?;
    let spec = |s: String| if s.is_empty() { "HEAD".to_owned() } else { s };
    Ok(RepositoryComparisonNode::new(
        h.clone(),
        spec(base),
        Some(base_oid),
        spec(head),
        head_oid,
        true,
    ))
}
