-- Code Ownership signals and analytics (docs/PLAN.md §6.18): recent contributors from the
-- commit history, recent viewers from file view events, ownership coverage per directory,
-- the per-signal configuration and the background jobs computing them.

-- Each signal can be turned on and off and excluded for repositories (glob patterns over
-- repository names, `*` matching any run of characters), as the original's
-- `own_signal_configurations`. Off by default, as in the original.
CREATE TABLE own_signal_configurations (
    id                     integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name                   text    NOT NULL UNIQUE,
    description            text    NOT NULL DEFAULT '',
    enabled                boolean NOT NULL DEFAULT false,
    excluded_repo_patterns text[]  NOT NULL DEFAULT '{}'
);
INSERT INTO own_signal_configurations (name, description) VALUES
    ('recent-contributors', 'Indexes contributors in each file using repository history.'),
    ('recent-views', 'Indexes users that recently viewed files in Seeker.'),
    ('analytics', 'Indexes ownership data to present in aggregated views like Site admin > Ownership analytics and the repository ownership stats.');

-- Commit authors as written in commits.
CREATE TABLE commit_authors (
    id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name  text NOT NULL,
    email text NOT NULL,
    UNIQUE (name, email)
);
CREATE INDEX commit_authors_email_lower_idx ON commit_authors (lower(email));

-- The commits of the last 90 days reachable from a repository's HEAD (merges left out), with
-- the files each changed: what the aggregate below was computed from, so that commits leaving
-- the window (or HEAD's history) are subtracted exactly.
CREATE TABLE own_signal_recent_contribution (
    repo_id          integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    commit_id        text        NOT NULL,
    commit_author_id bigint      NOT NULL REFERENCES commit_authors (id) ON DELETE CASCADE,
    committed_at     timestamptz NOT NULL,
    paths            text[]      NOT NULL,
    PRIMARY KEY (repo_id, commit_id)
);

-- Commits per author per path prefix (a file, each of its directories and the repository
-- root, `''`), over the rows above.
CREATE TABLE own_aggregate_recent_contribution (
    repo_id             integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    path                text    NOT NULL,
    commit_author_id    bigint  NOT NULL REFERENCES commit_authors (id) ON DELETE CASCADE,
    contributions_count integer NOT NULL CHECK (contributions_count > 0),
    PRIMARY KEY (repo_id, path, commit_author_id)
);
CREATE INDEX own_aggregate_recent_contribution_rank_idx
    ON own_aggregate_recent_contribution (repo_id, path, contributions_count DESC);

-- Views of a file per user (`ViewBlob` events in `event_logs`, last 90 days).
CREATE TABLE own_aggregate_recent_view (
    repo_id     integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    path        text    NOT NULL,
    viewer_id   bigint  NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    views_count integer NOT NULL CHECK (views_count > 0),
    PRIMARY KEY (repo_id, path, viewer_id)
);
CREATE INDEX own_aggregate_recent_view_rank_idx
    ON own_aggregate_recent_view (repo_id, path, views_count DESC);

-- How far the views indexer has read `event_logs`: events up to `last_event_id` are counted
-- unless older than `window_start` (NULL before the first run). One row.
CREATE TABLE own_recent_view_state (
    id            boolean     PRIMARY KEY DEFAULT true CHECK (id),
    last_event_id bigint      NOT NULL DEFAULT 0,
    window_start  timestamptz
);
INSERT INTO own_recent_view_state DEFAULT VALUES;

-- Ownership coverage of a repository's default branch per directory (`''` for the root):
-- files, files with a CODEOWNERS owner, with an assigned owner, and with either.
CREATE TABLE ownership_path_stats (
    repo_id          integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    path             text        NOT NULL,
    total_files      integer     NOT NULL,
    codeowned_files  integer     NOT NULL,
    assigned_files   integer     NOT NULL,
    owned_files      integer     NOT NULL,
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, path)
);

-- When each repository was last indexed for a signal (the scheduler refreshes each daily).
CREATE TABLE own_repo_index_state (
    repo_id    integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    job_type   text        NOT NULL,
    indexed_at timestamptz NOT NULL,
    PRIMARY KEY (repo_id, job_type)
);

-- How far the scheduler has consumed repo_ref_events. One row.
CREATE TABLE own_ref_events_cursor (
    id            boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_event_id bigint  NOT NULL DEFAULT 0
);
INSERT INTO own_ref_events_cursor (last_event_id)
    SELECT COALESCE(max(id), 0) FROM repo_ref_events;

-- The signal indexers' jobs (a pgqueue table): per repository for `recent-contributors` and
-- `analytics`, global (`repo_id` NULL) for `recent-views`.
CREATE TABLE own_background_jobs (
    id bigserial PRIMARY KEY,
    state text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'processing', 'completed', 'errored', 'failed', 'canceled')),
    num_resets integer NOT NULL DEFAULT 0,
    num_failures integer NOT NULL DEFAULT 0,
    queued_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,
    process_after timestamptz,
    worker_hostname text NOT NULL DEFAULT '',
    last_heartbeat_at timestamptz,
    failure_message text,
    execution_logs jsonb NOT NULL DEFAULT '[]'::jsonb,
    cancel boolean NOT NULL DEFAULT false,
    repo_id integer REFERENCES repo (id) ON DELETE CASCADE,
    job_type text NOT NULL CHECK (job_type IN ('recent-contributors', 'recent-views', 'analytics'))
);
CREATE INDEX own_background_jobs_dequeue_idx ON own_background_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX own_background_jobs_heartbeat_idx ON own_background_jobs (last_heartbeat_at)
    WHERE state = 'processing';
-- At most one queued job per repository and type.
CREATE UNIQUE INDEX own_background_jobs_queued_key
    ON own_background_jobs (COALESCE(repo_id, 0), job_type) WHERE state = 'queued';
CREATE INDEX own_background_jobs_finished_at_idx ON own_background_jobs (finished_at)
    WHERE finished_at IS NOT NULL;
