//! syntect (Sublime Text grammars) for languages without a tree-sitter
//! grammar.

use std::sync::LazyLock;
use std::time::Instant;

use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use crate::kind::{SyntaxKind, kind_for_scope};
use crate::{Error, Limits};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Finds a syntect syntax for a file.
pub(crate) fn find(
    path: &str,
    language: Option<&sourcegull_languages::Language>,
    first_line: &str,
) -> Option<&'static SyntaxReference> {
    let ss = &*SYNTAXES;
    let name = path.rsplit('/').next().unwrap_or(path);
    let by_ext = name
        .rsplit_once('.')
        .and_then(|(_, ext)| ss.find_syntax_by_extension(ext));
    by_ext
        .or_else(|| ss.find_syntax_by_extension(name))
        .or_else(|| language.and_then(|l| ss.find_syntax_by_name(l.name)))
        .or_else(|| language.and_then(|l| ss.find_syntax_by_token(l.id)))
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

/// Highlights `source` line by line, returning sorted byte spans.
pub(crate) fn highlight(
    syntax: &SyntaxReference,
    source: &str,
    limits: &Limits,
) -> Result<Vec<(usize, usize, SyntaxKind)>, Error> {
    let ss = &*SYNTAXES;
    let start = Instant::now();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut spans: Vec<(usize, usize, SyntaxKind)> = Vec::new();
    let mut offset = 0usize;
    let mut kind_cache: std::collections::HashMap<syntect::parsing::Scope, Option<SyntaxKind>> =
        std::collections::HashMap::new();
    for (i, line) in source.split_inclusive('\n').enumerate() {
        if i % 64 == 0 && start.elapsed() > limits.timeout {
            return Err(Error::Timeout);
        }
        let ops = state
            .parse_line(line, ss)
            .map_err(|e| Error::Highlight(e.to_string()))?;
        let mut pos = 0usize;
        let mut emit = |stack: &ScopeStack, from: usize, to: usize| {
            if from >= to {
                return;
            }
            let kind = stack.as_slice().iter().rev().find_map(|scope| {
                *kind_cache
                    .entry(*scope)
                    .or_insert_with(|| kind_for_scope(&scope.build_string()))
            });
            if let Some(kind) = kind {
                spans.push((offset + from, offset + to, kind));
            }
        };
        for (at, op) in ops {
            emit(&stack, pos, at);
            pos = at;
            stack
                .apply(&op)
                .map_err(|e| Error::Highlight(e.to_string()))?;
        }
        emit(&stack, pos, line.len());
        offset += line.len();
    }
    Ok(spans)
}
