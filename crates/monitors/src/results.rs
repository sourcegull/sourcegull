//! What a trigger job found: commit and diff matches in a stored form that actions render.

use serde::{Deserialize, Serialize};
use sourcegull_search::stream::Match;

/// One commit a watch's query matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorResult {
    /// `commit` or `diff`.
    pub kind: String,
    pub repository: String,
    #[serde(rename = "repositoryID")]
    pub repository_id: i32,
    pub commit: String,
    /// Relative URL of the commit page.
    pub url: String,
    pub subject: String,
    pub message: String,
    pub author_name: String,
    /// RFC 3339.
    pub author_date: String,
    /// Diff results: the matching hunks (`old new` path lines, `@@` headers, lines).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub diff: String,
}

impl MonitorResult {
    /// Converts a commit match of the stream API; other matches are not watch results.
    pub fn from_match(m: Match) -> Option<MonitorResult> {
        let Match::Commit {
            url,
            repository_id,
            repository,
            oid,
            message,
            author_name,
            author_date,
            content,
            ..
        } = m
        else {
            return None;
        };
        let diff = content
            .strip_prefix("```diff\n")
            .and_then(|c| c.strip_suffix("\n```").or(c.strip_suffix("```")))
            .map(str::to_owned);
        Some(MonitorResult {
            kind: if diff.is_some() { "diff" } else { "commit" }.to_owned(),
            subject: message.lines().next().unwrap_or_default().to_owned(),
            repository,
            repository_id,
            commit: oid,
            url,
            message,
            author_name,
            author_date,
            diff: diff.unwrap_or_default(),
        })
    }

    /// The first 7 hex digits of the commit.
    pub fn short_commit(&self) -> &str {
        self.commit.get(..7).unwrap_or(&self.commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(content: &str) -> Match {
        Match::Commit {
            label: String::new(),
            url: "/r/-/commit/abcdef0123".into(),
            detail: String::new(),
            repository_id: 3,
            repository: "r".into(),
            oid: "abcdef0123".into(),
            message: "fix: things\n\nbody\n".into(),
            author_name: "alice".into(),
            author_date: "2026-01-01T00:00:00Z".into(),
            committer_name: "alice".into(),
            committer_date: "2026-01-01T00:00:00Z".into(),
            content: content.into(),
            ranges: Vec::new(),
        }
    }

    #[test]
    fn converts_commit_and_diff_matches() {
        let r = MonitorResult::from_match(commit("```COMMIT_EDITMSG\nfix: things\n```")).unwrap();
        assert_eq!(r.kind, "commit");
        assert_eq!(r.subject, "fix: things");
        assert_eq!(r.short_commit(), "abcdef0");
        assert!(r.diff.is_empty());
        let r = MonitorResult::from_match(commit("```diff\na.rs a.rs\n@@ -1,0 +1,1 @@\n+x\n```"))
            .unwrap();
        assert_eq!(r.kind, "diff");
        assert_eq!(r.diff, "a.rs a.rs\n@@ -1,0 +1,1 @@\n+x");
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["repositoryID"], 3);
        assert_eq!(serde_json::from_value::<MonitorResult>(json).unwrap(), r);
        let repo = Match::Repo {
            repository_id: 1,
            repository: "r".into(),
            description: None,
            fork: false,
            archived: false,
            private: false,
        };
        assert!(MonitorResult::from_match(repo).is_none());
    }
}
