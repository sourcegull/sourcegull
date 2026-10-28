//! Dynamic filters: facets computed as results stream in.
//!
//! One suggestion per repository, language and file extension, with the
//! query fragment that narrows the search to it.

use std::collections::HashMap;

use crate::stream::{Filter, Match};

/// Suggestions per kind in a `filters` event.
pub const MAX_FILTERS_PER_KIND: usize = 20;

#[derive(Debug, Default)]
pub struct Facets {
    /// (kind, value) → (label, count)
    counts: HashMap<(&'static str, String), (String, u64)>,
}

impl Facets {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&mut self, kind: &'static str, value: String, label: String, n: u64) {
        self.counts.entry((kind, value)).or_insert((label, 0)).1 += n;
    }

    /// Counts one result.
    pub fn observe(&mut self, m: &Match) {
        let (repo, path, language, n) = match m {
            Match::Content {
                repository,
                path,
                language,
                chunk_matches,
                ..
            } => {
                let n = chunk_matches.iter().map(|c| c.ranges.len()).sum::<usize>();
                (repository, Some(path), language.as_deref(), n.max(1))
            }
            Match::Path {
                repository,
                path,
                language,
                ..
            } => (repository, Some(path), language.as_deref(), 1),
            Match::Symbol {
                repository,
                path,
                language,
                symbols,
                ..
            } => (
                repository,
                Some(path),
                language.as_deref(),
                symbols.len().max(1),
            ),
            Match::Repo { repository, .. } | Match::Commit { repository, .. } => {
                (repository, None, None, 1)
            }
            // Owners have no repository, path or language.
            Match::Person { .. } | Match::Team { .. } => return,
        };
        let n = n as u64;
        self.add(
            "repo",
            format!("repo:^{}$", regex::escape(repo)),
            repo.clone(),
            n,
        );
        if let Some(lang) = language {
            let value = if lang.contains(' ') {
                format!("lang:\"{}\"", lang.to_lowercase())
            } else {
                format!("lang:{}", lang.to_lowercase())
            };
            self.add("lang", value, lang.to_owned(), n);
        }
        if let Some(ext) = path.and_then(|p| extension(p)) {
            self.add(
                "file",
                format!("file:\\.{}$", regex::escape(ext)),
                format!(".{ext}"),
                n,
            );
        }
    }

    /// The top suggestions per kind, by count then label.
    pub fn filters(&self) -> Vec<Filter> {
        let mut all: Vec<Filter> = self
            .counts
            .iter()
            .map(|((kind, value), (label, count))| Filter {
                value: value.clone(),
                label: label.clone(),
                count: *count,
                exhaustive: true,
                kind: (*kind).to_owned(),
            })
            .collect();
        all.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then(b.count.cmp(&a.count))
                .then(a.label.cmp(&b.label))
        });
        let mut per_kind: HashMap<String, usize> = HashMap::new();
        all.retain(|f| {
            let n = per_kind.entry(f.kind.clone()).or_default();
            *n += 1;
            *n <= MAX_FILTERS_PER_KIND
        });
        all
    }
}

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then_some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_match(repo: &str, path: &str, lang: Option<&str>) -> Match {
        Match::Path {
            path: path.into(),
            path_matches: Vec::new(),
            repository_id: 1,
            repository: repo.into(),
            commit: "c".into(),
            branches: Vec::new(),
            language: lang.map(Into::into),
        }
    }

    #[test]
    fn counts_repos_langs_and_extensions() {
        let mut f = Facets::new();
        f.observe(&path_match("gitlab.com/a/b", "x.go", Some("Go")));
        f.observe(&path_match("gitlab.com/a/b", "y.go", Some("Go")));
        f.observe(&path_match("gitlab.com/c", ".gitignore", None));
        let filters = f.filters();
        let get = |kind: &str| -> Vec<(String, u64)> {
            filters
                .iter()
                .filter(|x| x.kind == kind)
                .map(|x| (x.value.clone(), x.count))
                .collect()
        };
        assert_eq!(get("file"), [("file:\\.go$".to_owned(), 2)]);
        assert_eq!(get("lang"), [("lang:go".to_owned(), 2)]);
        assert_eq!(
            get("repo"),
            [
                ("repo:^gitlab\\.com/a/b$".to_owned(), 2),
                ("repo:^gitlab\\.com/c$".to_owned(), 1)
            ]
        );
    }
}
