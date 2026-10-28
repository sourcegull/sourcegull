-- Telegram and Mattermost watch actions. A Telegram action keeps its bot token where the
-- webhook actions keep their URL (`url`, or `url_enc` when encrypted with the
-- `code_monitor_webhook` data key) and the chat it posts to in `chat_id`. A Mattermost action
-- keeps its incoming webhook URL there too.
ALTER TABLE cm_actions DROP CONSTRAINT cm_actions_kind_check;
ALTER TABLE cm_actions ADD CONSTRAINT cm_actions_kind_check
    CHECK (kind IN ('email', 'webhook', 'slack_webhook', 'mattermost_webhook', 'telegram'));
ALTER TABLE cm_actions ADD COLUMN chat_id text NOT NULL DEFAULT '';
