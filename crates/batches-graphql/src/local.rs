//! What local runs need besides uploads: `resolveWorkspacesForBatchSpec`, which command-line
//! clients such as `gull bulk preview|apply` call to learn where to run a spec's steps, and the
//! files a server-side spec's steps mount (`BatchSpec.files`, uploaded through
//! `/.api/files/batch-changes/…`, see `sourcegull-http-api`).
//!
//! `resolveWorkspacesForBatchSpec(batchSpec:)` resolves the spec synchronously as the viewer, with
//! their repository permissions, through the same [`Resolver`] the worker runs for server-side
//! specs, and stores nothing. As command-line clients expect, repositories on unsupported code
//! hosts and repositories with a `.batchignore` are returned flagged (`unsupported`, `ignored`);
//! the client leaves them out unless run with `-allow-unsupported` / `-allow-ignored`. Archived
//! repositories are left out. The execution cache is not consulted (local runs have their own).

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_batches::store::{BatchSpec, BatchSpecWorkspaceFile, BatchSpecWorkspaceFileStore};
use sourcegull_batches_resolver::{Options, ResolveError, Resolver};
use sourcegull_services::batch_files;
use sourcegull_services::relay::marshal_string_id;
use sourcegull_types::RepoId;

use super::{check_first, invalid, readable_repos, require_write, spec_errors};
use crate::context::{codes, db_error, error, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::shared::{BranchData, BranchRef, RepositoryRef};

/// Node kind of workspace file IDs (keyed by the file's random id).
pub const WORKSPACE_FILE_KIND: &str = "BatchSpecWorkspaceFile";

/// A workspace of a spec resolved on the fly (`ResolvedBatchSpecWorkspace`).
pub struct ResolvedWorkspace {
    repo: RepositoryRef,
    branch: BranchRef,
    path: String,
    only_fetch_workspace: bool,
    ignored: bool,
    unsupported: bool,
    search_result_paths: Vec<String>,
}

#[Object(name = "ResolvedBatchSpecWorkspace")]
impl ResolvedWorkspace {
    /// The repository to run in.
    async fn repository(&self) -> &RepositoryRef {
        &self.repo
    }

    /// The branch to run on, at the commit it was resolved to.
    async fn branch(&self) -> &BranchRef {
        &self.branch
    }

    /// The workspace directory relative to the repository root; '' for the root.
    async fn path(&self) -> &str {
        &self.path
    }

    /// Only the workspace directory needs to be fetched.
    async fn only_fetch_workspace(&self) -> bool {
        self.only_fetch_workspace
    }

    /// The repository has a `.batchignore` file at its root.
    async fn ignored(&self) -> bool {
        self.ignored
    }

    /// The repository's code host is not supported by bulk changes.
    async fn unsupported(&self) -> bool {
        self.unsupported
    }

    /// Paths of the `on` query's results in the workspace.
    async fn search_result_paths(&self) -> &[String] {
        &self.search_result_paths
    }
}

/// `resolveWorkspacesForBatchSpec`.
pub(super) async fn resolve_workspaces(
    ctx: &Context<'_>,
    raw: &str,
) -> Result<Vec<ResolvedWorkspace>> {
    let actor = require_write(ctx)?;
    let spec = sourcegull_batches::parse_batch_spec(raw)
        .map_err(|e| spec_errors(&e))?
        .spec;
    let s = services(ctx)?;
    let search = match &s.app {
        Some(app) => app.search_backend(),
        None => sourcegull_services::search_backend(
            s.db.clone(),
            s.authz.clone(),
            s.repo_clients.clone(),
        ),
    };
    let options = Options {
        allow_ignored: true,
        allow_unsupported: true,
        no_cache: true,
    };
    let resolution = Resolver::new(s.db.clone(), search)
        .resolve(&spec, actor.user_id, options)
        .await
        .map_err(|e| match e {
            ResolveError::Invalid(m) => invalid(m),
            ResolveError::Unavailable(m) => {
                tracing::warn!(error = %m, "resolveWorkspacesForBatchSpec");
                error(
                    codes::UNAVAILABLE,
                    "Resolving the workspaces failed; try again later.",
                )
            }
        })?;
    // Skipped although unsupported and ignored repositories are allowed: archived.
    let workspaces: Vec<_> = resolution
        .workspaces
        .into_iter()
        .filter(|w| !w.skipped)
        .collect();
    let ids: Vec<RepoId> = workspaces.iter().map(|w| w.repo_id).collect();
    let repos = readable_repos(ctx, &ids).await?;
    let mut out = Vec::with_capacity(workspaces.len());
    for w in workspaces {
        let Some(repo) = repos.get(&w.repo_id) else {
            continue;
        };
        out.push(ResolvedWorkspace {
            branch: BranchRef(BranchData {
                repo: repo.clone(),
                name: w.branch.clone(),
                commit_oid: w.commit.clone(),
            }),
            repo: RepositoryRef(repo.clone()),
            path: w.path,
            only_fetch_workspace: w.only_fetch_workspace,
            ignored: w.ignored,
            unsupported: w.unsupported,
            search_result_paths: w.file_matches,
        });
    }
    Ok(out)
}

/// A file uploaded with a server-side spec for its steps to mount (`BatchSpecWorkspaceFile`).
pub struct WorkspaceFileNode {
    file: BatchSpecWorkspaceFile,
    spec_rand_id: String,
}

#[Object(name = "BatchSpecWorkspaceFile")]
impl WorkspaceFileNode {
    async fn id(&self) -> ID {
        ID(marshal_string_id(WORKSPACE_FILE_KIND, &self.file.rand_id))
    }

    /// When the file was last modified on the uploader's machine.
    async fn modified_at(&self) -> DateTime<Utc> {
        self.file.modified_at
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.file.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.file.updated_at
    }

    /// The path relative to the bulk change spec's directory (`scripts/fix.sh`).
    async fn path(&self) -> String {
        self.file.full_path()
    }

    /// The file name.
    async fn name(&self) -> &str {
        &self.file.filename
    }

    /// Always false.
    async fn is_directory(&self) -> bool {
        false
    }

    /// The size in bytes.
    async fn byte_size(&self) -> i32 {
        to_int(self.file.size)
    }

    /// Whether the content is binary (not UTF-8 text).
    async fn binary(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(std::str::from_utf8(&self.load(ctx).await?).is_err())
    }

    /// The content as text (binary content is replaced lossily).
    async fn content(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.load(ctx).await?).into_owned())
    }

    /// Where to download the file (for the spec's administrators).
    async fn url(&self) -> String {
        batch_files::file_path(
            batch_files::API_PREFIX,
            &self.spec_rand_id,
            &self.file.rand_id,
        )
    }
}

