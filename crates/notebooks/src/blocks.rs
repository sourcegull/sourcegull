//! Notebook blocks: the four kinds, their stored shape, validation, and the
//! merge that keeps blocks a viewer could not see when they save the notebook.
//!
//! Blocks are stored as a jsonb array, so exported data and the
//! GraphQL inputs map one to one: `{"id", "type": "md", "markdownInput": {"text"}}`,
//! `{"type": "query", "queryInput": {"text"}}`, `{"type": "file", "fileInput":
//! {"repositoryName", "filePath", "revision"?, "lineRange"?}}` and `{"type": "symbol",
//! "symbolInput": {"repositoryName", "filePath", "revision"?, "lineContext", "symbolName",
//! "symbolContainerName", "symbolKind"}}`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// The most blocks a notebook may have.
pub const MAX_BLOCKS: usize = 1000;
/// The longest title, in characters.
pub const MAX_TITLE_LEN: usize = 1024;
/// The most bytes of all blocks' text together.
pub const MAX_CONTENT_BYTES: usize = 1 << 20;
/// The most lines of context a symbol block may show around the symbol.
pub const MAX_LINE_CONTEXT: i32 = 1000;

/// One block of a notebook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// The client-chosen ID, unique in the notebook.
    pub id: String,
    #[serde(flatten)]
    pub kind: BlockKind,
}

/// What a block holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BlockKind {
    /// Markdown text, rendered on view.
    #[serde(rename = "md", rename_all = "camelCase")]
    Markdown { markdown_input: TextInput },
    /// A search query, run (streamed) on view.
    #[serde(rename = "query", rename_all = "camelCase")]
    Query { query_input: TextInput },
    /// A file or a range of lines in it, highlighted on view.
    #[serde(rename = "file", rename_all = "camelCase")]
    File { file_input: FileInput },
    /// A symbol, resolved to its current definition on view.
    #[serde(rename = "symbol", rename_all = "camelCase")]
    Symbol { symbol_input: SymbolInput },
}

/// The text of a Markdown or query block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextInput {
    pub text: String,
}

/// Lines `start_line..end_line`, 0-based (as in the GraphQL API).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineRange {
    pub start_line: i32,
    pub end_line: i32,
}

/// The file of a file block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInput {
    pub repository_name: String,
    pub file_path: String,
    /// `None`: the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// `None`: the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<LineRange>,
}

/// The symbol of a symbol block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInput {
    pub repository_name: String,
    pub file_path: String,
    /// `None`: the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Lines shown before and after the symbol's line.
    pub line_context: i32,
    pub symbol_name: String,
    pub symbol_container_name: String,
    /// The GraphQL `SymbolKind` name, e.g. `FUNCTION`.
    pub symbol_kind: String,
}

impl Block {
    /// The repository a file or symbol block refers to.
    pub fn repository_name(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::File { file_input } => Some(&file_input.repository_name),
            BlockKind::Symbol { symbol_input } => Some(&symbol_input.repository_name),
            BlockKind::Markdown { .. } | BlockKind::Query { .. } => None,
        }
    }

    fn text_len(&self) -> usize {
        match &self.kind {
            BlockKind::Markdown { markdown_input: t } | BlockKind::Query { query_input: t } => {
                t.text.len()
            }
            BlockKind::File { file_input: f } => {
                f.repository_name.len()
                    + f.file_path.len()
                    + f.revision.as_ref().map_or(0, String::len)
            }
            BlockKind::Symbol { symbol_input: s } => {
                s.repository_name.len()
                    + s.file_path.len()
                    + s.revision.as_ref().map_or(0, String::len)
                    + s.symbol_name.len()
                    + s.symbol_container_name.len()
                    + s.symbol_kind.len()
            }
        }
    }
}

