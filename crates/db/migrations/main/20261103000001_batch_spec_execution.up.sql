-- Server-side execution of batch specs (§6.14 "Execution", §9 M5).
--
-- `execution_requested_at`: when `executeBatchSpec` ran. Until then a resolved spec is
-- PENDING; afterwards one with no execution jobs (every workspace skipped or cached) is
-- COMPLETED.
-- `auto_apply_enabled`: apply the spec once every execution completed
-- (`executeBatchSpec(autoApplyEnabled: true)`); `auto_applied_at` records that it was
-- applied, so only one finishing job applies it.
ALTER TABLE batch_specs
    ADD COLUMN execution_requested_at timestamptz,
    ADD COLUMN auto_apply_enabled boolean NOT NULL DEFAULT false,
    ADD COLUMN auto_applied_at timestamptz;
