-- Orgs and teams pages (docs/PLAN.md §6.22, §6.23).
--
-- Org invitations get a link token (only its SHA-256 is stored) so an invitation can be opened
-- from its URL (`invitationByToken`), as in the original's invitation links.
ALTER TABLE org_invitations ADD COLUMN IF NOT EXISTS token_sha256 bytea UNIQUE;

-- Teams synced from GitLab groups (`own.syncTeamsFromGitLabGroups`): the code host connection
-- and the group id they mirror. Synced teams are read-only; a group that disappears deletes its
-- team. NULL for teams created in Seeker.
ALTER TABLE teams
    ADD COLUMN IF NOT EXISTS external_service_id bigint
        REFERENCES external_services (id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS external_id text;
CREATE UNIQUE INDEX IF NOT EXISTS teams_external_key ON teams (external_service_id, external_id)
    WHERE external_service_id IS NOT NULL;
