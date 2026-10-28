//! Streaming search events.
//!
//! The shapes and field names are a stable wire format that command-line
//! clients and editor extensions read; `tests/stream_schema.rs` checks them
//! against reference fixtures. Each event is one SSE message: `event: <name>`
//! and a JSON `data:`. Optional fields are omitted when empty.

use serde::Serialize;

/// A position in a file, 0-based; `column` counts bytes in the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Location {
    pub offset: u32,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Range {
    pub start: Location,
    pub end: Location,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkMatch {
    pub content: String,
    pub content_start: Location,
    pub ranges: Vec<Range>,
}

/// One result in a `matches` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Match {
    /// File contents; no chunks for a path-only result.
    #[serde(rename = "content", rename_all = "camelCase")]
    Content {
        path: String,
        #[serde(rename = "repositoryID")]
        repository_id: i32,
        repository: String,
        commit: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        branches: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        chunk_matches: Vec<ChunkMatch>,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    #[serde(rename = "path", rename_all = "camelCase")]
    Path {
        path: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        path_matches: Vec<Range>,
        #[serde(rename = "repositoryID")]
        repository_id: i32,
        repository: String,
        commit: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        branches: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Symbols defined in a file (`type:symbol`).
    #[serde(rename = "symbol", rename_all = "camelCase")]
    Symbol {
        path: String,
        #[serde(rename = "repositoryID")]
        repository_id: i32,
        repository: String,
        commit: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        branches: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        symbols: Vec<Symbol>,
    },
    /// A commit (`type:commit`). `content` is the message in a
    /// `COMMIT_EDITMSG` code fence; `ranges` are `[line, column, length]`
    /// within `content`.
    #[serde(rename = "commit", rename_all = "camelCase")]
    Commit {
        label: String,
        url: String,
        detail: String,
        #[serde(rename = "repositoryID")]
        repository_id: i32,
        repository: String,
        oid: String,
        message: String,
        author_name: String,
        /// RFC 3339.
        author_date: String,
        committer_name: String,
        committer_date: String,
        content: String,
        ranges: Vec<[u32; 3]>,
    },
    #[serde(rename = "repo", rename_all = "camelCase")]
    Repo {
        #[serde(rename = "repositoryID")]
        repository_id: i32,
        repository: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "is_false")]
        fork: bool,
        #[serde(skip_serializing_if = "is_false")]
        archived: bool,
        #[serde(skip_serializing_if = "is_false")]
        private: bool,
    },
    /// A person owning matched files (`select:file.owners`). `user` is set when the
    /// owner resolved to a user.
    #[serde(rename = "person", rename_all = "camelCase")]
    Person {
        handle: String,
        email: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<PersonUser>,
    },
    /// A team owning matched files (`select:file.owners`); `name` is empty for a team that
    /// is not in the database (a GitLab group).
    #[serde(rename = "team", rename_all = "camelCase")]
    Team {
        handle: String,
        email: String,
        name: String,
        display_name: String,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The user of a [`Match::Person`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonUser {
    pub username: String,
    pub display_name: String,
    #[serde(rename = "avatarURL")]
    pub avatar_url: String,
}

/// A symbol in a symbol match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    /// The definition: `/repo[@rev]/-/blob/path#L<line>:<col>-<line>:<col>`
    /// (1-based; columns count bytes).
    pub url: String,
    pub name: String,
    /// Empty when the symbol has no container.
    pub container_name: String,
    /// Upper-case LSP kind name, e.g. `FUNCTION`.
    pub kind: String,
    /// 1-based.
    pub line: u32,
    /// The name's range (byte columns); not part of the wire format.
    #[serde(skip)]
    pub range: Range,
}

/// Why something was skipped; the names are part of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkippedReason {
    DocumentMatchLimit,
    ShardMatchLimit,
    RepositoryLimit,
    ShardTimeout,
    RepositoryCloning,
    RepositoryMissing,
    BackendMissing,
    #[serde(rename = "repository-fork")]
    ExcludedFork,
    ExcludedArchive,
    #[serde(rename = "display")]
    DisplayLimit,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub reason: SkippedReason,
    pub title: String,
    pub message: String,
    pub severity: Severity,
    /// A query expression that would avoid the skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<SkippedSuggested>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSuggested {
    pub title: String,
    pub query_expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub done: bool,
    /// Set once the repositories are resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repositories_count: Option<u32>,
    pub match_count: u64,
    pub duration_ms: u64,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    pub value: String,
    pub label: String,
    pub count: u64,
    /// Whether `count` is exact.
    pub exhaustive: bool,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProposedQuery {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub proposed_queries: Vec<ProposedQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Matches(Vec<Match>),
    Progress(Progress),
    Filters(Vec<Filter>),
    Alert(Alert),
    Error(String),
    Done,
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Matches(_) => "matches",
            Event::Progress(_) => "progress",
            Event::Filters(_) => "filters",
            Event::Alert(_) => "alert",
            Event::Error(_) => "error",
            Event::Done => "done",
        }
    }

    /// The JSON `data` payload.
    pub fn data(&self) -> String {
        let json = match self {
            Event::Matches(m) => serde_json::to_string(m),
            Event::Progress(p) => serde_json::to_string(p),
            Event::Filters(f) => serde_json::to_string(f),
            Event::Alert(a) => serde_json::to_string(a),
            Event::Error(message) => {
                serde_json::to_string(&serde_json::json!({ "message": message }))
            }
            Event::Done => Ok("{}".to_owned()),
        };
        json.expect("stream events serialize")
    }

    /// Serializes content matches in the stream API's default `lineMatches`
    /// form. Clients requesting `cm=t` use `data` instead.
    pub fn data_with_line_matches(&self) -> String {
        let Event::Matches(matches) = self else {
            return self.data();
        };
        let mut rows = serde_json::to_value(matches).expect("matches serialize");
        for (row, matched) in rows
            .as_array_mut()
            .expect("matches are an array")
            .iter_mut()
            .zip(matches)
        {
            let Match::Content { chunk_matches, .. } = matched else {
                continue;
            };
            let object = row.as_object_mut().expect("matches are objects");
            object.remove("chunkMatches");
            object.insert(
                "lineMatches".to_owned(),
                serde_json::to_value(line_matches(chunk_matches)).expect("lines serialize"),
            );
        }
        serde_json::to_string(&rows).expect("matches serialize")
    }

    /// The full SSE message, terminated by a blank line.
    pub fn to_sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.name(), self.data())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LineMatch<'a> {
    line: &'a str,
    line_number: u32,
    offset_and_lengths: Vec<[u32; 2]>,
}

