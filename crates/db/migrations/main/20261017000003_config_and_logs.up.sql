-- Versioned settings and site config, feature flags, audit and analytics logs,
-- and encryption keys (docs/PLAN.md §4 "Core").

-- Settings cascade: each row is one immutable version for a subject
-- (global, an org or a user). The latest row per subject wins.
CREATE TABLE settings (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    subject_type   text        NOT NULL CHECK (subject_type IN ('global', 'org', 'user')),
    org_id         bigint      REFERENCES orgs (id) ON DELETE CASCADE,
    user_id        bigint      REFERENCES users (id) ON DELETE CASCADE,
    -- JSONC text, kept verbatim (comments and formatting preserved).
    contents       text        NOT NULL,
    author_user_id bigint      REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (subject_type = 'global' AND org_id IS NULL AND user_id IS NULL) OR
        (subject_type = 'org' AND org_id IS NOT NULL AND user_id IS NULL) OR
        (subject_type = 'user' AND user_id IS NOT NULL AND org_id IS NULL)
    )
);
CREATE INDEX settings_global_idx ON settings (id DESC) WHERE subject_type = 'global';
CREATE INDEX settings_org_idx ON settings (org_id, id DESC) WHERE org_id IS NOT NULL;
CREATE INDEX settings_user_idx ON settings (user_id, id DESC) WHERE user_id IS NOT NULL;

-- Site configuration: each row is one immutable version; the latest wins.
CREATE TABLE site_config (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    contents       text        NOT NULL,
    author_user_id bigint      REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE feature_flags (
    name       text PRIMARY KEY CHECK (name <> ''),
    flag_type  text        NOT NULL CHECK (flag_type IN ('bool', 'rollout')),
    bool_value boolean,
    -- Rollout in basis points (0..=10000) of users.
    rollout    integer CHECK (rollout BETWEEN 0 AND 10000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    CHECK (
        (flag_type = 'bool' AND bool_value IS NOT NULL AND rollout IS NULL) OR
        (flag_type = 'rollout' AND rollout IS NOT NULL AND bool_value IS NULL)
    )
);

CREATE TABLE feature_flag_overrides (
    flag_name  text        NOT NULL REFERENCES feature_flags (name) ON DELETE CASCADE ON UPDATE CASCADE,
    user_id    bigint      REFERENCES users (id) ON DELETE CASCADE,
    org_id     bigint      REFERENCES orgs (id) ON DELETE CASCADE,
    value      boolean     NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((user_id IS NULL) <> (org_id IS NULL))
);
CREATE UNIQUE INDEX feature_flag_overrides_user_key ON feature_flag_overrides (flag_name, user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX feature_flag_overrides_org_key ON feature_flag_overrides (flag_name, org_id) WHERE org_id IS NOT NULL;

-- Audit log (§6.23). No FK on user_id: entries outlive the user.
CREATE TABLE security_event_logs (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name              text        NOT NULL CHECK (name <> ''),
    url               text        NOT NULL DEFAULT '',
    user_id           bigint,
    anonymous_user_id text        NOT NULL DEFAULT '',
    source            text        NOT NULL DEFAULT '',
    argument          jsonb       NOT NULL DEFAULT '{}'::jsonb,
    version           text        NOT NULL DEFAULT '',
    timestamp         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX security_event_logs_timestamp_idx ON security_event_logs (timestamp DESC);
CREATE INDEX security_event_logs_user_id_idx ON security_event_logs (user_id, timestamp DESC);

-- Local usage analytics (§6.23). A plain table for now; to be time-partitioned.
CREATE TABLE event_logs (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name              text        NOT NULL CHECK (name <> ''),
    url               text        NOT NULL DEFAULT '',
    user_id           bigint,
    anonymous_user_id text        NOT NULL DEFAULT '',
    source            text        NOT NULL DEFAULT '',
    client            text,
    argument          jsonb       NOT NULL DEFAULT '{}'::jsonb,
    version           text        NOT NULL DEFAULT '',
    timestamp         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX event_logs_timestamp_idx ON event_logs (timestamp);
CREATE INDEX event_logs_name_timestamp_idx ON event_logs (name, timestamp);
CREATE INDEX event_logs_user_id_idx ON event_logs (user_id) WHERE user_id IS NOT NULL;

-- Data encryption keys, wrapped by the master key (§5.5).
CREATE TABLE encryption_keys (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    key_name    text        NOT NULL DEFAULT 'primary',
    version     integer     NOT NULL CHECK (version > 0),
    wrapped_key bytea       NOT NULL,
    -- active: used to encrypt and decrypt; retired: decrypt only; destroyed: unusable.
    state       text        NOT NULL CHECK (state IN ('active', 'retired', 'destroyed')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    retired_at  timestamptz,
    UNIQUE (key_name, version)
);
CREATE UNIQUE INDEX encryption_keys_one_active_key ON encryption_keys (key_name) WHERE state = 'active';
