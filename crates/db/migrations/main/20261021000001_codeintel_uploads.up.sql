-- Precise code intelligence uploads (docs/PLAN.md §4 "Code intel", §6.12.1).
--
-- One row per SCIP index uploaded for (repository, commit, root, indexer). The row is a
-- pgqueue queue (§5.2) for processing; uploads still receiving parts are in state
-- 'uploading', which the queue never dequeues. Deleted uploads stay as 'deleting' until the
-- janitor has removed their blobs, then 'deleted'.
CREATE TABLE codeintel_uploads (
    id bigserial PRIMARY KEY,
    state text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('uploading', 'queued', 'processing', 'completed', 'errored', 'failed',
                         'canceled', 'deleting', 'deleted')),
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
    -- From the request, or from the index's tool info when the request has none.
    indexer text NOT NULL DEFAULT '',
    indexer_version text NOT NULL DEFAULT '',
    num_parts integer NOT NULL DEFAULT 1 CHECK (num_parts BETWEEN 1 AND 100000),
    uploaded_parts integer[] NOT NULL DEFAULT '{}',
    -- Bytes received (as sent, usually gzip-compressed).
    upload_size bigint,
    uploaded_at timestamptz NOT NULL DEFAULT now(),
    uploader_user_id bigint REFERENCES users (id) ON DELETE SET NULL,
    -- The auto-indexing job that produced the upload, if any.
    associated_index_id bigint,
    -- The code graph file written by processing.
    graph_key text,
    -- Statistics from processing.
    num_documents integer,
    num_occurrences bigint,
    -- Set by the retention scan (§6.12.1).
    expired boolean NOT NULL DEFAULT false,
    last_retention_scan_at timestamptz,
    deleted_at timestamptz,
    deleted_reason text
);
CREATE INDEX codeintel_uploads_dequeue_idx ON codeintel_uploads (queued_at, id)
    WHERE state = 'queued';
CREATE INDEX codeintel_uploads_heartbeat_idx ON codeintel_uploads (last_heartbeat_at)
    WHERE state = 'processing';
CREATE INDEX codeintel_uploads_repo_idx ON codeintel_uploads (repository_id, commit);
CREATE INDEX codeintel_uploads_visible_idx ON codeintel_uploads (repository_id, root, indexer)
    WHERE state = 'completed';
