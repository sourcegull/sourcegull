-- Code intelligence configuration policies and upload retention (docs/PLAN.md §6.12.1).
--
-- A policy names git objects (branches, tags or one commit) by a pattern and says how long
-- precise indexes visible from them are kept (`retention_*`) and, with auto-indexing
-- (§6.12.5), which commits are indexed (`index*`). A policy is global, applies to one
-- repository (`repository_id`), or to the repositories whose names match one of
-- `repository_patterns` (globs). Protected policies are built in: they can be edited but not
-- deleted, and keep their name, type and pattern.
CREATE TABLE codeintel_configuration_policies (
    id serial PRIMARY KEY,
    repository_id integer REFERENCES repo (id) ON DELETE CASCADE,
    repository_patterns text[],
    name text NOT NULL,
    -- GraphQL's GitObjectType: a commit, a branch (GIT_TREE, as in the original) or a tag.
    type text NOT NULL CHECK (type IN ('GIT_COMMIT', 'GIT_TREE', 'GIT_TAG')),
    pattern text NOT NULL,
    protected boolean NOT NULL DEFAULT false,
    retention_enabled boolean NOT NULL DEFAULT false,
    -- NULL keeps data forever.
    retention_duration_hours integer CHECK (retention_duration_hours > 0),
    retain_intermediate_commits boolean NOT NULL DEFAULT false,
    indexing_enabled boolean NOT NULL DEFAULT false,
    index_commit_max_age_hours integer CHECK (index_commit_max_age_hours > 0),
    index_intermediate_commits boolean NOT NULL DEFAULT false,
    syntactic_indexing_enabled boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (repository_id IS NULL OR repository_patterns IS NULL)
);
CREATE INDEX codeintel_configuration_policies_repo_idx
    ON codeintel_configuration_policies (repository_id);

-- The original's defaults: data visible from a branch tip is kept 12 weeks, from a tag 48
-- weeks, and from any commit on a branch one week.
INSERT INTO codeintel_configuration_policies
    (name, type, pattern, protected, retention_enabled, retention_duration_hours,
     retain_intermediate_commits)
VALUES
    ('Default tip-of-branch retention policy', 'GIT_TREE', '*', true, true, 2016, false),
    ('Default tag retention policy', 'GIT_TAG', '*', true, true, 8064, false),
    ('Default commit retention policy', 'GIT_TREE', '*', true, true, 168, true);

-- When the expirer last scanned a repository's uploads.
CREATE TABLE codeintel_retention_scans (
    repository_id integer PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    scanned_at timestamptz NOT NULL
);

CREATE INDEX codeintel_uploads_retention_idx ON codeintel_uploads (repository_id, uploaded_at)
    WHERE state = 'completed' AND NOT expired;
