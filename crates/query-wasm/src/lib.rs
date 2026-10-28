//! sourcegull-query-wasm: the query parser for the web UI's query box, built with
//! `cargo build --target wasm32-unknown-unknown`.
//!
//! [`analyze`] parses and validates a query and returns JSON with the tokens
//! to highlight and the diagnostics to show, offsets in UTF-16 code units (as
//! JavaScript strings count them):
//!
//! ```text
//! {"patternType":"keyword",
//!  "tokens":[[start,end,"field"],[start,end,"value"],…],
//!  "diagnostics":[{"start":0,"end":5,"message":"…"}]}
//! ```
//!
//! Token kinds: `field` (with its `:`), `value`, `predicate`, `negation`,
//! `pattern`, `regexp`, `quoted`, `keyword` (`AND`, `OR`, `NOT`), `paren`.
//!
//! The WASM interface passes bytes through two buffers owned by this module,
//! so no raw pointer is ever dereferenced on this side: JavaScript asks for
//! the input buffer ([`wasm_input`]), writes the UTF-8 query into it, calls
//! [`wasm_analyze`] and reads [`wasm_output`]'s bytes.

use std::cell::RefCell;
use std::fmt::Write;

use sourcegull_query::{FIELDS, Node, PatternKind, PatternType, Query, Span, parse, validate};

/// A highlighted range, byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: &'static str,
}

/// A problem in the query, byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub message: String,
}

/// Tokens and diagnostics of `input`, byte offsets.
pub fn tokens(input: &str, default: PatternType) -> (PatternType, Vec<Token>, Vec<Diagnostic>) {
    let mut out = Vec::new();
    let mut diagnostics = Vec::new();
    let pattern_type = match parse(input, default) {
        Ok(q) => {
            if let Err(e) = validate(&q) {
                diagnostics.push(diagnostic(input, e.span(), e.to_string()));
            }
            if let Some(root) = &q.root {
                leaves(input, root, &mut out);
            }
            out.sort_by_key(|t| t.start);
            gaps(input, &mut out);
            q.pattern_type
        }
        Err(e) => {
            diagnostics.push(diagnostic(input, e.span(), e.to_string()));
            fallback(input, &mut out);
            default
        }
    };
    out.sort_by_key(|t| t.start);
    (pattern_type, out, diagnostics)
}

fn diagnostic(input: &str, span: Span, message: String) -> Diagnostic {
    let start = span.start.min(input.len());
    let end = span.end.clamp(start, input.len());
    Diagnostic {
        start,
        end,
        message,
    }
}

fn leaves(input: &str, node: &Node, out: &mut Vec<Token>) {
    match node {
        Node::Parameter(p) => {
            let mut at = not_prefix(input, p.span.start, p.span.end, out);
            let text = &input[at..p.span.end];
            if text.starts_with('-') {
                out.push(Token {
                    start: at,
                    end: at + 1,
                    kind: "negation",
                });
                at += 1;
            }
            let Some(colon) = input[at..p.span.end].find(':') else {
                return;
            };
            out.push(Token {
                start: at,
                end: at + colon + 1,
                kind: "field",
            });
            let value_start = at + colon + 1;
            if value_start < p.span.end {
                let kind = if matches!(p.predicate(), Some(Ok(_))) {
                    "predicate"
                } else {
                    "value"
                };
                out.push(Token {
                    start: value_start,
                    end: p.span.end,
                    kind,
                });
            }
        }
        Node::Pattern(p) => {
            let mut start = not_prefix(input, p.span.start, p.span.end, out);
            if p.negated && input[start..p.span.end].starts_with('-') {
                out.push(Token {
                    start,
                    end: start + 1,
                    kind: "negation",
                });
                start += 1;
            }
            let kind = match p.kind {
                PatternKind::Literal => "pattern",
                PatternKind::Regexp => "regexp",
                PatternKind::Quoted => "quoted",
            };
            if start < p.span.end {
                out.push(Token {
                    start,
                    end: p.span.end,
                    kind,
                });
            }
        }
        Node::Operator { operands, .. } => {
            for o in operands {
                leaves(input, o, out);
            }
        }
    }
}

/// Emits a `NOT` keyword at the start of a leaf's span and returns where
/// the rest begins.
fn not_prefix(input: &str, start: usize, end: usize, out: &mut Vec<Token>) -> usize {
    let text = &input[start..end];
    let is_not = text.len() > 3
        && text.as_bytes()[..3].eq_ignore_ascii_case(b"not")
        && text[3..].starts_with(|c: char| c.is_whitespace());
    if !is_not {
        return start;
    }
    out.push(Token {
        start,
        end: start + 3,
        kind: "keyword",
    });
    end - text[3..].trim_start().len()
}

