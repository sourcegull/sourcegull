-- Set by a repo node when it shuts down cleanly (a restart or an upgrade), cleared by its next
-- heartbeat: an unreachable node with a recent value is restarting rather than down.
ALTER TABLE repo_nodes ADD COLUMN stopping_at timestamptz;
