INSERT INTO permissions (namespace, action) VALUES ('CODY', 'ACCESS');
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id
FROM roles r CROSS JOIN permissions p
WHERE p.namespace = 'CODY' AND r.name IN ('USER', 'SITE_ADMINISTRATOR');
