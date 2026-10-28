DROP INDEX IF EXISTS gitserver_repos_next_fetch_at_idx;
ALTER TABLE gitserver_repos
    DROP COLUMN IF EXISTS next_fetch_at,
    DROP COLUMN IF EXISTS mean_change_interval_secs,
    DROP COLUMN IF EXISTS consecutive_failures,
    DROP COLUMN IF EXISTS last_attempt_at;
