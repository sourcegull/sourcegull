//! `codeintel_uploads` store: precise code intelligence uploads.
//!
//! An upload starts in `uploading` while the client sends its parts, moves to `queued` once
//! its index blob is complete, and is processed through [`sourcegull_pgqueue`] (`processing` →
//! `completed` | `errored` | `failed`). Deleting marks it `deleting`; the janitor removes its
//! blobs and marks it `deleted`.

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_pgqueue::{QueueConfig, notify_enqueued};
use sourcegull_types::{RepoAccess, RepoId, UserId};
use sqlx::PgPool;

/// The queue table of uploads.
pub const UPLOADS_TABLE: &str = "codeintel_uploads";

/// The state of an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadState {
    Uploading,
    Queued,
    Processing,
    Completed,
    Errored,
    Failed,
    Canceled,
    Deleting,
    Deleted,
}

impl UploadState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "uploading" => Self::Uploading,
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "errored" => Self::Errored,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            "deleting" => Self::Deleting,
            _ => Self::Deleted,
        }
    }
}

/// An upload row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub id: i64,
    pub state: UploadState,
    pub repo_id: RepoId,
    pub commit: String,
    pub root: String,
    pub indexer: String,
    pub indexer_version: String,
    pub num_parts: i32,
    pub uploaded_parts: Vec<i32>,
    pub upload_size: Option<i64>,
    pub uploaded_at: DateTime<Utc>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
    pub num_failures: i32,
    pub uploader_user_id: Option<UserId>,
    pub associated_index_id: Option<i64>,
    pub graph_key: Option<String>,
    pub num_documents: Option<i32>,
    pub num_occurrences: Option<i64>,
    pub num_symbols: Option<i32>,
    pub expired: bool,
    pub deleted_at: Option<DateTime<Utc>>,
    /// Marked for replacement by a new auto-indexing job.
    pub should_reindex: bool,
    /// The GitLab CI job that uploaded with its job token.
    pub ci_job: Option<UploadCiJob>,
    /// Bytes of the parts received so far (multipart uploads).
    pub uploaded_bytes: i64,
}

/// The GitLab CI job of an upload made with a CI job token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadCiJob {
    pub job_id: i64,
    /// The branch or tag the job ran for.
    pub git_ref: String,
    /// The GitLab username of the user who triggered the job (empty when unknown).
    pub username: String,
}

/// The result of [`CodeIntelStore::add_part`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPart {
    Added,
    /// The upload is gone or no longer `uploading`, or the index is out of range.
    NotAccepting,
    /// The part would take the upload's total past the limit; nothing was recorded.
    TooLarge,
}

const COLUMNS: &str = "id, state, repository_id, commit, root, indexer, indexer_version, \
    num_parts, uploaded_parts, upload_size, uploaded_at, queued_at, started_at, finished_at, \
    failure_message, num_failures, uploader_user_id, associated_index_id, graph_key, \
    num_documents, num_occurrences, num_symbols, expired, deleted_at, should_reindex, \
    ci_job_id, ci_job_ref, ci_job_username, uploaded_bytes";

