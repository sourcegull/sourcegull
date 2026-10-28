-- The lease epoch of each singleton: bumped every time a process takes the lease. Fenced
-- singleton writes check it, so a leader that lost its lease cannot overwrite its successor.
CREATE TABLE leader_epochs (
    name text PRIMARY KEY,
    epoch bigint NOT NULL,
    acquired_at timestamptz NOT NULL DEFAULT now()
);
