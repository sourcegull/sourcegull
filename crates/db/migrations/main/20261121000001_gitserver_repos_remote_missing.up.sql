-- A code host project whose git repository does not exist yet (GitLab `empty_repo`: created
-- without a README and never pushed to) is recorded as cloned-but-empty with this flag set,
-- instead of as a failed clone (docs/PLAN.md §6.3). The repo node clears it on the first fetch
-- that finds the repository.
ALTER TABLE gitserver_repos ADD COLUMN remote_missing boolean NOT NULL DEFAULT false;
