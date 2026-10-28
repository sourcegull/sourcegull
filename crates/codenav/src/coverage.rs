//! Coverage of precise usages: repositories that may use a global symbol but have no
//! precise data for it answer search-based.
//!
//! Precise usages of a global symbol only come from uploads, so a repository that uses the
//! symbol's package without a usable upload (never indexed, or its index failed) would be
//! missing. The candidates are the repositories the viewer can read that mention the
//! package's name (for Go, in a `go.mod`), other than the ones with precise usages, that
//! have no completed upload serving their default branch head. Search-based usages of the
//! symbol's name come from those repositories only; for Go, only from files that import the
//! symbol's package. The lookup has a time budget ([`BUDGET`]) and a repository cap
//! ([`MAX_REPOS`]); [`Coverage`] says which repositories answered how, including those whose
//! precise usages were linked through another version of the package.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use sourcegull_codeintel_scip::store::{
    CodeIntelStore, IndexJobStore, ListIndexJobs, ListUploads, UploadState,
};
use sourcegull_codeintel_scip::symbol::package;
use sourcegull_search::stream::Match;
use sourcegull_services::git::GitReader;
use sourcegull_types::{Repo, RepoId};

use crate::precise::{Hit, Raw};
use crate::{CodeNav, FileAt, NavError, RepoFilter, Usage, search_based};

/// How long finding and searching repositories without precise data may take.
pub const BUDGET: Duration = Duration::from_secs(3);
/// Repositories without precise data searched at most.
pub const MAX_REPOS: usize = 20;
/// Matches of the package's name looked at to find candidate repositories.
const MAX_PACKAGE_MATCHES: usize = 2000;
/// Files importing the package (Go) looked at.
const MAX_IMPORTING_FILES: usize = 5000;

/// Why a repository answered search-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverageReason {
    /// No upload serves its default branch.
    NoIndex,
    /// Its newest upload or index job failed.
    IndexFailed,
    /// The time budget ran out before it was searched; it has no usages listed.
    Budget,
}

impl CoverageReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoIndex => "no index",
            Self::IndexFailed => "index failed",
            Self::Budget => "budget hit",
        }
    }
}

/// Which repositories answered a precise usages request, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Repositories with precise usages, in result order.
    pub precise: Vec<String>,
    /// Repositories among `precise` with usages linked across package versions, with the
    /// version their upload has, in result order.
    pub cross_version: Vec<(String, String)>,
    /// Repositories without precise data that were searched (or left out by the budget),
    /// by name.
    pub search_based: Vec<(String, CoverageReason)>,
    /// More repositories mention the package than [`MAX_REPOS`]; the rest were not looked at.
    pub truncated: bool,
}

impl Coverage {
    /// One line, e.g. `precise: 5 repositories · other versions: lib (v1.1.0) ·
    /// search-based: push-service (no index)`.
    pub fn summary(&self) -> String {
        let n = self.precise.len();
        let mut out = format!(
            "precise: {n} {}",
            if n == 1 { "repository" } else { "repositories" }
        );
        if !self.cross_version.is_empty() {
            let parts: Vec<String> = self
                .cross_version
                .iter()
                .map(|(r, v)| format!("{r} ({v})"))
                .collect();
            out.push_str(" · other versions: ");
            out.push_str(&parts.join(", "));
        }
        out.push_str(" · search-based: ");
        if self.search_based.is_empty() {
            out.push_str("none");
        } else {
            let parts: Vec<String> = self
                .search_based
                .iter()
                .map(|(r, why)| format!("{r} ({})", why.label()))
                .collect();
            out.push_str(&parts.join(", "));
        }
        if self.truncated {
            out.push_str(&format!(
                " · more repositories mention the package (only {MAX_REPOS} checked)"
            ));
        }
        out
    }
}

/// Search-based usages added to a precise list, and the coverage of both.
#[derive(Debug, Clone, Default)]
pub struct Complement {
    pub usages: Vec<Usage>,
    pub coverage: Coverage,
}

/// What to look for: the package's name, and for Go the import path of the symbol's package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub name: String,
    pub go_import: Option<String>,
}

