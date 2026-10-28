-- Code host permissions sync (docs/PLAN.md §4 "Permissions", §6.21).

-- Sync runs, user-centric (`user_id`) or repo-centric (`repository_id`): a pgqueue queue
-- (§5.2) dequeued by priority. The finished rows are the sync history shown to admins. At most
-- one queued run per user and per repository; queueing again raises its priority.
CREATE TABLE permission_sync_jobs (
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
    user_id              bigint   REFERENCES users (id) ON DELETE CASCADE,
    repository_id        integer  REFERENCES repo (id) ON DELETE CASCADE,
    -- 'user_signed_in', 'repo_created', 'manual', 'webhook', 'scheduled'.
    reason               text     NOT NULL,
    -- Higher runs first (see PermsSyncReason in seeker-db).
    priority             smallint NOT NULL DEFAULT 0,
    triggered_by_user_id bigint   REFERENCES users (id) ON DELETE SET NULL,
    -- Filled in by the run: grants added and removed, and what the code host returned
    -- (repositories for a user, users for a repository).
    added                integer  NOT NULL DEFAULT 0,
    removed              integer  NOT NULL DEFAULT 0,
    found                integer  NOT NULL DEFAULT 0,
    -- One entry per code host connection: {externalServiceID, status, message}.
    code_host_states     jsonb    NOT NULL DEFAULT '[]'::jsonb,
    CHECK ((user_id IS NULL) <> (repository_id IS NULL))
);
CREATE INDEX permission_sync_jobs_dequeue_idx ON permission_sync_jobs (priority DESC, queued_at, id)
    WHERE state = 'queued';
CREATE INDEX permission_sync_jobs_heartbeat_idx ON permission_sync_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE UNIQUE INDEX permission_sync_jobs_one_queued_user ON permission_sync_jobs (user_id)
    WHERE state = 'queued' AND user_id IS NOT NULL;
CREATE UNIQUE INDEX permission_sync_jobs_one_queued_repo ON permission_sync_jobs (repository_id)
    WHERE state = 'queued' AND repository_id IS NOT NULL;
CREATE INDEX permission_sync_jobs_user_idx ON permission_sync_jobs (user_id, id DESC)
    WHERE user_id IS NOT NULL;
CREATE INDEX permission_sync_jobs_repo_idx ON permission_sync_jobs (repository_id, id DESC)
    WHERE repository_id IS NOT NULL;

-- When each user's code host permissions were last synced (repositories have
-- `repo_permissions.synced_at`). The scheduler refreshes the oldest first.
CREATE TABLE user_permissions_sync (
    user_id   bigint      PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    synced_at timestamptz NOT NULL DEFAULT now()
);
