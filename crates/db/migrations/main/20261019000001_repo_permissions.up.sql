-- Repository permissions (docs/PLAN.md §4 "Permissions", §6.21).
--
-- A repository is restricted when it is private, not marked unrestricted, and not listed by a
-- connection without an `authorization` block (unless explicit permissions are enabled, which
-- disregards that, as in the original). A restricted repository is readable by the users with
-- a `user_repo_permissions` row for it; every other repository is readable by everyone.

-- Whether the connection's config has no `authorization` block, so the private repos it lists
-- are readable by every user. Existing rows start restricted (fail closed); the discovery
-- sync sets the flag from the config.
ALTER TABLE external_services ADD COLUMN unrestricted boolean NOT NULL DEFAULT false;

-- Per-repository permission state.
CREATE TABLE repo_permissions (
    repo_id      integer     PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    -- Readable by everyone (`setRepositoryPermissionsUnrestricted`).
    unrestricted boolean     NOT NULL DEFAULT false,
    -- When the code host permissions of the repository were last synced (repo-centric sync).
    synced_at    timestamptz,
    updated_at   timestamptz NOT NULL DEFAULT now()
);

-- Who may read which restricted repository. `explicit` rows come from the explicit
-- permissions API, `sync` rows from the code host; each source replaces only its own rows.
CREATE TABLE user_repo_permissions (
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id    integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    source     text        NOT NULL CHECK (source IN ('explicit', 'sync')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, repo_id, source)
);
CREATE INDEX user_repo_permissions_repo_id_idx ON user_repo_permissions (repo_id);

-- The repositories of `user_repo_permissions` per user as a serialized roaring bitmap, rewritten
-- in the same transaction as the rows (§6.21 "Enforcement"). Web nodes cache it per user.
CREATE TABLE user_perm_bitmaps (
    user_id    bigint      PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    bitmap     bytea       NOT NULL,
    version    bigint      NOT NULL DEFAULT 1,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Explicit permissions for users who do not exist yet, by the `permissions.userMapping.bindID`
-- in effect when they were set (a lowercased username or email). They become
-- `user_repo_permissions` rows when a user with that username is created or verifies that email.
CREATE TABLE user_pending_permissions (
    bind_kind  text        NOT NULL CHECK (bind_kind IN ('username', 'email')),
    bind_id    text        NOT NULL CHECK (bind_id <> ''),
    repo_id    integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bind_kind, bind_id, repo_id)
);
CREATE INDEX user_pending_permissions_repo_id_idx ON user_pending_permissions (repo_id);

-- Whether reading repository `p_repo_id` (with `p_private`) needs a `user_repo_permissions` row.
-- `p_user_mapping` is `permissions.userMapping.enabled`. The one definition of the rule: every
-- authorized query and the web nodes' unrestricted bitmap use it.
CREATE FUNCTION repo_restricted(p_repo_id integer, p_private boolean, p_user_mapping boolean)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT p_private
       AND NOT EXISTS (SELECT 1 FROM repo_permissions rp WHERE rp.repo_id = p_repo_id AND rp.unrestricted)
       AND (p_user_mapping OR NOT EXISTS (
               SELECT 1 FROM external_service_repos esr
               JOIN external_services es ON es.id = esr.external_service_id
               WHERE esr.repo_id = p_repo_id AND es.unrestricted AND es.deleted_at IS NULL))
$$;
