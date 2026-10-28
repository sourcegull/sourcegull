-- Access requests (§6.22).
CREATE TABLE access_requests (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
    email text NOT NULL,
    additional_info text NOT NULL DEFAULT '',
    status text NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'APPROVED', 'REJECTED')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    decision_by_user_id bigint REFERENCES users (id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX access_requests_email_key ON access_requests (lower(email));
CREATE INDEX access_requests_status_id_idx ON access_requests (status, id);

-- Single-use account setup links; only a SHA-256 hash is stored.
CREATE TABLE access_request_invites (
    token_hash bytea PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);
CREATE INDEX access_request_invites_expires_idx ON access_request_invites (expires_at);
