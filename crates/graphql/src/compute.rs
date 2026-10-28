//! Compute over GraphQL: `compute(query)`.
//!
//! The schema: a list of
//! `ComputeResult`, the union of `ComputeMatchContext` (match-only queries:
//! matches with their capture groups) and `ComputeText` (output and
//! replace). The query runs like `/.api/compute/stream`
//! ([`sourcegull_services::compute::collect`]) with the viewer's repository
//! permissions, to its end.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use async_graphql::{Context, Object, Result, SimpleObject, Union};
use sourcegull_compute::result as cr;
use sourcegull_types::RepoId;

use crate::context::{codes, db_error, error, repo_access, services, viewer};
use crate::repos::{RepositoryNode, require_reader};
use sourcegull_codeintel_graphql::codenav::{Position, Range};

#[derive(Default)]
pub struct ComputeQuery;

#[Object]
impl ComputeQuery {
    /// Computes values from search results.
    async fn compute(
        &self,
        ctx: &Context<'_>,
        #[graphql(default)] query: String,
    ) -> Result<Vec<ComputeResult>> {
        require_reader(ctx)?;
        let state = services(ctx)?
            .app
            .as_ref()
            .ok_or_else(|| error(codes::INTERNAL, "Search is not available."))?;
        let out =
            sourcegull_services::compute::collect(&state.search_backend(), viewer(ctx), &query)
                .await
                .map_err(|e| error(codes::INVALID_INPUT, e.to_string()))?;
        if let Some(e) = out.error {
            return Err(error(codes::INTERNAL, e));
        }
        if out.progress.is_none()
            && let Some(alert) = out.alerts.first()
        {
            // The search rejected the query.
            return Err(error(
                codes::INVALID_INPUT,
                format!("{}: {}", alert.title, alert.description),
            ));
        }
        // Results come from repositories the viewer may read; load each once (re-checked).
        let access = repo_access(ctx).await?;
        let db = &services(ctx)?.db;
        let mut repos: HashMap<i32, Option<RepositoryNode>> = HashMap::new();
        let mut list = Vec::with_capacity(out.results.len());
        for result in out.results {
            let id = result.origin().repository_id;
            if let Entry::Vacant(e) = repos.entry(id) {
                let repo = db
                    .repos()
                    .get_authorized(RepoId(id), &access)
                    .await
                    .map_err(db_error)?;
                e.insert(repo.map(|repo| RepositoryNode {
                    repo,
                    gitserver: None,
                }));
            }
            let Some(Some(repository)) = repos.get(&id) else {
                continue;
            };
            let repository = repository.clone();
            list.push(match result {
                cr::ComputeResult::MatchContext(mc) => {
                    ComputeResult::MatchContext(ComputeMatchContext {
                        repository,
                        commit: mc.origin.commit,
                        path: mc.origin.path,
                        matches: mc.matches.into_iter().map(|m| Some(m.into())).collect(),
                    })
                }
                cr::ComputeResult::Text(t) => ComputeResult::Text(ComputeText {
                    repository: Some(repository),
                    commit: Some(t.origin.commit),
                    path: Some(t.origin.path),
                    kind: Some(t.kind),
                    value: t.value,
                }),
            });
        }
        Ok(list)
    }
}

/// A compute operation result.
#[derive(Union)]
pub enum ComputeResult {
    MatchContext(ComputeMatchContext),
    Text(ComputeText),
}

/// The result of matching data that satisfy a search pattern, including an environment of
/// submatches.
#[derive(SimpleObject)]
pub struct ComputeMatchContext {
    /// The repository.
    pub repository: RepositoryNode,
    /// The commit.
    pub commit: String,
    /// The file path.
    pub path: String,
    /// Computed match results.
    pub matches: Vec<Option<ComputeMatch>>,
}

/// A value matched within file content, and an environment of submatches within this value
/// corresponding to an input pattern (e.g. regular expression capture groups).
#[derive(SimpleObject)]
pub struct ComputeMatch {
    /// The string value.
    pub value: String,
    /// The range of this value within the file.
    pub range: Range,
    /// The environment of submatches within value.
    pub environment: Vec<Option<ComputeEnvironmentEntry>>,
}

/// An entry in a match environment: a variable with a value spanning a range. Named capture
/// groups use their names; unnamed ones are `1`, `2`, ….
#[derive(SimpleObject)]
pub struct ComputeEnvironmentEntry {
    /// The variable name.
    pub variable: String,
    /// The value associated with this variable.
    pub value: String,
    /// The absolute range spanned by this value in the input.
    pub range: Range,
}

/// A general computed result for arbitrary textual data. A result optionally specifies a
/// related repository, commit, file path, or the kind of textual data.
#[derive(SimpleObject)]
pub struct ComputeText {
    /// The repository.
    pub repository: Option<RepositoryNode>,
    /// The commit.
    pub commit: Option<String>,
    /// The file path.
    pub path: Option<String>,
    /// An arbitrary label communicating the kind of data the value represents.
    pub kind: Option<String>,
    /// The computed value.
    pub value: String,
}

fn range(r: cr::Range) -> Range {
    let p = |l: cr::Location| Position {
        line: i32::try_from(l.line).unwrap_or(i32::MAX),
        character: i32::try_from(l.column).unwrap_or(i32::MAX),
    };
    Range {
        start: p(r.start),
        end: p(r.end),
    }
}

impl From<cr::ComputeMatch> for ComputeMatch {
    fn from(m: cr::ComputeMatch) -> Self {
        ComputeMatch {
            value: m.value,
            range: range(m.range),
            environment: m
                .environment
                .0
                .into_iter()
                .map(|(variable, d)| {
                    Some(ComputeEnvironmentEntry {
                        variable,
                        value: d.value,
                        range: range(d.range),
                    })
                })
                .collect(),
        }
    }
}
