//! `NOTIFY` helper and the channel names used across the system.
//!
//! Notifications sent inside a transaction are delivered on commit, so stores
//! send them in the same transaction as the write they announce.

use crate::Result;
use sqlx::PgExecutor;

/// Channel names. Payloads are small strings (usually an id); listeners reload
/// the data from the table.
pub mod channels {
    /// A new site config version was written. Payload: the new `site_config.id`.
    pub const SITE_CONFIG: &str = "site_config";
    /// A new settings version was written. Payload: `<subject_type>:<subject id or empty>`.
    pub const SETTINGS: &str = "settings";
    /// Feature flags or overrides changed. Payload: the flag name.
    pub const FEATURE_FLAGS: &str = "feature_flags";
    /// Encryption keys were rotated. Payload: the key name.
    pub const ENCRYPTION_KEYS: &str = "encryption_keys";

    /// Refs of a repo changed (`repo_ref_events` rows were added). Payload: the repo id.
    pub const REPO_REF_EVENTS: &str = "repo_ref_events";

    /// Repo node assignments (`repo.shard`) changed. Payload: the ids of the repos whose
    /// assignment changed (see [`repo_shards_payload`](super::repo_shards_payload)), or empty
    /// when any may have.
    pub const REPO_SHARDS: &str = "repo_shards";

    /// Repository permissions changed. Payload: `user:<id>`, `all` or
    /// `unrestricted` (see `stores::authz_payload`).
    pub const AUTHZ: &str = "authz";

    /// Every channel above.
    pub const ALL: &[&str] = &[
        SITE_CONFIG,
        SETTINGS,
        FEATURE_FLAGS,
        ENCRYPTION_KEYS,
        REPO_REF_EVENTS,
        REPO_SHARDS,
        AUTHZ,
    ];
}

/// Payloads longer than this are sent empty (Postgres limits them to 8000 bytes).
const MAX_REPO_SHARDS_PAYLOAD: usize = 7000;

/// The `repo_shards` payload naming `repos`: their ids separated by commas, or empty (any
/// assignment may have changed) when that does not fit in a notification.
pub fn repo_shards_payload(repos: &[sourcegull_types::RepoId]) -> String {
    let mut out = String::new();
    for (i, id) in repos.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.get().to_string());
        if out.len() > MAX_REPO_SHARDS_PAYLOAD {
            return String::new();
        }
    }
    out
}

/// The repos a `repo_shards` payload names; `None` when it is empty or malformed (any
/// assignment may have changed).
pub fn parse_repo_shards_payload(payload: &str) -> Option<Vec<sourcegull_types::RepoId>> {
    if payload.is_empty() {
        return None;
    }
    payload
        .split(',')
        .map(|s| s.trim().parse::<i32>().ok().map(sourcegull_types::RepoId))
        .collect()
}

/// Sends `NOTIFY channel, payload` (via `pg_notify`, so any channel name is safe).
pub async fn notify<'e, E: PgExecutor<'e>>(
    executor: E,
    channel: &str,
    payload: &str,
) -> Result<()> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(channel)
        .bind(payload)
        .execute(executor)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_types::RepoId;

    #[test]
    fn repo_shards_payloads_round_trip_and_fall_back_to_empty() {
        let ids = [RepoId(3), RepoId(17)];
        assert_eq!(repo_shards_payload(&ids), "3,17");
        assert_eq!(parse_repo_shards_payload("3,17"), Some(ids.to_vec()));
        assert_eq!(parse_repo_shards_payload(""), None);
        assert_eq!(parse_repo_shards_payload("3,x"), None);
        let many: Vec<RepoId> = (1_000_000..1_002_000).map(RepoId).collect();
        assert_eq!(repo_shards_payload(&many), "");
    }
}
