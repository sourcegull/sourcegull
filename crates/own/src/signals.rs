//! Ownership signals and analytics (migration `20261116000001_own_signals`).
//!
//! - **Configuration** ([`SignalConfig`]): `recent-contributors`, `recent-views` and
//!   `analytics` can each be enabled and excluded for repositories by glob patterns over
//!   repository names (`ownSignalConfigurations`). The indexers and the
//!   ownership list honour them.
//! - **Recent contributors**: the commits of the last [`RECENT_WINDOW_DAYS`] days reachable
//!   from HEAD (from the repo node's commit metadata log and changes file), counted per author
//!   and path prefix (the file, each directory above it and the repository root). The store
//!   keeps the window's commits, so each run applies only the difference ([`deltas`]): commits
//!   new in the window are added, commits that left it (or HEAD's history) are subtracted.
//!   Postgres keeps the aggregate: the ownership panel reads one indexed row range per path
//!   without a call to the repo node, and analytics and resolution to users join it there.
//! - **Recent views**: `ViewBlob` events in `event_logs` (the file page logs them), counted per
//!   viewer and file over the same window, read incrementally after a cursor; events leaving
//!   the window are subtracted.
//! - **Analytics** ([`path_stats`]): files of the default branch with a CODEOWNERS owner, an
//!   assigned owner or either, per directory.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_db::{Db, DbError};
use sourcegull_types::{RepoId, UserId};
use sqlx::{PgPool, Row};

use crate::filter::RepoOwners;

type Result<T> = std::result::Result<T, DbError>;

/// The recent contributors signal.
pub const RECENT_CONTRIBUTORS: &str = "recent-contributors";
/// The recent views signal.
pub const RECENT_VIEWS: &str = "recent-views";
/// Ownership analytics.
pub const ANALYTICS: &str = "analytics";
/// The window of the recent signals.
pub const RECENT_WINDOW_DAYS: i64 = 90;
/// The event the file page logs when a file is viewed.
pub const VIEW_BLOB_EVENT: &str = "ViewBlob";

/// The start of the recent window at `now`.
pub fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now - TimeDelta::days(RECENT_WINDOW_DAYS)
}

// ------------------------------------------------------------------ configuration

/// The configuration of one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalConfig {
    pub id: i32,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// Glob patterns over repository names (`*` matches any run of characters).
    pub excluded_repo_patterns: Vec<String>,
}

impl SignalConfig {
    /// Whether the signal is computed and shown for `repo`.
    pub fn applies_to(&self, repo: &str) -> bool {
        self.enabled && !self.excludes(repo)
    }

    /// Whether `repo` matches an excluded pattern.
    pub fn excludes(&self, repo: &str) -> bool {
        self.excluded_repo_patterns.iter().any(|p| {
            globset::GlobBuilder::new(p)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .is_ok_and(|g| g.compile_matcher().is_match(repo))
        })
    }
}

/// All signal configurations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalConfigs(pub Vec<SignalConfig>);

impl SignalConfigs {
    /// The configuration named `name`.
    pub fn get(&self, name: &str) -> Option<&SignalConfig> {
        self.0.iter().find(|c| c.name == name)
    }

    /// Whether signal `name` applies to `repo`.
    pub fn applies(&self, name: &str, repo: &str) -> bool {
        self.get(name).is_some_and(|c| c.applies_to(repo))
    }
}

/// A change to one configuration (`updateOwnSignalConfigurations`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalConfigUpdate {
    pub name: String,
    pub enabled: bool,
    /// `None` keeps the current patterns.
    pub excluded_repo_patterns: Option<Vec<String>>,
}

/// Whether `pattern` is a valid exclusion pattern.
pub fn valid_pattern(pattern: &str) -> bool {
    !pattern.trim().is_empty() && globset::Glob::new(pattern).is_ok()
}

// ------------------------------------------------------------------ aggregation

/// One commit of the recent window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    /// Hex commit id.
    pub commit: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: DateTime<Utc>,
    /// Files changed.
    pub paths: Vec<String>,
}

