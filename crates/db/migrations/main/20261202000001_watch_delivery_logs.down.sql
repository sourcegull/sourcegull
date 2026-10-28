DELETE FROM outbound_webhook_logs WHERE cm_action_job_id IS NOT NULL;
DROP INDEX outbound_webhook_logs_cm_action_job_idx;
ALTER TABLE outbound_webhook_logs DROP CONSTRAINT outbound_webhook_logs_one_target;
ALTER TABLE outbound_webhook_logs
    DROP COLUMN channel,
    DROP COLUMN cm_action_job_id,
    ALTER COLUMN job_id SET NOT NULL,
    ALTER COLUMN outbound_webhook_id SET NOT NULL;
