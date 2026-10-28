//! Reordering a site configuration document by section.
//!
//! [`organize_site_config`] rewrites the top-level members of a JSONC site config in section
//! order ([`SITE_CONFIG_SECTIONS`]), with a `// ── Title ──` header before each section that
//! has keys. Values are kept byte for byte (comments inside them too). Top-level comments move
//! with the key they belong to:
//!
//! - comments directly above a key (no blank line between) and a comment after it on the same
//!   line stay with that key;
//! - a comment block that contains a commented-out key (`// "auth.public": true,`) is placed in
//!   that key's section, where the key would go (unless it sits directly above that same key);
//! - any other comment block separated by a blank line goes with the next key or commented-out
//!   key after it; comments before the closing `}` stay at the end.
//!
//! Comments before the opening `{` are kept as the document's preamble. Section headers from
//! an earlier run are dropped and written again, so organizing twice changes nothing.

use crate::jsonc::{JsoncError, parse_jsonc};
use crate::sections::{
    ConfigSection, OTHER_SECTION, SITE_CONFIG_SECTIONS, is_section_header, key_rank, section_header,
};

/// Why a document could not be organized.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OrganizeError {
    /// The text is not valid JSONC.
    #[error("syntax error: {0}")]
    Syntax(#[from] JsoncError),
    /// The document is not a JSON object.
    #[error("the site configuration must be a JSON object")]
    NotAnObject,
    /// A key appears twice; reordering could change which value wins.
    #[error("the key {0:?} appears more than once; remove the duplicate first")]
    DuplicateKey(String),
    /// Reordering would change the configuration (a bug); the document is left as is.
    #[error("reordering would change the configuration; it was left as is")]
    Changed,
}

/// One top-level entry: a member, or a comment block holding a commented-out key.
#[derive(Debug)]
struct Item {
    key: String,
    /// Comment blocks above the entry (each a list of lines, blocks separated by blank lines).
    leading: Vec<Vec<String>>,
    /// `"key": value` as written, `None` for a commented-out key.
    body: Option<String>,
    /// A comment after the member on the same line.
    trailing: Option<String>,
}

/// Returns `text` with its top-level keys ordered by section, under section headers.
pub fn organize_site_config(text: &str) -> Result<String, OrganizeError> {
    organize_document(text, SITE_CONFIG_SECTIONS)
}

/// Like [`organize_site_config`], for a document whose keys are grouped in `sections` (such
/// as settings, [`crate::SETTINGS_SECTIONS`]).
pub fn organize_document(text: &str, sections: &[ConfigSection]) -> Result<String, OrganizeError> {
    let value = parse_jsonc(text)?;
    if !value.is_object() {
        return Err(OrganizeError::NotAnObject);
    }
    let doc = scan(text)?;

    let mut seen = std::collections::HashSet::new();
    for item in doc.items.iter().filter(|i| i.body.is_some()) {
        if !seen.insert(item.key.as_str()) {
            return Err(OrganizeError::DuplicateKey(item.key.clone()));
        }
    }

    let out = render(&doc, sections);
    if parse_jsonc(&out).ok().as_ref() != Some(&value) {
        return Err(OrganizeError::Changed);
    }
    Ok(out)
}

struct Doc<'a> {
    preamble: &'a str,
    items: Vec<Item>,
    /// Comment blocks before the closing `}`.
    closing: Vec<Vec<String>>,
    epilogue: &'a str,
}

fn render(doc: &Doc<'_>, sections: &[ConfigSection]) -> String {
    let mut order: Vec<(usize, &Item)> = doc.items.iter().enumerate().collect();
    order.sort_by_key(|(i, item)| (key_rank(sections, &item.key), *i));

    let mut out = String::from(doc.preamble);
    out.push('{');
    out.push('\n');
    let mut current: Option<usize> = None;
    for (_, item) in &order {
        let section = key_rank(sections, &item.key).0;
        if current != Some(section) {
            if current.is_some() {
                out.push('\n');
            }
            let s = sections.get(section).unwrap_or(&OTHER_SECTION);
            out.push_str("  ");
            out.push_str(&section_header(s));
            out.push('\n');
            current = Some(section);
        } else {
            out.push('\n');
        }
        push_blocks(&mut out, &item.leading);
        if let Some(body) = &item.body {
            out.push_str("  ");
            out.push_str(body);
            out.push(',');
            if let Some(t) = &item.trailing {
                out.push(' ');
                out.push_str(t);
            }
            out.push('\n');
        }
    }
    if !doc.closing.is_empty() {
        if current.is_some() {
            out.push('\n');
        }
        push_blocks(&mut out, &doc.closing);
    }
    out.push('}');
    let epilogue = doc.epilogue.trim_end();
    if !epilogue.is_empty() {
        out.push_str(doc.epilogue.trim_end());
    }
    out.push('\n');
    out
}