/// The problems with a notebook's title and blocks; empty when it is valid: block IDs are
/// unique, a symbol block's line context is not negative, and sizes and line ranges are
/// within limits.
pub fn validate(title: &str, blocks: &[Block]) -> Vec<String> {
    let mut problems = Vec::new();
    if title.trim().is_empty() {
        problems.push("The title is empty.".to_owned());
    } else if title.chars().count() > MAX_TITLE_LEN {
        problems.push(format!(
            "The title is longer than {MAX_TITLE_LEN} characters."
        ));
    }
    if blocks.len() > MAX_BLOCKS {
        problems.push(format!("A notebook has at most {MAX_BLOCKS} blocks."));
    }
    let total: usize = blocks.iter().map(Block::text_len).sum();
    if total > MAX_CONTENT_BYTES {
        problems.push(format!(
            "The blocks are larger than {MAX_CONTENT_BYTES} bytes."
        ));
    }
    let mut ids = HashSet::new();
    for b in blocks {
        if b.id.is_empty() {
            problems.push("A block has an empty ID.".to_owned());
        } else if !ids.insert(b.id.as_str()) {
            problems.push(format!("Duplicate block ID found: {}.", b.id));
        }
        match &b.kind {
            BlockKind::File { file_input: f } => {
                if f.repository_name.trim().is_empty() || f.file_path.trim().is_empty() {
                    problems.push(format!(
                        "File block {} needs a repository and a file path.",
                        b.id
                    ));
                }
                if let Some(r) = f.line_range
                    && (r.start_line < 0 || r.end_line < r.start_line)
                {
                    problems.push(format!("File block {} has an invalid line range.", b.id));
                }
            }
            BlockKind::Symbol { symbol_input: s } => {
                if s.repository_name.trim().is_empty()
                    || s.file_path.trim().is_empty()
                    || s.symbol_name.trim().is_empty()
                {
                    problems.push(format!(
                        "Symbol block {} needs a repository, a file path and a symbol name.",
                        b.id
                    ));
                }
                if s.line_context < 0 {
                    problems.push(format!(
                        "Symbol block line context cannot be negative, block ID: {}.",
                        b.id
                    ));
                } else if s.line_context > MAX_LINE_CONTEXT {
                    problems.push(format!(
                        "Symbol block {} shows more than {MAX_LINE_CONTEXT} lines of context.",
                        b.id
                    ));
                }
            }
            BlockKind::Markdown { .. } | BlockKind::Query { .. } => {}
        }
    }
    problems
}

