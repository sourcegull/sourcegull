//! Which files are searched by content even above the size limit
//! (`search.index.alwaysIndex`). Indexed and unindexed search use the same
//! rule, so both return the same matches.
//!
//! Patterns match the whole path from the repository root: `*` and `?` do
//! not cross `/`, `**` matches any number of directories, `{a,b}` and
//! `[abc]` work as in shells.

use std::fmt;
use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Files matched by `search.index.alwaysIndex` are read up to this size.
pub const ALWAYS_INDEX_MAX_FILE_SIZE: u64 = 16 << 20;

/// Why a pattern does not compile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid path pattern {pattern:?}: {reason}")]
pub struct AlwaysIndexError {
    pub pattern: String,
    pub reason: String,
}

/// Compiled `search.index.alwaysIndex` patterns of one repository. Cheap to
/// clone; equal when the patterns are.
#[derive(Clone, Default)]
pub struct AlwaysIndex {
    patterns: Vec<String>,
    set: Option<Arc<GlobSet>>,
}

impl AlwaysIndex {
    /// Compiles `patterns`; none gives a rule that matches nothing.
    pub fn new<S: AsRef<str>>(
        patterns: impl IntoIterator<Item = S>,
    ) -> Result<Self, AlwaysIndexError> {
        let mut builder = GlobSetBuilder::new();
        let mut sources = Vec::new();
        for p in patterns {
            let p = p.as_ref();
            let glob = GlobBuilder::new(p)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map_err(|e| AlwaysIndexError {
                    pattern: p.to_owned(),
                    reason: e.kind().to_string(),
                })?;
            builder.add(glob);
            sources.push(p.to_owned());
        }
        if sources.is_empty() {
            return Ok(Self::default());
        }
        let set = builder.build().map_err(|e| AlwaysIndexError {
            pattern: sources.join(", "),
            reason: e.to_string(),
        })?;
        Ok(Self {
            patterns: sources,
            set: Some(Arc::new(set)),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn matches(&self, path: &str) -> bool {
        self.set.as_ref().is_some_and(|s| s.is_match(path))
    }

    /// The largest file at `path` whose content is read, given the size
    /// limit for other files.
    pub fn max_file_size(&self, path: &str, max_file_size: u64) -> u64 {
        if self.matches(path) {
            max_file_size.max(ALWAYS_INDEX_MAX_FILE_SIZE)
        } else {
            max_file_size
        }
    }
}

impl PartialEq for AlwaysIndex {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for AlwaysIndex {}

impl fmt::Debug for AlwaysIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AlwaysIndex").field(&self.patterns).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_whole_paths() {
        let rule = AlwaysIndex::new(["gen/*.ts", "**/schema.graphql", "*.{pb,gen}.go"]).unwrap();
        assert!(rule.matches("gen/api.ts"));
        assert!(!rule.matches("gen/sub/api.ts"), "* stops at /");
        assert!(!rule.matches("src/gen/api.ts"));
        assert!(rule.matches("schema.graphql"));
        assert!(rule.matches("a/b/schema.graphql"));
        assert!(rule.matches("x.pb.go"));
        assert!(!rule.matches("a/x.pb.go"));
        assert_eq!(
            rule.max_file_size("x.gen.go", 10),
            ALWAYS_INDEX_MAX_FILE_SIZE
        );
        assert_eq!(rule.max_file_size("x.go", 10), 10);
    }

    #[test]
    fn empty_and_invalid() {
        let none = AlwaysIndex::new(Vec::<String>::new()).unwrap();
        assert!(none.is_empty() && !none.matches("a"));
        assert_eq!(none, AlwaysIndex::default());
        let err = AlwaysIndex::new(["ok/*", "bad/[a"]).unwrap_err();
        assert_eq!(err.pattern, "bad/[a");
    }
}
