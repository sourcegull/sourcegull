-- Batch changes (docs/PLAN.md §4 "Batch changes", §6.14, §5.2, §5.5): batch specs, their
-- workspaces and executions, changeset specs, batch changes, changesets with their events and
-- bulk jobs, and the credentials used to publish changesets. Compatible in behaviour with the
-- original schema; written for this project.
--
-- Queue tables have the standard pgqueue shape (§5.2): `batch_spec_resolution_jobs`
-- (workspace resolution, worker), `batch_spec_workspace_execution_jobs` (the `batches`
-- executor queue), `changeset_jobs` (bulk operations, worker) and `changesets` itself (the
-- reconciler queue: its `state` column is the reconciler state).

-- A batch spec: the YAML as uploaded (`raw_spec`) and parsed to JSON (`spec`). Specs are
-- immutable once created, except for the flags of a server-side spec before it runs.
CREATE TABLE batch_specs (
    id bigserial PRIMARY KEY,
    -- The id in GraphQL ids, so spec ids cannot be guessed.
    rand_id text NOT NULL UNIQUE DEFAULT replace(gen_random_uuid()::text, '-', ''),
    raw_spec text NOT NULL,
    spec jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(spec) = 'object'),
    namespace_user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    -- NULL once the creator has been deleted.
    user_id bigint REFERENCES users (id) ON DELETE SET NULL,
    -- Created from raw YAML for server-side execution (createBatchSpecFromRaw), as opposed
    -- to uploaded with changeset specs by a local run (createBatchSpec).
    created_from_raw boolean NOT NULL DEFAULT false,
    allow_unsupported boolean NOT NULL DEFAULT false,
    allow_ignored boolean NOT NULL DEFAULT false,
    no_cache boolean NOT NULL DEFAULT false,
    -- The batch change a server-side spec was created for (set by the batch_changes FK
    -- below).
    batch_change_id bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT batch_specs_has_1_namespace
        CHECK ((namespace_user_id IS NULL) <> (namespace_org_id IS NULL))
);
CREATE INDEX batch_specs_namespace_user_idx ON batch_specs (namespace_user_id);
CREATE INDEX batch_specs_namespace_org_idx ON batch_specs (namespace_org_id);
CREATE INDEX batch_specs_batch_change_idx ON batch_specs (batch_change_id);
CREATE INDEX batch_specs_user_idx ON batch_specs (user_id);

-- A batch change: a named campaign in a namespace, pointing at the last applied spec.
-- `last_applied_at` is NULL for a draft (created for server-side execution, never applied).
CREATE TABLE batch_changes (
    id bigserial PRIMARY KEY,
    name text NOT NULL CHECK (name <> '' AND name ~ '^[\w.-]+$'),
    description text NOT NULL DEFAULT '',
    namespace_user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    creator_id bigint REFERENCES users (id) ON DELETE SET NULL,
    last_applier_id bigint REFERENCES users (id) ON DELETE SET NULL,
    last_applied_at timestamptz,
    closed_at timestamptz,
    batch_spec_id bigint NOT NULL REFERENCES batch_specs (id),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT batch_changes_has_1_namespace
        CHECK ((namespace_user_id IS NULL) <> (namespace_org_id IS NULL))
);
CREATE UNIQUE INDEX batch_changes_unique_user_name
    ON batch_changes (namespace_user_id, name) WHERE namespace_user_id IS NOT NULL;
CREATE UNIQUE INDEX batch_changes_unique_org_name
    ON batch_changes (namespace_org_id, name) WHERE namespace_org_id IS NOT NULL;
CREATE INDEX batch_changes_batch_spec_idx ON batch_changes (batch_spec_id);

ALTER TABLE batch_specs ADD CONSTRAINT batch_specs_batch_change_id_fkey
    FOREIGN KEY (batch_change_id) REFERENCES batch_changes (id) ON DELETE SET NULL;

