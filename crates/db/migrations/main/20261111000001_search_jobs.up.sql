-- Search Jobs, exhaustive search (docs/PLAN.md §4 "Search", §6.9).
--
-- A job is split in three pgqueue queues (§5.2): the job itself resolves the query's
-- repositories into repo jobs, each repo job resolves its revision specifiers into revision
-- jobs, and each revision job searches one repository at one revision and writes its results
-- to the blob store (`search-jobs/<job>/<revision job>.jsonl`). The job's state shown to users
-- is aggregated over the three tables by `exhaustive_search_job_state`.

CREATE TABLE exhaustive_search_jobs (
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
    -- The job runs with its creator's repository permissions.
    initiator_id bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    query        text        NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- As in the original, a user runs a query once at a time.
    UNIQUE (query, initiator_id)
);
CREATE INDEX exhaustive_search_jobs_dequeue_idx ON exhaustive_search_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX exhaustive_search_jobs_heartbeat_idx ON exhaustive_search_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE INDEX exhaustive_search_jobs_initiator_idx ON exhaustive_search_jobs (initiator_id, id);

-- One repository of a job, with the revision specifiers the query gave it (`:`-separated,
-- `HEAD` for the default branch, `*refs/heads/*` globs).
CREATE TABLE exhaustive_search_repo_jobs (
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
    search_job_id bigint      NOT NULL REFERENCES exhaustive_search_jobs (id) ON DELETE CASCADE,
    repo_id       integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    ref_spec      text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (search_job_id, repo_id)
);
CREATE INDEX exhaustive_search_repo_jobs_dequeue_idx ON exhaustive_search_repo_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX exhaustive_search_repo_jobs_heartbeat_idx
    ON exhaustive_search_repo_jobs (last_heartbeat_at) WHERE state = 'processing';

-- One revision of a repository job: the unit of search, retried and timed out on its own.
CREATE TABLE exhaustive_search_repo_revision_jobs (
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
    search_repo_job_id bigint      NOT NULL
        REFERENCES exhaustive_search_repo_jobs (id) ON DELETE CASCADE,
    revision           text        NOT NULL,
    -- Results written to the blob store by the last successful run.
    result_count       bigint      NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (search_repo_job_id, revision)
);
CREATE INDEX exhaustive_search_repo_revision_jobs_dequeue_idx
    ON exhaustive_search_repo_revision_jobs (queued_at, id) WHERE state = 'queued';
CREATE INDEX exhaustive_search_repo_revision_jobs_heartbeat_idx
    ON exhaustive_search_repo_revision_jobs (last_heartbeat_at) WHERE state = 'processing';

-- The state of a whole job, from the states of its rows in the three tables: canceled if
-- anything was canceled; processing while something runs or waits for a retry, or waits
-- after something completed; queued while nothing started; then failed if anything failed,
-- else completed (the original's rules).
CREATE FUNCTION exhaustive_search_job_state(job bigint) RETURNS text
LANGUAGE sql STABLE AS $$
    WITH states AS (
        SELECT state FROM exhaustive_search_jobs WHERE id = job
        UNION ALL
        SELECT state FROM exhaustive_search_repo_jobs WHERE search_job_id = job
        UNION ALL
        SELECT rr.state
        FROM exhaustive_search_repo_revision_jobs rr
        JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
        WHERE r.search_job_id = job
    ), counts AS (
        SELECT count(*) FILTER (WHERE state = 'canceled')   AS canceled,
               count(*) FILTER (WHERE state = 'processing') AS processing,
               count(*) FILTER (WHERE state = 'errored')    AS errored,
               count(*) FILTER (WHERE state = 'queued')     AS queued,
               count(*) FILTER (WHERE state = 'failed')     AS failed,
               count(*) FILTER (WHERE state = 'completed')  AS completed
        FROM states
    )
    SELECT CASE
        WHEN canceled > 0 THEN 'canceled'
        WHEN processing > 0 OR errored > 0 THEN 'processing'
        WHEN queued > 0 AND completed = 0 THEN 'queued'
        WHEN queued > 0 THEN 'processing'
        WHEN failed > 0 THEN 'failed'
        WHEN completed > 0 THEN 'completed'
        ELSE 'queued'
    END
    FROM counts
$$;
