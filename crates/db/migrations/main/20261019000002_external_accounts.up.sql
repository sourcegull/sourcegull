-- Linked code host accounts and OAuth sign-in flows (docs/PLAN.md §4 "Core", §6.22).

-- A user's account on a code host: created by GitLab OAuth sign-in (with the user's tokens) or
-- by the permissions sync matching a user by username (without tokens). One live row per code
-- host account, and at most one account per user on each code host instance.
CREATE TABLE user_external_accounts (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id       bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 'gitlab'.
    service_type  text        NOT NULL CHECK (service_type <> ''),
    -- The code host instance (normalized base URL, as `repo.external_service_id`).
    service_id    text        NOT NULL CHECK (service_id <> ''),
    -- The OAuth application that issued `auth_data_enc` ('' without tokens).
    client_id     text        NOT NULL DEFAULT '',
    -- The account id on the code host (the GitLab user id).
    account_id    text        NOT NULL CHECK (account_id <> ''),
    -- What the code host says about the account (username, name, email, avatar). No secrets.
    account_data  jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- The OAuth tokens (JSON), encrypted with the `oauth_token` data key (§5.5).
    auth_data_enc bytea,
    -- Set when the tokens stopped working (refresh failed or the code host answered 401).
    expired_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    deleted_at    timestamptz
);
CREATE UNIQUE INDEX user_external_accounts_account_key
    ON user_external_accounts (service_type, service_id, account_id) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX user_external_accounts_user_service_key
    ON user_external_accounts (user_id, service_type, service_id) WHERE deleted_at IS NULL;

-- OAuth authorization-code flows in progress: the CSRF state (hashed) and the PKCE verifier,
-- kept until the code host redirects back or the flow expires.
CREATE TABLE oauth_flows (
    state_hash    bytea       PRIMARY KEY,
    -- The auth provider (`auth.providers` entry) the flow started with.
    provider      text        NOT NULL,
    pkce_verifier text        NOT NULL,
    -- Where to send the browser after signing in (a path on this instance).
    return_to     text        NOT NULL DEFAULT '/',
    expires_at    timestamptz NOT NULL
);
CREATE INDEX oauth_flows_expires_at_idx ON oauth_flows (expires_at);
