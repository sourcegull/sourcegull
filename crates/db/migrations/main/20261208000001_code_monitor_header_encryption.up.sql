-- Watch email headers at rest. The header of an email action (`Approved: password`) can carry
-- a mailing list password, so with encryption keys configured it is stored encrypted with the
-- `code_monitor_webhook` data key in `header_enc` and `header` is left empty. Without keys, or
-- for rows written before this migration, the plaintext stays in `header`; out-of-band
-- migration 2 encrypts those rows once keys exist (and decrypts them back when applied in
-- reverse, before a downgrade).
ALTER TABLE cm_actions ADD COLUMN header_enc bytea;
ALTER TABLE cm_actions ADD CONSTRAINT cm_actions_header_one_form
    CHECK (header_enc IS NULL OR header = '');
