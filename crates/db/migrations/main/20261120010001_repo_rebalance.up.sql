-- Repo node rebalancing (docs/PLAN.md §2.2): when repo nodes are added or removed, the repos
-- whose rendezvous-hash owner changed are copied to the new owner, verified, flipped
-- (`repo.shard`) and then removed from the old owner.

-- Each repo node's heartbeat with the node list it runs with. The planner moves repos only
-- while every listed node is alive and runs with the same list, so nodes in the middle of a
-- rollout cannot plan moves back and forth.
CREATE TABLE repo_nodes (
    addr       text        PRIMARY KEY,
    -- The node list (`SEEKER_REPO_NODES`) the node runs with, sorted.
    nodes      text[]      NOT NULL,
    started_at timestamptz NOT NULL,
    seen_at    timestamptz NOT NULL
);

-- One move per repo: the latest, kept for a while after it ends for the admin page.
--   queued → copying → flipped → done        copy-then-flip (`kind` copy)
--   queued → done                            the old owner is gone: reassign and re-clone
-- A failed attempt goes back to queued with a delay, and to failed after the last attempt;
-- a move that no longer matches the hash is cancelled.
CREATE TABLE repo_moves (
    repo_id         integer     PRIMARY KEY REFERENCES repo (id) ON DELETE CASCADE,
    from_node       text        NOT NULL,
    to_node         text        NOT NULL,
    kind            text        NOT NULL CHECK (kind IN ('copy', 'reclone')),
    state           text        NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'copying', 'flipped', 'done', 'failed', 'cancelled')),
    attempts        integer     NOT NULL DEFAULT 0,
    -- Pack bytes received by the new owner in the current attempt.
    bytes_copied    bigint      NOT NULL DEFAULT 0,
    last_error      text,
    next_attempt_at timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    started_at      timestamptz,
    flipped_at      timestamptz,
    finished_at     timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX repo_moves_to_idx ON repo_moves (to_node, state);
CREATE INDEX repo_moves_from_idx ON repo_moves (from_node, state);
CREATE INDEX repo_moves_state_idx ON repo_moves (state, updated_at);