/// The descriptors of a SCIP symbol (what follows scheme, manager, name and version).
fn descriptors(symbol: &str) -> Option<&str> {
    let b = symbol.as_bytes();
    let mut i = 0;
    for _ in 0..4 {
        loop {
            match b.get(i)? {
                b' ' if b.get(i + 1) == Some(&b' ') => i += 2,
                b' ' => {
                    i += 1;
                    break;
                }
                _ => i += 1,
            }
        }
    }
    symbol.get(i..)
}

/// The package a global symbol belongs to; `None` for local symbols and those without a
/// package name.
pub(crate) fn target(symbol: &str) -> Option<Target> {
    let p = package(symbol)?;
    if p.name.is_empty() {
        return None;
    }
    let go_import = (p.manager == "gomod").then(|| {
        let d = descriptors(symbol).unwrap_or("");
        let path = match d.strip_prefix('`') {
            Some(rest) => rest.split('`').next().unwrap_or(""),
            None => d.split('/').next().unwrap_or(""),
        };
        if path == p.name || path.starts_with(&format!("{}/", p.name)) {
            path.to_owned()
        } else {
            p.name.clone()
        }
    });
    Some(Target {
        name: p.name,
        go_import,
    })
}

/// A query (pattern type `regexp`) for files mentioning the package: for Go, `go.mod` files
/// naming the module; else files in the language family naming it.
pub(crate) fn package_query(t: &Target, excluded: &[&str], lang: Option<&str>) -> String {
    let mut q: Vec<String> = excluded
        .iter()
        .map(|r| search_based::exclude_repo_filter(r))
        .collect();
    q.push(format!(
        "type:file case:yes count:{MAX_PACKAGE_MATCHES} select:repo"
    ));
    if t.go_import.is_some() {
        q.push(r"file:(^|/)go\.mod$".to_owned());
        q.push(format!(r"{}(\s|$)", regex::escape(&t.name)));
    } else {
        if let Some(f) = search_based::lang_filter(lang) {
            q.push(f);
        }
        q.push(regex::escape(&t.name));
    }
    q.join(" ")
}

/// `repo:^(a|b)$` for the repositories.
pub(crate) fn repos_scope(repos: &[&str]) -> String {
    let names: Vec<String> = repos.iter().map(|r| regex::escape(r)).collect();
    format!("repo:^(?:{})$", names.join("|"))
}

/// A query (pattern type `regexp`) for Go files importing `import`.
pub(crate) fn imports_query(scope: &str, import: &str) -> String {
    format!(
        r#"{scope} type:file case:yes count:{MAX_IMPORTING_FILES} file:\.go$ ["]{}["]"#,
        regex::escape(import)
    )
}

