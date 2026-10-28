-- OAuth authorization server for MCP clients.

-- Clients: registered through Dynamic Client Registration, or described by a client ID
-- metadata document (an https URL as client_id, fetched and cached).
CREATE TABLE oauth_clients (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    client_id     text        NOT NULL UNIQUE,
    kind          text        NOT NULL CHECK (kind IN ('registered', 'metadata_document')),
    client_name   text        NOT NULL DEFAULT '',
    client_uri    text,
    redirect_uris text[]      NOT NULL,
    -- Registered clients: the address that registered it (registrations are rate limited).
    registered_ip inet,
    created_at    timestamptz NOT NULL DEFAULT now(),
    -- Metadata document clients: when the document was last fetched.
    fetched_at    timestamptz
);
CREATE INDEX oauth_clients_registered_idx ON oauth_clients (created_at) WHERE kind = 'registered';

-- Authorization requests waiting for the user's consent, then their authorization code.
-- Only hashes of the request handle and the code are stored.
CREATE TABLE oauth_authorization_codes (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    request_sha256 bytea       NOT NULL UNIQUE,
    code_sha256    bytea       UNIQUE,
    client_id      bigint      NOT NULL REFERENCES oauth_clients (id) ON DELETE CASCADE,
    -- The user who approved the request.
    user_id        bigint      REFERENCES users (id) ON DELETE CASCADE,
    redirect_uri   text        NOT NULL,
    state          text,
    code_challenge text        NOT NULL,
    resource       text        NOT NULL,
    scopes         text[]      NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    approved_at    timestamptz,
    used_at        timestamptz
);
CREATE INDEX oauth_authorization_codes_expires_at_idx ON oauth_authorization_codes (expires_at);

-- What a user allowed a client: the tokens of one authorization.
CREATE TABLE oauth_grants (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    client_id    bigint      NOT NULL REFERENCES oauth_clients (id) ON DELETE CASCADE,
    user_id      bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    resource     text        NOT NULL,
    scopes       text[]      NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);
CREATE INDEX oauth_grants_user_id_idx ON oauth_grants (user_id) WHERE revoked_at IS NULL;
CREATE INDEX oauth_grants_client_id_idx ON oauth_grants (client_id);

-- Access and refresh tokens of a grant. Only the SHA-256 of a token is stored.
CREATE TABLE oauth_tokens (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    grant_id     bigint      NOT NULL REFERENCES oauth_grants (id) ON DELETE CASCADE,
    kind         text        NOT NULL CHECK (kind IN ('access', 'refresh')),
    value_sha256 bytea       NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    -- Refresh tokens: when it was exchanged. Presenting it again revokes the grant.
    used_at      timestamptz
);
CREATE INDEX oauth_tokens_grant_id_idx ON oauth_tokens (grant_id);
