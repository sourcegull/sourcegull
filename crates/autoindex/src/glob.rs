//! Path globs of recognizer patterns, with CODEOWNERS-like semantics: a pattern without a leading
//! `/` matches at any depth (as if it started with `**/`), a trailing `/` matches everything below
//! a directory, `**` matches any number of directories, and `*` / `?` match within one path
//! segment.

/// One part of a compiled glob.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// `**`: zero or more segments.
    AnyPath,
    /// A segment, possibly with `*` and `?` wildcards.
    Segment(String),
}

/// A compiled path glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGlob {
    parts: Vec<Part>,
}

impl PathGlob {
    /// Compiles `pattern`; `None` when it has an empty segment (`a//b`) or is empty.
    pub fn new(pattern: &str) -> Option<Self> {
        let trimmed = pattern.trim_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !pattern.starts_with('/') {
            parts.push(Part::AnyPath);
        }
        for seg in trimmed.split('/') {
            match seg {
                "" => return None,
                "**" => {
                    if parts.last() != Some(&Part::AnyPath) {
                        parts.push(Part::AnyPath);
                    }
                }
                s => parts.push(Part::Segment(s.to_owned())),
            }
        }
        if pattern.ends_with('/') && parts.last() != Some(&Part::AnyPath) {
            parts.push(Part::AnyPath);
        }
        // A trailing `**` must match at least one segment: `dir/` does not match `dir`.
        if parts.last() == Some(&Part::AnyPath) {
            parts.push(Part::Segment("*".into()));
        }
        Some(Self { parts })
    }

    /// Whether the repository-relative `path` (with or without a leading `/`) matches.
    pub fn is_match(&self, path: &str) -> bool {
        let segs: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        // states[i]: the first i parts match the segments consumed so far.
        let n = self.parts.len();
        let mut states = vec![false; n + 1];
        states[0] = true;
        close(&self.parts, &mut states);
        for seg in segs {
            let mut next = vec![false; n + 1];
            for i in 0..n {
                if !states[i] {
                    continue;
                }
                match &self.parts[i] {
                    Part::AnyPath => next[i] = true,
                    Part::Segment(p) => {
                        if segment_match(p.as_bytes(), seg.as_bytes()) {
                            next[i + 1] = true;
                        }
                    }
                }
            }
            close(&self.parts, &mut next);
            if !next.iter().any(|&b| b) {
                return false;
            }
            states = next;
        }
        states[n]
    }
}

/// `**` matches nothing, too: a state before it also stands after it.
fn close(parts: &[Part], states: &mut [bool]) {
    for i in 0..parts.len() {
        if states[i] && parts[i] == Part::AnyPath {
            states[i + 1] = true;
        }
    }
}

/// Wildcard match of one segment (`*` any run, `?` one character).
fn segment_match(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        PathGlob::new(pattern).unwrap().is_match(path)
    }

    #[test]
    fn basenames_match_at_any_depth() {
        assert!(m("go.mod", "go.mod"));
        assert!(m("go.mod", "/a/b/go.mod"));
        assert!(!m("go.mod", "a/go.mod.bak"));
        assert!(m("*.go", "main.go"));
        assert!(m("*.go", "cmd/x/main.go"));
        assert!(!m("*.go", "main.gox"));
    }

    #[test]
    fn rooted_and_directory_patterns() {
        assert!(m("/BUILD.bazel", "BUILD.bazel"));
        assert!(!m("/BUILD.bazel", "a/BUILD.bazel"));
        assert!(m("vendor/", "vendor/a/go.mod"));
        assert!(m("vendor/", "x/vendor/go.mod"));
        assert!(!m("vendor/", "vendor"));
        assert!(!m("vendor/", "vendors/go.mod"));
        assert!(m("/a/**/*.java", "a/b/c/X.java"));
        assert!(m("/a/**/*.java", "a/X.java"));
        assert!(!m("/a/**/*.java", "b/a/X.java"));
        assert!(m("**/*.kt", "src/A.kt"));
        assert!(m("a?c", "x/abc"));
        assert!(PathGlob::new("a//b").is_none());
        assert!(PathGlob::new("/").is_none());
    }
}
