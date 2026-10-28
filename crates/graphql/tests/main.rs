//! The integration tests: one module per file, linked into a single test binary.
//! A new file under `tests/` must be declared here.

mod account;
mod admin_logs;
mod analytics;
mod autoindex;
mod batches;
mod batches_apply;
mod batches_execution;
mod batches_local;
mod code_browsing;
mod code_monitors;
mod code_navigation;
mod codeintel_policies;
mod declared;
mod executor_secrets;
mod executors;
mod external_services;
mod graphql;
mod insights;
mod insights_admin;
mod notebooks;
mod oob_migrations;
mod orgs;
mod outbound_webhooks;
mod own;
mod own_signals;
mod permissions;
mod precise_index_coverage;
mod precise_indexes;
mod precise_navigation;
mod ranking;
mod repos;
mod repos_admin;
mod schema_file;
mod temporary_settings;
mod upgrade_readiness;
