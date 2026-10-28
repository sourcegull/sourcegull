//! Owner filters for search: `file:has.owner(handle)`,
//! `-file:has.owner()` and the owners `select:file.owners` reports.
//!
//! `web` resolves each `has.owner` argument to the set of owner keys ([`owner_key`]) that
//! name the same user or team (username, GitLab usernames, verified emails; a team's name)
//! and sends them with each repository's ingested CODEOWNERS file and assigned owners. The
//! repo node evaluates the filter on candidate paths ([`RepoOwners::passes`]) against the
//! CODEOWNERS file at the searched commit, so no path list ever leaves the node.

use std::collections::HashSet;
use std::sync::Arc;

use crate::codeowners::{CompiledCodeowners, owner_key};

/// One `file:has.owner(…)` term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerTerm {
    /// `-file:has.owner(…)`.
    pub negated: bool,
    /// Keys naming the owner; `None` (`has.owner()`) means "any owner".
    pub keys: Option<HashSet<String>>,
}

/// A manually assigned owner as the filter sees it: the path (`""` for the whole
/// repository) and the owner's text (`@username` or `@team-name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedEntry {
    pub path: String,
    pub owner: String,
}

/// Whether `path` is `dir` or below it; `""` covers everything.
pub fn path_covers(dir: &str, path: &str) -> bool {
    let dir = dir.trim_matches('/');
    dir.is_empty()
        || path == dir
        || (path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/')
}

/// What decides ownership in one repository at one commit.
#[derive(Debug, Clone, Default)]
pub struct RepoOwners {
    /// The CODEOWNERS file in force (ingested, or found in the commit).
    pub codeowners: Option<Arc<CompiledCodeowners>>,
    pub assigned: Vec<AssignedEntry>,
}

impl RepoOwners {
    /// The owners of `path` as written (`@alice`, `alice@example.com`, `@team`),
    /// deduplicated by key: CODEOWNERS owners first, then assigned owners.
    pub fn owner_texts(&self, path: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        if let Some(c) = &self.codeowners {
            for o in c.owners(path) {
                if seen.insert(o.key()) {
                    out.push(o.text());
                }
            }
        }
        for a in &self.assigned {
            if path_covers(&a.path, path) && seen.insert(owner_key(&a.owner)) {
                out.push(a.owner.clone());
            }
        }
        out
    }

    /// Whether `path` passes every term.
    pub fn passes(&self, terms: &[OwnerTerm], path: &str) -> bool {
        if terms.is_empty() {
            return true;
        }
        let keys: Vec<String> = self
            .owner_texts(path)
            .iter()
            .map(|t| owner_key(t))
            .collect();
        terms.iter().all(|t| {
            let has = match &t.keys {
                None => !keys.is_empty(),
                Some(want) => keys.iter().any(|k| want.contains(k)),
            };
            has != t.negated
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(negated: bool, keys: Option<&[&str]>) -> OwnerTerm {
        OwnerTerm {
            negated,
            keys: keys.map(|ks| ks.iter().map(|k| (*k).to_owned()).collect()),
        }
    }

    #[test]
    fn covers() {
        assert!(path_covers("", "a/b"));
        assert!(path_covers("a", "a/b"));
        assert!(path_covers("a/", "a/b"));
        assert!(path_covers("a/b", "a/b"));
        assert!(!path_covers("a", "ab/c"));
        assert!(!path_covers("a/b", "a"));
    }

    #[test]
    fn terms_combine_codeowners_and_assignments() {
        let owners = RepoOwners {
            codeowners: Some(Arc::new(CompiledCodeowners::compile(
                "*.go @Alice\n/docs/ docs@example.com\n",
            ))),
            assigned: vec![AssignedEntry {
                path: "src".into(),
                owner: "@bob".into(),
            }],
        };
        assert_eq!(owners.owner_texts("src/main.go"), vec!["@Alice", "@bob"]);
        assert_eq!(owners.owner_texts("README"), Vec::<String>::new());
        let alice = term(false, Some(&["alice"]));
        assert!(owners.passes(std::slice::from_ref(&alice), "x/y.go"));
        assert!(!owners.passes(std::slice::from_ref(&alice), "docs/a.md"));
        assert!(owners.passes(&[term(false, Some(&["docs@example.com"]))], "docs/a.md"));
        assert!(owners.passes(&[term(false, Some(&["bob"]))], "src/x.txt"));
        assert!(owners.passes(&[term(false, None)], "docs/a.md"));
        assert!(!owners.passes(&[term(false, None)], "README"));
        assert!(owners.passes(&[term(true, None)], "README"));
        assert!(owners.passes(&[alice.clone(), term(true, Some(&["bob"]))], "lib/a.go"));
        assert!(!owners.passes(&[alice, term(true, Some(&["bob"]))], "src/a.go"));
    }
}