fn line_matches(chunks: &[ChunkMatch]) -> Vec<LineMatch<'_>> {
    let mut lines = Vec::new();
    for chunk in chunks {
        for (index, line) in chunk.content.split('\n').enumerate() {
            let line_number = chunk.content_start.line + index as u32;
            let mut offsets = Vec::new();
            for range in &chunk.ranges {
                if range.start.line > line_number || range.end.line < line_number {
                    continue;
                }
                let start = if range.start.line == line_number {
                    range.start.column
                } else {
                    0
                };
                let end = if range.end.line == line_number {
                    range.end.column
                } else {
                    line.len() as u32
                };
                if end > start {
                    offsets.push([start, end - start]);
                }
            }
            if !offsets.is_empty() {
                lines.push(LineMatch {
                    line,
                    line_number,
                    offset_and_lengths: offsets,
                });
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_match_wire_shape() {
        let loc = |offset, column| Location {
            offset,
            line: 0,
            column,
        };
        let ev = Event::Matches(vec![Match::Content {
            path: "a.go".into(),
            repository_id: 3,
            repository: "gitlab.com/x/y".into(),
            commit: "abc".into(),
            branches: Vec::new(),
            chunk_matches: vec![ChunkMatch {
                content: "foo".into(),
                content_start: loc(0, 0),
                ranges: vec![Range {
                    start: loc(0, 0),
                    end: loc(3, 3),
                }],
            }],
            language: Some("Go".into()),
        }]);
        assert_eq!(
            ev.to_sse(),
            "event: matches\ndata: [{\"type\":\"content\",\"path\":\"a.go\",\"repositoryID\":3,\
             \"repository\":\"gitlab.com/x/y\",\"commit\":\"abc\",\"chunkMatches\":[{\"content\":\"foo\",\
             \"contentStart\":{\"offset\":0,\"line\":0,\"column\":0},\"ranges\":[{\"start\":\
             {\"offset\":0,\"line\":0,\"column\":0},\"end\":{\"offset\":3,\"line\":0,\"column\":3}}]}],\
             \"language\":\"Go\"}]\n\n"
        );
        let default_data: serde_json::Value =
            serde_json::from_str(&ev.data_with_line_matches()).unwrap();
        assert_eq!(
            default_data[0]["lineMatches"],
            serde_json::json!([{
                "line": "foo",
                "lineNumber": 0,
                "offsetAndLengths": [[0, 3]]
            }])
        );
        assert!(default_data[0].get("chunkMatches").is_none());
    }

    #[test]
    fn line_matches_split_multiline_ranges() {
        let loc = |offset, line, column| Location {
            offset,
            line,
            column,
        };
        let chunks = vec![ChunkMatch {
            content: "ab\ncd".into(),
            content_start: loc(0, 0, 0),
            ranges: vec![Range {
                start: loc(1, 0, 1),
                end: loc(4, 1, 1),
            }],
        }];
        assert_eq!(
            serde_json::to_value(line_matches(&chunks)).unwrap(),
            serde_json::json!([
                {"line": "ab", "lineNumber": 0, "offsetAndLengths": [[1, 1]]},
                {"line": "cd", "lineNumber": 1, "offsetAndLengths": [[0, 1]]}
            ])
        );
    }

    #[test]
    fn progress_and_done() {
        let p = Event::Progress(Progress {
            done: true,
            repositories_count: Some(2),
            match_count: 5,
            duration_ms: 7,
            skipped: vec![Skipped {
                reason: SkippedReason::ExcludedFork,
                title: "1 forked".into(),
                message: "".into(),
                severity: Severity::Info,
                suggested: Some(SkippedSuggested {
                    title: "include forked".into(),
                    query_expression: "fork:yes".into(),
                }),
            }],
        });
        assert_eq!(
            p.data(),
            "{\"done\":true,\"repositoriesCount\":2,\"matchCount\":5,\"durationMs\":7,\"skipped\":\
             [{\"reason\":\"repository-fork\",\"title\":\"1 forked\",\"message\":\"\",\"severity\":\"info\",\
             \"suggested\":{\"title\":\"include forked\",\"queryExpression\":\"fork:yes\"}}]}"
        );
        assert_eq!(Event::Done.to_sse(), "event: done\ndata: {}\n\n");
    }
}
