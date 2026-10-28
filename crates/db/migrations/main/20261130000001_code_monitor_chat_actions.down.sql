DELETE FROM cm_actions WHERE kind IN ('mattermost_webhook', 'telegram');
ALTER TABLE cm_actions DROP COLUMN chat_id;
ALTER TABLE cm_actions DROP CONSTRAINT cm_actions_kind_check;
ALTER TABLE cm_actions ADD CONSTRAINT cm_actions_kind_check
    CHECK (kind IN ('email', 'webhook', 'slack_webhook'));
