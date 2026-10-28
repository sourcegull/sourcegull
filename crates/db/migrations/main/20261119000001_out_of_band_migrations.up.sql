-- Out-of-band migrations (docs/PLAN.md §6.27, §5.5): long-running data migrations that workers
-- run in the background, in batches, after the schema migrations. The registry lives in the
-- binary (`seeker-oobmigration`); processes upsert one row per registered migration here, and
-- the leader-elected runner records its progress. The columns follow the original's
-- `out_of_band_migrations` table so the admin API has the same shape.
CREATE TABLE out_of_band_migrations (
    -- Stable id from the registry (never reused).
    id                       integer          PRIMARY KEY,
    -- Owning team and the component whose data is migrated.
    team                     text             NOT NULL,
    component                text             NOT NULL,
    description              text             NOT NULL,
    -- The release (major.minor) that introduced the migration.
    introduced_version_major integer          NOT NULL,
    introduced_version_minor integer          NOT NULL,
    -- The release from which the data must be fully migrated: upgrades to it (and startup of
    -- it) are refused until progress reaches 1. NULL: not deprecated yet.
    deprecated_version_major integer,
    deprecated_version_minor integer,
    -- Fraction of the data in the new format (0..1), as the migration last measured it.
    progress                 double precision NOT NULL DEFAULT 0
        CHECK (progress >= 0 AND progress <= 1),
    created                  timestamptz      NOT NULL DEFAULT now(),
    -- When progress last changed.
    last_updated             timestamptz,
    -- Whether old-format readers still work while it runs (it may run while serving).
    non_destructive          boolean          NOT NULL,
    -- Always false in Seeker (no enterprise split); kept for API parity.
    is_enterprise            boolean          NOT NULL DEFAULT false,
    -- Run `down` instead of `up` (an admin is preparing a downgrade).
    apply_reverse            boolean          NOT NULL DEFAULT false,
    metadata                 jsonb            NOT NULL DEFAULT '{}'::jsonb,
    CHECK ((deprecated_version_major IS NULL) = (deprecated_version_minor IS NULL))
);

-- Errors from the migration's batches (the runner keeps the newest 100 per migration).
CREATE TABLE out_of_band_migrations_errors (
    id           bigserial   PRIMARY KEY,
    migration_id integer     NOT NULL REFERENCES out_of_band_migrations (id) ON DELETE CASCADE,
    message      text        NOT NULL,
    created      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX out_of_band_migrations_errors_migration_idx
    ON out_of_band_migrations_errors (migration_id, created DESC, id DESC);
