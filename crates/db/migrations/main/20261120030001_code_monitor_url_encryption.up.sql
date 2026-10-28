-- Code monitor webhook and Slack webhook URLs at rest (docs/PLAN.md §5.5, §6.16). A URL often
-- carries its own credential (Slack incoming webhooks, tokens in query strings), so with
-- encryption keys configured it is stored encrypted with the `code_monitor_webhook` data key
-- in `url_enc` and `url` is left empty. Without keys, or for rows written before this
-- migration, the plaintext stays in `url`; out-of-band migration 2 encrypts those rows once
-- keys exist (and decrypts them back when applied in reverse, before a downgrade).
ALTER TABLE cm_actions ADD COLUMN url_enc bytea;
ALTER TABLE cm_actions ADD CONSTRAINT cm_actions_url_one_form CHECK (url_enc IS NULL OR url = '');
