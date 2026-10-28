//! `select:` projections over results.
//!
//! `repo` turns every result into its repository, `file` and `file.path`
//! into its file (a path result), `file.directory` into its directory,
//! `content` keeps content matches, `symbol[.kind]` keeps symbols (of that
//! kind) and `commit` keeps commits. Projections are deduplicated.
//! `commit.diff.added`/`removed` are applied where diffs are rendered (only
//! added or removed lines that matched are kept); here they keep commit
//! results. `file.owners` (Code Ownership) keeps the owner results
//! (`person`, `team`) the repo nodes derive from file results, deduplicated
//! by owner.

use std::collections::HashSet;

use crate::RepoCatalog;
use crate::stream::Match;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectPath {
    Repo,
    File,
    FileDirectory,
    FileOwners,
    Content,
    /// Symbols, optionally of one kind (lowercase LSP kind name).
    Symbol(Option<String>),
    Commit,
    /// Diff lines added (`+`) or removed (`-`).
    CommitDiff(char),
}

impl SelectPath {
    /// Parses a `select:` value (validated by the query parser).
    pub fn parse(v: &str) -> Option<SelectPath> {
        let v = v.to_ascii_lowercase();
        Some(match v.as_str() {
            "repo" => SelectPath::Repo,
            "file" | "file.path" => SelectPath::File,
            "file.directory" => SelectPath::FileDirectory,
            "file.owners" => SelectPath::FileOwners,
            "content" => SelectPath::Content,
            "symbol" => SelectPath::Symbol(None),
            "commit" => SelectPath::Commit,
            "commit.diff.added" => SelectPath::CommitDiff('+'),
            "commit.diff.removed" => SelectPath::CommitDiff('-'),
            v => SelectPath::Symbol(Some(v.strip_prefix("symbol.")?.to_owned())),
        })
    }

    /// The result types searched when the query names none.
    pub fn default_types(&self, has_pattern: bool) -> Option<&'static [&'static str]> {
        Some(match self {
            SelectPath::Repo => return None,
            SelectPath::File | SelectPath::FileDirectory | SelectPath::FileOwners
                if has_pattern =>
            {
                &["file", "path"]
            }
            SelectPath::File | SelectPath::FileDirectory | SelectPath::FileOwners => &["path"],
            SelectPath::Content => &["file"],
            SelectPath::Symbol(_) => &["symbol"],
            SelectPath::Commit => &["commit"],
            SelectPath::CommitDiff(_) => &["diff"],
        })
    }
}

/// Applies a projection to a search's results, remembering what it has
/// emitted so each projected result comes once.
#[derive(Debug)]
pub struct Selector {
    path: SelectPath,
    catalog: std::sync::Arc<RepoCatalog>,
    seen: HashSet<String>,
}

impl Selector {
    pub fn new(path: SelectPath, catalog: std::sync::Arc<RepoCatalog>) -> Selector {
        Selector {
            path,
            catalog,
            seen: HashSet::new(),
        }
    }

    pub fn path(&self) -> &SelectPath {
        &self.path
    }

    /// The projection of `m` if it was not emitted before.
    pub fn apply_one(&mut self, m: Match) -> Option<Match> {
        let (key, m) = self.project(m)?;
        key.is_none_or(|k| self.seen.insert(k)).then_some(m)
    }

    /// The projections of `matches` not emitted before.
    pub fn apply(&mut self, matches: Vec<Match>) -> Vec<Match> {
        let mut out = Vec::with_capacity(matches.len());
        for m in matches {
            if let Some((key, m)) = self.project(m)
                && key.is_none_or(|k| self.seen.insert(k))
            {
                out.push(m);
            }
        }
        out
    }