/// [`COLUMNS`] qualified with a table alias.
fn prefixed_columns(alias: &str) -> String {
    COLUMNS
        .split(", ")
        .map(|c| format!("{alias}.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Upload {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let state: String = row.try_get("state")?;
        Ok(Upload {
            id: row.try_get("id")?,
            state: UploadState::parse(&state),
            repo_id: RepoId(row.try_get("repository_id")?),
            commit: row.try_get("commit")?,
            root: row.try_get("root")?,
            indexer: row.try_get("indexer")?,
            indexer_version: row.try_get("indexer_version")?,
            num_parts: row.try_get("num_parts")?,
            uploaded_parts: row.try_get("uploaded_parts")?,
            upload_size: row.try_get("upload_size")?,
            uploaded_at: row.try_get("uploaded_at")?,
            queued_at: row.try_get("queued_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            failure_message: row.try_get("failure_message")?,
            num_failures: row.try_get("num_failures")?,
            uploader_user_id: row
                .try_get::<Option<i64>, _>("uploader_user_id")?
                .map(UserId),
            associated_index_id: row.try_get("associated_index_id")?,
            graph_key: row.try_get("graph_key")?,
            num_documents: row.try_get("num_documents")?,
            num_occurrences: row.try_get("num_occurrences")?,
            num_symbols: row.try_get("num_symbols")?,
            expired: row.try_get("expired")?,
            deleted_at: row.try_get("deleted_at")?,
            should_reindex: row.try_get("should_reindex")?,
            ci_job: row
                .try_get::<Option<i64>, _>("ci_job_id")?
                .map(|job_id| -> sqlx::Result<UploadCiJob> {
                    Ok(UploadCiJob {
                        job_id,
                        git_ref: row
                            .try_get::<Option<String>, _>("ci_job_ref")?
                            .unwrap_or_default(),
                        username: row
                            .try_get::<Option<String>, _>("ci_job_username")?
                            .unwrap_or_default(),
                    })
                })
                .transpose()?,
            uploaded_bytes: row.try_get("uploaded_bytes")?,
        })
    }
}

/// A new upload.
#[derive(Debug, Clone)]
pub struct NewUpload {
    pub repo_id: RepoId,
    pub commit: String,
    pub root: String,
    pub indexer: String,
    pub indexer_version: String,
    pub num_parts: i32,
    pub uploader: Option<UserId>,
    pub associated_index_id: Option<i64>,
    /// The GitLab CI job uploading with its job token.
    pub ci_job: Option<UploadCiJob>,
}

/// Filters of [`CodeIntelStore::list_uploads`] and [`CodeIntelStore::matching_ids`].
#[derive(Debug, Clone, Default)]
pub struct ListUploads {
    /// Only this repository.
    pub repo: Option<RepoId>,
    /// Only these states (any by default; deleted ones only with `include_deleted` or when
    /// asked for here).
    pub states: Vec<UploadState>,
    /// Matches the commit, root, indexer or repository name (case-insensitive substring).
    pub query: Option<String>,
    /// Only uploads made by one of these indexers (any when empty).
    pub indexers: Vec<String>,
    pub include_deleted: bool,
    /// Only uploads that import a package this upload exports (its dependents).
    pub dependent_of: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

/// The `WHERE` clause of [`ListUploads`] over `codeintel_uploads u`, with binds `$1`..`$7`
/// in the order of [`bind_filter!`].
const LIST_FILTER: &str = "($1::int IS NULL OR u.repository_id = $1)
    AND ($2::int[] IS NULL OR u.repository_id = ANY($2))
    AND (cardinality($3::text[]) = 0 OR u.state = ANY($3))
    AND ($4::text IS NULL OR u.commit ILIKE $4 OR u.root ILIKE $4 OR u.indexer ILIKE $4
         OR EXISTS (SELECT 1 FROM repo r WHERE r.id = u.repository_id AND r.name ILIKE $4))
    AND (cardinality($5::text[]) = 0 OR u.indexer = ANY($5))
    AND ($6 OR u.state NOT IN ('deleting', 'deleted') OR u.state = ANY($3))
    AND ($7::bigint IS NULL OR (u.id <> $7 AND EXISTS (
        SELECT 1 FROM codeintel_package_monikers e
        JOIN codeintel_package_monikers i ON i.kind = 'import' AND i.scheme = e.scheme
            AND i.manager = e.manager AND i.name = e.name AND i.version = e.version
        WHERE e.upload_id = $7 AND e.kind = 'export' AND i.upload_id = u.id)))
    AND EXISTS (SELECT 1 FROM repo r WHERE r.id = u.repository_id AND r.deleted_at IS NULL)";

/// The bind values of [`LIST_FILTER`].
struct FilterBinds<'a> {
    repo: Option<i32>,
    readable: Option<Vec<i32>>,
    states: Vec<&'static str>,
    pattern: Option<String>,
    indexers: &'a [String],
    include_deleted: bool,
    dependent_of: Option<i64>,
}

impl<'a> FilterBinds<'a> {
    fn new(opts: &'a ListUploads, access: &RepoAccess) -> Self {
        let readable = access.readable_ids();
        let pattern = opts
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        Self {
            repo: opts.repo.map(|r| r.get()),
            readable,
            states: opts.states.iter().map(|s| s.as_str()).collect(),
            pattern: pattern.map(|q| {
                format!(
                    "%{}%",
                    q.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            }),
            indexers: &opts.indexers,
            include_deleted: opts.include_deleted,
            dependent_of: opts.dependent_of,
        }
    }
}

/// Binds [`FilterBinds`] as `$1`..`$7`.
macro_rules! bind_filter {
    ($q:expr, $f:expr) => {
        $q.bind($f.repo)
            .bind(&$f.readable)
            .bind(&$f.states)
            .bind(&$f.pattern)
            .bind($f.indexers)
            .bind($f.include_deleted)
            .bind($f.dependent_of)
    };
}

/// The blob of an upload's complete index.
pub fn upload_index_key(id: i64) -> String {
    format!("uploads/{id}/index")
}

/// The blob of one part of a multipart upload.
pub fn upload_part_key(id: i64, index: i32) -> String {
    format!("uploads/{id}/part-{index:05}")
}

/// The blob of an upload's code graph file.
pub fn upload_graph_key(id: i64) -> String {
    format!("uploads/{id}/graph")
}

/// Rows inserted per statement when recording postings and monikers.
const POSTINGS_BATCH: usize = 20_000;

/// A package an upload defines (`kind` "export") or only uses ("import").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMoniker<'a> {
    pub kind: &'a str,
    pub scheme: String,
    pub manager: String,
    pub name: String,
    pub version: String,
}

/// What processing an upload produced ([`CodeIntelStore::record_processed`]).
#[derive(Debug, Clone, Default)]
pub struct ProcessedUpload<'a> {
    /// The indexer from the index's metadata, used when the upload request named none.
    pub indexer: &'a str,
    pub indexer_version: &'a str,
    pub graph_key: &'a str,
    pub num_documents: i32,
    pub num_occurrences: i64,
    pub num_symbols: i32,
    /// (symbol hash, flags) of the global symbols in the upload.
    pub postings: &'a [(i64, i16)],
    pub packages: &'a [PackageMoniker<'a>],
}

