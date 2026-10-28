//! Revision specifiers of a repo job (`main:dev`, `HEAD`, `*refs/heads/*`, `*!refs/heads/old*`)
//! and their expansion into the revisions searched one by one.

/// The default branch.
pub const DEFAULT_REVISION: &str = "HEAD";

/// The stored specifier list for a repository's revisions (empty: the default branch).
pub fn ref_spec(revs: &[String]) -> String {
    let revs: Vec<&str> = revs
        .iter()
        .map(String::as_str)
        .filter(|r| !r.is_empty())
        .collect();
    if revs.is_empty() {
        DEFAULT_REVISION.to_owned()
    } else {
        revs.join(":")
    }
}

/// Whether the specifiers include ref globs, which need the repository's refs.
pub fn has_globs(spec: &str) -> bool {
    spec.split(':').any(|s| s.starts_with('*'))
}

/// The revisions of `spec`, in order and without duplicates: plain revisions as they are,
/// then the refs (full names, from `refs`) matching an include glob (`*refs/heads/*`) and no
/// exclude glob (`*!refs/heads/old*`). Exclude globs alone apply to all branches.
pub fn expand(spec: &str, refs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for s in spec.split(':').filter(|s| !s.is_empty()) {
        match s.strip_prefix('*') {
            Some(glob) => match glob.strip_prefix('!') {
                Some(glob) => exclude.push(glob_regex(glob)),
                None => include.push(glob_regex(glob)),
            },
            None => {
                if !out.iter().any(|r| r == s) {
                    out.push(s.to_owned());
                }
            }
        }
    }
    if include.is_empty() && !exclude.is_empty() {
        include.push(glob_regex("refs/heads/*"));
    }
    for r in refs {
        if include.iter().any(|re| re.is_match(r))
            && !exclude.iter().any(|re| re.is_match(r))
            && !out.contains(r)
        {
            out.push(r.clone());
        }
    }
    out
}

/// A ref glob as an anchored regex: `*` matches any run of characters.
fn glob_regex(glob: &str) -> regex::Regex {
    let pattern = glob
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    regex::Regex::new(&format!("^{pattern}$")).expect("escaped glob is a valid regex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(ref_spec(&[]), "HEAD");
        assert_eq!(ref_spec(&["".into()]), "HEAD");
        assert_eq!(ref_spec(&["main".into(), "v1.0".into()]), "main:v1.0");
        assert!(has_globs("main:*refs/heads/*"));
        assert!(!has_globs("main:dev"));
    }

    #[test]
    fn expands_globs() {
        let refs: Vec<String> = [
            "refs/heads/main",
            "refs/heads/old-1",
            "refs/heads/feature/x",
            "refs/tags/v1",
        ]
        .map(String::from)
        .into();
        assert_eq!(expand("HEAD", &refs), ["HEAD"]);
        assert_eq!(expand("main:dev:main", &refs), ["main", "dev"]);
        assert_eq!(
            expand("*refs/heads/*", &refs),
            [
                "refs/heads/main",
                "refs/heads/old-1",
                "refs/heads/feature/x"
            ]
        );
        assert_eq!(
            expand("*refs/heads/*:*!refs/heads/old*", &refs),
            ["refs/heads/main", "refs/heads/feature/x"]
        );
        assert_eq!(
            expand("*!refs/heads/feature/*", &refs),
            ["refs/heads/main", "refs/heads/old-1"]
        );
        assert_eq!(expand("v1:*refs/tags/*", &refs), ["v1", "refs/tags/v1"]);
        assert!(expand("*refs/remotes/*", &refs).is_empty());
    }
}
