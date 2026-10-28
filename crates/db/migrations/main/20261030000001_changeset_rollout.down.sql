DROP INDEX IF EXISTS changesets_external_branch_idx;
DROP INDEX IF EXISTS changesets_scheduled_idx;
ALTER TABLE changesets DROP COLUMN IF EXISTS window_released;
