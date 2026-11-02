-- When each code host connection's token expires, as last checked, and which expiry alert
-- has been emailed for that expiry.
CREATE TABLE external_service_token_checks (
    external_service_id bigint      PRIMARY KEY REFERENCES external_services (id) ON DELETE CASCADE,
    checked_at          timestamptz NOT NULL,
    -- `external_services.updated_at` of the configuration that was checked.
    config_updated_at   timestamptz NOT NULL,
    -- 'unknown' (the code host does not say), 'never' or 'at' (`expires_at`).
    expiry              text        NOT NULL CHECK (expiry IN ('unknown', 'never', 'at')),
    expires_at          timestamptz CHECK ((expiry = 'at') = (expires_at IS NOT NULL)),
    -- Why the last check failed; the previous expiry is kept.
    error               text,
    -- The smallest number of days before `expires_at` an alert was sent for (0: expired).
    alerted_days        integer
);
