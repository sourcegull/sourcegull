-- Configuration as code: the declarative files (SEEKER_RESOURCES_FILE, SEEKER_GLOBAL_SETTINGS_FILE)
-- and the objects they own. The leader worker writes these tables; the UI and API read them to
-- keep owned objects read-only.

-- One row per declared entry of the resources file, naming the object it owns. `kind` is the
-- file's section name, `object_id` the object's id as text (a feature flag's name).
-- `applied_sha256` hashes the resolved entry last written to the object.
CREATE TABLE declared_resources (
    kind           text        NOT NULL,
    key            text        NOT NULL,
    object_id      text        NOT NULL,
    applied_sha256 bytea       NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, key),
    UNIQUE (kind, object_id)
);

-- The declarative files a deployment uses and how their last application went. `kinds` lists
-- the sections the resources file manages; with `exclusive` they own every object of those
-- kinds. A row exists only while the file is configured.
CREATE TABLE declared_sources (
    source         text        PRIMARY KEY CHECK (source IN ('resources', 'global_settings')),
    path           text        NOT NULL,
    kinds          text[]      NOT NULL DEFAULT '{}',
    exclusive      boolean     NOT NULL DEFAULT false,
    applied_sha256 bytea,
    applied_at     timestamptz,
    error          text,
    error_at       timestamptz,
    checked_at     timestamptz NOT NULL DEFAULT now()
);

-- The `siteAdmins` email addresses, lower-cased: a user with one of them verified is site admin,
-- and while any are declared the first user to sign up is not made site admin.
CREATE TABLE declared_site_admins (
    email text PRIMARY KEY CHECK (email = lower(email))
);

-- What the resources file granted: site admin (`kind` 'siteAdmins', `key` '') or a role (`kind`
-- 'roles' and the role's key). Removing a user from the file takes back only these grants.
CREATE TABLE declared_grants (
    kind    text   NOT NULL,
    key     text   NOT NULL,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (kind, key, user_id)
);
CREATE INDEX declared_grants_user_id_idx ON declared_grants (user_id);
