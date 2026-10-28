-- Cross-upload lookups for precise code navigation (docs/PLAN.md §6.12.1, §6.12.4).
--
-- Occurrences, hovers and per-document data live in each upload's code graph file in the blob
-- store. Postgres only keeps what must be queried across uploads:
-- - which uploads hold a global symbol (references across repositories), keyed by the first
--   eight bytes of the symbol's SHA-256;
-- - which packages an upload defines ('export') or only uses ('import'), to resolve a
--   definition through package monikers.
CREATE TABLE codeintel_symbol_postings (
    symbol_hash bigint NOT NULL,
    upload_id bigint NOT NULL REFERENCES codeintel_uploads (id) ON DELETE CASCADE,
    -- Bit 1: a definition, 2: a reference, 4: an implementation's definition.
    flags smallint NOT NULL,
    PRIMARY KEY (symbol_hash, upload_id)
);
CREATE INDEX codeintel_symbol_postings_upload_idx ON codeintel_symbol_postings (upload_id);

CREATE TABLE codeintel_package_monikers (
    upload_id bigint NOT NULL REFERENCES codeintel_uploads (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('export', 'import')),
    scheme text NOT NULL,
    manager text NOT NULL,
    name text NOT NULL,
    version text NOT NULL,
    PRIMARY KEY (upload_id, kind, scheme, manager, name, version)
);
CREATE INDEX codeintel_package_monikers_package_idx
    ON codeintel_package_monikers (scheme, manager, name, version, kind);

ALTER TABLE codeintel_uploads ADD COLUMN num_symbols integer;
