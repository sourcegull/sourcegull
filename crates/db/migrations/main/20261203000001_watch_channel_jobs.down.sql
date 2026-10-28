DELETE FROM outbound_webhook_jobs WHERE event_type IS NULL;
ALTER TABLE outbound_webhook_logs DROP CONSTRAINT outbound_webhook_logs_one_target;
ALTER TABLE outbound_webhook_logs ADD CONSTRAINT outbound_webhook_logs_one_target CHECK (
    (job_id IS NOT NULL AND outbound_webhook_id IS NOT NULL
        AND cm_action_job_id IS NULL AND channel IS NULL)
    OR (job_id IS NULL AND outbound_webhook_id IS NULL
        AND cm_action_job_id IS NOT NULL AND channel IS NOT NULL)
);
DROP INDEX outbound_webhook_jobs_cm_trigger_job_idx;
DROP INDEX outbound_webhook_jobs_cm_action_idx;
ALTER TABLE outbound_webhook_jobs DROP CONSTRAINT outbound_webhook_jobs_kind;
ALTER TABLE outbound_webhook_jobs
    DROP COLUMN cm_trigger_job_id,
    DROP COLUMN cm_action_id,
    DROP COLUMN secret_enc,
    DROP COLUMN target_enc,
    DROP COLUMN channel,
    ALTER COLUMN event_type SET NOT NULL;
