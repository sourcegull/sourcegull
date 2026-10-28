-- Code host connections, repositories, discovery sync history, repo update (clone/fetch)
-- requests, ref events and webhook logs (docs/PLAN.md §4 "Code hosts and repos", §6.2).

-- A code host connection. The JSONC configuration (with its token and webhook secrets) is
-- stored encrypted with the `external_service_config` data key (§5.5).
CREATE TABLE external_services (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind              text        NOT NULL CHECK (kind IN ('GITLAB')),
    display_name      text        NOT NULL CHECK (display_name <> ''),
    config_enc        bytea       NOT NULL,
    -- Identifies the connection in `/.api/webhooks/{uuid}`.
    webhook_uuid      text        NOT NULL UNIQUE DEFAULT gen_random_uuid()::text,
    last_sync_at      timestamptz,
    -- When the next discovery sync is due; NULL = never (deleted connections).
    next_sync_at      timestamptz DEFAULT now(),
    last_full_sync_at timestamptz,
    -- Incremental listing watermark: the newest `last_activity_at` seen so far.
    sync_watermark    timestamptz,
    last_sync_error   text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz
);
CREATE INDEX external_services_next_sync_at_idx ON external_services (next_sync_at)
    WHERE deleted_at IS NULL;

-- Discovery sync runs (a pgqueue queue, §5.2). The finished rows are the sync history shown
-- in the admin UI. At most one queued run per connection.
CREATE TABLE external_service_sync_jobs (
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
    external_service_id bigint  NOT NULL REFERENCES external_services (id) ON DELETE CASCADE,
    -- Why the run was queued: 'scheduled', 'manual', 'config_changed', 'webhook'.
    reason              text    NOT NULL DEFAULT 'scheduled',
    -- Filled in by the run.
    full_sync           boolean,
    repos_synced        integer NOT NULL DEFAULT 0,
    repos_added         integer NOT NULL DEFAULT 0,
    repos_modified      integer NOT NULL DEFAULT 0,
    repos_deleted       integer NOT NULL DEFAULT 0,
    repos_unmodified    integer NOT NULL DEFAULT 0
);
CREATE INDEX external_service_sync_jobs_dequeue_idx ON external_service_sync_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX external_service_sync_jobs_heartbeat_idx ON external_service_sync_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE UNIQUE INDEX external_service_sync_jobs_one_queued ON external_service_sync_jobs (external_service_id)
    WHERE state = 'queued';
CREATE INDEX external_service_sync_jobs_service_idx ON external_service_sync_jobs (external_service_id, id DESC);

-- Repositories. Ids are int so they fit in roaring bitmaps (§4). A repo is identified on its
-- code host by (external_service_type, external_service_id, external_id), which survives
-- renames and transfers; soft-deleted rows keep it so a returning repo gets its old id back.
CREATE TABLE repo (
    id                    integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name                  text        NOT NULL CHECK (name <> ''),
    uri                   text        NOT NULL DEFAULT '',
    url                   text        NOT NULL DEFAULT '',
    description           text        NOT NULL DEFAULT '',
    fork                  boolean     NOT NULL DEFAULT false,
    archived              boolean     NOT NULL DEFAULT false,
    private               boolean     NOT NULL DEFAULT false,
    visibility            text        NOT NULL DEFAULT 'public'
        CHECK (visibility IN ('public', 'internal', 'private')),
    stars                 integer     NOT NULL DEFAULT 0,
    topics                text[]      NOT NULL DEFAULT '{}',
    default_branch        text,
    last_activity_at      timestamptz,
    external_id           text        NOT NULL,
    external_service_type text        NOT NULL,
    external_service_id   text        NOT NULL,
    -- Host-specific metadata (the raw project object, trimmed).
    metadata              jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- Pinned repo node (§2.2); NULL = rendezvous-hash assignment.
    shard                 text,
    -- Reason the repo is blocked from being served; NULL = not blocked.
    blocked               text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    deleted_at            timestamptz,
    UNIQUE (external_service_type, external_service_id, external_id)
);
CREATE UNIQUE INDEX repo_name_key ON repo (lower(name)) WHERE deleted_at IS NULL;
CREATE INDEX repo_name_pattern_idx ON repo (lower(name) text_pattern_ops) WHERE deleted_at IS NULL;
CREATE INDEX repo_created_at_idx ON repo (created_at) WHERE deleted_at IS NULL;

