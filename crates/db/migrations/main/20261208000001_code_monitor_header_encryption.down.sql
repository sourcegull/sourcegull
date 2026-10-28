-- Run out-of-band migration 2 in reverse first: encrypted headers cannot be decrypted in SQL
-- and are lost here.
ALTER TABLE cm_actions DROP CONSTRAINT cm_actions_header_one_form;
ALTER TABLE cm_actions DROP COLUMN header_enc;
