DROP INDEX IF EXISTS oauth_clients_metadata_document_idx;
ALTER TABLE oauth_authorization_codes DROP COLUMN IF EXISTS replayed_at;
DROP INDEX IF EXISTS oauth_grants_authorization_id_idx;
ALTER TABLE oauth_grants DROP COLUMN IF EXISTS authorization_id;