    /// A match's projection and its deduplication key (`None`: not
    /// deduplicated).
    fn project(&self, m: Match) -> Option<(Option<String>, Match)> {
        if self.path == SelectPath::FileOwners {
            let key = owner_of(&m)?;
            return Some((Some(key), m));
        }
        let (repo_id, repo) = repo_of(&m)?;
        let repo = repo.to_owned();
        match &self.path {
            SelectPath::Repo => {
                let r = self.catalog.get(repo_id);
                Some((
                    Some(repo_id.to_string()),
                    Match::Repo {
                        repository_id: repo_id,
                        repository: repo,
                        description: r
                            .filter(|r| !r.description.is_empty())
                            .map(|r| r.description.clone()),
                        fork: r.is_some_and(|r| r.fork),
                        archived: r.is_some_and(|r| r.archived),
                        private: r.is_some_and(|r| r.private),
                    },
                ))
            }
            SelectPath::File | SelectPath::FileDirectory => {
                let (path, commit, branches, language) = file_of(m)?;
                let path = match self.path {
                    SelectPath::FileDirectory => path[..=path.rfind('/')?].to_owned(),
                    _ => path,
                };
                let key = format!("{repo_id}\0{commit}\0{path}");
                Some((
                    Some(key),
                    Match::Path {
                        path,
                        path_matches: Vec::new(),
                        repository_id: repo_id,
                        repository: repo,
                        commit,
                        branches,
                        language,
                    },
                ))
            }
            SelectPath::Content => matches!(m, Match::Content { .. }).then_some((None, m)),
            SelectPath::Symbol(kind) => match m {
                Match::Symbol {
                    path,
                    repository_id,
                    repository,
                    commit,
                    branches,
                    language,
                    mut symbols,
                } => {
                    if let Some(k) = kind {
                        symbols.retain(|s| s.kind.eq_ignore_ascii_case(k));
                    }
                    (!symbols.is_empty()).then_some((
                        None,
                        Match::Symbol {
                            path,
                            repository_id,
                            repository,
                            commit,
                            branches,
                            language,
                            symbols,
                        },
                    ))
                }
                _ => None,
            },
            SelectPath::Commit | SelectPath::CommitDiff(_) => {
                matches!(m, Match::Commit { .. }).then_some((None, m))
            }
            SelectPath::FileOwners => None,
        }
    }
}

/// The deduplication key of an owner result: one result per handle or email, compared
/// case-insensitively without a leading `@`.
fn owner_of(m: &Match) -> Option<String> {
    let text = match m {
        Match::Person { handle, email, .. } | Match::Team { handle, email, .. } => {
            if handle.is_empty() { email } else { handle }
        }
        _ => return None,
    };
    let t = text.trim();
    Some(format!(
        "owner\0{}",
        t.strip_prefix('@').unwrap_or(t).to_lowercase()
    ))
}

fn repo_of(m: &Match) -> Option<(i32, &str)> {
    Some(match m {
        Match::Content {
            repository_id,
            repository,
            ..
        }
        | Match::Path {
            repository_id,
            repository,
            ..
        }
        | Match::Symbol {
            repository_id,
            repository,
            ..
        }
        | Match::Commit {
            repository_id,
            repository,
            ..
        }
        | Match::Repo {
            repository_id,
            repository,
            ..
        } => (*repository_id, repository),
        Match::Person { .. } | Match::Team { .. } => return None,
    })
}

/// A file result's path, commit, branches and language.
type FileOf = (String, String, Vec<String>, Option<String>);