/// The blocks a viewer submits when saving, plus the blocks of `old` they were not shown
/// (`hidden`, block IDs of file and symbol blocks in repositories they may not read), so that
/// saving never drops content the viewer could not see. A hidden block goes back after the
/// block that preceded it in `old` if that one is still there (following any hidden blocks
/// already placed there), else where it was relative to the start. A submitted block with
/// the ID of a hidden one replaces nothing: the hidden block wins and the submitted one is
/// dropped, so a viewer cannot overwrite a block they never saw.
pub fn merge_hidden(old: &[Block], submitted: Vec<Block>, hidden: &HashSet<String>) -> Vec<Block> {
    if hidden.is_empty() {
        return submitted;
    }
    let mut out: Vec<Block> = submitted
        .into_iter()
        .filter(|b| !hidden.contains(&b.id))
        .collect();
    // Where each hidden block goes: after the nearest earlier block of `old` that is still
    // in `out` (by ID), or at the front.
    let mut after: HashMap<Option<String>, Vec<Block>> = HashMap::new();
    let mut anchor: Option<String> = None;
    let present: HashSet<String> = out.iter().map(|b| b.id.clone()).collect();
    for b in old {
        if hidden.contains(&b.id) {
            after.entry(anchor.clone()).or_default().push(b.clone());
        } else if present.contains(&b.id) {
            anchor = Some(b.id.clone());
        }
    }
    let mut merged = Vec::with_capacity(out.len() + hidden.len());
    if let Some(front) = after.remove(&None) {
        merged.extend(front);
    }
    for b in out.drain(..) {
        let id = Some(b.id.clone());
        merged.push(b);
        if let Some(rest) = after.remove(&id) {
            merged.extend(rest);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(id: &str, text: &str) -> Block {
        Block {
            id: id.into(),
            kind: BlockKind::Markdown {
                markdown_input: TextInput { text: text.into() },
            },
        }
    }

    fn file(id: &str, repo: &str) -> Block {
        Block {
            id: id.into(),
            kind: BlockKind::File {
                file_input: FileInput {
                    repository_name: repo.into(),
                    file_path: "a.go".into(),
                    revision: None,
                    line_range: Some(LineRange {
                        start_line: 1,
                        end_line: 3,
                    }),
                },
            },
        }
    }

    #[test]
    fn stored_shape() {
        let blocks = vec![
            md("1", "# Hi"),
            Block {
                id: "2".into(),
                kind: BlockKind::Query {
                    query_input: TextInput {
                        text: "repo:a b".into(),
                    },
                },
            },
            file("3", "gitlab.com/a/b"),
            Block {
                id: "4".into(),
                kind: BlockKind::Symbol {
                    symbol_input: SymbolInput {
                        repository_name: "r".into(),
                        file_path: "p".into(),
                        revision: Some("main".into()),
                        line_context: 3,
                        symbol_name: "f".into(),
                        symbol_container_name: "".into(),
                        symbol_kind: "FUNCTION".into(),
                    },
                },
            },
        ];
        let json = serde_json::to_value(&blocks).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"id": "1", "type": "md", "markdownInput": {"text": "# Hi"}},
                {"id": "2", "type": "query", "queryInput": {"text": "repo:a b"}},
                {"id": "3", "type": "file", "fileInput": {"repositoryName": "gitlab.com/a/b",
                    "filePath": "a.go", "lineRange": {"startLine": 1, "endLine": 3}}},
                {"id": "4", "type": "symbol", "symbolInput": {"repositoryName": "r",
                    "filePath": "p", "revision": "main", "lineContext": 3, "symbolName": "f",
                    "symbolContainerName": "", "symbolKind": "FUNCTION"}},
            ])
        );
        let back: Vec<Block> = serde_json::from_value(json).unwrap();
        assert_eq!(back, blocks);
    }

    #[test]
    fn validation() {
        assert!(validate("T", &[md("1", "x"), file("2", "r")]).is_empty());
        assert_eq!(validate(" ", &[]), ["The title is empty."]);
        let p = validate("T", &[md("1", "x"), md("1", "y")]);
        assert_eq!(p, ["Duplicate block ID found: 1."]);
        let mut bad = file("2", "r");
        if let BlockKind::File { file_input } = &mut bad.kind {
            file_input.line_range = Some(LineRange {
                start_line: 5,
                end_line: 2,
            });
        }
        assert_eq!(
            validate("T", &[bad]),
            ["File block 2 has an invalid line range."]
        );
        let sym = Block {
            id: "s".into(),
            kind: BlockKind::Symbol {
                symbol_input: SymbolInput {
                    repository_name: "r".into(),
                    file_path: "p".into(),
                    revision: None,
                    line_context: -1,
                    symbol_name: "f".into(),
                    symbol_container_name: "".into(),
                    symbol_kind: "FUNCTION".into(),
                },
            },
        };
        assert_eq!(
            validate("T", &[sym]),
            ["Symbol block line context cannot be negative, block ID: s."]
        );
    }

    #[test]
    fn merge_keeps_hidden_blocks_in_place() {
        let old = vec![
            file("h0", "secret"),
            md("a", "A"),
            file("h1", "secret"),
            file("h2", "secret"),
            md("b", "B"),
            md("c", "C"),
            file("h3", "secret"),
        ];
        let hidden: HashSet<String> = ["h0", "h1", "h2", "h3"].map(String::from).into();
        // The viewer saw a, b, c; edited b, removed c, added d, and tried to overwrite h1.
        let submitted = vec![md("a", "A"), md("b", "B2"), md("d", "D"), md("h1", "evil")];
        let merged = merge_hidden(&old, submitted, &hidden);
        let ids: Vec<&str> = merged.iter().map(|b| b.id.as_str()).collect();
        // h3 followed c, which is gone: its anchor is b (the last earlier block still there).
        assert_eq!(ids, ["h0", "a", "h1", "h2", "b", "h3", "d"]);
        assert_eq!(merged[2], file("h1", "secret"));
        assert_eq!(merged[4], md("b", "B2"));
        // Nothing hidden: the submission as is.
        let none = merge_hidden(&old, vec![md("x", "X")], &HashSet::new());
        assert_eq!(none, [md("x", "X")]);
    }
}
