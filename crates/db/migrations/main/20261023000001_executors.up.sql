-- Executors and auto-indexing jobs (docs/PLAN.md §4 "Executors", §6.12.5, §6.13).
--
-- codeintel_index_jobs is the `codeintel` executor queue: one row per index job for
-- (repository, commit, root, indexer). Executors dequeue it over HTTP (/.executors/queue/…),
-- run the steps in containers and upload the index with a job token.
CREATE TABLE codeintel_index_jobs (
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
    repository_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    commit text NOT NULL CHECK (commit ~ '^[0-9a-f]{40}$'),
    -- '' for the repository root, else a relative directory with a trailing '/'.
    root text NOT NULL DEFAULT '',
    -- The indexer's container image, e.g. sourcegraph/scip-go:latest.
    indexer text NOT NULL,
    -- The indexer command line, run in `root`.
    indexer_args text[] NOT NULL DEFAULT '{}',
    -- The index file the indexer writes, relative to `root`.
    outfile text NOT NULL DEFAULT 'index.scip',
    -- Steps run before the indexer: [{"root": "", "image": "…", "commands": ["…"]}].
    docker_steps jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Commands run in the indexer's container before the indexer.
    local_steps text[] NOT NULL DEFAULT '{}',
    -- Names of executor environment variables passed through to the steps.
    requested_env_vars text[] NOT NULL DEFAULT '{}',
    should_reindex boolean NOT NULL DEFAULT false,
    enqueuer_user_id bigint REFERENCES users (id) ON DELETE SET NULL,
    -- The upload the job produced.
    associated_upload_id bigint
);
CREATE INDEX codeintel_index_jobs_dequeue_idx ON codeintel_index_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX codeintel_index_jobs_heartbeat_idx ON codeintel_index_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE INDEX codeintel_index_jobs_repo_idx ON codeintel_index_jobs (repository_id, commit);

-- One row per executor, upserted by every heartbeat.
CREATE TABLE executor_heartbeats (
    id bigserial PRIMARY KEY,
    hostname text NOT NULL UNIQUE,
    queue_names text[] NOT NULL DEFAULT '{}',
    os text NOT NULL DEFAULT '',
    architecture text NOT NULL DEFAULT '',
    executor_version text NOT NULL DEFAULT '',
    runtime text NOT NULL DEFAULT '',
    runtime_version text NOT NULL DEFAULT '',
    git_version text NOT NULL DEFAULT '',
    -- [{"queue": "codeintel", "id": 12}, …]: the jobs the executor reported running.
    active_jobs jsonb NOT NULL DEFAULT '[]'::jsonb,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now()
);

-- Job tokens (§6.13): scoped to one job of one queue and its repository, for git checkouts
-- and uploads. Deleted when the job finishes.
CREATE TABLE executor_job_tokens (
    id bigserial PRIMARY KEY,
    queue text NOT NULL,
    job_id bigint NOT NULL,
    repository_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    token_sha256 bytea NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (queue, job_id)
);
