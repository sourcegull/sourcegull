-- Background jobs and admin logs (docs/PLAN.md §6.23, §5.3, §5.6). Every table here is bounded:
-- writers prune as they insert, so none grows with uptime.

-- Leader-elected routines (§5.3): one row per (routine, process) that ever held its lease.
CREATE TABLE background_routine_instances (
    routine         text        NOT NULL,
    host_name       text        NOT NULL,
    last_started_at timestamptz,
    last_stopped_at timestamptz,
    PRIMARY KEY (routine, host_name)
);

-- The most recent runs of each routine (a run is one pass of its loop, not a start or stop).
-- Writers keep the newest few per routine.
CREATE TABLE background_routine_runs (
    id          bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    routine     text        NOT NULL,
    host_name   text        NOT NULL,
    at          timestamptz NOT NULL,
    duration_ms bigint      NOT NULL CHECK (duration_ms >= 0),
    error       text
);
CREATE INDEX background_routine_runs_routine_idx ON background_routine_runs (routine, id DESC);

-- Daily run statistics per routine, for the last few days.
CREATE TABLE background_routine_stats (
    routine           text    NOT NULL,
    day               date    NOT NULL,
    run_count         bigint  NOT NULL DEFAULT 0,
    error_count       bigint  NOT NULL DEFAULT 0,
    min_duration_ms   bigint  NOT NULL,
    max_duration_ms   bigint  NOT NULL,
    total_duration_ms bigint  NOT NULL DEFAULT 0,
    PRIMARY KEY (routine, day)
);

-- The outbound request log (§5.6): code host API calls, newest `outboundRequestLogLimit` kept.
-- URLs and headers are stored with secrets redacted.
CREATE TABLE outbound_requests (
    id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    started_at           timestamptz NOT NULL,
    method               text        NOT NULL,
    url                  text        NOT NULL,
    request_headers      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    request_body         text        NOT NULL DEFAULT '',
    status_code          integer     NOT NULL DEFAULT 0,
    response_headers     jsonb       NOT NULL DEFAULT '{}'::jsonb,
    duration_ms          bigint      NOT NULL DEFAULT 0,
    error_message        text        NOT NULL DEFAULT '',
    creation_stack_frame text        NOT NULL DEFAULT '',
    call_stack           text        NOT NULL DEFAULT ''
);

-- Slow GraphQL requests and searches (§5.6), above `observability.logSlowGraphQLRequests` /
-- `observability.logSlowSearches`; the newest few kept.
CREATE TABLE slow_requests (
    id          bigint           GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    started_at  timestamptz      NOT NULL,
    duration_ms double precision NOT NULL,
    -- `graphql` or `search`.
    kind        text             NOT NULL CHECK (kind IN ('graphql', 'search')),
    user_id     bigint,
    name        text             NOT NULL DEFAULT '',
    source      text             NOT NULL DEFAULT '',
    repository  text,
    variables   text             NOT NULL DEFAULT '',
    errors      jsonb            NOT NULL DEFAULT '[]'::jsonb,
    query       text             NOT NULL DEFAULT '',
    filepath    text
);
