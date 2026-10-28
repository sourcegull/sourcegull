-- Outbound webhooks (docs/PLAN.md §6.23), with the original's data model
-- (`internal/database/outbound_webhooks.go`, read for behaviour only).
--
-- Site admins register URLs that receive events (`batch_change:apply`, `changeset:publish`,
-- ...). A webhook's URL and HMAC secret are encrypted with the `outbound_webhook` data key, as
-- are queued payloads and delivery logs (they hold the payloads and the receiver's answers).

CREATE TABLE outbound_webhooks (
    id bigserial PRIMARY KEY,
    -- NULL once the user has been deleted.
    created_by bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_by bigint REFERENCES users (id) ON DELETE SET NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    url_enc bytea NOT NULL,
    secret_enc bytea NOT NULL
);

-- The events a webhook subscribes to. A NULL scope matches events of every scope.
CREATE TABLE outbound_webhook_event_types (
    id bigserial PRIMARY KEY,
    outbound_webhook_id bigint NOT NULL REFERENCES outbound_webhooks (id) ON DELETE CASCADE,
    event_type text NOT NULL CHECK (event_type <> ''),
    scope text
);
CREATE INDEX outbound_webhook_event_types_webhook_idx
    ON outbound_webhook_event_types (outbound_webhook_id);
CREATE INDEX outbound_webhook_event_types_event_type_idx
    ON outbound_webhook_event_types (event_type, scope);

-- One job per event (a pgqueue table); its worker sends the payload to every webhook
-- subscribed to the event type when it runs, and retries the ones that failed.
CREATE TABLE outbound_webhook_jobs (
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
    event_type text NOT NULL,
    scope text,
    -- The JSON body sent, encrypted.
    payload_enc bytea NOT NULL
);
CREATE INDEX outbound_webhook_jobs_dequeue_idx ON outbound_webhook_jobs (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX outbound_webhook_jobs_heartbeat_idx
    ON outbound_webhook_jobs (last_heartbeat_at) WHERE state = 'processing';
CREATE INDEX outbound_webhook_jobs_finished_at_idx ON outbound_webhook_jobs (finished_at)
    WHERE finished_at IS NOT NULL;

-- One delivery attempt of a job to a webhook. `status_code` is 0 when no response arrived
-- (refused by the SSRF guard, connection error, timeout); `error` then says why.
CREATE TABLE outbound_webhook_logs (
    id bigserial PRIMARY KEY,
    job_id bigint NOT NULL REFERENCES outbound_webhook_jobs (id) ON DELETE CASCADE,
    outbound_webhook_id bigint NOT NULL REFERENCES outbound_webhooks (id) ON DELETE CASCADE,
    sent_at timestamptz NOT NULL DEFAULT now(),
    status_code integer NOT NULL,
    -- Encrypted JSON of the request (method, URL, headers, body) and response (status,
    -- headers, body, truncated), and the encrypted error message.
    request_enc bytea NOT NULL,
    response_enc bytea,
    error_enc bytea
);
CREATE INDEX outbound_webhook_logs_webhook_idx
    ON outbound_webhook_logs (outbound_webhook_id, sent_at DESC, id DESC);
CREATE INDEX outbound_webhook_logs_job_idx ON outbound_webhook_logs (job_id, outbound_webhook_id);
CREATE INDEX outbound_webhook_logs_sent_at_idx ON outbound_webhook_logs (sent_at);