-- Workspace resolution of a server-side spec: one job per spec, re-queued on re-resolution.
CREATE TABLE batch_spec_resolution_jobs (
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
    batch_spec_id bigint NOT NULL UNIQUE REFERENCES batch_specs (id) ON DELETE CASCADE,
    initiator_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX batch_spec_resolution_jobs_dequeue_idx ON batch_spec_resolution_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX batch_spec_resolution_jobs_heartbeat_idx
    ON batch_spec_resolution_jobs (last_heartbeat_at) WHERE state = 'processing';

-- A resolved workspace: a repository at a commit and a path in it where the steps run.
CREATE TABLE batch_spec_workspaces (
    id bigserial PRIMARY KEY,
    batch_spec_id bigint NOT NULL REFERENCES batch_specs (id) ON DELETE CASCADE,
    repo_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    -- The branch ref (refs/heads/…) and its resolved commit.
    branch text NOT NULL,
    commit text NOT NULL,
    -- '' for the repository root.
    path text NOT NULL DEFAULT '',
    -- Paths that matched the `on` query, for `repository.search_result_paths`.
    file_matches text[] NOT NULL DEFAULT '{}',
    only_fetch_workspace boolean NOT NULL DEFAULT false,
    -- Ignored by a .batchignore, on an unsupported code host, or skipped for another
    -- reason (`skipped_reason`); skipped workspaces get no execution job.
    ignored boolean NOT NULL DEFAULT false,
    unsupported boolean NOT NULL DEFAULT false,
    skipped boolean NOT NULL DEFAULT false,
    skipped_reason text,
    -- The whole result came from the execution cache; no execution job is needed.
    cached_result_found boolean NOT NULL DEFAULT false,
    -- Per-step cache hits: {"<step index>": <cached step result>}.
    step_cache_results jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(step_cache_results) = 'object'),
    -- Changeset specs the execution produced: {"<changeset spec id>": {}}.
    changeset_spec_ids jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(changeset_spec_ids) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX batch_spec_workspaces_batch_spec_idx ON batch_spec_workspaces (batch_spec_id, id);
CREATE INDEX batch_spec_workspaces_repo_idx ON batch_spec_workspaces (repo_id);

-- The `batches` executor queue: one job per workspace execution. Jobs are dequeued fairly
-- between users (the user who dequeued least recently goes first, as in the original).
CREATE TABLE batch_spec_workspace_execution_jobs (
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
    batch_spec_workspace_id bigint NOT NULL REFERENCES batch_spec_workspaces (id) ON DELETE CASCADE,
    -- The user whose credentials and secrets the job runs with, for fair dequeueing.
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- The job payload format; executors dequeue only versions they support.
    version integer NOT NULL DEFAULT 2,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX batch_spec_workspace_execution_jobs_dequeue_idx
    ON batch_spec_workspace_execution_jobs (user_id, queued_at, id) WHERE state = 'queued';
CREATE INDEX batch_spec_workspace_execution_jobs_heartbeat_idx
    ON batch_spec_workspace_execution_jobs (last_heartbeat_at) WHERE state = 'processing';
CREATE INDEX batch_spec_workspace_execution_jobs_workspace_idx
    ON batch_spec_workspace_execution_jobs (batch_spec_workspace_id);

-- When each user last had an execution job dequeued, kept by a trigger.
CREATE TABLE batch_spec_workspace_execution_last_dequeues (
    user_id bigint PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    latest_dequeue timestamptz NOT NULL
);

CREATE FUNCTION batch_spec_workspace_execution_last_dequeue() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO batch_spec_workspace_execution_last_dequeues (user_id, latest_dequeue)
    VALUES (NEW.user_id, COALESCE(NEW.started_at, now()))
    ON CONFLICT (user_id) DO UPDATE SET latest_dequeue = GREATEST(
        batch_spec_workspace_execution_last_dequeues.latest_dequeue, EXCLUDED.latest_dequeue);
    RETURN NULL;
END;
$$;

CREATE TRIGGER batch_spec_workspace_execution_last_dequeue
    AFTER UPDATE OF state ON batch_spec_workspace_execution_jobs
    FOR EACH ROW WHEN (NEW.state = 'processing' AND OLD.state <> 'processing')
    EXECUTE FUNCTION batch_spec_workspace_execution_last_dequeue();

-- Step results cached per user: key = hash of the repository, commit, workspace, steps and
-- image digests up to a step (§6.14). Least recently used entries are evicted first.
CREATE TABLE batch_spec_execution_cache_entries (
    id bigserial PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    key text NOT NULL,
    value text NOT NULL,
    version integer NOT NULL,
    last_used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, key)
);
CREATE INDEX batch_spec_execution_cache_entries_lru_idx
    ON batch_spec_execution_cache_entries (COALESCE(last_used_at, created_at), id);

-- A changeset spec: what one changeset should look like, from an execution or uploaded by a
-- local run. `type` 'branch' describes a new changeset (a diff on a branch); 'existing'
-- imports a changeset by `external_id`.
CREATE TABLE changeset_specs (
    id bigserial PRIMARY KEY,
    rand_id text NOT NULL UNIQUE DEFAULT replace(gen_random_uuid()::text, '-', ''),
    -- NULL until a local run's spec is attached to its batch spec.
    batch_spec_id bigint REFERENCES batch_specs (id) ON DELETE CASCADE,
    repo_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    user_id bigint REFERENCES users (id) ON DELETE SET NULL,
    type text NOT NULL CHECK (type IN ('branch', 'existing')),
    external_id text,
    base_ref text,
    base_rev text,
    head_ref text,
    title text,
    body text,
    -- true, false, "draft", or NULL (not set: decided in the UI).
    published jsonb CHECK (published IS NULL OR published IN ('true', 'false', '"draft"')),
    diff bytea,
    diff_stat_added integer,
    diff_stat_deleted integer,
    commit_message text,
    commit_author_name text,
    commit_author_email text,
    fork_namespace text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (type <> 'existing' OR external_id IS NOT NULL),
    CHECK (type <> 'branch' OR (head_ref IS NOT NULL AND base_ref IS NOT NULL))
);
CREATE INDEX changeset_specs_batch_spec_idx ON changeset_specs (batch_spec_id, id);
CREATE INDEX changeset_specs_created_at_idx ON changeset_specs (created_at)
    WHERE batch_spec_id IS NULL;

-- The UI state of a changeset (§6.14), from its reconciler state (the pgqueue `state` and
-- `num_failures`: a queued job that failed before is retrying), publication state and
-- external state. Mirrored by `seeker_db::compute_changeset_state`.
CREATE FUNCTION changeset_computed_state(
    p_state text, p_num_failures integer, p_publication_state text, p_external_state text
) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN p_state = 'errored' OR (p_state = 'queued' AND p_num_failures > 0) THEN 'RETRYING'
        WHEN p_state IN ('failed', 'canceled') THEN 'FAILED'
        WHEN p_state = 'scheduled' THEN 'SCHEDULED'
        WHEN p_state <> 'completed' THEN 'PROCESSING'
        WHEN p_publication_state = 'UNPUBLISHED' THEN 'UNPUBLISHED'
        ELSE COALESCE(p_external_state, 'OPEN')
    END
$$;

-- A changeset (a GitLab merge request) tracked by one or more batch changes, created by one
-- (`owned_by_batch_change_id`) or imported. The row is the reconciler's queue entry.
CREATE TABLE changesets (
    id bigserial PRIMARY KEY,
    -- pgqueue columns; `state` is the reconciler state, plus 'scheduled' (waiting for a
    -- rollout window before becoming 'queued').
    state text NOT NULL DEFAULT 'queued' CHECK (state IN (
        'scheduled', 'queued', 'processing', 'completed', 'errored', 'failed', 'canceled')),
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
    repo_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    -- {"<batch change id>": {"detach": bool, "archive": bool, "isArchived": bool}}.
    batch_change_ids jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(batch_change_ids) = 'object'),
    owned_by_batch_change_id bigint REFERENCES batch_changes (id) ON DELETE SET NULL,
    current_spec_id bigint REFERENCES changeset_specs (id),
    previous_spec_id bigint REFERENCES changeset_specs (id),
    publication_state text NOT NULL DEFAULT 'UNPUBLISHED'
        CHECK (publication_state IN ('UNPUBLISHED', 'PUBLISHED')),
    -- Set in the UI when the spec leaves `published` unset.
    ui_publication_state text CHECK (ui_publication_state IN ('UNPUBLISHED', 'DRAFT', 'PUBLISHED')),
    external_id text CHECK (external_id <> ''),
    external_service_type text NOT NULL CHECK (external_service_type <> ''),
    external_branch text CHECK (external_branch LIKE 'refs/heads/%'),
    external_fork_namespace text,
    external_fork_name text,
    external_state text
        CHECK (external_state IN ('DRAFT', 'OPEN', 'CLOSED', 'MERGED', 'READONLY', 'DELETED')),
    external_review_state text CHECK (external_review_state IN
        ('APPROVED', 'CHANGES_REQUESTED', 'PENDING', 'COMMENTED', 'DISMISSED')),
    external_check_state text CHECK (external_check_state IN ('PENDING', 'PASSED', 'FAILED', 'UNKNOWN')),
    external_updated_at timestamptz,
    external_deleted_at timestamptz,
    computed_state text NOT NULL GENERATED ALWAYS AS (changeset_computed_state(
        state, num_failures, publication_state, external_state)) STORED,
    diff_stat_added integer,
    diff_stat_deleted integer,
    -- The merge request as last synced from the code host.
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    -- Normalized from the metadata on sync, for search and sorting.
    title text,
    body text,
    -- The syncer's view of the head and base (to detect changes), and its schedule.
    sync_state jsonb NOT NULL DEFAULT '{}'::jsonb,
    last_synced_at timestamptz,
    next_sync_at timestamptz,
    syncer_error text,
    detached_at timestamptz,
    -- The reconciler should close the changeset on the code host.
    closing boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
-- One changeset per merge request.
CREATE UNIQUE INDEX changesets_repo_external_id_unique
    ON changesets (repo_id, external_service_type, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX changesets_batch_change_ids_idx ON changesets USING gin (batch_change_ids);
CREATE INDEX changesets_owned_by_idx ON changesets (owned_by_batch_change_id);
CREATE INDEX changesets_specs_idx ON changesets (current_spec_id, previous_spec_id);
CREATE INDEX changesets_computed_state_idx ON changesets (computed_state);
CREATE INDEX changesets_detached_at_idx ON changesets (detached_at) WHERE detached_at IS NOT NULL;
CREATE INDEX changesets_next_sync_idx ON changesets (next_sync_at)
    WHERE external_id IS NOT NULL AND external_deleted_at IS NULL;
CREATE INDEX changesets_dequeue_idx ON changesets (queued_at, id) WHERE state = 'queued';
CREATE INDEX changesets_heartbeat_idx ON changesets (last_heartbeat_at) WHERE state = 'processing';

-- A deleted batch change no longer tracks its changesets.
CREATE FUNCTION batch_changes_delete_changeset_refs() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE changesets SET batch_change_ids = batch_change_ids - OLD.id::text
    WHERE batch_change_ids ? OLD.id::text;
    RETURN NULL;
END;
$$;

CREATE TRIGGER batch_changes_delete_changeset_refs
    AFTER DELETE ON batch_changes
    FOR EACH ROW EXECUTE FUNCTION batch_changes_delete_changeset_refs();

-- Events of a changeset from the code host (notes, approvals, state changes, pipelines),
-- for the timeline and the burndown chart. Upserted by (changeset, kind, key) on sync.
CREATE TABLE changeset_events (
    id bigserial PRIMARY KEY,
    changeset_id bigint NOT NULL REFERENCES changesets (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind <> ''),
    key text NOT NULL CHECK (key <> ''),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (changeset_id, kind, key)
);

-- Bulk operations on changesets: one job per changeset, grouped by `bulk_group`.
CREATE TABLE changeset_jobs (
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
    bulk_group text NOT NULL,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    batch_change_id bigint NOT NULL REFERENCES batch_changes (id) ON DELETE CASCADE,
    changeset_id bigint NOT NULL REFERENCES changesets (id) ON DELETE CASCADE,
    job_type text NOT NULL
        CHECK (job_type IN ('comment', 'detach', 'reenqueue', 'merge', 'close', 'publish')),
    payload jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(payload) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX changeset_jobs_bulk_group_idx ON changeset_jobs (bulk_group);
CREATE INDEX changeset_jobs_batch_change_idx ON changeset_jobs (batch_change_id, created_at);
CREATE INDEX changeset_jobs_dequeue_idx ON changeset_jobs (queued_at, id) WHERE state = 'queued';
CREATE INDEX changeset_jobs_heartbeat_idx ON changeset_jobs (last_heartbeat_at)
    WHERE state = 'processing';

-- A user's credential for publishing changesets on a code host (`domain` 'batches'),
-- encrypted with the `user_credential` data key. `encryption_key_version` is the data key
-- version of `credential_enc`, so re-encryption after rotation can find old rows.
CREATE TABLE user_credentials (
    id bigserial PRIMARY KEY,
    domain text NOT NULL CHECK (domain IN ('batches')),
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    external_service_type text NOT NULL,
    -- The code host's base URL, e.g. https://gitlab.com/.
    external_service_id text NOT NULL,
    credential_enc bytea NOT NULL,
    encryption_key_version integer NOT NULL,
    -- The public half of the credential's SSH keypair, if it has one (shown to the user to
    -- add to the code host).
    ssh_public_key text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (domain, user_id, external_service_type, external_service_id)
);
CREATE INDEX user_credentials_key_version_idx ON user_credentials (encryption_key_version);

-- The site-wide credential for a code host, used when a user has none (admin-managed),
-- encrypted with the `site_credential` data key.
CREATE TABLE site_credentials (
    id bigserial PRIMARY KEY,
    external_service_type text NOT NULL,
    external_service_id text NOT NULL,
    credential_enc bytea NOT NULL,
    encryption_key_version integer NOT NULL,
    ssh_public_key text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (external_service_type, external_service_id)
);
