-- Problems a sync run reported that did not fail it (e.g. a project where a webhook could not
-- be registered).
ALTER TABLE external_service_sync_jobs ADD COLUMN warnings text[] NOT NULL DEFAULT '{}';