/// Tokens for the text between leaves: operators and parentheses.
fn gaps(input: &str, out: &mut Vec<Token>) {
    let mut extra = Vec::new();
    let mut at = 0;
    let scan = |from: usize, to: usize, extra: &mut Vec<Token>| {
        let text = &input[from..to];
        let mut i = 0;
        let bytes = text.as_bytes();
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'(' || b == b')' {
                extra.push(Token {
                    start: from + i,
                    end: from + i + 1,
                    kind: "paren",
                });
                i += 1;
            } else if b.is_ascii_whitespace() {
                i += 1;
            } else {
                let end = text[i..]
                    .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
                    .map_or(text.len(), |n| i + n);
                let word = &text[i..end];
                let kind = if ["and", "or", "not"].contains(&word.to_ascii_lowercase().as_str()) {
                    "keyword"
                } else {
                    "pattern"
                };
                extra.push(Token {
                    start: from + i,
                    end: from + end,
                    kind,
                });
                i = end;
            }
        }
    };
    for t in out.iter() {
        if t.start > at {
            scan(at, t.start, &mut extra);
        }
        at = at.max(t.end);
    }
    if at < input.len() {
        scan(at, input.len(), &mut extra);
    }
    out.extend(extra);
}

/// Highlighting for a query that does not parse: whitespace-separated
/// words, `field:value` where the field is known.
fn fallback(input: &str, out: &mut Vec<Token>) {
    let mut words = Vec::new();
    gaps(input, &mut words);
    for w in words {
        let text = &input[w.start..w.end];
        let neg = usize::from(text.starts_with('-'));
        let field = text[neg..]
            .split_once(':')
            .filter(|(f, _)| sourcegull_query::canonical_field(f).is_some());
        match field {
            Some((f, _)) => {
                if neg == 1 {
                    out.push(Token {
                        start: w.start,
                        end: w.start + 1,
                        kind: "negation",
                    });
                }
                let colon = w.start + neg + f.len() + 1;
                out.push(Token {
                    start: w.start + neg,
                    end: colon,
                    kind: "field",
                });
                if colon < w.end {
                    out.push(Token {
                        start: colon,
                        end: w.end,
                        kind: "value",
                    });
                }
            }
            None => out.push(w),
        }
    }
}

fn pattern_type_name(t: PatternType) -> &'static str {
    match t {
        PatternType::Keyword => "keyword",
        PatternType::Standard => "standard",
        PatternType::Literal => "literal",
        PatternType::Regexp => "regexp",
        PatternType::Structural => "structural",
    }
}

/// UTF-16 offset of every byte offset that starts a char (and of the end).
fn utf16_offsets(input: &str) -> Vec<u32> {
    let mut map = vec![0u32; input.len() + 1];
    let mut u = 0u32;
    for (i, c) in input.char_indices() {
        map[i] = u;
        u += c.len_utf16() as u32;
    }
    map[input.len()] = u;
    // Offsets inside a char (never produced by the parser) map to its start.
    for i in 1..map.len() {
        if !input.is_char_boundary(i) {
            map[i] = map[i - 1];
        }
    }
    map
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The JSON described in the module docs.
pub fn analyze(input: &str, default: PatternType) -> String {
    let (pattern_type, toks, diags) = tokens(input, default);
    let u = utf16_offsets(input);
    let mut out = String::with_capacity(64 + toks.len() * 16);
    out.push_str("{\"patternType\":");
    push_json_str(&mut out, pattern_type_name(pattern_type));
    out.push_str(",\"tokens\":[");
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{},{},\"{}\"]", u[t.start], u[t.end], t.kind);
    }
    out.push_str("],\"diagnostics\":[");
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"start\":{},\"end\":{},\"message\":",
            u[d.start], u[d.end]
        );
        push_json_str(&mut out, &d.message);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// The fields with their aliases as a JSON array of arrays, canonical first.
pub fn fields_json() -> String {
    let mut out = String::from("[");
    for (i, names) in FIELDS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, n) in names.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            push_json_str(&mut out, n);
        }
        out.push(']');
    }
    out.push(']');
    out
}

/// Parses a query as the stream API does (for tests and native callers).
pub fn parse_query(input: &str, default: PatternType) -> Option<Query> {
    parse(input, default).ok()
}

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

// The exports below need unmangled names (`no_mangle` is an unsafe attribute
// because two items with one symbol name would clash); the `wasm_` prefix
// keeps them unique. Their bodies are safe code.

