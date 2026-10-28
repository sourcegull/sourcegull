-- RBAC (docs/PLAN.md §4, §6.22): roles, permissions and their assignments,
-- seeded with the system roles and default permissions.

CREATE TABLE roles (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       text        NOT NULL CHECK (name <> ''),
    -- System roles cannot be renamed or deleted.
    system     boolean     NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);
CREATE UNIQUE INDEX roles_name_key ON roles (name) WHERE deleted_at IS NULL;

CREATE TABLE permissions (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    namespace  text        NOT NULL CHECK (namespace <> ''),
    action     text        NOT NULL CHECK (action <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (namespace, action)
);

CREATE TABLE role_permissions (
    role_id       bigint      NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
    permission_id bigint      NOT NULL REFERENCES permissions (id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (role_id, permission_id)
);
CREATE INDEX role_permissions_permission_id_idx ON role_permissions (permission_id);

CREATE TABLE user_roles (
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role_id    bigint      NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role_id)
);
CREATE INDEX user_roles_role_id_idx ON user_roles (role_id);

INSERT INTO roles (name, system) VALUES ('USER', true), ('SITE_ADMINISTRATOR', true);

INSERT INTO permissions (namespace, action) VALUES
    ('BATCH_CHANGES', 'READ'),
    ('BATCH_CHANGES', 'WRITE'),
    ('OWNERSHIP', 'ASSIGN'),
    ('REPO_METADATA', 'WRITE'),
    ('CODY', 'ACCESS');

-- Defaults: USER gets everything except OWNERSHIP; SITE_ADMINISTRATOR gets all.
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r CROSS JOIN permissions p
WHERE r.name = 'SITE_ADMINISTRATOR'
   OR (r.name = 'USER' AND p.namespace <> 'OWNERSHIP');
