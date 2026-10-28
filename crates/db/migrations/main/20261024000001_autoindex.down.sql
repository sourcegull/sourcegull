DROP INDEX IF EXISTS codeintel_index_jobs_queued_idx;
ALTER TABLE codeintel_uploads DROP COLUMN IF EXISTS should_reindex;
DROP TABLE IF EXISTS codeintel_autoindex_cursor;
DROP TABLE IF EXISTS codeintel_autoindex_repos;
DROP TABLE IF EXISTS codeintel_inference_scripts;
DROP TABLE IF EXISTS codeintel_index_configuration;
