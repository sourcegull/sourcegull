//! Executors: `executors` lists the executors that sent a
//! heartbeat recently, with their versions, queues and running jobs, and
//! `areExecutorsConfigured` tells whether `executors.accessToken` is set. Site admins only.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Duration, Utc};
use sourcegull_db::Executor;
use sourcegull_services::relay::marshal_id;

use crate::context::{db_error, page_size, require_site_admin, services, to_int};

const KIND: &str = "Executor";

/// An executor that has not sent a heartbeat for this long is inactive (heartbeats are sent
/// every 5 seconds).
const ACTIVE_WITHIN_SECS: i64 = 30;

/// How an executor's version compares with the server's.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExecutorCompatibility {
    UpToDate,
    Outdated,
    TooNew,
}

/// A job an executor is running.
#[derive(SimpleObject)]
pub struct ExecutorJob {
    queue: String,
    id: i32,
}

/// An executor.
pub struct ExecutorNode(pub Executor);

#[Object(name = "Executor")]
impl ExecutorNode {
    async fn id(&self) -> ID {
        ID(marshal_id(KIND, self.0.id))
    }

    async fn hostname(&self) -> &str {
        &self.0.hostname
    }

    /// The first queue (single-queue clients read this field).
    async fn queue_name(&self) -> &str {
        self.0.queue_names.first().map_or("", String::as_str)
    }

    async fn queue_names(&self) -> &[String] {
        &self.0.queue_names
    }

    /// Whether the executor sent a heartbeat in the last 30 seconds.
    async fn active(&self) -> bool {
        Utc::now() - self.0.last_seen_at < Duration::seconds(ACTIVE_WITHIN_SECS)
    }

    async fn os(&self) -> &str {
        &self.0.os
    }

    async fn architecture(&self) -> &str {
        &self.0.architecture
    }

    async fn executor_version(&self) -> &str {
        &self.0.executor_version
    }

    /// `docker` or `kubernetes`.
    async fn runtime(&self) -> &str {
        &self.0.runtime
    }

    async fn runtime_version(&self) -> &str {
        &self.0.runtime_version
    }

    /// The Docker version, when the runtime is Docker.
    async fn docker_version(&self) -> &str {
        if self.0.runtime == "docker" {
            &self.0.runtime_version
        } else {
            ""
        }
    }

    async fn git_version(&self) -> &str {
        &self.0.git_version
    }

    async fn first_seen_at(&self) -> DateTime<Utc> {
        self.0.first_seen_at
    }

    async fn last_seen_at(&self) -> DateTime<Utc> {
        self.0.last_seen_at
    }

    /// The jobs the executor reported running in its last heartbeat.
    async fn active_jobs(&self) -> Vec<ExecutorJob> {
        self.0
            .active_jobs
            .iter()
            .map(|j| ExecutorJob {
                queue: j.queue.clone(),
                id: to_int(j.id),
            })
            .collect()
    }

    /// Null when either version cannot be compared (development builds).
    async fn compatibility(&self) -> Option<ExecutorCompatibility> {
        compatibility(env!("CARGO_PKG_VERSION"), &self.0.executor_version)
    }
}

fn compatibility(server: &str, executor: &str) -> Option<ExecutorCompatibility> {
    use sourcegull_executor_api::major_minor;
    let (s, e) = (major_minor(server)?, major_minor(executor)?);
    Some(if sourcegull_executor_api::compatible(server, executor) {
        ExecutorCompatibility::UpToDate
    } else if e < s {
        ExecutorCompatibility::Outdated
    } else {
        ExecutorCompatibility::TooNew
    })
}

/// A page of executors.
#[derive(SimpleObject)]
pub struct ExecutorConnection {
    nodes: Vec<ExecutorNode>,
    total_count: i32,
}

/// Executor queries.
#[derive(Default)]
pub struct ExecutorsQuery;

#[Object]
impl ExecutorsQuery {
    /// Executors seen in the last day, most recently seen first (site admins only). `query`
    /// matches the hostname or a queue; `active` keeps the ones seen in the last 30 seconds.
    async fn executors(
        &self,
        ctx: &Context<'_>,
        query: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
    ) -> Result<ExecutorConnection> {
        require_site_admin(ctx)?;
        let since = Utc::now()
            - if active == Some(true) {
                Duration::seconds(ACTIVE_WITHIN_SECS)
            } else {
                Duration::days(1)
            };
        let mut rows = services(ctx)?
            .db
            .executors()
            .list(Some(since))
            .await
            .map_err(db_error)?;
        if let Some(q) = query
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty())
        {
            rows.retain(|e| {
                e.hostname.to_lowercase().contains(&q)
                    || e.queue_names.iter().any(|n| n.to_lowercase().contains(&q))
            });
        }
        let total = rows.len();
        rows.truncate(usize::try_from(page_size(first, 50, 500)).unwrap_or(50));
        Ok(ExecutorConnection {
            nodes: rows.into_iter().map(ExecutorNode).collect(),
            total_count: i32::try_from(total).unwrap_or(i32::MAX),
        })
    }

    /// Whether executors can connect: `executors.accessToken` is set (site admins only).
    async fn are_executors_configured(&self, ctx: &Context<'_>) -> Result<bool> {
        require_site_admin(ctx)?;
        Ok(services(ctx)?
            .config
            .load()
            .value
            .executors_access_token
            .as_deref()
            .is_some_and(|t| t.len() >= 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_levels() {
        assert_eq!(
            compatibility("0.3.0", "0.2.5"),
            Some(ExecutorCompatibility::UpToDate)
        );
        assert_eq!(
            compatibility("0.3.0", "0.1.0"),
            Some(ExecutorCompatibility::Outdated)
        );
        assert_eq!(
            compatibility("0.3.0", "1.0.0"),
            Some(ExecutorCompatibility::TooNew)
        );
        assert_eq!(compatibility("0.3.0", "dev"), None);
    }
}
