-- Search contexts and saved searches (docs/PLAN.md §4 "Search", §6.10).

-- A named set of repositories (and revisions) searches can be scoped to with `context:`.
-- Owned by a user or an org, or by the instance when both are NULL. Either lists repositories
-- (`search_context_repos`) or is defined by a query of repo filters (`query`).
CREATE TABLE search_contexts (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name              text        NOT NULL CHECK (name ~ '^[a-zA-Z0-9_.\-/]+$' AND char_length(name) <= 255),
    description       text        NOT NULL DEFAULT '',
    public            boolean     NOT NULL DEFAULT true,
    namespace_user_id bigint      REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id  bigint      REFERENCES orgs (id) ON DELETE CASCADE,
    query             text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (namespace_user_id IS NULL OR namespace_org_id IS NULL)
);
CREATE UNIQUE INDEX search_contexts_name_key
    ON search_contexts (coalesce(namespace_user_id, 0), coalesce(namespace_org_id, 0), lower(name));

CREATE TABLE search_context_repos (
    search_context_id bigint  NOT NULL REFERENCES search_contexts (id) ON DELETE CASCADE,
    repo_id           integer NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    -- '' is the default branch.
    revision          text    NOT NULL,
    PRIMARY KEY (search_context_id, repo_id, revision)
);

CREATE TABLE search_context_stars (
    search_context_id bigint      NOT NULL REFERENCES search_contexts (id) ON DELETE CASCADE,
    user_id           bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (search_context_id, user_id)
);

-- A user's default context; no row means `global`.
CREATE TABLE search_context_default (
    user_id           bigint NOT NULL PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    search_context_id bigint NOT NULL REFERENCES search_contexts (id) ON DELETE CASCADE
);

-- A query kept for later, owned by a user or an org. `secret` ones are visible to the owner
-- (and the org's members); `public` ones to every user.
CREATE TABLE saved_searches (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    description   text        NOT NULL CHECK (description <> ''),
    query         text        NOT NULL CHECK (query <> ''),
    owner_user_id bigint      REFERENCES users (id) ON DELETE CASCADE,
    owner_org_id  bigint      REFERENCES orgs (id) ON DELETE CASCADE,
    draft         boolean     NOT NULL DEFAULT false,
    visibility    text        NOT NULL DEFAULT 'secret' CHECK (visibility IN ('secret', 'public')),
    created_by    bigint      REFERENCES users (id) ON DELETE SET NULL,
    updated_by    bigint      REFERENCES users (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK ((owner_user_id IS NULL) <> (owner_org_id IS NULL))
);
CREATE INDEX saved_searches_owner_user_id_idx ON saved_searches (owner_user_id);
CREATE INDEX saved_searches_owner_org_id_idx ON saved_searches (owner_org_id);