impl CodeNav<'_> {
    /// Search-based usages of the hit's symbol (`token`) in repositories that may use it but
    /// have no precise data, after the precise usages `raw`. Failures are logged and leave
    /// the additions out.
    pub(crate) async fn complement(
        &self,
        file: &FileAt,
        hit: &Hit,
        filter: &RepoFilter,
        raw: &[Raw],
        token: &str,
        lang: Option<&'static str>,
    ) -> Complement {
        let mut precise: Vec<String> = Vec::new();
        let mut cross_version: Vec<(String, String)> = Vec::new();
        for r in raw {
            if !precise.iter().any(|p| **p == *r.repo) {
                precise.push(r.repo.to_string());
            }
            if let Some(c) = &r.cross_version
                && !cross_version
                    .iter()
                    .any(|(repo, v)| **repo == *r.repo && *v == c.found)
            {
                cross_version.push((r.repo.to_string(), c.found.clone()));
            }
        }
        let mut out = Complement {
            usages: Vec::new(),
            coverage: Coverage {
                precise,
                cross_version,
                ..Coverage::default()
            },
        };
        let Some(t) = target(&hit.symbol) else {
            return out;
        };
        let deadline = tokio::time::Instant::now() + BUDGET;
        if let Err(e) = self
            .complement_in(file, filter, &t, token, lang, deadline, &mut out)
            .await
        {
            tracing::warn!(symbol = hit.symbol, error = %e, "complementing precise usages");
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    async fn complement_in(
        &self,
        file: &FileAt,
        filter: &RepoFilter,
        t: &Target,
        token: &str,
        lang: Option<&'static str>,
        deadline: tokio::time::Instant,
        out: &mut Complement,
    ) -> Result<(), NavError> {
        let mut excluded: Vec<&str> = out.coverage.precise.iter().map(String::as_str).collect();
        if !excluded.contains(&file.repo.name.as_str()) {
            excluded.push(&file.repo.name);
        }
        let q = package_query(t, &excluded, lang);
        let Ok(matches) =
            tokio::time::timeout_at(deadline, self.search(&q, MAX_PACKAGE_MATCHES)).await
        else {
            out.coverage.truncated = true;
            return Ok(());
        };
        let mut candidates: Vec<(i32, String)> = Vec::new();
        for m in matches? {
            let (id, name) = match m {
                Match::Repo {
                    repository_id,
                    repository,
                    ..
                }
                | Match::Content {
                    repository_id,
                    repository,
                    ..
                }
                | Match::Path {
                    repository_id,
                    repository,
                    ..
                } => (repository_id, repository),
                _ => continue,
            };
            if excluded.contains(&name.as_str())
                || !filter.allows(&name)
                || candidates.iter().any(|c| c.0 == id)
            {
                continue;
            }
            candidates.push((id, name));
        }
        candidates.sort_by(|a, b| a.1.cmp(&b.1));
        if candidates.len() > MAX_REPOS {
            candidates.truncate(MAX_REPOS);
            out.coverage.truncated = true;
        }
        if candidates.is_empty() {
            return Ok(());
        }

        // Which candidates have no completed upload serving their default branch head.
        let checks = futures::stream::iter(candidates.clone())
            .map(|(id, name)| async move {
                let r = self.uncovered(id, &name).await;
                (name, r)
            })
            .buffered(8)
            .take_until(tokio::time::sleep_until(deadline));
        let checked: Vec<(String, Result<Option<CoverageReason>, NavError>)> =
            checks.collect().await;
        let mut uncovered: Vec<(String, CoverageReason)> = Vec::new();
        for (name, r) in &checked {
            match r {
                Ok(Some(why)) => uncovered.push((name.clone(), *why)),
                Ok(None) => {}
                Err(e) => tracing::debug!(repo = name, error = %e, "checking precise data"),
            }
        }
        let budget_left: Vec<(String, CoverageReason)> = candidates
            .iter()
            .skip(checked.len())
            .map(|(_, n)| (n.clone(), CoverageReason::Budget))
            .collect();
        if uncovered.is_empty() {
            out.coverage.search_based = budget_left;
            return Ok(());
        }

        let names: Vec<&str> = uncovered.iter().map(|(n, _)| n.as_str()).collect();
        let scope = repos_scope(&names);
        let search = async {
            let usages = self.search_usages(file, &scope, token, lang);
            match &t.go_import {
                Some(import) => {
                    let q = imports_query(&scope, import);
                    let (usages, files) =
                        tokio::join!(usages, self.search(&q, MAX_IMPORTING_FILES));
                    let files: HashSet<(i32, String)> = files?
                        .into_iter()
                        .filter_map(|m| match m {
                            Match::Content {
                                repository_id,
                                path,
                                ..
                            } => Some((repository_id, path)),
                            _ => None,
                        })
                        .collect();
                    let mut usages = usages?;
                    usages
                        .retain(|u| files.contains(&(u.location.repo_id, u.location.path.clone())));
                    Ok::<_, NavError>(usages)
                }
                None => usages.await,
            }
        };
        match tokio::time::timeout_at(deadline, search).await {
            Ok(usages) => {
                out.usages = usages?;
                out.coverage.search_based = uncovered;
            }
            Err(_) => {
                out.coverage.search_based = uncovered
                    .into_iter()
                    .map(|(n, _)| (n, CoverageReason::Budget))
                    .collect();
            }
        }
        out.coverage.search_based.extend(budget_left);
        Ok(())
    }

    /// Why repository `id` has no precise data for its default branch, or `None` when a
    /// completed upload serves its head (or the viewer cannot read it).
    async fn uncovered(&self, id: i32, name: &str) -> Result<Option<CoverageReason>, NavError> {
        let access = self.access().await?;
        let Some(repo): Option<Repo> = self
            .state
            .db
            .repos()
            .get_authorized(RepoId(id), &access)
            .await?
        else {
            return Ok(None);
        };
        let head = GitReader::new(self.state.repo_clients.clone(), &repo)
            .resolve_revision("HEAD")
            .await?;
        if !self.nearest_uploads(&repo, &head).await?.is_empty() {
            return Ok(None);
        }
        let store = CodeIntelStore::new(&self.state.db);
        let (uploads, _) = store
            .list_uploads(
                &ListUploads {
                    repo: Some(repo.id),
                    limit: 1,
                    ..ListUploads::default()
                },
                &access,
            )
            .await?;
        let upload_failed = uploads
            .first()
            .is_some_and(|u| matches!(u.state, UploadState::Errored | UploadState::Failed));
        let (jobs, _) = IndexJobStore::new(&self.state.db)
            .list(
                &ListIndexJobs {
                    repo: Some(repo.id),
                    limit: 1,
                    ..ListIndexJobs::default()
                },
                &access,
            )
            .await?;
        let job_failed = jobs
            .first()
            .is_some_and(|j| matches!(j.state.as_str(), "errored" | "failed"));
        tracing::trace!(repo = name, upload_failed, job_failed, "no precise data");
        Ok(Some(if upload_failed || job_failed {
            CoverageReason::IndexFailed
        } else {
            CoverageReason::NoIndex
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_targets() {
        let t =
            target("scip-go gomod example.com/lib v1.1.2 `example.com/lib/batch`/Producer#Send().")
                .unwrap();
        assert_eq!(t.name, "example.com/lib");
        assert_eq!(t.go_import.as_deref(), Some("example.com/lib/batch"));
        let root = target("scip-go gomod example.com/lib v1.1.2 `example.com/lib`/New().").unwrap();
        assert_eq!(root.go_import.as_deref(), Some("example.com/lib"));
        // A package path outside the module falls back to the module.
        let other = target("scip-go gomod example.com/lib v1 `other.org/x`/F().").unwrap();
        assert_eq!(other.go_import.as_deref(), Some("example.com/lib"));
        assert_eq!(target("local 3"), None);
    }

    #[test]
    fn other_targets() {
        let t = target("scip-typescript npm @scope/pkg 1.0.0 src/`index.ts`/f().").unwrap();
        assert_eq!(t.name, "@scope/pkg");
        assert_eq!(t.go_import, None);
        assert_eq!(target("scip-python python . . foo/bar()."), None);
    }

    #[test]
    fn escaped_spaces() {
        assert_eq!(descriptors("a b  c d e f#"), Some("f#"));
        assert_eq!(descriptors("a b c"), None);
    }

    #[test]
    fn queries() {
        let go = Target {
            name: "example.com/lib".into(),
            go_import: Some("example.com/lib/batch".into()),
        };
        assert_eq!(
            package_query(&go, &["own", "a.b/c"], Some("go")),
            r"-repo:^own$ -repo:^a\.b/c$ type:file case:yes count:2000 select:repo file:(^|/)go\.mod$ example\.com/lib(\s|$)"
        );
        let npm = Target {
            name: "@scope/pkg".into(),
            go_import: None,
        };
        assert_eq!(
            package_query(&npm, &[], Some("typescript")),
            r"type:file case:yes count:2000 select:repo file:\.(ts|tsx|mts|cts|js|jsx|mjs|cjs)$ @scope/pkg"
        );
        assert_eq!(repos_scope(&["a/b", "c.d"]), r"repo:^(?:a/b|c\.d)$");
        assert_eq!(
            imports_query("repo:^x$", "example.com/lib"),
            r#"repo:^x$ type:file case:yes count:5000 file:\.go$ ["]example\.com/lib["]"#
        );
    }

    #[test]
    fn summaries() {
        let c = Coverage {
            precise: vec!["a".into(), "b".into()],
            cross_version: vec![],
            search_based: vec![
                ("push-service".into(), CoverageReason::NoIndex),
                ("x".into(), CoverageReason::IndexFailed),
            ],
            truncated: false,
        };
        assert_eq!(
            c.summary(),
            "precise: 2 repositories · search-based: push-service (no index), x (index failed)"
        );
        let one = Coverage {
            precise: vec!["a".into()],
            cross_version: vec![],
            search_based: vec![],
            truncated: true,
        };
        assert_eq!(
            one.summary(),
            "precise: 1 repository · search-based: none · more repositories mention the package (only 20 checked)"
        );
        let versions = Coverage {
            precise: vec!["svc".into(), "lib".into()],
            cross_version: vec![("lib".into(), "v1.1.0".into())],
            search_based: vec![],
            truncated: false,
        };
        assert_eq!(
            versions.summary(),
            "precise: 2 repositories · other versions: lib (v1.1.0) · search-based: none"
        );
    }
}