impl WorkspaceFileNode {
    async fn load(&self, ctx: &Context<'_>) -> Result<Vec<u8>> {
        Ok(BatchSpecWorkspaceFileStore::new(&services(ctx)?.db)
            .content(self.file.id)
            .await
            .map_err(db_error)?
            .unwrap_or_default())
    }
}

/// A page of workspace files.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceFileConnection")]
pub struct WorkspaceFileConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<WorkspaceFileNode>,
}

/// `BatchSpec.files`: by path and name; the cursor is the offset of the next page.
pub(super) async fn files(
    ctx: &Context<'_>,
    spec: &BatchSpec,
    first: Option<i32>,
    after: Option<String>,
) -> Result<WorkspaceFileConnection> {
    let limit = check_first(first)?;
    let offset: i64 = match after.as_deref() {
        None | Some("") => 0,
        Some(c) => c
            .parse()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| invalid(format!("Invalid cursor {c:?}.")))?,
    };
    let store = BatchSpecWorkspaceFileStore::new(&services(ctx)?.db);
    let total = store.count(spec.id).await.map_err(db_error)?;
    let rows = if limit == 0 {
        Vec::new()
    } else {
        store.list(spec.id, offset, limit).await.map_err(db_error)?
    };
    let next = offset + i64::try_from(rows.len()).unwrap_or(0);
    let has_next_page = next < total;
    Ok(WorkspaceFileConnection {
        total_count: to_int(total),
        page_info: PageInfo {
            has_next_page,
            end_cursor: has_next_page.then(|| next.to_string()),
        },
        nodes: rows
            .into_iter()
            .map(|file| WorkspaceFileNode {
                file,
                spec_rand_id: spec.rand_id.clone(),
            })
            .collect(),
    })
}
