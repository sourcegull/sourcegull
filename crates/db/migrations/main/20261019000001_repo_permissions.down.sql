DROP FUNCTION IF EXISTS repo_restricted(integer, boolean, boolean);
DROP TABLE IF EXISTS user_pending_permissions;
DROP TABLE IF EXISTS user_perm_bitmaps;
DROP TABLE IF EXISTS user_repo_permissions;
DROP TABLE IF EXISTS repo_permissions;
ALTER TABLE external_services DROP COLUMN IF EXISTS unrestricted;
