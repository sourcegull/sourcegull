-- Core identity tables (docs/PLAN.md §4 "Core"): users, emails, orgs,
-- invitations, access tokens and sessions.

CREATE TABLE users (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    username       text        NOT NULL CHECK (username <> '' AND char_length(username) <= 255),
    display_name   text,
    avatar_url     text,
    site_admin     boolean     NOT NULL DEFAULT false,
    password_hash  text,
    failed_logins  integer     NOT NULL DEFAULT 0 CHECK (failed_logins >= 0),
    locked_until   timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz
);
-- Usernames are unique case-insensitively among live users; soft-deleting a
-- user frees the name.
CREATE UNIQUE INDEX users_username_lower_key ON users (lower(username)) WHERE deleted_at IS NULL;
CREATE INDEX users_created_at_idx ON users (created_at);

CREATE TABLE user_emails (
    user_id                   bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email                     text        NOT NULL CHECK (email <> ''),
    verified_at               timestamptz,
    is_primary                boolean     NOT NULL DEFAULT false,
    verification_code_hash    text,
    last_verification_sent_at timestamptz,
    created_at                timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, email)
);
-- A verified address belongs to at most one user.
CREATE UNIQUE INDEX user_emails_verified_email_key ON user_emails (lower(email)) WHERE verified_at IS NOT NULL;
-- At most one primary address per user.
CREATE UNIQUE INDEX user_emails_one_primary_key ON user_emails (user_id) WHERE is_primary;
CREATE INDEX user_emails_email_lower_idx ON user_emails (lower(email));

CREATE TABLE orgs (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name         text        NOT NULL CHECK (name <> '' AND char_length(name) <= 255),
    display_name text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz
);
CREATE UNIQUE INDEX orgs_name_lower_key ON orgs (lower(name)) WHERE deleted_at IS NULL;

CREATE TABLE org_members (
    org_id     bigint      NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role       text        NOT NULL DEFAULT 'member' CHECK (role IN ('member', 'admin')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);
CREATE INDEX org_members_user_id_idx ON org_members (user_id);

CREATE TABLE org_invitations (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    org_id            bigint      NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    sender_user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    recipient_user_id bigint      REFERENCES users (id) ON DELETE CASCADE,
    recipient_email   text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    notified_at       timestamptz,
    expires_at        timestamptz,
    responded_at      timestamptz,
    -- true = accepted, false = declined, NULL = no response yet.
    response_accepted boolean,
    revoked_at        timestamptz,
    CHECK (recipient_user_id IS NOT NULL OR recipient_email IS NOT NULL)
);
CREATE INDEX org_invitations_org_id_idx ON org_invitations (org_id);
CREATE INDEX org_invitations_recipient_user_id_idx ON org_invitations (recipient_user_id);
CREATE INDEX org_invitations_recipient_email_idx ON org_invitations (lower(recipient_email));

-- Personal access tokens. Only the SHA-256 of the secret is stored (§6.22).
CREATE TABLE access_tokens (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    subject_user_id bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    creator_user_id bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    value_sha256    bytea       NOT NULL UNIQUE,
    -- Non-secret display prefix of the token, e.g. `skp_ab12`.
    prefix          text        NOT NULL DEFAULT '',
    note            text        NOT NULL DEFAULT '',
    scopes          text[]      NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_used_at    timestamptz,
    expires_at      timestamptz,
    deleted_at      timestamptz
);
CREATE INDEX access_tokens_subject_user_id_idx ON access_tokens (subject_user_id) WHERE deleted_at IS NULL;

-- Server-side sessions, keyed by the hash of the cookie value.
CREATE TABLE sessions (
    id_hash        bytea PRIMARY KEY,
    user_id        bigint      REFERENCES users (id) ON DELETE CASCADE,
    data           jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_active_at timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL
);
CREATE INDEX sessions_user_id_idx ON sessions (user_id);
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);
