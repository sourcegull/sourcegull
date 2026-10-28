//! Search contexts in queries.
//!
//! `context:` names a stored set of repositories: `global` (every
//! repository, the default), an instance-level context by name, or a user's
//! or org's context as `@owner/name`. Contexts come in two kinds, handled at
//! different steps:
//!
//! - A **query-defined** context holds a query of repository filters. Its
//!   query replaces the `context:` term before planning
//!   ([`substitute_context`]), so `repo:a or repo:b` in the context expands
//!   into parts like any other OR.
//! - A **listed** context holds repositories with revisions. It becomes a
//!   [`ContextScope`] that [`crate::RepoMatcher`] intersects with the
//!   query's own filters; the context's revisions win over the query's.

use std::collections::HashMap;
use std::sync::Arc;

use sourcegull_query::{Node, PatternType, parse};

/// The context searched when a query names none.
pub const GLOBAL_CONTEXT: &str = "global";

const MAX_NAME_LEN: usize = 32;
const MAX_DESCRIPTION_LEN: usize = 1024;
/// Repositories per context.
const MAX_REPOS: usize = 10_000;

/// A parsed `context:` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSpec<'a> {
    /// `global` (or empty): every repository.
    Global,
    /// An instance-level context's name.
    Instance(&'a str),
    /// `@owner/name`.
    Namespaced { owner: &'a str, name: &'a str },
    /// `@owner` alone: not a context; it resolves to "not found" (the only
    /// auto-defined context is `global`).
    Owner(&'a str),
}

impl<'a> ContextSpec<'a> {
    pub fn parse(spec: &'a str) -> ContextSpec<'a> {
        if spec.is_empty() || spec == GLOBAL_CONTEXT {
            return ContextSpec::Global;
        }
        match spec.strip_prefix('@') {
            Some(rest) => match rest.split_once('/') {
                Some((owner, name)) => ContextSpec::Namespaced { owner, name },
                None => ContextSpec::Owner(rest),
            },
            None => ContextSpec::Instance(spec),
        }
    }
}

/// The `context:` value of a query, if it has one. `context:` may appear
/// once (validation enforces it), so the first one is the one.
pub fn context_of(query: &str, default: PatternType) -> Option<String> {
    let q = parse(query, default).ok()?;
    q.parameters()
        .into_iter()
        .find(|p| p.field == "context")
        .map(|p| p.value.clone())
}

/// Replaces the `context:` term of `query` with `(context_query)`. Returns
/// the query unchanged when it has no `context:` term or does not parse
/// (planning reports the error).
pub fn substitute_context(query: &str, default: PatternType, context_query: &str) -> String {
    let Ok(q) = parse(query, default) else {
        return query.to_owned();
    };
    let Some(p) = q.parameters().into_iter().find(|p| p.field == "context") else {
        return query.to_owned();
    };
    let (start, end) = (p.span.start, p.span.end);
    format!("{}({context_query}){}", &query[..start], &query[end..])
}

/// Fields a query-defined context may use: repository filters, plus
/// `file:`, `lang:`, `case:` and `rev:`.
const CONTEXT_QUERY_FIELDS: &[&str] = &[
    "repo",
    "rev",
    "fork",
    "archived",
    "visibility",
    "case",
    "file",
    "lang",
];

/// `repo:` predicates a query-defined context may use: those answered from
/// repository metadata or a cheap lookup, never a content search.
const CONTEXT_QUERY_PREDICATES: &[&str] = &[
    "has",
    "has.tag",
    "has.key",
    "has.meta",
    "has.topic",
    "has.description",
];

/// Checks a query-defined context's query; returns every problem found.
pub fn validate_context_query(query: &str) -> Result<(), Vec<String>> {
    let parsed = match parse(query, PatternType::Regexp) {
        Ok(q) => q,
        Err(e) => return Err(vec![e.to_string()]),
    };
    if let Err(e) = sourcegull_query::validate(&parsed) {
        return Err(vec![e.to_string()]);
    }
    let mut errors = Vec::new();
    fn walk(node: &Node, errors: &mut Vec<String>) {
        match node {
            Node::Operator { operands, .. } => operands.iter().for_each(|n| walk(n, errors)),
            Node::Pattern(p) => {
                if !p.value.is_empty() {
                    errors.push(format!(
                        "unsupported pattern in search context query: {:?}",
                        p.value
                    ));
                }
            }
            Node::Parameter(p) => {
                if !CONTEXT_QUERY_FIELDS.contains(&p.field) {
                    errors.push(format!(
                        "unsupported field in search context query: {:?}",
                        p.field
                    ));
                    return;
                }
                if p.field == "repo" {
                    if let Some(name) = predicate_name(&p.value).filter(|_| p.predicate().is_some())
                    {
                        if !CONTEXT_QUERY_PREDICATES.contains(&name) {
                            errors.push(format!(
                                "unsupported repo field predicate in search context query: {:?}",
                                p.value
                            ));
                        }
                        return;
                    }
                }
                let revs = match p.field {
                    "repo" => p.value.split_once('@').map(|(_, revs)| revs),
                    "rev" => Some(p.value.as_str()),
                    _ => None,
                };
                if revs.is_some_and(|revs| revs.split(':').any(is_rev_glob)) {
                    errors.push(format!(
                        "unsupported rev glob in search context query: {:?}",
                        p.value
                    ));
                }
            }
        }
    }
    if let Some(root) = &parsed.root {
        walk(root, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The checks of a context's fields that need no database: name, description, query and the
/// number of repositories (shared by the API and the resources file).
pub fn check_context_fields(
    instance_level: bool,
    name: &str,
    description: &str,
    query: &str,
    repositories: usize,
) -> Vec<String> {
    let mut problems = Vec::new();
    if name.chars().count() > MAX_NAME_LEN {
        problems.push(format!(
            "The name {name:?} is longer than {MAX_NAME_LEN} characters."
        ));
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.'))
    {
        problems.push(format!(
            "{name:?} is not a valid search context name (letters, digits, _ - / .)."
        ));
    }
    if instance_level && name.eq_ignore_ascii_case(GLOBAL_CONTEXT) {
        problems.push("The name \"global\" is reserved.".to_owned());
    }
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        problems.push(format!(
            "The description is longer than {MAX_DESCRIPTION_LEN} characters."
        ));
    }
    let query = query.trim();
    if !query.is_empty() {
        if let Err(errs) = validate_context_query(query) {
            problems.extend(errs);
        }
        if repositories > 0 {
            problems.push("A context has either a query or repositories, not both.".to_owned());
        }
    }
    if repositories > MAX_REPOS {
        problems.push(format!("A context lists at most {MAX_REPOS} repositories."));
    }
    problems
}

fn predicate_name(value: &str) -> Option<&str> {
    value.split_once('(').map(|(name, _)| name)
}

/// `*refs/heads/*` and `*!refs/heads/old` select refs by glob.
fn is_rev_glob(rev: &str) -> bool {
    rev.starts_with('*')
}

/// The repositories a search may cover after `context:`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ContextScope {
    /// No listed context: every repository the filters match.
    #[default]
    Global,
    /// A listed context: only these repositories, each with the revisions
    /// the context names (empty: the query's, or the default branch).
    Listed {
        /// The `context:` value it was resolved from.
        spec: String,
        repos: Arc<HashMap<i32, Vec<String>>>,
    },
}

impl ContextScope {
    /// A listed context from its stored repositories and revisions (`""`
    /// is the default branch).
    pub fn listed(
        spec: impl Into<String>,
        repos: impl IntoIterator<Item = (i32, Vec<String>)>,
    ) -> ContextScope {
        let repos = repos
            .into_iter()
            .map(|(id, revs)| {
                let revs = if revs.iter().all(String::is_empty) {
                    Vec::new()
                } else {
                    revs.into_iter()
                        .map(|r| if r.is_empty() { "HEAD".to_owned() } else { r })
                        .collect()
                };
                (id, revs)
            })
            .collect();
        ContextScope::Listed {
            spec: spec.into(),
            repos: Arc::new(repos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(ContextSpec::parse(""), ContextSpec::Global);
        assert_eq!(ContextSpec::parse("global"), ContextSpec::Global);
        assert_eq!(ContextSpec::parse("team"), ContextSpec::Instance("team"));
        assert_eq!(
            ContextSpec::parse("@alice/backend/api"),
            ContextSpec::Namespaced {
                owner: "alice",
                name: "backend/api"
            }
        );
        assert_eq!(ContextSpec::parse("@alice"), ContextSpec::Owner("alice"));
    }

    #[test]
    fn finds_and_substitutes_the_context_term() {
        let q = "foo context:@acme/web lang:go";
        assert_eq!(
            context_of(q, PatternType::Standard).as_deref(),
            Some("@acme/web")
        );
        assert_eq!(context_of("foo", PatternType::Standard), None);
        assert_eq!(
            substitute_context(q, PatternType::Standard, "repo:a or repo:b"),
            "foo (repo:a or repo:b) lang:go"
        );
        assert_eq!(
            substitute_context("foo", PatternType::Standard, "repo:a"),
            "foo"
        );
    }

    #[test]
    fn substituted_or_expands_into_parts() {
        let q = substitute_context(
            "context:x foo",
            PatternType::Standard,
            "repo:^a$ or repo:^b$",
        );
        let parsed = parse(&q, PatternType::Standard).unwrap();
        let basics = crate::basic_queries(&parsed).unwrap();
        assert_eq!(basics.len(), 2);
    }

    #[test]
    fn context_queries_hold_repository_filters_only() {
        assert!(validate_context_query("").is_ok());
        assert!(validate_context_query("repo:^gitlab\\.com/acme/ fork:yes").is_ok());
        assert!(validate_context_query("(repo:a@main or repo:b) file:^src/ lang:go").is_ok());
        assert!(validate_context_query("repo:has.topic(rust) -repo:has.description(old)").is_ok());
        let errs = validate_context_query("repo:a foo type:diff").unwrap_err();
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].contains("pattern"), "{errs:?}");
        assert!(errs[1].contains("\"type\""), "{errs:?}");
        assert!(
            validate_context_query("repo:has.file(path:README)").unwrap_err()[0]
                .contains("predicate")
        );
        assert!(validate_context_query("repo:a@*refs/heads/*").unwrap_err()[0].contains("glob"));
    }

    #[test]
    fn listed_scope_maps_default_branch() {
        let ContextScope::Listed { repos, .. } = ContextScope::listed(
            "x",
            [
                (1, vec![String::new()]),
                (2, vec![String::new(), "v1".to_owned()]),
            ],
        ) else {
            unreachable!()
        };
        assert!(repos[&1].is_empty());
        assert_eq!(repos[&2], ["HEAD", "v1"]);
    }
}
