-- Code Ownership (docs/PLAN.md §6.18): teams (§6.22, the minimal model owner resolution and
-- assigned teams need), CODEOWNERS files uploaded by site admins, and manually assigned owners.

-- A team: a name owners can be resolved to (`@group/subgroup` in a CODEOWNERS file resolves to
-- the team of that name), with direct members. Names are unique case-insensitively and may
-- not equal a username or org name (checked when a team is created or renamed).
CREATE TABLE teams (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name           text        NOT NULL CHECK (name <> '' AND char_length(name) <= 255),
    display_name   text,
    -- Managed elsewhere: only site admins may change it.
    readonly       boolean     NOT NULL DEFAULT false,
    parent_team_id bigint      REFERENCES teams (id) ON DELETE CASCADE,
    creator_id     bigint      REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (parent_team_id IS DISTINCT FROM id)
);
CREATE UNIQUE INDEX teams_name_lower_key ON teams (lower(name));
CREATE INDEX teams_parent_team_id_idx ON teams (parent_team_id);

CREATE TABLE team_members (
    team_id    bigint      NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, user_id)
);
CREATE INDEX team_members_user_id_idx ON team_members (user_id);

-- A CODEOWNERS file ingested for a repository (`addCodeownersFile`); it replaces the file in
-- the repository.
CREATE TABLE codeowners_files (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id    integer     NOT NULL UNIQUE REFERENCES repo (id) ON DELETE CASCADE,
    contents   text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Owners assigned to a repository (`file_path` = ''), a directory or a file; an assignment
-- covers everything below its path.
CREATE TABLE assigned_owners (
    id                   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_user_id        bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id              integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    file_path            text        NOT NULL,
    who_assigned_user_id bigint      REFERENCES users (id) ON DELETE SET NULL,
    assigned_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repo_id, file_path, owner_user_id)
);
CREATE INDEX assigned_owners_owner_user_id_idx ON assigned_owners (owner_user_id);

CREATE TABLE assigned_teams (
    id                   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_team_id        bigint      NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
    repo_id              integer     NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    file_path            text        NOT NULL,
    who_assigned_user_id bigint      REFERENCES users (id) ON DELETE SET NULL,
    assigned_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repo_id, file_path, owner_team_id)
);
CREATE INDEX assigned_teams_owner_team_id_idx ON assigned_teams (owner_team_id);