/// An author as written in commits.
pub type AuthorKey = (String, String);

/// The prefixes a commit counts for: each file, each directory above it and the repository
/// root (`""`), once each.
pub fn prefixes<'a>(paths: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    for p in paths {
        out.insert(p);
        let mut at = p.len();
        while let Some(i) = p[..at].rfind('/') {
            if !out.insert(&p[..i]) {
                break;
            }
            at = i;
        }
        out.insert("");
    }
    out
}

/// Changes of the aggregate (per path prefix and author) for adding and removing commits.
pub fn deltas<'a>(
    added: impl IntoIterator<Item = &'a Contribution>,
    removed: impl IntoIterator<Item = &'a Contribution>,
) -> HashMap<(String, AuthorKey), i64> {
    let mut out: HashMap<(String, AuthorKey), i64> = HashMap::new();
    for (sign, commits) in [
        (1i64, added.into_iter().collect::<Vec<_>>()),
        (-1, removed.into_iter().collect()),
    ] {
        for c in commits {
            let author = (c.author_name.clone(), c.author_email.clone());
            for p in prefixes(c.paths.iter().map(String::as_str)) {
                *out.entry((p.to_owned(), author.clone())).or_default() += sign;
            }
        }
    }
    out.retain(|_, d| *d != 0);
    out
}

/// An aggregate in memory: commits per path prefix and author.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aggregate(pub BTreeMap<(String, AuthorKey), i64>);

impl Aggregate {
    /// The aggregate of `commits`, computed from scratch.
    pub fn full<'a>(commits: impl IntoIterator<Item = &'a Contribution>) -> Self {
        let mut a = Aggregate::default();
        a.apply(&deltas(commits, []));
        a
    }

    /// Applies [`deltas`]; entries reaching zero are dropped.
    pub fn apply(&mut self, deltas: &HashMap<(String, AuthorKey), i64>) {
        for (k, d) in deltas {
            let v = self.0.entry(k.clone()).or_default();
            *v += d;
            if *v == 0 {
                self.0.remove(k);
            }
        }
    }
}

/// What a contributions sync did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub added: usize,
    pub removed: usize,
}

/// A recent contributor of a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentContributor {
    pub name: String,
    pub email: String,
    /// Commits in the window that changed the path (or something below it).
    pub count: i64,
}

// ------------------------------------------------------------------ analytics

/// Ownership coverage of a directory (or the repository, `""`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathStats {
    pub path: String,
    pub total_files: i64,
    /// Files with an owner from the CODEOWNERS file.
    pub codeowned_files: i64,
    /// Files with an assigned owner (on the file or above it).
    pub assigned_files: i64,
    /// Files with either.
    pub owned_files: i64,
    pub updated_at: Option<DateTime<Utc>>,
}

impl PathStats {
    /// Adds another directory's numbers.
    pub fn add(&mut self, o: &PathStats) {
        self.total_files += o.total_files;
        self.codeowned_files += o.codeowned_files;
        self.assigned_files += o.assigned_files;
        self.owned_files += o.owned_files;
        self.updated_at = self.updated_at.max(o.updated_at);
    }
}

/// The coverage of every directory of `files` (paths of the default branch's files) under
/// `owners`, the root first.
pub fn path_stats<'a>(
    files: impl IntoIterator<Item = &'a str>,
    owners: &RepoOwners,
) -> Vec<PathStats> {
    let mut dirs: BTreeMap<&str, PathStats> = BTreeMap::new();
    dirs.insert("", PathStats::default());
    for f in files {
        let codeowned = owners
            .codeowners
            .as_ref()
            .is_some_and(|c| !c.owners(f).is_empty());
        let assigned = owners
            .assigned
            .iter()
            .any(|a| crate::filter::path_covers(&a.path, f));
        let mut at = f.len();
        let bump = |s: &mut PathStats| {
            s.total_files += 1;
            s.codeowned_files += i64::from(codeowned);
            s.assigned_files += i64::from(assigned);
            s.owned_files += i64::from(codeowned || assigned);
        };
        bump(dirs.get_mut("").expect("root"));
        while let Some(i) = f[..at].rfind('/') {
            bump(dirs.entry(&f[..i]).or_default());
            at = i;
        }
    }
    dirs.into_iter()
        .map(|(path, s)| PathStats {
            path: path.to_owned(),
            ..s
        })
        .collect()
}

