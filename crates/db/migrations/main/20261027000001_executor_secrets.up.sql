-- Executor secrets (docs/PLAN.md §4 "Executors", §5.5, §6.13), as in the original.
--
-- A secret is global (no namespace) or belongs to a user or an organization, and is usable by
-- jobs of one scope: `batches` (batch changes) or `codeintel` (auto-indexing). The value is
-- encrypted with the `executor_secret` data key and never returned by the API. A namespace's
-- secret overwrites the global secret with the same key.
CREATE TABLE executor_secrets (
    id bigserial PRIMARY KEY,
    scope text NOT NULL CHECK (scope IN ('batches', 'codeintel')),
    -- The environment variable the secret is injected as.
    key text NOT NULL CHECK (key ~ '^[A-Z][A-Z0-9_]*$'),
    value_enc bytea NOT NULL,
    namespace_user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    namespace_org_id bigint REFERENCES orgs (id) ON DELETE CASCADE,
    -- NULL once the creator has been deleted.
    creator_id bigint REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (namespace_user_id IS NULL OR namespace_org_id IS NULL)
);
-- One secret per key and scope in each namespace, and in the global one.
CREATE UNIQUE INDEX executor_secrets_unique_key_namespace_user
    ON executor_secrets (key, namespace_user_id, scope) WHERE namespace_user_id IS NOT NULL;
CREATE UNIQUE INDEX executor_secrets_unique_key_namespace_org
    ON executor_secrets (key, namespace_org_id, scope) WHERE namespace_org_id IS NOT NULL;
CREATE UNIQUE INDEX executor_secrets_unique_key_global
    ON executor_secrets (key, scope)
    WHERE namespace_user_id IS NULL AND namespace_org_id IS NULL;

-- Every decryption of a secret's value: in the name of a user, or of an internal procedure
-- (`machine_user`, e.g. `codeintel-autoindexing`, with the executor job it was for).
CREATE TABLE executor_secret_access_logs (
    id bigserial PRIMARY KEY,
    executor_secret_id bigint NOT NULL REFERENCES executor_secrets (id) ON DELETE CASCADE,
    user_id bigint REFERENCES users (id) ON DELETE CASCADE,
    machine_user text NOT NULL DEFAULT '',
    job_id bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT user_id_or_machine_user CHECK (
        (user_id IS NULL AND machine_user <> '') OR (user_id IS NOT NULL AND machine_user = '')
    )
);
CREATE INDEX executor_secret_access_logs_secret_idx
    ON executor_secret_access_logs (executor_secret_id, created_at DESC, id DESC);
