-- Batch spec workspace files (§6.14 "Local", src-cli compatibility, §9 M5): the files a
-- server-side batch spec's steps `mount`, uploaded by src-cli (`src batch remote`) to
-- `POST /.api/files/batch-changes/{batchSpecID}` before the spec is executed, and downloaded
-- by executors (`/.executors/files/batch-changes/…`) and the spec's administrators
-- (`/.api/files/batch-changes/…`).
--
-- `path` is the file's directory relative to the batch spec's directory (`''` at its root),
-- `filename` its name; a new upload of the same path and name replaces the old one.
CREATE TABLE batch_spec_workspace_files (
    id bigserial PRIMARY KEY,
    rand_id text NOT NULL UNIQUE DEFAULT replace(gen_random_uuid()::text, '-', ''),
    batch_spec_id bigint NOT NULL REFERENCES batch_specs (id) ON DELETE CASCADE,
    path text NOT NULL,
    filename text NOT NULL,
    size bigint NOT NULL,
    content bytea NOT NULL,
    modified_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (batch_spec_id, path, filename)
);
