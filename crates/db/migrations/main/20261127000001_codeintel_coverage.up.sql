-- The newest uploads and index jobs of a repository, for the precise index coverage report
-- (its latest upload and index job, and whether the latest one failed).
CREATE INDEX codeintel_uploads_repo_latest_idx ON codeintel_uploads (repository_id, id DESC);
CREATE INDEX codeintel_index_jobs_repo_latest_idx ON codeintel_index_jobs (repository_id, id DESC);
