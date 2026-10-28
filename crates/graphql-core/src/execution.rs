//! The steps an executor ran, as `ExecutionLogEntry` (auto-indexing jobs and bulk change
//! workspaces).

use async_graphql::{Context, Object};
use chrono::{DateTime, Utc};
use sourcegull_pgqueue::ExecutionLogEntry;

use crate::context::viewer;

/// `ExecutionLogEntry`: one step an executor ran.
#[derive(Debug, Clone)]
pub struct ExecutionLogEntryNode {
    entry: ExecutionLogEntry,
    /// `out` is shown to everyone who sees the entry, not only to site admins.
    out_visible: bool,
}

impl ExecutionLogEntryNode {
    /// An entry whose output only site admins see (auto-indexing jobs).
    pub fn admin_only(entry: ExecutionLogEntry) -> Self {
        Self {
            entry,
            out_visible: false,
        }
    }

    /// An entry whose output everyone who may see it sees (the caller checked access, e.g. a
    /// bulk change spec workspace's logs for those who may administer the spec).
    pub fn visible(entry: ExecutionLogEntry) -> Self {
        Self {
            entry,
            out_visible: true,
        }
    }
}

#[Object(name = "ExecutionLogEntry")]
impl ExecutionLogEntryNode {
    /// The step, e.g. `step.docker.indexer`.
    async fn key(&self) -> &str {
        &self.entry.key
    }

    async fn command(&self) -> &[String] {
        &self.entry.command
    }

    async fn start_time(&self) -> Option<DateTime<Utc>> {
        self.entry.start_time
    }

    /// Null while the step runs.
    async fn exit_code(&self) -> Option<i32> {
        self.entry.exit_code
    }

    /// The step's output (for auto-indexing jobs empty for viewers who are not site admins).
    async fn out(&self, ctx: &Context<'_>) -> &str {
        if self.out_visible || viewer(ctx).is_some_and(|a| a.site_admin) {
            &self.entry.out
        } else {
            ""
        }
    }

    async fn duration_milliseconds(&self) -> Option<i32> {
        self.entry
            .duration_ms
            .map(|d| i32::try_from(d).unwrap_or(i32::MAX))
    }
}
