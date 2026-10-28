-- Code Insights (docs/PLAN.md §6.15), the engine half: series definitions, their points per
-- repository and pre-summed per series, the per-repository state that incremental counting
-- continues from, and the backfill and recording job queues. The concepts follow the
-- original's `internal/insights` (read for behaviour only); the design is the plan's:
-- backfill counts each repository's history incrementally through tree diffs, and recording
-- carries values forward for repositories whose refs did not change.

CREATE TABLE insight_series (
    id bigserial PRIMARY KEY,
    -- Stable public identifier (the original's `series_id`).
    series_id text NOT NULL UNIQUE,
    query text NOT NULL CHECK (query <> ''),
    -- `search`: the match count; `mapping-compute`: one value per captured string (the
    -- first capture group); `search-compute`: one value per repository, language, path or
    -- commit author (`group_by`).
    generation_method text NOT NULL
        CHECK (generation_method IN ('search', 'mapping-compute', 'search-compute')),
    group_by text CHECK (group_by IN ('repo', 'lang', 'path', 'author')),
    CHECK ((generation_method = 'search-compute') = (group_by IS NOT NULL)),
    -- Which repositories: every repository, the `repositories` listed by name, or those
    -- whose repository filters `repository_criteria` matches (re-evaluated per recording).
    repo_scope text NOT NULL CHECK (repo_scope IN ('all', 'list', 'query')),
    repositories text[] NOT NULL DEFAULT '{}',
    repository_criteria text,
    CHECK ((repo_scope = 'query') = (repository_criteria IS NOT NULL)),
    sample_interval_unit text NOT NULL
        CHECK (sample_interval_unit IN ('HOUR', 'DAY', 'WEEK', 'MONTH', 'YEAR')),
    sample_interval_value integer NOT NULL CHECK (sample_interval_value > 0),
    created_by bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Deleted series stop being counted; the janitor removes them with their points.
    deleted_at timestamptz,
    -- `new` until the enqueuer queues one backfill job per repository, `running` until
    -- every job finished, then `completed` (or `failed` when the query cannot be counted).
    backfill_state text NOT NULL DEFAULT 'new'
        CHECK (backfill_state IN ('new', 'running', 'completed', 'failed')),
    backfill_queued_at timestamptz,
    backfill_completed_at timestamptz,
    -- The point times every repository's backfill counts.
    backfill_times timestamptz[] NOT NULL DEFAULT '{}',
    repos_total integer NOT NULL DEFAULT 0,
    repos_done integer NOT NULL DEFAULT 0,
    repos_failed integer NOT NULL DEFAULT 0,
    -- The repositories with points, as a serialized roaring bitmap: the read path sums
    -- the aggregate when the viewer can read all of them.
    repo_bitmap bytea,
    -- The newest point's time.
    last_recorded_at timestamptz,
    -- Why the series cannot be counted (backfill_state = 'failed').
    error text
);
CREATE INDEX insight_series_live_idx ON insight_series (backfill_state, id)
    WHERE deleted_at IS NULL;

-- Points per repository, value and time. `value` is '' for the count of a `search` series
-- and for the "other" bucket of the other series. Partitioned by year; the store creates
-- partitions before it writes into them.
CREATE TABLE insight_series_points (
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    repo_id integer NOT NULL,
    time timestamptz NOT NULL,
    value text NOT NULL DEFAULT '',
    count bigint NOT NULL CHECK (count >= 0),
    PRIMARY KEY (series_id, time, repo_id, value)
) PARTITION BY RANGE (time);

-- Points summed over the series' repositories, with the top values kept and the rest in
-- '' (other).
CREATE TABLE insight_series_points_agg (
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    time timestamptz NOT NULL,
    value text NOT NULL DEFAULT '',
    count bigint NOT NULL CHECK (count >= 0),
    PRIMARY KEY (series_id, time, value)
);

-- Every point time of a series, also those without any match.
CREATE TABLE insight_series_times (
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    time timestamptz NOT NULL,
    PRIMARY KEY (series_id, time)
);

-- Per series and repository, the newest point counted: the next point is counted from it
-- (or carried forward without a search when the repository's refs did not change since
-- `checked_at`). `counts` holds every value, not only the top ones.
CREATE TABLE insight_series_repos (
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    repo_id integer NOT NULL,
    time timestamptz NOT NULL,
    -- '' when the repository had no commit at `time`.
    commit_oid text NOT NULL DEFAULT '',
    counts jsonb NOT NULL DEFAULT '{}'::jsonb,
    checked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (series_id, repo_id)
);
CREATE INDEX insight_series_repos_repo_idx ON insight_series_repos (repo_id);

-- One backfill per series and repository (a pgqueue table). Newer series first.
CREATE TABLE insight_backfill_jobs (
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
    priority bigint NOT NULL DEFAULT 0,
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    repo_id integer NOT NULL,
    UNIQUE (series_id, repo_id)
);
CREATE INDEX insight_backfill_jobs_dequeue_idx ON insight_backfill_jobs (priority DESC, queued_at, id)
    WHERE state = 'queued';
CREATE INDEX insight_backfill_jobs_heartbeat_idx ON insight_backfill_jobs (last_heartbeat_at)
    WHERE state = 'processing';

-- One recording of a series at a point time (a pgqueue table).
CREATE TABLE insight_recording_jobs (
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
    series_id bigint NOT NULL REFERENCES insight_series (id) ON DELETE CASCADE,
    time timestamptz NOT NULL,
    UNIQUE (series_id, time)
);
CREATE INDEX insight_recording_jobs_dequeue_idx ON insight_recording_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX insight_recording_jobs_heartbeat_idx ON insight_recording_jobs (last_heartbeat_at)
    WHERE state = 'processing';
