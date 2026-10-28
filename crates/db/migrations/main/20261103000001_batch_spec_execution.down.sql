ALTER TABLE batch_specs
    DROP COLUMN auto_applied_at,
    DROP COLUMN auto_apply_enabled,
    DROP COLUMN execution_requested_at;
