-- Per-run count of repository listing errors, shown in the sync history (§6.23; the
-- original's ExternalServiceSyncJob.repoSyncErrors).
ALTER TABLE external_service_sync_jobs
    ADD COLUMN IF NOT EXISTS repo_sync_errors integer NOT NULL DEFAULT 0;