// ------------------------------------------------------------------ store

/// The signals and analytics tables.
#[derive(Debug, Clone)]
pub struct SignalStore {
    pool: PgPool,
}

impl SignalStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    // ----------------------------------------------------------- configuration

    /// All signal configurations by id.
    pub async fn configs(&self) -> Result<SignalConfigs> {
        let rows = sqlx::query(
            "SELECT id, name, description, enabled, excluded_repo_patterns \
             FROM own_signal_configurations ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(SignalConfig {
                id: r.try_get("id")?,
                name: r.try_get("name")?,
                description: r.try_get("description")?,
                enabled: r.try_get("enabled")?,
                excluded_repo_patterns: r.try_get("excluded_repo_patterns")?,
            });
        }
        Ok(SignalConfigs(out))
    }

    /// Applies configuration changes in one transaction. Unknown names and invalid
    /// patterns fail with `Invalid`.
    pub async fn update_configs(&self, updates: &[SignalConfigUpdate]) -> Result<SignalConfigs> {
        let mut tx = self.pool.begin().await?;
        for u in updates {
            if let Some(ps) = &u.excluded_repo_patterns
                && !ps.iter().all(|p| valid_pattern(p))
            {
                return Err(DbError::Invalid("invalid excluded repository pattern"));
            }
            let r = sqlx::query(
                "UPDATE own_signal_configurations SET enabled = $2, \
                     excluded_repo_patterns = COALESCE($3, excluded_repo_patterns) \
                 WHERE name = $1",
            )
            .bind(&u.name)
            .bind(u.enabled)
            .bind(&u.excluded_repo_patterns)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() == 0 {
                return Err(DbError::Invalid("unknown signal configuration"));
            }
        }
        tx.commit().await?;
        self.configs().await
    }

    // ----------------------------------------------------------- recent contributors

    /// Makes the stored window of `repo` equal to `window` (the commits authored since the
    /// window start reachable from HEAD), applying only the difference to the aggregate.
    pub async fn sync_contributions(
        &self,
        repo: RepoId,
        window: &[Contribution],
    ) -> Result<SyncStats> {
        let mut tx = self.pool.begin().await?;
        // One sync per repository at a time.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('own_recent_contribution'), $1)")
            .bind(repo.get())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query(
            "SELECT c.commit_id, a.name, a.email, c.committed_at, c.paths \
             FROM own_signal_recent_contribution c \
             JOIN commit_authors a ON a.id = c.commit_author_id WHERE c.repo_id = $1",
        )
        .bind(repo.get())
        .fetch_all(&mut *tx)
        .await?;
        let mut stored = HashMap::with_capacity(rows.len());
        for r in rows {
            let c = Contribution {
                commit: r.try_get("commit_id")?,
                author_name: r.try_get("name")?,
                author_email: r.try_get("email")?,
                authored_at: r.try_get("committed_at")?,
                paths: r.try_get("paths")?,
            };
            stored.insert(c.commit.clone(), c);
        }
        let current: HashSet<&str> = window.iter().map(|c| c.commit.as_str()).collect();
        let mut seen = HashSet::new();
        let added: Vec<&Contribution> = window
            .iter()
            .filter(|c| !stored.contains_key(&c.commit) && seen.insert(c.commit.as_str()))
            .collect();
        let removed: Vec<&Contribution> = stored
            .values()
            .filter(|c| !current.contains(c.commit.as_str()))
            .collect();
        if added.is_empty() && removed.is_empty() {
            tx.commit().await?;
            return Ok(SyncStats::default());
        }

        // Author ids of every author involved.
        let mut authors: Vec<AuthorKey> = added
            .iter()
            .chain(&removed)
            .map(|c| (c.author_name.clone(), c.author_email.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        authors.dedup();
        let (names, emails): (Vec<String>, Vec<String>) = authors.iter().cloned().unzip();
        let rows = sqlx::query(
            "WITH input AS (SELECT * FROM unnest($1::text[], $2::text[]) AS t(name, email)), \
             ins AS (INSERT INTO commit_authors (name, email) SELECT name, email FROM input \
                     ON CONFLICT (name, email) DO NOTHING RETURNING id, name, email) \
             SELECT id, name, email FROM ins \
             UNION ALL SELECT a.id, a.name, a.email FROM commit_authors a \
               JOIN input i ON i.name = a.name AND i.email = a.email",
        )
        .bind(&names)
        .bind(&emails)
        .fetch_all(&mut *tx)
        .await?;
        let mut author_ids: HashMap<AuthorKey, i64> = HashMap::new();
        for r in rows {
            author_ids.insert((r.try_get("name")?, r.try_get("email")?), r.try_get("id")?);
        }
        let author_id = |c: &Contribution| -> Result<i64> {
            author_ids
                .get(&(c.author_name.clone(), c.author_email.clone()))
                .copied()
                .ok_or(DbError::Invalid("commit author not stored"))
        };

        if !removed.is_empty() {
            let ids: Vec<&str> = removed.iter().map(|c| c.commit.as_str()).collect();
            sqlx::query(
                "DELETE FROM own_signal_recent_contribution \
                 WHERE repo_id = $1 AND commit_id = ANY($2)",
            )
            .bind(repo.get())
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        }
        for chunk in added.chunks(1000) {
            let mut ids = Vec::new();
            let mut authors = Vec::new();
            let mut times = Vec::new();
            let mut paths = Vec::new();
            for c in chunk {
                ids.push(c.commit.as_str());
                authors.push(author_id(c)?);
                times.push(c.authored_at);
                // A text[][] needs equal lengths: store the list as one JSON array.
                paths.push(serde_json::to_string(&c.paths).unwrap_or_else(|_| "[]".into()));
            }
            sqlx::query(
                "INSERT INTO own_signal_recent_contribution \
                     (repo_id, commit_id, commit_author_id, committed_at, paths) \
                 SELECT $1, t.id, t.author, t.at, \
                     ARRAY(SELECT jsonb_array_elements_text(t.paths::jsonb)) \
                 FROM unnest($2::text[], $3::bigint[], $4::timestamptz[], $5::text[]) \
                     AS t(id, author, at, paths)",
            )
            .bind(repo.get())
            .bind(&ids)
            .bind(&authors)
            .bind(&times)
            .bind(&paths)
            .execute(&mut *tx)
            .await?;
        }

        let d = deltas(added.iter().copied(), removed.iter().copied());
        let mut entries: Vec<(&String, i64, i64)> = Vec::with_capacity(d.len());
        for ((path, author), delta) in &d {
            let id = author_ids
                .get(author)
                .copied()
                .ok_or(DbError::Invalid("commit author not stored"))?;
            entries.push((path, id, *delta));
        }
        entries.sort();
        for chunk in entries.chunks(5000) {
            let paths: Vec<&str> = chunk.iter().map(|e| e.0.as_str()).collect();
            let ids: Vec<i64> = chunk.iter().map(|e| e.1).collect();
            let deltas: Vec<i64> = chunk.iter().map(|e| e.2).collect();
            // Positive and negative changes merge into the existing counts; rows at zero go.
            sqlx::query(
                "WITH d AS (SELECT * FROM unnest($2::text[], $3::bigint[], $4::bigint[]) \
                     AS t(path, author, delta)), \
                 upd AS (UPDATE own_aggregate_recent_contribution a \
                     SET contributions_count = a.contributions_count + d.delta FROM d \
                     WHERE a.repo_id = $1 AND a.path = d.path AND a.commit_author_id = d.author \
                       AND a.contributions_count + d.delta > 0 \
                     RETURNING a.path, a.commit_author_id), \
                 del AS (DELETE FROM own_aggregate_recent_contribution a USING d \
                     WHERE a.repo_id = $1 AND a.path = d.path AND a.commit_author_id = d.author \
                       AND a.contributions_count + d.delta <= 0 \
                     RETURNING a.path, a.commit_author_id) \
                 INSERT INTO own_aggregate_recent_contribution \
                     (repo_id, path, commit_author_id, contributions_count) \
                 SELECT $1, d.path, d.author, d.delta FROM d \
                 WHERE d.delta > 0 \
                   AND NOT EXISTS (SELECT 1 FROM own_aggregate_recent_contribution a \
                       WHERE a.repo_id = $1 AND a.path = d.path \
                         AND a.commit_author_id = d.author)",
            )
            .bind(repo.get())
            .bind(&paths)
            .bind(&ids)
            .bind(&deltas)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(SyncStats {
            added: added.len(),
            removed: removed.len(),
        })
    }

    /// Drops the recent contributions of `repo` (signal disabled or repository excluded).
    pub async fn clear_contributions(&self, repo: RepoId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for table in [
            "own_aggregate_recent_contribution",
            "own_signal_recent_contribution",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE repo_id = $1"))
                .bind(repo.get())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The aggregate of `repo` as stored, for comparisons with [`Aggregate`].
    pub async fn contribution_aggregate(&self, repo: RepoId) -> Result<Aggregate> {
        let rows = sqlx::query(
            "SELECT g.path, a.name, a.email, g.contributions_count \
             FROM own_aggregate_recent_contribution g \
             JOIN commit_authors a ON a.id = g.commit_author_id WHERE g.repo_id = $1",
        )
        .bind(repo.get())
        .fetch_all(&self.pool)
        .await?;
        let mut out = Aggregate::default();
        for r in rows {
            let count: i32 = r.try_get("contributions_count")?;
            out.0.insert(
                (
                    r.try_get("path")?,
                    (r.try_get("name")?, r.try_get("email")?),
                ),
                i64::from(count),
            );
        }
        Ok(out)
    }

    /// The authors with the most recent commits to `path` (a file or directory; `""` for the
    /// repository), most first.
    pub async fn recent_contributors(
        &self,
        repo: RepoId,
        path: &str,
        limit: i64,
    ) -> Result<Vec<RecentContributor>> {
        let rows = sqlx::query(
            "SELECT a.name, a.email, g.contributions_count \
             FROM own_aggregate_recent_contribution g \
             JOIN commit_authors a ON a.id = g.commit_author_id \
             WHERE g.repo_id = $1 AND g.path = $2 \
             ORDER BY g.contributions_count DESC, a.name, a.email LIMIT $3",
        )
        .bind(repo.get())
        .bind(path)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let count: i32 = r.try_get("contributions_count")?;
                Ok(RecentContributor {
                    name: r.try_get("name")?,
                    email: r.try_get("email")?,
                    count: i64::from(count),
                })
            })
            .collect()
    }

    // ----------------------------------------------------------- recent views

    /// Counts the `ViewBlob` events logged since the last run and subtracts the counted
    /// events that left the window. Returns the number of events added.
    pub async fn index_views(&self, now: DateTime<Utc>) -> Result<u64> {
        let start = window_start(now);
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query("SELECT last_event_id, window_start FROM own_recent_view_state FOR UPDATE")
                .fetch_one(&mut *tx)
                .await?;
        let last: i64 = row.try_get("last_event_id")?;
        let old_start: Option<DateTime<Utc>> = row.try_get("window_start")?;
        let max: i64 = sqlx::query_scalar("SELECT COALESCE(max(id), 0) FROM event_logs")
            .fetch_one(&mut *tx)
            .await?;
        // Views of a file per repository, user and path, for events in an id range
        // (`$1 < id <= $2`) and a time range (`$3 <= timestamp < $4`, open when NULL).
        const VIEWS: &str = "SELECT r.id AS repo_id, e.argument->>'filePath' AS path, \
                 e.user_id AS viewer_id, count(*)::integer AS n \
             FROM event_logs e \
             JOIN repo r ON r.name = e.argument->>'repoName' AND r.deleted_at IS NULL \
             JOIN users u ON u.id = e.user_id \
             WHERE e.name = 'ViewBlob' AND e.id > $1 AND e.id <= $2 \
               AND ($3::timestamptz IS NULL OR e.timestamp >= $3) \
               AND ($4::timestamptz IS NULL OR e.timestamp < $4) \
               AND COALESCE(e.argument->>'filePath', '') <> '' \
             GROUP BY 1, 2, 3";
        // Counted events that left the window are subtracted: rows at zero go first, then
        // the others are lowered.
        if last > 0 && old_start.is_none_or(|o| start > o) {
            for stmt in [
                "DELETE FROM own_aggregate_recent_view a USING d \
                 WHERE a.repo_id = d.repo_id AND a.path = d.path \
                   AND a.viewer_id = d.viewer_id AND a.views_count <= d.n",
                "UPDATE own_aggregate_recent_view a SET views_count = a.views_count - d.n \
                 FROM d WHERE a.repo_id = d.repo_id AND a.path = d.path \
                   AND a.viewer_id = d.viewer_id",
            ] {
                sqlx::query(&format!("WITH d AS ({VIEWS}) {stmt}"))
                    .bind(0i64)
                    .bind(last)
                    .bind(old_start)
                    .bind(start)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let added = sqlx::query(&format!(
            "INSERT INTO own_aggregate_recent_view (repo_id, path, viewer_id, views_count) \
             SELECT repo_id, path, viewer_id, n FROM ({VIEWS}) d \
             ON CONFLICT (repo_id, path, viewer_id) DO UPDATE \
                 SET views_count = own_aggregate_recent_view.views_count + EXCLUDED.views_count"
        ))
        .bind(last)
        .bind(max)
        .bind(Some(start))
        .bind(None::<DateTime<Utc>>)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("UPDATE own_recent_view_state SET last_event_id = $1, window_start = $2")
            .bind(max)
            .bind(start)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(added)
    }

    /// The users who viewed `path` most, most first.
    pub async fn recent_viewers(
        &self,
        repo: RepoId,
        path: &str,
        limit: i64,
    ) -> Result<Vec<(UserId, i64)>> {
        let rows = sqlx::query(
            "SELECT viewer_id, views_count FROM own_aggregate_recent_view \
             WHERE repo_id = $1 AND path = $2 \
             ORDER BY views_count DESC, viewer_id LIMIT $3",
        )
        .bind(repo.get())
        .bind(path)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let n: i32 = r.try_get("views_count")?;
                Ok((UserId(r.try_get("viewer_id")?), i64::from(n)))
            })
            .collect()
    }

    // ----------------------------------------------------------- analytics

    /// Replaces the coverage of `repo`.
    pub async fn save_path_stats(&self, repo: RepoId, stats: &[PathStats]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ownership_path_stats WHERE repo_id = $1")
            .bind(repo.get())
            .execute(&mut *tx)
            .await?;
        for chunk in stats.chunks(5000) {
            let col = |f: fn(&PathStats) -> i64| -> Vec<i64> { chunk.iter().map(f).collect() };
            let paths: Vec<&str> = chunk.iter().map(|s| s.path.as_str()).collect();
            sqlx::query(
                "INSERT INTO ownership_path_stats (repo_id, path, total_files, codeowned_files, \
                     assigned_files, owned_files) \
                 SELECT $1, * FROM unnest($2::text[], $3::bigint[], $4::bigint[], \
                     $5::bigint[], $6::bigint[])",
            )
            .bind(repo.get())
            .bind(&paths)
            .bind(col(|s| s.total_files))
            .bind(col(|s| s.codeowned_files))
            .bind(col(|s| s.assigned_files))
            .bind(col(|s| s.owned_files))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drops the coverage of `repo`.
    pub async fn clear_path_stats(&self, repo: RepoId) -> Result<()> {
        sqlx::query("DELETE FROM ownership_path_stats WHERE repo_id = $1")
            .bind(repo.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The coverage of a directory of `repo` (`""` for the repository).
    pub async fn path_stats(&self, repo: RepoId, path: &str) -> Result<Option<PathStats>> {
        let row = sqlx::query(&format!(
            "SELECT {STATS_COLUMNS} FROM ownership_path_stats s \
             WHERE s.repo_id = $1 AND s.path = $2"
        ))
        .bind(repo.get())
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(stats_from_row).transpose()
    }

    /// The coverage of all repositories together (the repositories' roots summed).
    pub async fn instance_stats(&self) -> Result<PathStats> {
        let row = sqlx::query(
            "SELECT '' AS path, COALESCE(sum(total_files), 0)::bigint AS total_files, \
                 COALESCE(sum(codeowned_files), 0)::bigint AS codeowned_files, \
                 COALESCE(sum(assigned_files), 0)::bigint AS assigned_files, \
                 COALESCE(sum(owned_files), 0)::bigint AS owned_files, \
                 max(s.updated_at) AS updated_at \
             FROM ownership_path_stats s JOIN repo r ON r.id = s.repo_id \
             WHERE s.path = '' AND r.deleted_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        stats_from_row(&row)
    }

    /// The coverage of each repository (its root), by name, after `after` (a name).
    pub async fn repo_stats(
        &self,
        limit: i64,
        after: Option<&str>,
    ) -> Result<Vec<(RepoId, String, PathStats)>> {
        let rows = sqlx::query(&format!(
            "SELECT s.repo_id, r.name, {STATS_COLUMNS} FROM ownership_path_stats s \
             JOIN repo r ON r.id = s.repo_id \
             WHERE s.path = '' AND r.deleted_at IS NULL AND ($2::text IS NULL OR r.name > $2) \
             ORDER BY r.name LIMIT $1"
        ))
        .bind(limit)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    RepoId(r.try_get("repo_id")?),
                    r.try_get("name")?,
                    stats_from_row(r)?,
                ))
            })
            .collect()
    }

    /// Repositories with coverage.
    pub async fn count_repo_stats(&self) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM ownership_path_stats s JOIN repo r ON r.id = s.repo_id \
             WHERE s.path = '' AND r.deleted_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    // ----------------------------------------------------------- index state

    /// Records that `repo` was indexed for `job_type` at `at`.
    pub async fn mark_indexed(
        &self,
        repo: RepoId,
        job_type: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO own_repo_index_state (repo_id, job_type, indexed_at) VALUES ($1, $2, $3) \
             ON CONFLICT (repo_id, job_type) DO UPDATE SET indexed_at = EXCLUDED.indexed_at",
        )
        .bind(repo.get())
        .bind(job_type)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

const STATS_COLUMNS: &str = "s.path, s.total_files::bigint AS total_files, s.codeowned_files::bigint AS codeowned_files, \
     s.assigned_files::bigint AS assigned_files, s.owned_files::bigint AS owned_files, \
     s.updated_at";

fn stats_from_row(r: &sqlx::postgres::PgRow) -> Result<PathStats> {
    Ok(PathStats {
        path: r.try_get("path")?,
        total_files: r.try_get("total_files")?,
        codeowned_files: r.try_get("codeowned_files")?,
        assigned_files: r.try_get("assigned_files")?,
        owned_files: r.try_get("owned_files")?,
        updated_at: r.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::codeowners::CompiledCodeowners;
    use crate::filter::AssignedEntry;

    fn c(commit: &str, author: &str, paths: &[&str]) -> Contribution {
        Contribution {
            commit: commit.into(),
            author_name: author.into(),
            author_email: format!("{}@example.com", author.to_lowercase()),
            authored_at: DateTime::<Utc>::UNIX_EPOCH,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn prefixes_cover_files_directories_and_root() {
        let p: Vec<&str> = prefixes(["a/b/c.go", "a/b/d.go", "e.md"])
            .into_iter()
            .collect();
        assert_eq!(p, vec!["", "a", "a/b", "a/b/c.go", "a/b/d.go", "e.md"]);
    }

    #[test]
    fn counts_commits_per_prefix() {
        let a = Aggregate::full(&[
            c("1", "Ann", &["src/a.rs", "src/b.rs"]),
            c("2", "Ann", &["src/a.rs"]),
            c("3", "Bob", &["README"]),
        ]);
        let get = |p: &str, who: &str| {
            a.0.get(&(
                p.to_owned(),
                (
                    who.to_owned(),
                    format!("{}@example.com", who.to_lowercase()),
                ),
            ))
            .copied()
        };
        // A commit counts once for a directory however many files it changed there.
        assert_eq!(get("src", "Ann"), Some(2));
        assert_eq!(get("src/b.rs", "Ann"), Some(1));
        assert_eq!(get("", "Ann"), Some(2));
        assert_eq!(get("", "Bob"), Some(1));
        assert_eq!(get("src", "Bob"), None);
    }

    /// Windows sliding over a history: applying each step's difference gives the aggregate
    /// computed from scratch.
    #[test]
    fn incremental_equals_full_recompute() {
        let authors = ["Ann", "Bob", "Cid"];
        let files = ["a/x.go", "a/y.go", "a/b/z.go", "c.md", "d/e/f/g.txt"];
        let history: Vec<Contribution> = (0..60u32)
            .map(|i| {
                let author = authors[(i as usize * 7 + i as usize / 3) % 3];
                let n = 1 + (i as usize * 5) % 3;
                let paths: Vec<&str> = (0..n)
                    .map(|k| files[(i as usize * 3 + k * 2) % 5])
                    .collect();
                c(&format!("{i:040x}"), author, &paths)
            })
            .collect();
        let mut agg = Aggregate::default();
        let mut stored: Vec<&Contribution> = Vec::new();
        for (lo, hi) in [
            (0, 10),
            (3, 25),
            (25, 25),
            (20, 50),
            (49, 60),
            (0, 60),
            (59, 60),
            (60, 60),
        ] {
            let window: Vec<&Contribution> = history[lo..hi].iter().collect();
            let added: Vec<&Contribution> = window
                .iter()
                .copied()
                .filter(|w| !stored.iter().any(|s| s.commit == w.commit))
                .collect();
            let removed: Vec<&Contribution> = stored
                .iter()
                .copied()
                .filter(|s| !window.iter().any(|w| w.commit == s.commit))
                .collect();
            agg.apply(&deltas(added, removed));
            stored = window.clone();
            assert_eq!(agg, Aggregate::full(window), "window {lo}..{hi}");
        }
        assert!(agg.0.is_empty());
    }

    #[test]
    fn coverage_per_directory() {
        let owners = RepoOwners {
            codeowners: Some(Arc::new(CompiledCodeowners::compile("*.go @ann\n"))),
            assigned: vec![AssignedEntry {
                path: "docs".into(),
                owner: "@bob".into(),
            }],
        };
        let stats = path_stats(["main.go", "docs/a.md", "docs/b.go", "lib/c.rs"], &owners);
        let get = |p: &str| stats.iter().find(|s| s.path == p).unwrap();
        let root = get("");
        assert_eq!(
            (
                root.total_files,
                root.codeowned_files,
                root.assigned_files,
                root.owned_files
            ),
            (4, 2, 2, 3)
        );
        let docs = get("docs");
        assert_eq!(
            (docs.total_files, docs.codeowned_files, docs.owned_files),
            (2, 1, 2)
        );
        assert_eq!(get("lib").owned_files, 0);
        assert_eq!(stats.len(), 3);
    }

    #[test]
    fn exclusions() {
        let cfg = SignalConfig {
            id: 1,
            name: RECENT_CONTRIBUTORS.into(),
            description: String::new(),
            enabled: true,
            excluded_repo_patterns: vec!["gitlab.com/acme/*".into()],
        };
        assert!(!cfg.applies_to("gitlab.com/acme/web"));
        assert!(!cfg.applies_to("gitlab.com/acme/sub/deep"));
        assert!(cfg.applies_to("gitlab.com/other/web"));
        assert!(
            !SignalConfig {
                enabled: false,
                ..cfg
            }
            .applies_to("x")
        );
    }
}
