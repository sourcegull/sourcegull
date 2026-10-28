-- A code monitor with a webhook action as releases before out-of-band migration 2 stored it
-- (URL in plaintext, url_enc NULL); scripts/test/upgrade.sh inserts it right after the schema
-- upgrade, and the workers must encrypt it. N-2 predates code monitors.
INSERT INTO cm_monitors (created_by, description, enabled, namespace_user_id, query)
SELECT id, 'upgrade check monitor', false, id, 'upgrade-check type:diff'
FROM users WHERE username = 'upgrade-admin';
INSERT INTO cm_actions (monitor_id, kind, url)
SELECT id, 'webhook', 'https://hooks.upgrade.invalid/upgrade-check-T0KEN'
FROM cm_monitors WHERE description = 'upgrade check monitor';