fn push_blocks(out: &mut String, blocks: &[Vec<String>]) {
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for line in block {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Splits the document into preamble, top-level entries and epilogue.
fn scan(text: &str) -> Result<Doc<'_>, OrganizeError> {
    let b = text.as_bytes();
    let mut i = 0;
    // Preamble: whitespace and comments before `{`.
    loop {
        i = skip_ws(b, i).0;
        match b.get(i) {
            Some(b'/') => i = comment_end(b, i),
            Some(b'{') => break,
            _ => return Err(OrganizeError::NotAnObject),
        }
    }
    let preamble = &text[..i];
    i += 1;

    let mut items: Vec<Item> = Vec::new();
    // Detached comment blocks waiting for the next entry.
    let mut pending: Vec<Vec<String>> = Vec::new();
    // The comment block being collected (no blank line since its last comment).
    let mut group: Vec<String> = Vec::new();
    // The index of a member that ended on the current line.
    let mut member_on_line: Option<usize> = None;

    let flush_group =
        |group: &mut Vec<String>, pending: &mut Vec<Vec<String>>, items: &mut Vec<Item>| {
            let lines: Vec<String> = std::mem::take(group)
                .into_iter()
                .filter(|l| !is_section_header(l))
                .collect();
            if lines.is_empty() {
                return;
            }
            if let Some(key) = lines.iter().find_map(|l| commented_out_key(l)) {
                let mut leading = std::mem::take(pending);
                leading.push(lines);
                items.push(Item {
                    key,
                    leading,
                    body: None,
                    trailing: None,
                });
            } else {
                pending.push(lines);
            }
        };

    loop {
        let (next, newlines) = skip_ws(b, i);
        if newlines > 0 {
            member_on_line = None;
        }
        if newlines > 1 {
            flush_group(&mut group, &mut pending, &mut items);
        }
        i = next;
        match b.get(i) {
            None => return Err(OrganizeError::NotAnObject),
            Some(b'/') => {
                let end = comment_end(b, i);
                let comment = text[i..end].trim_end();
                match member_on_line {
                    Some(m) if items[m].trailing.is_none() && !comment.contains('\n') => {
                        items[m].trailing = Some(comment.to_owned());
                    }
                    _ => group.extend(comment.lines().map(|l| l.trim().to_owned())),
                }
                i = end;
            }
            Some(b',') => i += 1,
            Some(b'}') => {
                flush_group(&mut group, &mut pending, &mut items);
                return Ok(Doc {
                    preamble,
                    items,
                    closing: pending,
                    epilogue: &text[i + 1..],
                });
            }
            Some(b'"') => {
                let key_end = string_end(b, i);
                let key: String = serde_json::from_str(&text[i..key_end])
                    .map_err(|_| OrganizeError::NotAnObject)?;
                let value_end = member_end(b, key_end);
                let column = text[..i].rsplit('\n').next().map_or(0, str::len);
                let body = reindent(&text[i..value_end], column);
                // A block above holding a commented-out key of another key keeps its own place;
                // an example of this same key stays with it.
                if group
                    .iter()
                    .filter_map(|l| commented_out_key(l))
                    .any(|k| k != key)
                {
                    flush_group(&mut group, &mut pending, &mut items);
                }
                let attached: Vec<String> = std::mem::take(&mut group)
                    .into_iter()
                    .filter(|l| !is_section_header(l))
                    .collect();
                let mut leading = std::mem::take(&mut pending);
                // An empty last block keeps a blank line between detached comments and the key.
                if !attached.is_empty() || !leading.is_empty() {
                    leading.push(attached);
                }
                items.push(Item {
                    key,
                    leading,
                    body: Some(body),
                    trailing: None,
                });
                member_on_line = Some(items.len() - 1);
                i = value_end;
            }
            Some(_) => return Err(OrganizeError::NotAnObject),
        }
    }
}

/// Removes the member's original indentation (`column` bytes) from its continuation lines.
fn reindent(body: &str, column: usize) -> String {
    let mut lines = body.split('\n');
    let mut out = lines.next().unwrap_or_default().to_owned();
    for line in lines {
        out.push('\n');
        let ws = line.len() - line.trim_start().len();
        let strip = ws.min(column);
        out.push_str("  ");
        out.push_str(&line[strip..]);
    }
    out
}

/// `"key"` of a commented-out member such as `// "auth.public": true,`.
fn commented_out_key(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("//")?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let end = string_end(rest.as_bytes(), 0);
    if !rest[end..].trim_start().starts_with(':') {
        return None;
    }
    serde_json::from_str(&rest[..end]).ok()
}

/// Skips whitespace from `i`; returns the next position and how many newlines were skipped.
fn skip_ws(b: &[u8], mut i: usize) -> (usize, usize) {
    let mut newlines = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'\n' => newlines += 1,
            b' ' | b'\t' | b'\r' => {}
            _ => break,
        }
        i += 1;
    }
    (i, newlines)
}

