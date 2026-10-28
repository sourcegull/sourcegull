-- Text search index state per repo (docs/PLAN.md §6.4.3, §6.23), as last reported by the repo
-- node that indexes it. The shards themselves live on the node; this row backs the site admin's
-- repository filters (`indexed`/`notIndexed`), counts and the index errors, which the node keeps
-- nowhere else. Written by the node's indexer after each build, removal or failure.
CREATE TABLE search_index_repos (
    repo_id                 integer     PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    -- The repo node that holds the index.
    node                    text        NOT NULL,
    index_status            text        NOT NULL DEFAULT 'not_indexed'
        CHECK (index_status IN ('not_indexed', 'indexed')),
    -- The indexed branches: [{"name": "HEAD", "commit": "<oid>"}, …].
    branches                jsonb       NOT NULL DEFAULT '[]'::jsonb,
    -- When the index last changed (a build finished).
    last_indexed_at         timestamptz,
    docs                    bigint      NOT NULL DEFAULT 0,
    content_bytes           bigint      NOT NULL DEFAULT 0,
    index_bytes             bigint      NOT NULL DEFAULT 0,
    lines                   bigint      NOT NULL DEFAULT 0,
    default_branch_lines    bigint      NOT NULL DEFAULT 0,
    shards                  integer     NOT NULL DEFAULT 0,
    -- The error of the last build attempt, cleared by the next success.
    last_error              text,
    last_error_at           timestamptz,
    updated_at              timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX search_index_repos_indexed_idx ON search_index_repos (repo_id)
    WHERE index_status = 'indexed';
CREATE INDEX search_index_repos_error_idx ON search_index_repos (repo_id)
    WHERE last_error IS NOT NULL;
