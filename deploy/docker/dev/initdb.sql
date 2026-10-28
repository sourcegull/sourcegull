-- Runs once, when the dev Postgres volume is created (compose.yaml). `sourcegull` is created by the
-- image itself; `sourcegull_app` is for the production image (`app` profile).
CREATE DATABASE sourcegull_app;
