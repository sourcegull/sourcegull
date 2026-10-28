-- The GitLab CI job that uploaded with its job token: job id, ref and the username of the
-- user who triggered it. Parts of a multipart upload started by a job are accepted only
-- from that job.
ALTER TABLE codeintel_uploads
    ADD COLUMN ci_job_id bigint,
    ADD COLUMN ci_job_ref text,
    ADD COLUMN ci_job_username text,
    -- Bytes of the parts received so far, checked against `codeIntel.maxUploadBytes`.
    ADD COLUMN uploaded_bytes bigint NOT NULL DEFAULT 0;