fn file_of(m: Match) -> Option<FileOf> {
    match m {
        Match::Content {
            path,
            commit,
            branches,
            language,
            ..
        }
        | Match::Path {
            path,
            commit,
            branches,
            language,
            ..
        }
        | Match::Symbol {
            path,
            commit,
            branches,
            language,
            ..
        } => Some((path, commit, branches, language)),
        Match::Commit { .. } | Match::Repo { .. } | Match::Person { .. } | Match::Team { .. } => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CatalogRepo;
    use crate::stream::Symbol;

    fn content(repo: i32, path: &str) -> Match {
        Match::Content {
            path: path.into(),
            repository_id: repo,
            repository: format!("r{repo}"),
            commit: "c".into(),
            branches: Vec::new(),
            chunk_matches: Vec::new(),
            language: None,
        }
    }

    fn catalog() -> std::sync::Arc<RepoCatalog> {
        std::sync::Arc::new(RepoCatalog::new(vec![CatalogRepo {
            id: 1,
            name: "r1".into(),
            description: "first".into(),
            fork: true,
            ..CatalogRepo::default()
        }]))
    }

    #[test]
    fn file_owners_keeps_owner_results_once() {
        let person = |handle: &str, email: &str| Match::Person {
            handle: handle.into(),
            email: email.into(),
            user: None,
        };
        let mut sel = Selector::new(SelectPath::FileOwners, catalog());
        let out = sel.apply(vec![
            person("alice", ""),
            content(1, "a.go"),
            person("Alice", ""),
            person("", "a@example.com"),
            person("", "A@example.com"),
        ]);
        assert_eq!(out, vec![person("alice", ""), person("", "a@example.com")]);
        assert_eq!(
            SelectPath::FileOwners.default_types(false),
            Some(&["path"][..])
        );
    }

    #[test]
    fn parses_paths() {
        assert_eq!(SelectPath::parse("Repo"), Some(SelectPath::Repo));
        assert_eq!(SelectPath::parse("file.path"), Some(SelectPath::File));
        assert_eq!(
            SelectPath::parse("symbol.function"),
            Some(SelectPath::Symbol(Some("function".into())))
        );
        assert_eq!(
            SelectPath::parse("commit.diff.removed"),
            Some(SelectPath::CommitDiff('-'))
        );
        assert_eq!(SelectPath::parse("nope"), None);
        assert_eq!(
            SelectPath::Symbol(None).default_types(true),
            Some(&["symbol"][..])
        );
        assert_eq!(SelectPath::Repo.default_types(true), None);
    }

    #[test]
    fn repos_and_files_are_deduplicated() {
        let mut s = Selector::new(SelectPath::Repo, catalog());
        let out = s.apply(vec![content(1, "a"), content(1, "b"), content(2, "c")]);
        assert_eq!(out.len(), 2);
        let Match::Repo {
            description, fork, ..
        } = &out[0]
        else {
            panic!()
        };
        assert_eq!(description.as_deref(), Some("first"));
        assert!(fork);
        assert!(s.apply(vec![content(2, "d")]).is_empty());

        let mut s = Selector::new(SelectPath::FileDirectory, catalog());
        let out = s.apply(vec![
            content(1, "src/a.rs"),
            content(1, "src/b.rs"),
            content(1, "top.rs"),
            content(1, "src/x/c.rs"),
        ]);
        let paths: Vec<_> = out
            .iter()
            .map(|m| match m {
                Match::Path { path, .. } => path.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(paths, ["src/", "src/x/"]);
    }

    #[test]
    fn symbols_by_kind() {
        let sym = |kind: &str| Symbol {
            url: String::new(),
            name: "x".into(),
            container_name: String::new(),
            kind: kind.into(),
            line: 1,
            range: crate::stream::Range {
                start: crate::stream::Location {
                    offset: 0,
                    line: 0,
                    column: 0,
                },
                end: crate::stream::Location {
                    offset: 0,
                    line: 0,
                    column: 1,
                },
            },
        };
        let m = Match::Symbol {
            path: "a.go".into(),
            repository_id: 1,
            repository: "r1".into(),
            commit: "c".into(),
            branches: Vec::new(),
            language: None,
            symbols: vec![sym("FUNCTION"), sym("STRUCT")],
        };
        let mut s = Selector::new(SelectPath::Symbol(Some("function".into())), catalog());
        let out = s.apply(vec![m.clone(), content(1, "a")]);
        let [Match::Symbol { symbols, .. }] = &out[..] else {
            panic!()
        };
        assert_eq!(symbols.len(), 1);
        let mut s = Selector::new(SelectPath::Symbol(Some("class".into())), catalog());
        assert!(s.apply(vec![m]).is_empty());
    }
}
