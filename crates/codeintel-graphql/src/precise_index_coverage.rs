//! Precise index coverage for site admins: `preciseIndexCoverage` counts repositories by
//! whether a current SCIP index serves their default branch, and pages through them with
//! their latest indexed commit and failure.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_codeintel_policies::store::{
    CoverageCounts, CoverageOptions, CoverageState, CoverageStore, ListCoverage, RepoCoverage,
};
use sourcegull_services::relay::marshal_id;

use crate::context::{codes, db_error, error, require_site_admin, services, to_int};
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::ids::REPOSITORY_KIND;

/// The largest page of `repositories`.
const MAX_PAGE: i64 = 500;

/// A repository's precise index coverage.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum PreciseIndexCoverageState {
    /// A completed index is at the default branch tip, or the newest one is within the stale
    /// window.
    UpToDate,
    /// Completed indexes exist, none at the default branch tip, the newest older than the
    /// stale window.
    Stale,
    /// The latest index job or upload failed after the newest completed index.
    Failing,
    /// No completed index, though an indexing policy applies.
    None,
    /// No completed index and no indexing policy applies.
    NotTargeted,
}

impl From<CoverageState> for PreciseIndexCoverageState {
    fn from(s: CoverageState) -> Self {
        match s {
            CoverageState::UpToDate => Self::UpToDate,
            CoverageState::Stale => Self::Stale,
            CoverageState::Failing => Self::Failing,
            CoverageState::None => Self::None,
            CoverageState::NotTargeted => Self::NotTargeted,
        }
    }
}

impl From<PreciseIndexCoverageState> for CoverageState {
    fn from(s: PreciseIndexCoverageState) -> Self {
        match s {
            PreciseIndexCoverageState::UpToDate => Self::UpToDate,
            PreciseIndexCoverageState::Stale => Self::Stale,
            PreciseIndexCoverageState::Failing => Self::Failing,
            PreciseIndexCoverageState::None => Self::None,
            PreciseIndexCoverageState::NotTargeted => Self::NotTargeted,
        }
    }
}

/// Repositories per coverage state.
#[derive(SimpleObject)]
pub struct PreciseIndexCoverageCounts {
    up_to_date: i32,
    stale: i32,
    failing: i32,
    none: i32,
    not_targeted: i32,
    /// All repositories (not deleted).
    total: i32,
}

impl From<CoverageCounts> for PreciseIndexCoverageCounts {
    fn from(c: CoverageCounts) -> Self {
        Self {
            up_to_date: to_int(c.up_to_date),
            stale: to_int(c.stale),
            failing: to_int(c.failing),
            none: to_int(c.none),
            not_targeted: to_int(c.not_targeted),
            total: to_int(c.up_to_date + c.stale + c.failing + c.none + c.not_targeted),
        }
    }
}

/// One repository's coverage.
pub struct PreciseIndexCoverageRepository(RepoCoverage);

#[Object]
impl PreciseIndexCoverageRepository {
    /// The repository's node ID.
    #[graphql(name = "repositoryID")]
    async fn repository_id(&self) -> ID {
        ID(marshal_id(REPOSITORY_KIND, i64::from(self.0.repo_id.get())))
    }

    async fn repository_name(&self) -> &str {
        &self.0.repo_name
    }

    /// The repository's Code graph page.
    #[graphql(name = "codeGraphURL")]
    async fn code_graph_url(&self) -> String {
        format!("/{}/-/code-graph", self.0.repo_name)
    }

    async fn state(&self) -> PreciseIndexCoverageState {
        self.0.state.into()
    }

    /// The default branch tip the repo node last fetched.
    async fn head_commit(&self) -> Option<&str> {
        self.0.head_commit.as_deref()
    }

    /// The commit of the newest completed index.
    async fn indexed_commit(&self) -> Option<&str> {
        self.0.indexed_commit.as_deref()
    }

