-- Fetch scheduling state kept by the repo node (docs/PLAN.md §6.3 "Scheduler"): the inputs of
-- the adaptive policy `clamp(2 × mean change interval, 45 s, 8 h)` with exponential backoff on
-- failures, and the computed due time so the scheduler only scans repos that are due.
ALTER TABLE gitserver_repos
    ADD COLUMN last_attempt_at           timestamptz,
    ADD COLUMN consecutive_failures      integer NOT NULL DEFAULT 0,
    -- Moving average of the time between fetches that changed refs, in seconds.
    ADD COLUMN mean_change_interval_secs double precision,
    -- When the next periodic fetch is due; NULL = evaluate at the next scheduler pass.
    ADD COLUMN next_fetch_at             timestamptz;
CREATE INDEX gitserver_repos_next_fetch_at_idx ON gitserver_repos (next_fetch_at);
