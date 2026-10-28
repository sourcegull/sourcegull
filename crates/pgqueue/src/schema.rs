//! The standard queue table shape.
//!
//! Each domain owns its queue table and migration; these helpers produce the
//! shared columns and indexes so every queue looks the same to [`crate::Queue`].

use crate::Error;

/// Longest accepted table name. Leaves room for the `queue_` channel prefix
/// and the `_dequeue_idx` / `_heartbeat_idx` index suffixes within
/// Postgres' 63-byte identifier limit.
pub const MAX_TABLE_NAME_LEN: usize = 48;

/// Column definitions shared by every queue table, to embed in `CREATE TABLE`.
///
/// Domain payload columns are appended after these by the domain migration.
/// `execution_logs` is a jsonb array of [`crate::ExecutionLogEntry`] objects.
pub fn queue_columns_sql() -> &'static str {
    "id bigserial PRIMARY KEY,
    state text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'processing', 'completed', 'errored', 'failed', 'canceled')),
    num_resets integer NOT NULL DEFAULT 0,
    num_failures integer NOT NULL DEFAULT 0,
    queued_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,
    process_after timestamptz,
    worker_hostname text NOT NULL DEFAULT '',
    last_heartbeat_at timestamptz,
    failure_message text,
    execution_logs jsonb NOT NULL DEFAULT '[]'::jsonb,
    cancel boolean NOT NULL DEFAULT false"
}

/// `CREATE INDEX` statements for the dequeue and stalled-job scans of `table`.
///
/// `table` must pass [`validate_table_name`]; it is interpolated verbatim.
pub fn queue_indexes_sql(table: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {table}_dequeue_idx ON {table} (queued_at, id) \
         WHERE state = 'queued';\n\
         CREATE INDEX IF NOT EXISTS {table}_heartbeat_idx ON {table} (last_heartbeat_at) \
         WHERE state = 'processing';"
    )
}

/// A full `CREATE TABLE` plus indexes, for tests and simple domains.
///
/// `extra_columns` are payload column definitions, e.g. `"repo_id bigint NOT NULL"`.
pub fn create_queue_table_sql(table: &str, extra_columns: &[&str]) -> Result<String, Error> {
    validate_table_name(table)?;
    let mut cols = queue_columns_sql().to_owned();
    for c in extra_columns {
        cols.push_str(",\n    ");
        cols.push_str(c);
    }
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} (\n    {cols}\n);\n{}",
        queue_indexes_sql(table)
    ))
}

/// The `LISTEN`/`NOTIFY` channel for a queue table: `queue_<table>`.
pub fn channel_name(table: &str) -> String {
    format!("queue_{table}")
}

/// Accepts lowercase unquoted identifiers: `[a-z_][a-z0-9_]*`, at most
/// [`MAX_TABLE_NAME_LEN`] bytes. Schema-qualified names are not supported.
pub fn validate_table_name(table: &str) -> Result<(), Error> {
    let mut chars = table.chars();
    let ok = table.len() <= MAX_TABLE_NAME_LEN
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidTableName(table.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names() {
        assert!(validate_table_name("index_jobs").is_ok());
        assert!(validate_table_name("_x1").is_ok());
        for bad in ["", "1abc", "Jobs", "a-b", "a.b", "a;drop", &"x".repeat(49)] {
            assert!(validate_table_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_sql_includes_extras() {
        let sql = create_queue_table_sql("t", &["repo_id bigint NOT NULL"]).unwrap();
        assert!(
            sql.contains("cancel boolean NOT NULL DEFAULT false,\n    repo_id bigint NOT NULL")
        );
        assert!(sql.contains("t_dequeue_idx"));
    }
}
