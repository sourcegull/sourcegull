-- Ranking from the code graph (docs/PLAN.md §6.12.6).
--
-- A ranking run exports the reference graph of the SCIP uploads serving each repository's
-- default branch head, runs PageRank over it and stores each repository's file ranks.
-- `inputs` fingerprints the serving uploads, so a run is skipped when they did not change.
CREATE TABLE ranking_runs (
    id bigserial PRIMARY KEY,
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    inputs text NOT NULL,
    num_uploads integer NOT NULL DEFAULT 0,
    num_paths bigint,
    num_edges bigint,
    iterations integer,
    -- Repositories whose ranks this run changed.
    num_repos_changed integer,
    failure_message text
);
CREATE INDEX ranking_runs_finished_idx ON ranking_runs (finished_at DESC) WHERE finished_at IS NOT NULL;

-- One row per repository: its ranks as one compact payload (sorted, front-coded paths with
-- 16-bit ranks, `seeker_types::ranking`), so a run writes one row per repository instead of
-- one per path (no row churn or vacuum load on large instances) and the indexer reads a
-- repository's ranks with one lookup. `generation` is the run that last changed the payload;
-- repo nodes rebuild a repository's shards when it differs from the one they hold.
CREATE TABLE ranking_path_ranks (
    repository_id integer PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    generation bigint NOT NULL,
    num_paths integer NOT NULL,
    payload bytea NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
