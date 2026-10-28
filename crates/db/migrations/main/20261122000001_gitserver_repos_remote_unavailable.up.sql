-- Why a code host project has no git repository the repo node can fetch (docs/PLAN.md §6.3):
-- 'missing' (GitLab `empty_repo`, never pushed to), 'disabled' (the project's repository
-- feature is switched off) or 'no_code_access' (the token sees the project but may not
-- download its code, e.g. below Reporter on a private project). The repo is recorded as
-- cloned with an empty local copy, not as a failed clone. This replaces the
-- `remote_missing` flag, which only covered the first case.
ALTER TABLE gitserver_repos ADD COLUMN remote_unavailable text
    CHECK (remote_unavailable IN ('missing', 'disabled', 'no_code_access'));
UPDATE gitserver_repos SET remote_unavailable = 'missing' WHERE remote_missing;
ALTER TABLE gitserver_repos DROP COLUMN remote_missing;
