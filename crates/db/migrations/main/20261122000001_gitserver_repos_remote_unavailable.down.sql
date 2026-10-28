ALTER TABLE gitserver_repos ADD COLUMN remote_missing boolean NOT NULL DEFAULT false;
UPDATE gitserver_repos SET remote_missing = remote_unavailable IS NOT NULL;
ALTER TABLE gitserver_repos DROP COLUMN remote_unavailable;
