-- Rollout windows and webhooks for changesets (§6.14).

-- Set when a rollout window (or "publish now") released the changeset from `scheduled`:
-- the reconciler then publishes it instead of scheduling it again. Cleared on re-enqueue.
ALTER TABLE changesets ADD COLUMN window_released boolean NOT NULL DEFAULT false;

-- Scheduled changesets are released oldest update first.
CREATE INDEX changesets_scheduled_idx ON changesets (updated_at, id) WHERE state = 'scheduled';

-- Changesets of a branch (pipeline webhooks without a merge request).
CREATE INDEX changesets_external_branch_idx ON changesets (repo_id, external_branch)
    WHERE external_branch IS NOT NULL;
