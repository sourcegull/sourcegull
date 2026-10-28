//! The ownership tables (migration `20261115000001_own`): teams and their members, CODEOWNERS
//! files ingested by site admins, and owners assigned to repositories, directories and files.
//!
//! Permission checks are the API's; this store only keeps the data consistent (team names
//! are unique among teams, users and orgs; a team cannot become its own ancestor).

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, DbError};
use sourcegull_types::{RepoId, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::filter::AssignedEntry;

type Result<T> = std::result::Result<T, DbError>;

/// A team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub readonly: bool,
    pub parent_team_id: Option<i64>,
    /// `None` once the creator is deleted.
    pub creator_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The code host connection the team is synced from (a GitLab group,
    /// `own.syncTeamsFromGitLabGroups`); `None` for teams made in Sourcegull.
    pub external_service_id: Option<i64>,
}

pub(crate) const TEAM_COLUMNS: &str = "id, name, display_name, readonly, parent_team_id, \
     creator_id, created_at, updated_at, external_service_id";

pub(crate) fn team_from_row(r: &PgRow) -> sqlx::Result<Team> {
    Ok(Team {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        display_name: r.try_get("display_name")?,
        readonly: r.try_get("readonly")?,
        parent_team_id: r.try_get("parent_team_id")?,
        creator_id: r.try_get::<Option<i64>, _>("creator_id")?.map(UserId),
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        external_service_id: r.try_get("external_service_id")?,
    })
}

/// Whether `name` is a valid team name: alphanumerics, `-`, `_`, `.` and `/` (between
/// non-empty parts, as GitLab group paths), starting with an alphanumeric.
pub fn valid_team_name(name: &str) -> bool {
    name.len() <= 255
        && name.split('/').all(|part| {
            part.chars().next().is_some_and(char::is_alphanumeric)
                && part
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// The fields of a new team.
#[derive(Debug, Clone, Default)]
pub struct NewTeam {
    pub name: String,
    pub display_name: Option<String>,
    pub readonly: bool,
    pub parent_team_id: Option<i64>,
    /// Becomes the first member.
    pub creator_id: Option<UserId>,
}

/// Which teams to list.
#[derive(Debug, Clone, Default)]
pub struct ListTeams {
    /// Case-insensitive substring of the name or display name.
    pub search: Option<String>,
    /// Only the children of this team.
    pub parent: Option<i64>,
    /// Only teams without a parent (ignored with `parent`).
    pub roots_only: bool,
    /// Leave out this team and its descendants.
    pub except_ancestor: Option<i64>,
    /// Only teams this user is a direct member of.
    pub member: Option<UserId>,
    pub limit: i64,
    pub offset: i64,
}

/// A CODEOWNERS file ingested for a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestedCodeowners {
    pub id: i64,
    pub repo_id: RepoId,
    pub contents: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn codeowners_from_row(r: &PgRow) -> sqlx::Result<IngestedCodeowners> {
    Ok(IngestedCodeowners {
        id: r.try_get("id")?,
        repo_id: RepoId(r.try_get("repo_id")?),
        contents: r.try_get("contents")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// Who an assignment names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Assignee {
    User(UserId),
    Team(i64),
}

/// An owner assigned to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: i64,
    pub assignee: Assignee,
    pub repo_id: RepoId,
    /// `""` for the whole repository.
    pub file_path: String,
    pub who_assigned: Option<UserId>,
    pub assigned_at: DateTime<Utc>,
}

/// Normalizes an assigned path: no leading or trailing `/`.
pub fn normalize_path(path: &str) -> String {
    path.trim_matches('/').to_owned()
}

/// The ownership store.
#[derive(Debug, Clone)]
pub struct OwnStore {
    pool: PgPool,
}

impl OwnStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // ------------------------------------------------------------------ teams

    /// Creates a team; its creator becomes a member. Fails with `AlreadyExists` when the name
    /// is taken by a team, a user or an org, and `Invalid` for a bad name.
    pub async fn create_team(&self, t: &NewTeam) -> Result<Team> {
        if !valid_team_name(&t.name) {
            return Err(DbError::Invalid("invalid team name"));
        }
        let mut tx = self.pool.begin().await?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE lower(username) = lower($1) \
                 AND deleted_at IS NULL) \
             OR EXISTS (SELECT 1 FROM orgs WHERE lower(name) = lower($1) AND deleted_at IS NULL)",
        )
        .bind(&t.name)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(DbError::AlreadyExists {
                constraint: "names".to_owned(),
            });
        }
        let row = sqlx::query(&format!(
            "INSERT INTO teams (name, display_name, readonly, parent_team_id, creator_id) \
             VALUES ($1, $2, $3, $4, $5) RETURNING {TEAM_COLUMNS}"
        ))
        .bind(&t.name)
        .bind(t.display_name.as_deref().filter(|d| !d.is_empty()))
        .bind(t.readonly)
        .bind(t.parent_team_id)
        .bind(t.creator_id.map(UserId::get))
        .fetch_one(&mut *tx)
        .await?;
        let team = team_from_row(&row)?;
        if let Some(creator) = t.creator_id {
            sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2)")
                .bind(team.id)
                .bind(creator.get())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(team)
    }

    pub async fn get_team(&self, id: i64) -> Result<Option<Team>> {
        let row = sqlx::query(&format!("SELECT {TEAM_COLUMNS} FROM teams WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(team_from_row).transpose()?)
    }

    /// The team named `name` (case-insensitively).
    pub async fn get_team_by_name(&self, name: &str) -> Result<Option<Team>> {
        let row = sqlx::query(&format!(
            "SELECT {TEAM_COLUMNS} FROM teams WHERE lower(name) = lower($1)"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(team_from_row).transpose()?)
    }

    /// Teams by id (unknown ids are left out).
    pub async fn get_teams(&self, ids: &[i64]) -> Result<Vec<Team>> {
        let rows = sqlx::query(&format!(
            "SELECT {TEAM_COLUMNS} FROM teams WHERE id = ANY($1) ORDER BY id"
        ))
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(team_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    fn team_filter(opts: &ListTeams) -> (String, Vec<TeamBind>) {
        let mut conds = vec!["TRUE".to_owned()];
        let mut binds = Vec::new();
        let mut n = 0;
        let mut next = |b: TeamBind, binds: &mut Vec<TeamBind>| {
            binds.push(b);
            n += 1;
            n
        };
        if let Some(s) = opts.search.as_deref().filter(|s| !s.is_empty()) {
            let i = next(TeamBind::Text(format!("%{}%", escape_like(s))), &mut binds);
            conds.push(format!(
                "(t.name ILIKE ${i} OR coalesce(t.display_name, '') ILIKE ${i})"
            ));
        }
        if let Some(p) = opts.parent {
            let i = next(TeamBind::Int(p), &mut binds);
            conds.push(format!("t.parent_team_id = ${i}"));
        } else if opts.roots_only {
            conds.push("t.parent_team_id IS NULL".to_owned());
        }
        if let Some(a) = opts.except_ancestor {
            let i = next(TeamBind::Int(a), &mut binds);
            conds.push(format!(
                "t.id NOT IN (WITH RECURSIVE d(id) AS (SELECT ${i}::bigint UNION \
                 SELECT c.id FROM teams c JOIN d ON c.parent_team_id = d.id) SELECT id FROM d)"
            ));
        }
        if let Some(u) = opts.member {
            let i = next(TeamBind::Int(u.get()), &mut binds);
            conds.push(format!(
                "EXISTS (SELECT 1 FROM team_members m WHERE m.team_id = t.id AND m.user_id = ${i})"
            ));
        }
        (conds.join(" AND "), binds)
    }

    /// Teams by name.
    pub async fn list_teams(&self, opts: &ListTeams) -> Result<Vec<Team>> {
        let (cond, binds) = Self::team_filter(opts);
        let n = binds.len();
        let sql = format!(
            "SELECT {TEAM_COLUMNS} FROM teams t WHERE {cond} ORDER BY lower(t.name), t.id \
             LIMIT ${} OFFSET ${}",
            n + 1,
            n + 2
        );
        let mut q = sqlx::query(&sql);
        for b in binds {
            q = b.bind(q);
        }
        let rows = q
            .bind(opts.limit)
            .bind(opts.offset)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(team_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    pub async fn count_teams(&self, opts: &ListTeams) -> Result<i64> {
        let (cond, binds) = Self::team_filter(opts);
        let sql = format!("SELECT count(*) FROM teams t WHERE {cond}");
        let mut q = sqlx::query_scalar(&sql);
        for b in binds {
            q = match b {
                TeamBind::Text(s) => q.bind(s),
                TeamBind::Int(i) => q.bind(i),
            };
        }
        Ok(q.fetch_one(&self.pool).await?)
    }

    /// Updates a team: `display_name` `Some(None)` clears it; `parent` `Some(None)` makes it
    /// a root team. Fails with `Invalid` when the new parent is the team or a descendant.
    pub async fn update_team(
        &self,
        id: i64,
        display_name: Option<Option<String>>,
        parent: Option<Option<i64>>,
    ) -> Result<Team> {
        if let Some(Some(p)) = parent {
            let cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE d(id) AS (SELECT $1::bigint UNION \
                 SELECT c.id FROM teams c JOIN d ON c.parent_team_id = d.id) \
                 SELECT EXISTS (SELECT 1 FROM d WHERE id = $2)",
            )
            .bind(id)
            .bind(p)
            .fetch_one(&self.pool)
            .await?;
            if cycle {
                return Err(DbError::Invalid(
                    "a team cannot be a child of itself or its descendants",
                ));
            }
        }
        let row = sqlx::query(&format!(
            "UPDATE teams SET \
                 display_name = CASE WHEN $2 THEN $3 ELSE display_name END, \
                 parent_team_id = CASE WHEN $4 THEN $5 ELSE parent_team_id END, \
                 updated_at = now() \
             WHERE id = $1 RETURNING {TEAM_COLUMNS}"
        ))
        .bind(id)
        .bind(display_name.is_some())
        .bind(display_name.flatten().filter(|d| !d.is_empty()))
        .bind(parent.is_some())
        .bind(parent.flatten())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(team_from_row)
            .transpose()?
            .ok_or(DbError::NotFound("team"))
    }

    /// Deletes a team, its child teams, memberships and assignments.
    pub async fn delete_team(&self, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM teams WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::NotFound("team"));
        }
        Ok(())
    }

    /// Adds members (existing ones are ignored).
    pub async fn add_members(&self, team: i64, users: &[UserId]) -> Result<()> {
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        sqlx::query(
            "INSERT INTO team_members (team_id, user_id) SELECT $1, u FROM unnest($2::bigint[]) u \
             ON CONFLICT DO NOTHING",
        )
        .bind(team)
        .bind(&ids)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_members(&self, team: i64, users: &[UserId]) -> Result<()> {
        let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
        sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = ANY($2)")
            .bind(team)
            .bind(&ids)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Replaces the members.
    pub async fn set_members(&self, team: i64, users: &[UserId]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        set_members(&mut tx, team, users).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Live members of a team by username, optionally filtered by a substring of the username
    /// or display name.
    pub async fn list_members(
        &self,
        team: i64,
        search: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<UserId>> {
        let pattern = search
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", escape_like(s)));
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT u.id FROM team_members m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = $1 AND u.deleted_at IS NULL \
               AND ($2::text IS NULL OR u.username ILIKE $2 \
                    OR coalesce(u.display_name, '') ILIKE $2) \
             ORDER BY lower(u.username), u.id LIMIT $3 OFFSET $4",
        )
        .bind(team)
        .bind(pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().map(UserId).collect())
    }

    pub async fn count_members(&self, team: i64, search: Option<&str>) -> Result<i64> {
        let pattern = search
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", escape_like(s)));
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM team_members m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = $1 AND u.deleted_at IS NULL \
               AND ($2::text IS NULL OR u.username ILIKE $2 \
                    OR coalesce(u.display_name, '') ILIKE $2)",
        )
        .bind(team)
        .bind(pattern)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn is_member(&self, team: i64, user: UserId) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM team_members WHERE team_id = $1 AND user_id = $2)",
        )
        .bind(team)
        .bind(user.get())
        .fetch_one(&self.pool)
        .await?)
    }

    // ------------------------------------------------------------------ CODEOWNERS files

    /// Ingests a CODEOWNERS file for `repo`; `AlreadyExists` when it has one.
    pub async fn add_codeowners(&self, repo: RepoId, contents: &str) -> Result<IngestedCodeowners> {
        let row = sqlx::query(
            "INSERT INTO codeowners_files (repo_id, contents) VALUES ($1, $2) \
             RETURNING id, repo_id, contents, created_at, updated_at",
        )
        .bind(repo.get())
        .bind(contents)
        .fetch_one(&self.pool)
        .await?;
        Ok(codeowners_from_row(&row)?)
    }

    /// Replaces the ingested file of `repo`; `NotFound` when it has none.
    pub async fn update_codeowners(
        &self,
        repo: RepoId,
        contents: &str,
    ) -> Result<IngestedCodeowners> {
        let row = sqlx::query(
            "UPDATE codeowners_files SET contents = $2, updated_at = now() WHERE repo_id = $1 \
             RETURNING id, repo_id, contents, created_at, updated_at",
        )
        .bind(repo.get())
        .bind(contents)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(codeowners_from_row)
            .transpose()?
            .ok_or(DbError::NotFound("codeowners file"))
    }

    /// Deletes the ingested files of `repos`; returns how many existed.
    pub async fn delete_codeowners(&self, repos: &[RepoId]) -> Result<u64> {
        let ids: Vec<i32> = repos.iter().map(|r| r.get()).collect();
        Ok(
            sqlx::query("DELETE FROM codeowners_files WHERE repo_id = ANY($1)")
                .bind(&ids)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub async fn codeowners_for_repo(&self, repo: RepoId) -> Result<Option<IngestedCodeowners>> {
        let row = sqlx::query(
            "SELECT id, repo_id, contents, created_at, updated_at FROM codeowners_files \
             WHERE repo_id = $1",
        )
        .bind(repo.get())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(codeowners_from_row).transpose()?)
    }

    pub async fn get_codeowners(&self, id: i64) -> Result<Option<IngestedCodeowners>> {
        let row = sqlx::query(
            "SELECT id, repo_id, contents, created_at, updated_at FROM codeowners_files \
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(codeowners_from_row).transpose()?)
    }

    /// Ingested files by id, after `after` (an id).
    pub async fn list_codeowners(
        &self,
        limit: i64,
        after: Option<i64>,
    ) -> Result<Vec<IngestedCodeowners>> {
        let rows = sqlx::query(
            "SELECT id, repo_id, contents, created_at, updated_at FROM codeowners_files \
             WHERE id > coalesce($2, 0) ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(codeowners_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    pub async fn count_codeowners(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM codeowners_files")
            .fetch_one(&self.pool)
            .await?)
    }

    /// The ingested files of those of `repos` that have one.
    pub async fn codeowners_for_repos(&self, repos: &[i32]) -> Result<Vec<IngestedCodeowners>> {
        let rows = sqlx::query(
            "SELECT id, repo_id, contents, created_at, updated_at FROM codeowners_files \
             WHERE repo_id = ANY($1)",
        )
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(codeowners_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    // ------------------------------------------------------------------ assignments

    /// Assigns `assignee` to `path` of `repo` (idempotent).
    pub async fn assign(
        &self,
        assignee: Assignee,
        repo: RepoId,
        path: &str,
        who: Option<UserId>,
    ) -> Result<()> {
        let (table, column, id) = assignee_columns(assignee);
        sqlx::query(&format!(
            "INSERT INTO {table} ({column}, repo_id, file_path, who_assigned_user_id) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING"
        ))
        .bind(id)
        .bind(repo.get())
        .bind(normalize_path(path))
        .bind(who.map(UserId::get))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes an assignment; `NotFound` when there is none.
    pub async fn remove_assignment(
        &self,
        assignee: Assignee,
        repo: RepoId,
        path: &str,
    ) -> Result<()> {
        let (table, column, id) = assignee_columns(assignee);
        let n = sqlx::query(&format!(
            "DELETE FROM {table} WHERE {column} = $1 AND repo_id = $2 AND file_path = $3"
        ))
        .bind(id)
        .bind(repo.get())
        .bind(normalize_path(path))
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(DbError::NotFound("assigned owner"));
        }
        Ok(())
    }

    /// The assignments that cover `path` in `repo` (on the path itself or an ancestor, the
    /// repository root included), users before teams, then by path.
    pub async fn assignments_for_path(&self, repo: RepoId, path: &str) -> Result<Vec<Assignment>> {
        let path = normalize_path(path);
        let rows = sqlx::query(
            "SELECT * FROM ( \
               SELECT a.id, a.owner_user_id AS user_id, NULL::bigint AS team_id, a.repo_id, \
                      a.file_path, a.who_assigned_user_id, a.assigned_at \
               FROM assigned_owners a JOIN users u ON u.id = a.owner_user_id \
               WHERE u.deleted_at IS NULL AND a.repo_id = $1 \
                 AND (a.file_path = '' OR a.file_path = $2 OR starts_with($2, a.file_path || '/')) \
               UNION ALL \
               SELECT a.id, NULL, a.owner_team_id, a.repo_id, a.file_path, \
                      a.who_assigned_user_id, a.assigned_at \
               FROM assigned_teams a \
               WHERE a.repo_id = $1 \
                 AND (a.file_path = '' OR a.file_path = $2 OR starts_with($2, a.file_path || '/')) \
             ) x ORDER BY (user_id IS NULL), length(file_path) DESC, id",
        )
        .bind(repo.get())
        .bind(&path)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let user: Option<i64> = r.try_get("user_id")?;
                let team: Option<i64> = r.try_get("team_id")?;
                Ok(Assignment {
                    id: r.try_get("id")?,
                    assignee: match (user, team) {
                        (Some(u), _) => Assignee::User(UserId(u)),
                        (None, Some(t)) => Assignee::Team(t),
                        (None, None) => return Err(DbError::Invalid("assignment without owner")),
                    },
                    repo_id: RepoId(r.try_get("repo_id")?),
                    file_path: r.try_get("file_path")?,
                    who_assigned: r
                        .try_get::<Option<i64>, _>("who_assigned_user_id")?
                        .map(UserId),
                    assigned_at: r.try_get("assigned_at")?,
                })
            })
            .collect()
    }

    /// Every assignment in `repos` as search sees them: `(repo, path, "@username" or
    /// "@team")`.
    pub async fn assigned_entries(&self, repos: &[i32]) -> Result<Vec<(i32, AssignedEntry)>> {
        let rows: Vec<(i32, String, String)> = sqlx::query_as(
            "SELECT a.repo_id, a.file_path, u.username FROM assigned_owners a \
             JOIN users u ON u.id = a.owner_user_id \
             WHERE a.repo_id = ANY($1) AND u.deleted_at IS NULL \
             UNION ALL \
             SELECT a.repo_id, a.file_path, t.name FROM assigned_teams a \
             JOIN teams t ON t.id = a.owner_team_id WHERE a.repo_id = ANY($1)",
        )
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(repo, path, name)| {
                (
                    repo,
                    AssignedEntry {
                        path,
                        owner: format!("@{name}"),
                    },
                )
            })
            .collect())
    }
}

fn assignee_columns(a: Assignee) -> (&'static str, &'static str, i64) {
    match a {
        Assignee::User(u) => ("assigned_owners", "owner_user_id", u.get()),
        Assignee::Team(t) => ("assigned_teams", "owner_team_id", t),
    }
}

enum TeamBind {
    Text(String),
    Int(i64),
}

impl TeamBind {
    fn bind<'q>(
        self,
        q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        match self {
            TeamBind::Text(s) => q.bind(s),
            TeamBind::Int(i) => q.bind(i),
        }
    }
}

/// Escapes `%`, `_` and `\` for `LIKE`.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// [`OwnStore::set_members`] on `conn` (inside the caller's transaction).
pub(crate) async fn set_members(
    conn: &mut sqlx::PgConnection,
    team: i64,
    users: &[UserId],
) -> Result<()> {
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND NOT (user_id = ANY($2))")
        .bind(team)
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO team_members (team_id, user_id) SELECT $1, u FROM unnest($2::bigint[]) u \
         ON CONFLICT DO NOTHING",
    )
    .bind(team)
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_names() {
        assert!(valid_team_name("search"));
        assert!(valid_team_name("acme/backend.core"));
        assert!(!valid_team_name(""));
        assert!(!valid_team_name("/x"));
        assert!(!valid_team_name("x/"));
        assert!(!valid_team_name("-x"));
        assert!(!valid_team_name("a b"));
        assert_eq!(normalize_path("/a/b/"), "a/b");
    }
}
