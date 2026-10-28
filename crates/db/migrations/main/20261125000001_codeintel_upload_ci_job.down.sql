ALTER TABLE codeintel_uploads
    DROP COLUMN ci_job_id,
    DROP COLUMN ci_job_ref,
    DROP COLUMN ci_job_username,
    DROP COLUMN uploaded_bytes;
