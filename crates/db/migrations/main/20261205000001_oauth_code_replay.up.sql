-- The authorization a grant was created from, so a replayed authorization code revokes the
-- tokens issued for it (RFC 6749 §4.1.2). Set to NULL when the expired code is deleted.
ALTER TABLE oauth_grants
    ADD COLUMN authorization_id bigint REFERENCES oauth_authorization_codes (id) ON DELETE SET NULL;
CREATE INDEX oauth_grants_authorization_id_idx ON oauth_grants (authorization_id)
    WHERE authorization_id IS NOT NULL;

-- When a used code was presented again. A grant is not created from a replayed code.
ALTER TABLE oauth_authorization_codes ADD COLUMN replayed_at timestamptz;

-- Metadata document clients are deleted when unused.
CREATE INDEX oauth_clients_metadata_document_idx ON oauth_clients (fetched_at)
    WHERE kind = 'metadata_document';
