-- Watch actions that post to a URL (webhook, Slack, Mattermost, Telegram) are queued on the
-- outbound webhook queue as channel jobs: the rendered message for one channel and target,
-- delivered by the same workers, retries and log as events. A job is either an event
-- (`event_type`, optional `scope`) or a channel job (`channel` and the encrypted `target_enc`,
-- optional `secret_enc`, with the watch action and trigger job it belongs to). `payload_enc`
-- is the body of both.

ALTER TABLE outbound_webhook_jobs
    ALTER COLUMN event_type DROP NOT NULL,
    ADD COLUMN channel text,
    ADD COLUMN target_enc bytea,
    ADD COLUMN secret_enc bytea,
    ADD COLUMN cm_action_id bigint REFERENCES cm_actions (id) ON DELETE CASCADE,
    ADD COLUMN cm_trigger_job_id bigint REFERENCES cm_trigger_jobs (id) ON DELETE CASCADE;
ALTER TABLE outbound_webhook_jobs ADD CONSTRAINT outbound_webhook_jobs_kind CHECK (
    (event_type IS NOT NULL AND channel IS NULL AND target_enc IS NULL AND secret_enc IS NULL
        AND cm_action_id IS NULL AND cm_trigger_job_id IS NULL)
    OR (event_type IS NULL AND scope IS NULL AND channel IS NOT NULL AND target_enc IS NOT NULL)
);
-- One channel job per watch action and trigger job.
CREATE UNIQUE INDEX outbound_webhook_jobs_cm_action_idx
    ON outbound_webhook_jobs (cm_action_id, cm_trigger_job_id) WHERE cm_action_id IS NOT NULL;
CREATE INDEX outbound_webhook_jobs_cm_trigger_job_idx ON outbound_webhook_jobs (cm_trigger_job_id)
    WHERE cm_trigger_job_id IS NOT NULL;

-- A log row is an attempt of an event to a registered webhook (`job_id`,
-- `outbound_webhook_id`), of a channel job (`job_id`, `channel`), or of a watch's action job
-- (`cm_action_job_id`, `channel`).
ALTER TABLE outbound_webhook_logs DROP CONSTRAINT outbound_webhook_logs_one_target;
ALTER TABLE outbound_webhook_logs ADD CONSTRAINT outbound_webhook_logs_one_target CHECK (
    (job_id IS NOT NULL AND outbound_webhook_id IS NOT NULL
        AND cm_action_job_id IS NULL AND channel IS NULL)
    OR (job_id IS NOT NULL AND outbound_webhook_id IS NULL
        AND cm_action_job_id IS NULL AND channel IS NOT NULL)
    OR (job_id IS NULL AND outbound_webhook_id IS NULL
        AND cm_action_job_id IS NOT NULL AND channel IS NOT NULL)
);
