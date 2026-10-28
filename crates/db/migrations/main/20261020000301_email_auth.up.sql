-- Single-use password reset links (§6.22).
CREATE TABLE password_reset_codes (
    user_id bigint PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    code_hash bytea NOT NULL,
    expires_at timestamptz NOT NULL
);
