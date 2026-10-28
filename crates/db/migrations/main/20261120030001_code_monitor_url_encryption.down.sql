-- Run out-of-band migration 2 in reverse first: encrypted URLs cannot be decrypted in SQL and
-- are lost here.
ALTER TABLE cm_actions DROP CONSTRAINT cm_actions_url_one_form;
ALTER TABLE cm_actions DROP COLUMN url_enc;
