-- Code monitors (docs/PLAN.md §6.16). The data model follows the original's
-- `internal/codemonitors` (read for behaviour only): a monitor owned by a user or an org, one
-- `type:commit`/`type:diff` query as its trigger, and email, webhook and Slack webhook actions.
--
-- Evaluation is event-driven: the evaluator searches only the commits a ref update added
-- (`cm_last_searched` holds, per monitor, repository and watched revision, the last commit
-- searched). Each evaluation that searched something is a trigger job (the run log); a trigger
-- job with results queues one action job per enabled action (the action log).

CREATE TABLE cm_monitors (
    id bigserial PRIMARY KEY,
    -- NULL once the user has been deleted.
    created_by bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_by bigint REFERENCES users (id) ON DELETE SET NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    description text NOT NULL CHECK (description <> ''),
    enabled boolean NOT NULL DEFAULT true,
    namespace_user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    -- The trigger query (the original's `cm_queries`; one per monitor).
    query text NOT NULL CHECK (query <> ''),
    query_updated_at timestamptz NOT NULL DEFAULT now(),
    -- Set by resetTriggerQueryTimestamps: the next evaluation searches the whole history of
    -- every watched revision instead of recording the heads.
    search_all boolean NOT NULL DEFAULT false,
    CHECK ((namespace_user_id IS NULL) <> (namespace_org_id IS NULL))
);
CREATE INDEX cm_monitors_namespace_user_idx ON cm_monitors (namespace_user_id);
CREATE INDEX cm_monitors_namespace_org_idx ON cm_monitors (namespace_org_id);

CREATE TABLE cm_actions (
    id bigserial PRIMARY KEY,
    monitor_id bigint NOT NULL REFERENCES cm_monitors (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('email', 'webhook', 'slack_webhook')),
    enabled boolean NOT NULL DEFAULT true,
    include_results boolean NOT NULL DEFAULT false,
    -- Webhook and Slack webhook actions.
    url text NOT NULL DEFAULT '',
    -- Email actions.
    priority text NOT NULL DEFAULT 'NORMAL' CHECK (priority IN ('NORMAL', 'CRITICAL')),
    header text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cm_actions_monitor_idx ON cm_actions (monitor_id, id);

-- Recipients of an email action: users, or every member of an org.
CREATE TABLE cm_recipients (
    id bigserial PRIMARY KEY,
    action_id bigint NOT NULL REFERENCES cm_actions (id) ON DELETE CASCADE,
    namespace_user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    CHECK ((namespace_user_id IS NULL) <> (namespace_org_id IS NULL))
);
CREATE INDEX cm_recipients_action_idx ON cm_recipients (action_id);

-- The last commit searched per monitor, repository and watched revision ('' is the default
-- branch). A missing row means the pair was never seen: the evaluator records the head
-- without notifying (a new monitor, or a repository that newly matches).
CREATE TABLE cm_last_searched (
    monitor_id bigint NOT NULL REFERENCES cm_monitors (id) ON DELETE CASCADE,
    repo_id integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    rev text NOT NULL,
    commit_oid text NOT NULL,
    checked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (monitor_id, repo_id, rev)
);
CREATE INDEX cm_last_searched_repo_idx ON cm_last_searched (repo_id);

-- One evaluation of a monitor (a pgqueue table): the commit ranges to search, then the
-- results found.
CREATE TABLE cm_trigger_jobs (
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
    monitor_id bigint NOT NULL REFERENCES cm_monitors (id) ON DELETE CASCADE,
    -- [{repoId, repo, rev, from, to}]: `from` is NULL to search the whole history.
    ranges jsonb NOT NULL,
    -- The query as run (the monitor's query when the job was queued).
    query text NOT NULL,
    results jsonb,
    num_results integer NOT NULL DEFAULT 0
);
CREATE INDEX cm_trigger_jobs_dequeue_idx ON cm_trigger_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX cm_trigger_jobs_heartbeat_idx ON cm_trigger_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE INDEX cm_trigger_jobs_monitor_idx ON cm_trigger_jobs (monitor_id, id DESC);
CREATE INDEX cm_trigger_jobs_finished_at_idx ON cm_trigger_jobs (finished_at)
    WHERE finished_at IS NOT NULL;

-- One delivery of a trigger job's results through an action (a pgqueue table, retried with
-- backoff).
CREATE TABLE cm_action_jobs (
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
    action_id bigint NOT NULL REFERENCES cm_actions (id) ON DELETE CASCADE,
    trigger_job_id bigint NOT NULL REFERENCES cm_trigger_jobs (id) ON DELETE CASCADE,
    UNIQUE (action_id, trigger_job_id)
);
CREATE INDEX cm_action_jobs_dequeue_idx ON cm_action_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX cm_action_jobs_heartbeat_idx ON cm_action_jobs (last_heartbeat_at)
    WHERE state = 'processing';
CREATE INDEX cm_action_jobs_trigger_idx ON cm_action_jobs (trigger_job_id);

-- How far the evaluator has consumed repo_ref_events. One row.
CREATE TABLE cm_ref_events_cursor (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_event_id bigint NOT NULL DEFAULT 0
);
INSERT INTO cm_ref_events_cursor (last_event_id)
    SELECT COALESCE(max(id), 0) FROM repo_ref_events;