/// The end of the comment starting at `i` (a line comment ends before its newline).
fn comment_end(b: &[u8], i: usize) -> usize {
    match b.get(i + 1) {
        Some(b'/') => b[i..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |p| i + p),
        Some(b'*') => b[i + 2..]
            .windows(2)
            .position(|w| w == b"*/")
            .map_or(b.len(), |p| i + 2 + p + 2),
        _ => i + 1,
    }
}

/// The position after the string starting at `i` (a `"`).
fn string_end(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while let Some(&c) = b.get(i) {
        match c {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The end of the member's value: the last significant byte before the `,` or `}` that ends
/// it at depth 0. `i` is just after the key.
fn member_end(b: &[u8], mut i: usize) -> usize {
    let mut depth = 0usize;
    let mut last = i;
    while let Some(&c) = b.get(i) {
        match c {
            b'"' => {
                i = string_end(b, i);
                last = i;
                continue;
            }
            b'/' if matches!(b.get(i + 1), Some(b'/' | b'*')) => {
                i = comment_end(b, i);
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return last,
            b'}' | b']' => depth -= 1,
            b',' if depth == 0 => return last,
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        i += 1;
        last = i;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_keys_by_section_with_headers() {
        let text = r#"{
  "search.limits": { "maxTimeoutSeconds": 60 },
  "externalURL": "https://x.example",
  "auth.public": true,
}"#;
        let out = organize_site_config(text).unwrap();
        assert_eq!(
            out,
            r#"{
  // ── General ──
  "externalURL": "https://x.example",

  // ── Authentication & sign-in ──
  "auth.public": true,

  // ── Search & indexing ──
  "search.limits": { "maxTimeoutSeconds": 60 },
}
"#
        );
    }

    #[test]
    fn keeps_comments_with_their_keys() {
        let text = r#"// Preamble.
{
  // About search.
  "search.limits": {
    // inner
    "maxTimeoutSeconds": 60,
  },

  // Free-standing note.

  "auth.public": true, // trailing
  // Branches:
  // "search.index.branches": { "a": ["b"] },

  "unknownKey": 1,
  // closing note
}
"#;
        let out = organize_site_config(text).unwrap();
        assert_eq!(
            out,
            r#"// Preamble.
{
  // ── Authentication & sign-in ──
  // Free-standing note.

  "auth.public": true, // trailing

  // ── Search & indexing ──
  // About search.
  "search.limits": {
    // inner
    "maxTimeoutSeconds": 60,
  },

  // Branches:
  // "search.index.branches": { "a": ["b"] },

  // ── Other ──
  "unknownKey": 1,

  // closing note
}
"#
        );
        assert_eq!(organize_site_config(&out).unwrap(), out, "idempotent");
    }

    #[test]
    fn rejects_duplicates_and_non_objects() {
        assert_eq!(
            organize_site_config(r#"{"auth.public": true, "auth.public": false}"#),
            Err(OrganizeError::DuplicateKey("auth.public".into()))
        );
        assert_eq!(organize_site_config("[]"), Err(OrganizeError::NotAnObject));
        assert!(matches!(
            organize_site_config("{"),
            Err(OrganizeError::Syntax(_))
        ));
    }

    #[test]
    fn empty_object() {
        assert_eq!(organize_site_config("{}").unwrap(), "{\n}\n");
        assert_eq!(organize_site_config(" {\n}\n").unwrap(), " {\n}\n");
    }

    #[test]
    fn reindents_nested_values() {
        let text = "{\n    \"log\": {\n        \"a\": 1\n    }\n}";
        let out = organize_site_config(text).unwrap();
        assert!(
            out.contains("  \"log\": {\n      \"a\": 1\n  },\n"),
            "{out}"
        );
    }
}
