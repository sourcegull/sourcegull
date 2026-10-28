-- Auto-indexing (docs/PLAN.md §6.12.5): per-repository index configuration, the site's
-- inference script, scheduler bookkeeping and reindex marks on uploads.

-- Index configuration edited in the UI (the original's lsif_index_configuration): JSON
-- {"index_jobs": [...]}. It takes precedence over seeker.yaml / sourcegraph.yaml and
-- inference.
CREATE TABLE codeintel_index_configuration (
    repository_id integer PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    data text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The site's inference script (Lua) that adds to or disables the built-in recognizers. One
-- row at most.
CREATE TABLE codeintel_inference_scripts (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    script text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The scheduler's view of each repository: when the periodic sweep last looked at it, and
-- the per-repository rate limit of commits queued for inference (a fixed window).
CREATE TABLE codeintel_autoindex_repos (
    repository_id integer PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    scanned_at timestamptz,
    window_started_at timestamptz NOT NULL DEFAULT now(),
    window_commits integer NOT NULL DEFAULT 0
);

-- How far the scheduler has consumed repo_ref_events. One row.
CREATE TABLE codeintel_autoindex_cursor (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_event_id bigint NOT NULL DEFAULT 0
);
INSERT INTO codeintel_autoindex_cursor (last_event_id)
    SELECT COALESCE(max(id), 0) FROM repo_ref_events;

-- Set by reindexPreciseIndex on an upload an index job produced: the next index of its
-- commit, root and indexer replaces it.
ALTER TABLE codeintel_uploads ADD COLUMN should_reindex boolean NOT NULL DEFAULT false;

CREATE INDEX codeintel_index_jobs_queued_idx ON codeintel_index_jobs (repository_id, queued_at DESC);
