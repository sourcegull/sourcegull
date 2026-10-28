-- The CODY/ACCESS permission guarded no feature; drop it with its role grants.
DELETE FROM permissions WHERE namespace = 'CODY' AND action = 'ACCESS';
