-- Watch actions that post to a URL (webhook, Slack, Mattermost, Telegram) are delivered by the
-- outbound webhooks sender as channels, and their attempts go to the same delivery log. A row
-- is either an attempt of an event to a registered webhook (`job_id`, `outbound_webhook_id`)
-- or an attempt of a watch's action job (`cm_action_job_id`, with the `channel` it used).

ALTER TABLE outbound_webhook_logs
    ALTER COLUMN job_id DROP NOT NULL,
    ALTER COLUMN outbound_webhook_id DROP NOT NULL,
    ADD COLUMN cm_action_job_id bigint REFERENCES cm_action_jobs (id) ON DELETE CASCADE,
    ADD COLUMN channel text;
ALTER TABLE outbound_webhook_logs ADD CONSTRAINT outbound_webhook_logs_one_target CHECK (
    (job_id IS NOT NULL AND outbound_webhook_id IS NOT NULL
        AND cm_action_job_id IS NULL AND channel IS NULL)
    OR (job_id IS NULL AND outbound_webhook_id IS NULL
        AND cm_action_job_id IS NOT NULL AND channel IS NOT NULL)
);
CREATE INDEX outbound_webhook_logs_cm_action_job_idx ON outbound_webhook_logs (cm_action_job_id)
    WHERE cm_action_job_id IS NOT NULL;