-- Which connections list which repos. A repo is soft-deleted once no connection lists it.
CREATE TABLE external_service_repos (
    external_service_id bigint      NOT NULL REFERENCES external_services (id) ON DELETE CASCADE,
    repo_id             integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    -- Clone URL without credentials; the repo node adds them from the connection config.
    clone_url           text        NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (external_service_id, repo_id)
);
CREATE INDEX external_service_repos_repo_id_idx ON external_service_repos (repo_id);

-- Free-form repo metadata (`repo:has.meta(key:value)`); a NULL value is a tag.
CREATE TABLE repo_kvps (
    repo_id    integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    key        text        NOT NULL CHECK (key <> ''),
    value      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, key)
);

-- The repo node's view of each repo, written by the repo node. A row is created (not_cloned)
-- when discovery first inserts the repo.
CREATE TABLE gitserver_repos (
    repo_id          integer     PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    clone_status     text        NOT NULL DEFAULT 'not_cloned'
        CHECK (clone_status IN ('not_cloned', 'cloning', 'cloned')),
    cloning_progress text,
    last_fetched     timestamptz,
    last_changed     timestamptz,
    last_error       text,
    repo_size_bytes  bigint,
    head_oid         text,
    -- The repo node that holds the repo, as reported by that node.
    node             text,
    corrupted_at     timestamptz,
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX gitserver_repos_clone_status_idx ON gitserver_repos (clone_status);
CREATE INDEX gitserver_repos_last_error_idx ON gitserver_repos (repo_id) WHERE last_error IS NOT NULL;

-- Requests to clone or fetch a repo (a pgqueue queue with a priority, §5.2, §6.2.5).
-- Coalesced per repo: at most one queued row per repo (enqueueing again raises its priority),
-- plus whatever is processing, so a push storm costs one fetch in flight plus one queued.
-- Higher priority is dequeued first. Producers send `NOTIFY queue_repo_update_jobs`.
CREATE TABLE repo_update_jobs (
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
    repo_id  integer  NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    priority smallint NOT NULL DEFAULT 0,
    -- Why it was queued: 'discovered', 'activity', 'webhook', 'manual', 'scheduled'.
    reason   text     NOT NULL DEFAULT 'scheduled'
);
CREATE INDEX repo_update_jobs_dequeue_idx ON repo_update_jobs (priority DESC, queued_at, id)
    WHERE state = 'queued';
CREATE INDEX repo_update_jobs_heartbeat_idx ON repo_update_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE UNIQUE INDEX repo_update_jobs_one_queued ON repo_update_jobs (repo_id)
    WHERE state = 'queued';
CREATE INDEX repo_update_jobs_repo_id_idx ON repo_update_jobs (repo_id, id DESC);

-- Ref changes: the fan-out source for event-driven consumers (indexing, monitors, insights,
-- auto-indexing; §1 item 4). 'webhook' rows are what the code host claimed (unverified);
-- 'fetch' rows are written by the repo node after a fetch actually moved the ref.
-- Short retention.
CREATE TABLE repo_ref_events (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    ref     text        NOT NULL,
    -- NULL when the ref was created.
    old_oid text,
    -- NULL when the ref was deleted.
    new_oid text,
    source  text        NOT NULL CHECK (source IN ('webhook', 'fetch')),
    at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX repo_ref_events_repo_id_idx ON repo_ref_events (repo_id, id);
CREATE INDEX repo_ref_events_at_idx ON repo_ref_events (at);

-- Incoming webhook deliveries, for the admin UI (§6.2.5). Retention-bounded.
CREATE TABLE webhook_logs (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    received_at         timestamptz NOT NULL DEFAULT now(),
    external_service_id bigint      REFERENCES external_services (id) ON DELETE SET NULL,
    -- The event name (`X-Gitlab-Event`), when known.
    event               text,
    status_code         integer     NOT NULL,
    -- Selected request headers and the body (secrets removed).
    request             jsonb       NOT NULL,
    error               text
);
CREATE INDEX webhook_logs_received_at_idx ON webhook_logs (received_at);
CREATE INDEX webhook_logs_external_service_id_idx ON webhook_logs (external_service_id, id DESC);
