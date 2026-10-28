//! Owner resolution: CODEOWNERS handles and emails, assigned owners and
//! `file:has.owner()` arguments to users and teams.
//!
//! - An email resolves to the user with that verified address.
//! - A handle resolves, in order, to the user whose GitLab account (an external account of
//!   service type `gitlab`) has that username, to the user with that username, and to the
//!   team with that name (a GitLab group path `group/subgroup` names a team of the same
//!   name).
//! - Anything else stays a plain-text owner: a person, or an external team for a handle
//!   with a `/` (always a GitLab group).

use std::collections::{HashMap, HashSet};

use sourcegull_db::{Db, DbError};
use sourcegull_types::{User, UserId};
use sqlx::PgPool;

use crate::codeowners::owner_key;
use crate::store::{TEAM_COLUMNS, Team, team_from_row};

type Result<T> = std::result::Result<T, DbError>;

/// What an owner reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedOwner {
    User(Box<User>),
    Team(Team),
    /// An unresolved person: the handle (without `@`) or the email as written, and the name
    /// a commit gives (recent contributors).
    Person {
        handle: Option<String>,
        email: Option<String>,
        name: Option<String>,
    },
    /// An unresolved group handle (`group/subgroup`, without `@`).
    ExternalTeam {
        handle: String,
    },
}

impl ResolvedOwner {
    /// One identity per user or team, so that the same owner reached through several
    /// references (a GitLab handle, an email, an assignment) is listed once.
    pub fn identity(&self) -> String {
        match self {
            ResolvedOwner::User(u) => format!("user:{}", u.id.get()),
            ResolvedOwner::Team(t) => format!("team:{}", t.id),
            ResolvedOwner::Person { handle, email, .. } => format!(
                "person:{}",
                owner_key(handle.as_deref().or(email.as_deref()).unwrap_or_default())
            ),
            ResolvedOwner::ExternalTeam { handle } => format!("team:{}", owner_key(handle)),
        }
    }

    /// The name shown and sorted by.
    pub fn display(&self) -> String {
        match self {
            ResolvedOwner::User(u) => u.username.clone(),
            ResolvedOwner::Team(t) => t.name.clone(),
            ResolvedOwner::Person {
                handle,
                email,
                name,
            } => handle
                .clone()
                .or_else(|| name.clone())
                .or_else(|| email.clone())
                .unwrap_or_default(),
            ResolvedOwner::ExternalTeam { handle } => handle.clone(),
        }
    }

    pub(crate) fn unresolved(text: &str) -> Self {
        let t = text.trim();
        if let Some(h) = t.strip_prefix('@') {
            if h.contains('/') {
                return ResolvedOwner::ExternalTeam {
                    handle: h.to_owned(),
                };
            }
            return ResolvedOwner::Person {
                handle: Some(h.to_owned()),
                email: None,
                name: None,
            };
        }
        if t.contains('@') {
            return ResolvedOwner::Person {
                handle: None,
                email: Some(t.to_owned()),
                name: None,
            };
        }
        ResolvedOwner::Person {
            handle: Some(t.to_owned()),
            email: None,
            name: None,
        }
    }
}

const USER_COLUMNS: &str = "u.id, u.username, u.display_name, u.avatar_url, u.site_admin, \
     u.created_at, u.updated_at, u.deleted_at, u.locked_until, u.failed_logins";

fn user_from_row(r: &sqlx::postgres::PgRow) -> sqlx::Result<User> {
    use sqlx::Row;
    Ok(User {
        id: UserId(r.try_get("id")?),
        username: r.try_get("username")?,
        display_name: r.try_get("display_name")?,
        avatar_url: r.try_get("avatar_url")?,
        site_admin: r.try_get("site_admin")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        deleted_at: r.try_get("deleted_at")?,
        locked_until: r.try_get("locked_until")?,
        failed_logins: r.try_get("failed_logins")?,
    })
}

/// Resolves owner references in batches.
#[derive(Debug, Clone)]
pub struct OwnerResolver {
    pool: PgPool,
}