    /// When the newest completed index finished processing.
    async fn indexed_at(&self) -> Option<DateTime<Utc>> {
        self.0.indexed_at
    }

    /// Whether a completed index is at the default branch tip.
    async fn indexed_at_head(&self) -> bool {
        self.0.indexed_at_head
    }

    /// The latest failure's message, when it is newer than the newest completed index.
    async fn failure_message(&self) -> Option<&str> {
        self.0.failure_message.as_deref()
    }

    async fn failed_at(&self) -> Option<DateTime<Utc>> {
        self.0.failed_at
    }

    /// Where the failure happened: `index_job` or `upload`.
    async fn failure_source(&self) -> Option<&str> {
        self.0.failure_source.as_deref()
    }
}

/// A page of repositories.
#[derive(SimpleObject)]
pub struct PreciseIndexCoverageConnection {
    nodes: Vec<PreciseIndexCoverageRepository>,
    total_count: i32,
    page_info: PageInfo,
}

/// The coverage report.
pub struct PreciseIndexCoverage {
    opts: CoverageOptions,
    stale_after_days: i32,
    auto_indexing_enabled: bool,
}

#[Object]
impl PreciseIndexCoverage {
    /// Days after which a completed index not at the default branch tip counts as stale.
    async fn stale_after_days(&self) -> i32 {
        self.stale_after_days
    }

    /// Whether auto-indexing is on (`codeIntelAutoIndexing.enabled`). While it is off no
    /// repository is targeted by indexing policies.
    async fn auto_indexing_enabled(&self) -> bool {
        self.auto_indexing_enabled
    }

    async fn counts(&self, ctx: &Context<'_>) -> Result<PreciseIndexCoverageCounts> {
        let s = services(ctx)?;
        let c = CoverageStore::new(&s.db)
            .counts(&self.opts)
            .await
            .map_err(db_error)?;
        Ok(c.into())
    }

    /// Repositories ordered by name, optionally in one state or with `query` in their name.
    async fn repositories(
        &self,
        ctx: &Context<'_>,
        state: Option<PreciseIndexCoverageState>,
        query: Option<String>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<PreciseIndexCoverageConnection> {
        let s = services(ctx)?;
        let offset = decode_cursor(after.as_deref())?;
        let limit = i64::from(first).clamp(0, MAX_PAGE);
        let (items, total) = CoverageStore::new(&s.db)
            .list(
                &self.opts,
                &ListCoverage {
                    state: state.map(Into::into),
                    query,
                    offset,
                    limit: limit + 1,
                },
            )
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, items.len());
        Ok(PreciseIndexCoverageConnection {
            nodes: items
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(PreciseIndexCoverageRepository)
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }
}

#[derive(Default)]
pub struct PreciseIndexCoverageQuery;

#[Object]
impl PreciseIndexCoverageQuery {
    /// Which repositories have a current precise index, which are failing and which have none
    /// (site admins only).
    async fn precise_index_coverage(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 7)] stale_after_days: i32,
    ) -> Result<PreciseIndexCoverage> {
        require_site_admin(ctx)?;
        if stale_after_days < 1 {
            return Err(error(
                codes::INVALID_INPUT,
                "staleAfterDays must be at least 1.",
            ));
        }
        let s = services(ctx)?;
        let (enabled, allow_global) = {
            let site = &s.config.load().value;
            (
                site.code_intel_auto_indexing_enabled,
                site.code_intel_auto_indexing_allow_global_policies,
            )
        };
        let targets = CoverageStore::new(&s.db)
            .targets(enabled, allow_global)
            .await
            .map_err(db_error)?;
        Ok(PreciseIndexCoverage {
            opts: CoverageOptions {
                targets,
                stale_after: TimeDelta::days(i64::from(stale_after_days)),
            },
            stale_after_days,
            auto_indexing_enabled: enabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for s in CoverageState::ALL {
            assert_eq!(CoverageState::from(PreciseIndexCoverageState::from(s)), s);
        }
    }
}
