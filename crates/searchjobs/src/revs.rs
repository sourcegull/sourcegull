//! Revision specifiers of a repo job (`main:dev`, `HEAD`, `*refs/heads/*`, `*!refs/heads/old*`)
//! and their expansion into the revisions searched one by one.

pub use sourcegull_search::rev_globs::expand;

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
    spec.split(':')
        .any(sourcegull_search::rev_globs::is_rev_glob)
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
