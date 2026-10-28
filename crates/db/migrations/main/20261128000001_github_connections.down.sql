DELETE FROM external_services WHERE kind = 'GITHUB';
ALTER TABLE external_services DROP CONSTRAINT external_services_kind_check;
ALTER TABLE external_services
    ADD CONSTRAINT external_services_kind_check CHECK (kind IN ('GITLAB'));
