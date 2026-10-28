DROP INDEX IF EXISTS teams_external_key;
ALTER TABLE teams DROP COLUMN IF EXISTS external_id, DROP COLUMN IF EXISTS external_service_id;
ALTER TABLE org_invitations DROP COLUMN IF EXISTS token_sha256;