/// Normalizes an upload root: `""` for the repository root, else `dir/` without leading
/// slashes or `.` segments. `None` for a root leaving the repository.
pub fn normalize_root(root: &str) -> Option<String> {
    let mut parts = Vec::new();
    for seg in root.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => parts.push(s),
        }
    }
    Some(if parts.is_empty() {
        String::new()
    } else {
        format!("{}/", parts.join("/"))
    })
}

/// Store for `codeintel_uploads`.
#[derive(Debug, Clone)]
pub struct CodeIntelStore {
    pool: PgPool,
}

impl CodeIntelStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The processing queue: FIFO, three attempts.
    pub fn queue_config() -> QueueConfig {
        QueueConfig::new(UPLOADS_TABLE)
    }

    /// Creates an upload in state `uploading`.
    pub async fn create_upload(&self, u: &NewUpload) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO codeintel_uploads (state, repository_id, commit, root, indexer,
                indexer_version, num_parts, uploader_user_id, associated_index_id, ci_job_id,
                ci_job_ref, ci_job_username)
             VALUES ('uploading', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
        )
        .bind(u.repo_id.get())
        .bind(&u.commit)
        .bind(&u.root)
        .bind(&u.indexer)
        .bind(&u.indexer_version)
        .bind(u.num_parts)
        .bind(u.uploader.map(|u| u.get()))
        .bind(u.associated_index_id)
        .bind(u.ci_job.as_ref().map(|j| j.job_id))
        .bind(u.ci_job.as_ref().map(|j| j.git_ref.as_str()))
        .bind(u.ci_job.as_ref().map(|j| j.username.as_str()))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records part `index` of an upload that is still `uploading`, adding `added_bytes`
    /// (negative when a re-sent part shrank) to its received bytes unless that takes them past
    /// `max_bytes`.
    pub async fn add_part(
        &self,
        id: i64,
        index: i32,
        added_bytes: i64,
        max_bytes: i64,
    ) -> Result<AddPart> {
        let n = sqlx::query(
            "UPDATE codeintel_uploads
             SET uploaded_parts = (SELECT array_agg(DISTINCT p ORDER BY p)
                                   FROM unnest(array_append(uploaded_parts, $2)) p),
                 uploaded_bytes = uploaded_bytes + $3
             WHERE id = $1 AND state = 'uploading' AND $2 >= 0 AND $2 < num_parts
                AND uploaded_bytes + $3 <= $4",
        )
        .bind(id)
        .bind(index)
        .bind(added_bytes)
        .bind(max_bytes)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 1 {
            return Ok(AddPart::Added);
        }
        let open: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM codeintel_uploads
                WHERE id = $1 AND state = 'uploading' AND $2 >= 0 AND $2 < num_parts)",
        )
        .bind(id)
        .bind(index)
        .fetch_one(&self.pool)
        .await?;
        Ok(if open {
            AddPart::TooLarge
        } else {
            AddPart::NotAccepting
        })
    }

    /// Deletes an upload that is still `uploading` (one refused for its size); its blobs are
    /// the caller's to delete. Returns whether it was deleted.
    pub async fn delete_uploading(&self, id: i64) -> Result<bool> {
        let n = sqlx::query("DELETE FROM codeintel_uploads WHERE id = $1 AND state = 'uploading'")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    /// Moves an `uploading` upload to `queued` with its size, and wakes the processors.
    pub async fn mark_queued(&self, id: i64, size: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE codeintel_uploads SET state = 'queued', upload_size = $2, queued_at = now()
             WHERE id = $1 AND state = 'uploading'",
        )
        .bind(id)
        .bind(size)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        notify_enqueued(&mut *tx, UPLOADS_TABLE).await?;
        tx.commit().await?;
        Ok(n == 1)
    }

    /// Marks an `uploading` upload failed (the client sent a bad index or gave up).
    pub async fn mark_upload_failed(&self, id: i64, message: &str) -> Result<()> {
        sqlx::query(
            "UPDATE codeintel_uploads SET state = 'failed', failure_message = $2,
                finished_at = now()
             WHERE id = $1 AND state = 'uploading'",
        )
        .bind(id)
        .bind(message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records the results of processing upload `id`, held by `worker`, in one transaction:
    /// its code graph and statistics, its symbol postings and package monikers (replacing
    /// any from an earlier run), and marks older uploads for the same repository, commit,
    /// root and indexer for deletion. Returns false, writing nothing, when the worker no
    /// longer holds the upload (it was reset, canceled or deleted meanwhile).
    pub async fn record_processed(
        &self,
        id: i64,
        worker: &str,
        p: &ProcessedUpload<'_>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let held: Option<(i32, String, String, String)> = sqlx::query_as(
            "UPDATE codeintel_uploads SET
                indexer = CASE WHEN indexer = '' THEN $3 ELSE indexer END,
                indexer_version = CASE WHEN indexer_version = '' THEN $4 ELSE indexer_version END,
                graph_key = $5, num_documents = $6, num_occurrences = $7, num_symbols = $8
             WHERE id = $1 AND state = 'processing' AND worker_hostname = $2
             RETURNING repository_id, commit, root, indexer",
        )
        .bind(id)
        .bind(worker)
        .bind(p.indexer)
        .bind(p.indexer_version)
        .bind(p.graph_key)
        .bind(p.num_documents)
        .bind(p.num_occurrences)
        .bind(p.num_symbols)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((repo, commit, root, indexer)) = held else {
            return Ok(false);
        };
        sqlx::query("DELETE FROM codeintel_symbol_postings WHERE upload_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM codeintel_package_monikers WHERE upload_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for chunk in p.postings.chunks(POSTINGS_BATCH) {
            let (hashes, flags): (Vec<i64>, Vec<i16>) = chunk.iter().copied().unzip();
            sqlx::query(
                "INSERT INTO codeintel_symbol_postings (symbol_hash, upload_id, flags)
                 SELECT h, $1, f FROM unnest($2::bigint[], $3::smallint[]) AS t (h, f)",
            )
            .bind(id)
            .bind(&hashes)
            .bind(&flags)
            .execute(&mut *tx)
            .await?;
        }
        for chunk in p.packages.chunks(POSTINGS_BATCH) {
            let mut cols: [Vec<&str>; 5] = Default::default();
            for m in chunk {
                for (col, v) in cols.iter_mut().zip([
                    m.kind,
                    m.scheme.as_str(),
                    m.manager.as_str(),
                    m.name.as_str(),
                    m.version.as_str(),
                ]) {
                    col.push(v);
                }
            }
            sqlx::query(
                "INSERT INTO codeintel_package_monikers
                    (upload_id, kind, scheme, manager, name, version)
                 SELECT $1, k, s, m, n, v
                 FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::text[])
                    AS t (k, s, m, n, v)
                 ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(&cols[0])
            .bind(&cols[1])
            .bind(&cols[2])
            .bind(&cols[3])
            .bind(&cols[4])
            .execute(&mut *tx)
            .await?;
        }
        // A newer index of the same (repository, commit, root, indexer) replaces older ones.
        sqlx::query(
            "UPDATE codeintel_uploads SET state = 'deleting', deleted_at = now(),
                deleted_reason = 'overwritten', cancel = true
             WHERE repository_id = $1 AND commit = $2 AND root = $3 AND indexer = $4
               AND id < $5 AND state IN ('completed', 'errored', 'failed')",
        )
        .bind(repo)
        .bind(&commit)
        .bind(&root)
        .bind(&indexer)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The symbol postings of an upload, sorted by hash (tests and diagnostics).
    pub async fn symbol_postings(&self, upload: i64) -> Result<Vec<(i64, i16)>> {
        Ok(sqlx::query_as(
            "SELECT symbol_hash, flags FROM codeintel_symbol_postings WHERE upload_id = $1
             ORDER BY symbol_hash",
        )
        .bind(upload)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The package monikers of an upload, sorted.
    pub async fn package_monikers(&self, upload: i64) -> Result<Vec<PackageMoniker<'static>>> {
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT kind, scheme, manager, name, version FROM codeintel_package_monikers
             WHERE upload_id = $1 ORDER BY kind, scheme, manager, name, version",
        )
        .bind(upload)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, scheme, manager, name, version)| PackageMoniker {
                kind: if kind == "export" { "export" } else { "import" },
                scheme,
                manager,
                name,
                version,
            })
            .collect())
    }

    pub async fn get_upload(&self, id: i64) -> Result<Option<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_uploads WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Uploads matching `opts` that `access` may see, newest first, and their total count.
    pub async fn list_uploads(
        &self,
        opts: &ListUploads,
        access: &RepoAccess,
    ) -> Result<(Vec<Upload>, i64)> {
        let f = FilterBinds::new(opts, access);
        let total: i64 = bind_filter!(
            sqlx::query_scalar(&format!(
                "SELECT count(*) FROM codeintel_uploads u WHERE {LIST_FILTER}"
            )),
            f
        )
        .fetch_one(&self.pool)
        .await?;
        let rows = bind_filter!(
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM codeintel_uploads u WHERE {LIST_FILTER}
                 ORDER BY u.uploaded_at DESC, u.id DESC LIMIT $8 OFFSET $9"
            )),
            f
        )
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?;
        Ok((rows, total))
    }

    /// The ids of all uploads matching `opts` that `access` may see (`limit` and `offset`
    /// are ignored), for bulk deletes and reindexing.
    pub async fn matching_ids(&self, opts: &ListUploads, access: &RepoAccess) -> Result<Vec<i64>> {
        let f = FilterBinds::new(opts, access);
        Ok(bind_filter!(
            sqlx::query_scalar(&format!(
                "SELECT u.id FROM codeintel_uploads u WHERE {LIST_FILTER} ORDER BY u.id"
            )),
            f
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The completed uploads of a repository at any of `commits` (precise navigation looks
    /// for the uploads nearest a commit among its ancestors).
    pub async fn completed_at_commits(
        &self,
        repo: RepoId,
        commits: &[String],
    ) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_uploads
             WHERE repository_id = $1 AND commit = ANY($2) AND state = 'completed'
             ORDER BY id DESC"
        ))
        .bind(repo.get())
        .bind(commits)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The newest `limit` completed uploads of a repository.
    pub async fn completed_for_repo(&self, repo: RepoId, limit: i64) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_uploads
             WHERE repository_id = $1 AND state = 'completed'
             ORDER BY uploaded_at DESC, id DESC LIMIT $2"
        ))
        .bind(repo.get())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Completed uploads in repositories `access` may read whose postings for the symbol
    /// `hash` have any of the `flags` bits, newest first.
    pub async fn uploads_with_symbol(
        &self,
        hash: i64,
        flags: i16,
        access: &RepoAccess,
        limit: i64,
    ) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {} FROM codeintel_symbol_postings p
             JOIN codeintel_uploads u ON u.id = p.upload_id
             JOIN repo r ON r.id = u.repository_id AND r.deleted_at IS NULL
             WHERE p.symbol_hash = $1 AND p.flags & $2 <> 0 AND u.state = 'completed'
               AND ($3::int[] IS NULL OR u.repository_id = ANY($3))
             ORDER BY u.uploaded_at DESC, u.id DESC LIMIT $4",
            prefixed_columns("u")
        ))
        .bind(hash)
        .bind(flags)
        .bind(access.readable_ids())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Completed uploads in repositories `access` may read that export the package and
    /// define the symbol `hash`, newest first (definitions through package monikers).
    pub async fn uploads_exporting(
        &self,
        package: &PackageMoniker<'_>,
        hash: i64,
        access: &RepoAccess,
        limit: i64,
    ) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {} FROM codeintel_package_monikers m
             JOIN codeintel_symbol_postings p ON p.upload_id = m.upload_id
                AND p.symbol_hash = $5 AND p.flags & 1 <> 0
             JOIN codeintel_uploads u ON u.id = m.upload_id
             JOIN repo r ON r.id = u.repository_id AND r.deleted_at IS NULL
             WHERE m.kind = 'export' AND m.scheme = $1 AND m.manager = $2 AND m.name = $3
               AND m.version = $4 AND u.state = 'completed'
               AND ($6::int[] IS NULL OR u.repository_id = ANY($6))
             ORDER BY u.uploaded_at DESC, u.id DESC LIMIT $7",
            prefixed_columns("u")
        ))
        .bind(&package.scheme)
        .bind(&package.manager)
        .bind(&package.name)
        .bind(&package.version)
        .bind(hash)
        .bind(access.readable_ids())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Versions of the package (`scheme`, `manager`, `name`) other than `except` that completed
    /// uploads in repositories `access` may read export (with `exports_only`) or export or
    /// import, the most recently uploaded first.
    #[allow(clippy::too_many_arguments)]
    pub async fn package_versions(
        &self,
        scheme: &str,
        manager: &str,
        name: &str,
        except: &str,
        exports_only: bool,
        access: &RepoAccess,
        limit: i64,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT m.version FROM codeintel_package_monikers m
             JOIN codeintel_uploads u ON u.id = m.upload_id
             JOIN repo r ON r.id = u.repository_id AND r.deleted_at IS NULL
             WHERE m.scheme = $1 AND m.manager = $2 AND m.name = $3 AND m.version <> $4
               AND (m.kind = 'export' OR NOT $5) AND u.state = 'completed'
               AND ($6::int[] IS NULL OR u.repository_id = ANY($6))
             GROUP BY m.version ORDER BY max(u.uploaded_at) DESC, m.version DESC LIMIT $7",
        )
        .bind(scheme)
        .bind(manager)
        .bind(name)
        .bind(except)
        .bind(exports_only)
        .bind(access.readable_ids())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The position of a queued upload in the processing queue (1 = next).
    pub async fn place_in_queue(&self, id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT n FROM (
                SELECT id, row_number() OVER (ORDER BY queued_at, id) AS n
                FROM codeintel_uploads WHERE state = 'queued') q
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The distinct indexers of a repository's uploads (or of all).
    pub async fn indexers(&self, repo: Option<RepoId>) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT indexer FROM codeintel_uploads
             WHERE ($1::int IS NULL OR repository_id = $1) AND indexer <> ''
               AND state NOT IN ('deleting', 'deleted')
             ORDER BY indexer",
        )
        .bind(repo.map(|r| r.get()))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks uploads for deletion (any state but `deleting`/`deleted`); returns how many.
    /// A `processing` upload is canceled first by its worker's next heartbeat.
    pub async fn mark_deleting(&self, ids: &[i64], reason: &str) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE codeintel_uploads SET state = 'deleting', deleted_at = now(),
                deleted_reason = $2, cancel = true
             WHERE id = ANY($1) AND state NOT IN ('deleting', 'deleted')",
        )
        .bind(ids)
        .bind(reason)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Marks uploads as replaceable by a new auto-indexing job.
    pub async fn mark_should_reindex(&self, ids: &[i64]) -> Result<u64> {
        Ok(
            sqlx::query("UPDATE codeintel_uploads SET should_reindex = true WHERE id = ANY($1)")
                .bind(ids)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Queues failed or completed uploads for processing again.
    pub async fn requeue(&self, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE codeintel_uploads SET state = 'queued', queued_at = now(), started_at = NULL,
                finished_at = NULL, failure_message = NULL, num_failures = 0, num_resets = 0,
                cancel = false, process_after = NULL
             WHERE id = ANY($1) AND state IN ('completed', 'errored', 'failed', 'canceled')
               AND upload_size IS NOT NULL",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        notify_enqueued(&mut *tx, UPLOADS_TABLE).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Uploads waiting for the janitor to delete their blobs.
    pub async fn deleting(&self, limit: i64) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_uploads WHERE state = 'deleting'
             ORDER BY deleted_at LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks a `deleting` upload whose blobs are gone as `deleted`, dropping its symbol
    /// postings and package monikers.
    pub async fn mark_deleted(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE codeintel_uploads SET state = 'deleted', graph_key = NULL
             WHERE id = $1 AND state = 'deleting'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            sqlx::query("DELETE FROM codeintel_symbol_postings WHERE upload_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM codeintel_package_monikers WHERE upload_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(n == 1)
    }

    /// Marks uploads stuck in `uploading` since before `before` (abandoned multipart uploads)
    /// for deletion; returns how many.
    pub async fn mark_abandoned(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE codeintel_uploads SET state = 'deleting', deleted_at = now(),
                deleted_reason = 'abandoned'
             WHERE state = 'uploading' AND uploaded_at < $1",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Marks the uploads of deleted repositories for deletion; returns how many.
    pub async fn mark_repo_deleted(&self) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE codeintel_uploads u SET state = 'deleting', deleted_at = now(),
                deleted_reason = 'repository deleted', cancel = true
             FROM repo r
             WHERE r.id = u.repository_id AND r.deleted_at IS NOT NULL
               AND u.state NOT IN ('deleting', 'deleted')",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Of `ids`, those that still have a row not yet `deleted` (to find orphaned blobs).
    pub async fn live_ids(&self, ids: &[i64]) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM codeintel_uploads WHERE id = ANY($1) AND state <> 'deleted'
             ORDER BY id",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Picks up to `limit` repositories with unexpired completed uploads that the retention
    /// expirer last scanned before `scanned_before` (never-scanned first), and records `now`
    /// as their scan time.
    pub async fn repos_for_retention_scan(
        &self,
        scanned_before: DateTime<Utc>,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<RepoId>> {
        let ids: Vec<i32> = sqlx::query_scalar(
            "WITH candidates AS (
                SELECT u.repository_id, s.scanned_at
                FROM codeintel_uploads u
                JOIN repo r ON r.id = u.repository_id AND r.deleted_at IS NULL
                LEFT JOIN codeintel_retention_scans s ON s.repository_id = u.repository_id
                WHERE u.state = 'completed' AND NOT u.expired
                  AND (s.scanned_at IS NULL OR s.scanned_at < $1)
                GROUP BY u.repository_id, s.scanned_at
                ORDER BY s.scanned_at NULLS FIRST, u.repository_id
                LIMIT $3)
             INSERT INTO codeintel_retention_scans (repository_id, scanned_at)
             SELECT repository_id, $2 FROM candidates
             ON CONFLICT (repository_id) DO UPDATE SET scanned_at = EXCLUDED.scanned_at
             RETURNING repository_id",
        )
        .bind(scanned_before)
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut ids: Vec<RepoId> = ids.into_iter().map(RepoId).collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// Up to `limit` unexpired completed uploads of `repo` not checked for retention since
    /// `scanned_before`, oldest first.
    pub async fn uploads_for_retention(
        &self,
        repo: RepoId,
        scanned_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Upload>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_uploads
             WHERE repository_id = $1 AND state = 'completed' AND NOT expired
               AND (last_retention_scan_at IS NULL OR last_retention_scan_at < $2)
             ORDER BY uploaded_at, id LIMIT $3"
        ))
        .bind(repo.get())
        .bind(scanned_before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records a retention check at `now`: the `protected` uploads stay, the `expired` ones
    /// are marked for the janitor.
    pub async fn update_retention(
        &self,
        protected: &[i64],
        expired: &[i64],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE codeintel_uploads SET last_retention_scan_at = $2 WHERE id = ANY($1)")
            .bind(protected)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE codeintel_uploads SET expired = true, last_retention_scan_at = $2
             WHERE id = ANY($1) AND state = 'completed'",
        )
        .bind(expired)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks expired uploads for deletion, except those another unexpired completed upload
    /// depends on (it imports a package they export, so its cross-repository definitions
    /// resolve through them); returns how many.
    pub async fn mark_expired_unreferenced(&self) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE codeintel_uploads u SET state = 'deleting', deleted_at = now(),
                deleted_reason = 'expired'
             WHERE u.state = 'completed' AND u.expired AND NOT EXISTS (
                SELECT 1 FROM codeintel_package_monikers e
                JOIN codeintel_package_monikers i ON i.kind = 'import' AND i.scheme = e.scheme
                    AND i.manager = e.manager AND i.name = e.name AND i.version = e.version
                JOIN codeintel_uploads d ON d.id = i.upload_id AND d.id <> u.id
                    AND d.state = 'completed' AND NOT d.expired
                WHERE e.upload_id = u.id AND e.kind = 'export')",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Deletes the rows of uploads `deleted` before `before`; returns how many.
    pub async fn purge_deleted(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(
            sqlx::query(
                "DELETE FROM codeintel_uploads WHERE state = 'deleted' AND deleted_at < $1",
            )
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_root;

    #[test]
    fn roots() {
        assert_eq!(normalize_root("").as_deref(), Some(""));
        assert_eq!(normalize_root(".").as_deref(), Some(""));
        assert_eq!(normalize_root("/").as_deref(), Some(""));
        assert_eq!(normalize_root("lib").as_deref(), Some("lib/"));
        assert_eq!(normalize_root("./a//b/").as_deref(), Some("a/b/"));
        assert_eq!(normalize_root("a/../b"), None);
    }
}
