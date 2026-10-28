-- Notebooks (docs/PLAN.md §6.17).

-- A notebook: a title and a list of blocks (Markdown, search query, file, symbol) kept as jsonb
-- in the original's shape (`[{id, type: "md"|"query"|"file"|"symbol", markdownInput |
-- queryInput | fileInput | symbolInput}]`). Owned by a user or an org; private ones are visible
-- to the owning user or the org's members, public ones to everyone.
CREATE TABLE notebooks (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title             text        NOT NULL,
    blocks            jsonb       NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(blocks) = 'array'),
    public            boolean     NOT NULL DEFAULT false,
    creator_user_id   bigint      REFERENCES users (id) ON DELETE SET NULL,
    updater_user_id   bigint      REFERENCES users (id) ON DELETE SET NULL,
    namespace_user_id bigint      REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id  bigint      REFERENCES orgs (id) ON DELETE CASCADE,
    pattern_type      text        NOT NULL DEFAULT 'standard',
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    -- Full-text search over the title and the text of Markdown and query blocks. File and
    -- symbol blocks are left out: their repository names must not be findable by viewers who
    -- may not read those repositories (§6.21).
    content_tsvector  tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', title), 'A') ||
        jsonb_to_tsvector(
            'english',
            jsonb_path_query_array(blocks, '$[*].markdownInput.text') ||
                jsonb_path_query_array(blocks, '$[*].queryInput.text'),
            '["string"]'
        )
    ) STORED,
    CHECK ((namespace_user_id IS NULL) <> (namespace_org_id IS NULL))
);
CREATE INDEX notebooks_namespace_user_id_idx ON notebooks (namespace_user_id);
CREATE INDEX notebooks_namespace_org_id_idx ON notebooks (namespace_org_id);
CREATE INDEX notebooks_creator_user_id_idx ON notebooks (creator_user_id);
CREATE INDEX notebooks_content_tsvector_idx ON notebooks USING gin (content_tsvector);

CREATE TABLE notebook_stars (
    notebook_id bigint      NOT NULL REFERENCES notebooks (id) ON DELETE CASCADE,
    user_id     bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (notebook_id, user_id)
);
CREATE INDEX notebook_stars_user_id_idx ON notebook_stars (user_id);