/// Makes the input buffer `len` bytes long and returns where to write.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_input(len: usize) -> *mut u8 {
    INPUT.with_borrow_mut(|b| {
        b.clear();
        b.resize(len, 0);
        b.as_mut_ptr()
    })
}

/// Analyzes the input buffer (`pattern_type`: 0 keyword, 1 standard,
/// 2 literal, 3 regexp, 4 structural) and returns the output's length.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_analyze(pattern_type: u32) -> usize {
    let default = match pattern_type {
        1 => PatternType::Standard,
        2 => PatternType::Literal,
        3 => PatternType::Regexp,
        4 => PatternType::Structural,
        _ => PatternType::Keyword,
    };
    let json = INPUT.with_borrow(|b| analyze(&String::from_utf8_lossy(b), default));
    set_output(json)
}

/// Writes [`fields_json`] to the output buffer and returns its length.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_fields() -> usize {
    set_output(fields_json())
}

/// The output buffer.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_output() -> *const u8 {
    OUTPUT.with_borrow(|b| b.as_ptr())
}

fn set_output(s: String) -> usize {
    OUTPUT.with_borrow_mut(|b| {
        *b = s.into_bytes();
        b.len()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(q: &str) -> Vec<(&str, &'static str)> {
        let (_, toks, _) = tokens(q, PatternType::Keyword);
        toks.iter().map(|t| (&q[t.start..t.end], t.kind)).collect()
    }

    #[test]
    fn highlights_fields_patterns_and_operators() {
        assert_eq!(
            kinds("repo:^a/b$ -file:test (foo OR bar) NOT baz"),
            [
                ("repo:", "field"),
                ("^a/b$", "value"),
                ("-", "negation"),
                ("file:", "field"),
                ("test", "value"),
                ("(", "paren"),
                ("foo", "pattern"),
                ("OR", "keyword"),
                ("bar", "pattern"),
                (")", "paren"),
                ("NOT", "keyword"),
                ("baz", "pattern"),
            ]
        );
        assert_eq!(
            kinds("repo:has.file(path:go.mod) \"exact phrase\""),
            [
                ("repo:", "field"),
                ("has.file(path:go.mod)", "predicate"),
                ("\"exact phrase\"", "quoted"),
            ]
        );
        // Regexp queries quote too.
        let (_, toks, _) = tokens(r#"foo "a.b" 'c d'"#, PatternType::Regexp);
        let kinds: Vec<_> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, ["pattern", "quoted", "quoted"]);
    }

    #[test]
    fn reports_parse_and_validation_errors() {
        let (_, toks, d) = tokens("repo:x foo)", PatternType::Keyword);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.starts_with("unsupported expression"),
            "{}",
            d[0].message
        );
        assert_eq!((d[0].start, d[0].end), (10, 11));
        assert_eq!(toks[0].kind, "field");
        // An unclosed `(` is text, and so is a `)` after it.
        assert!(tokens("repo:x (foo", PatternType::Keyword).2.is_empty());
        let (_, _, d) = tokens("x()(y or z)", PatternType::Standard);
        assert!(d[0].message.contains("content: filter"), "{}", d[0].message);
        let (_, _, d) = tokens("case:maybe foo", PatternType::Keyword);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].start, d[0].end), (0, 10));
        let (_, _, d) = tokens("a (b or c) d", PatternType::Standard);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.contains("trouble understanding"),
            "{}",
            d[0].message
        );
        assert!(tokens("a (b or c) d", PatternType::Keyword).2.is_empty());
    }

    #[test]
    fn json_uses_utf16_offsets() {
        let j = analyze("ü😀 lang:go", PatternType::Keyword);
        // "ü😀" is 1 + 2 UTF-16 units.
        assert_eq!(
            j,
            r#"{"patternType":"keyword","tokens":[[0,3,"pattern"],[4,9,"field"],[9,11,"value"]],"diagnostics":[]}"#
        );
        assert!(analyze("patterntype:regexp a", PatternType::Keyword).contains("\"regexp\""));
        assert!(fields_json().starts_with("[[\"repo\",\"r\"]"));
    }

    #[test]
    fn buffers_round_trip() {
        let q = b"lang:rust x";
        let p = wasm_input(q.len());
        assert!(!p.is_null());
        INPUT.with_borrow_mut(|b| b.copy_from_slice(q));
        let n = wasm_analyze(0);
        let out = OUTPUT.with_borrow(|b| String::from_utf8(b.clone()).unwrap());
        assert_eq!(out.len(), n);
        assert!(out.contains("\"field\""));
    }
}