impl OwnerResolver {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Live users by id.
    pub async fn users(&self, ids: &[UserId]) -> Result<Vec<User>> {
        let ids: Vec<i64> = ids.iter().map(|u| u.get()).collect();
        let rows = sqlx::query(&format!(
            "SELECT {USER_COLUMNS} FROM users u WHERE u.id = ANY($1) AND u.deleted_at IS NULL"
        ))
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(user_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    /// Resolves each text (`@handle`, `email`, or a bare name) to an owner, keyed by
    /// [`owner_key`]. Every text gets an entry.
    pub async fn resolve(&self, texts: &[String]) -> Result<HashMap<String, ResolvedOwner>> {
        let mut out: HashMap<String, ResolvedOwner> = HashMap::new();
        let mut emails = Vec::new();
        let mut handles = Vec::new();
        for t in texts {
            let key = owner_key(t);
            if key.is_empty() || out.contains_key(&key) {
                continue;
            }
            if key.starts_with('@') {
                // A role (`@@maintainer`): never resolved.
            } else if key.contains('@') {
                emails.push(key.clone());
            } else {
                handles.push(key.clone());
            }
            out.insert(key, ResolvedOwner::unresolved(t));
        }
        if !emails.is_empty() {
            let rows = sqlx::query(&format!(
                "SELECT lower(e.email) AS key, {USER_COLUMNS} FROM user_emails e \
                 JOIN users u ON u.id = e.user_id \
                 WHERE lower(e.email) = ANY($1) AND e.verified_at IS NOT NULL \
                   AND u.deleted_at IS NULL"
            ))
            .bind(&emails)
            .fetch_all(&self.pool)
            .await?;
            for r in &rows {
                use sqlx::Row;
                let key: String = r.try_get("key")?;
                out.insert(key, ResolvedOwner::User(Box::new(user_from_row(r)?)));
            }
        }
        if !handles.is_empty() {
            let mut found = HashSet::new();
            // GitLab accounts first: a CODEOWNERS handle is a GitLab username.
            let rows = sqlx::query(&format!(
                "SELECT DISTINCT ON (lower(a.account_data->>'username')) \
                     lower(a.account_data->>'username') AS key, {USER_COLUMNS} \
                 FROM user_external_accounts a JOIN users u ON u.id = a.user_id \
                 WHERE a.service_type = 'gitlab' AND a.deleted_at IS NULL \
                   AND u.deleted_at IS NULL \
                   AND lower(a.account_data->>'username') = ANY($1) \
                 ORDER BY lower(a.account_data->>'username'), a.id"
            ))
            .bind(&handles)
            .fetch_all(&self.pool)
            .await?;
            let mut users = Vec::new();
            for r in &rows {
                use sqlx::Row;
                let key: String = r.try_get("key")?;
                users.push((key, user_from_row(r)?));
            }
            let rows = sqlx::query(&format!(
                "SELECT lower(u.username) AS key, {USER_COLUMNS} FROM users u \
                 WHERE lower(u.username) = ANY($1) AND u.deleted_at IS NULL"
            ))
            .bind(&handles)
            .fetch_all(&self.pool)
            .await?;
            for r in &rows {
                use sqlx::Row;
                let key: String = r.try_get("key")?;
                users.push((key, user_from_row(r)?));
            }
            for (key, user) in users {
                if found.insert(key.clone()) {
                    out.insert(key, ResolvedOwner::User(Box::new(user)));
                }
            }
            let rest: Vec<String> = handles.into_iter().filter(|h| !found.contains(h)).collect();
            if !rest.is_empty() {
                let rows = sqlx::query(&format!(
                    "SELECT {TEAM_COLUMNS} FROM teams WHERE lower(name) = ANY($1)"
                ))
                .bind(&rest)
                .fetch_all(&self.pool)
                .await?;
                for r in &rows {
                    let t = team_from_row(r)?;
                    out.insert(t.name.to_lowercase(), ResolvedOwner::Team(t));
                }
            }
        }
        Ok(out)
    }

    /// The keys that name `user`: the username, GitLab usernames and verified emails.
    pub async fn user_keys(&self, user: UserId) -> Result<HashSet<String>> {
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT lower(username) FROM users WHERE id = $1 AND deleted_at IS NULL \
             UNION SELECT lower(account_data->>'username') FROM user_external_accounts \
               WHERE user_id = $1 AND service_type = 'gitlab' AND deleted_at IS NULL \
                 AND account_data ? 'username' \
             UNION SELECT lower(email) FROM user_emails \
               WHERE user_id = $1 AND verified_at IS NOT NULL",
        )
        .bind(user.get())
        .fetch_all(&self.pool)
        .await?;
        Ok(keys.into_iter().collect())
    }

    /// The keys a `file:has.owner(term)` matches: all names of the user or team the term
    /// resolves to, or the term itself when it resolves to nothing.
    pub async fn term_keys(&self, term: &str) -> Result<HashSet<String>> {
        let key = owner_key(term);
        let resolved = self.resolve(&[term.to_owned()]).await?;
        let mut keys = match resolved.get(&key) {
            Some(ResolvedOwner::User(u)) => self.user_keys(u.id).await?,
            Some(ResolvedOwner::Team(t)) => HashSet::from([t.name.to_lowercase()]),
            _ => HashSet::new(),
        };
        keys.insert(key);
        Ok(keys)
    }
}
