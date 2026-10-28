//! JSONC ("JSON with comments") parsing.
//!
//! Site config and settings are JSONC documents: standard JSON plus `//` line comments,
//! `/* */` block comments and trailing commas in objects and arrays. Comments and trailing
//! commas are blanked out with spaces (newlines are kept), so the line and column reported
//! by `serde_json` for any remaining syntax error point at the original text.

use serde_json::Value;

/// A JSONC syntax error with a 1-based position in the original text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at line {line}, column {column}")]
pub struct JsoncError {
    /// Human-readable description.
    pub message: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column (in bytes).
    pub column: usize,
}

/// Parses a JSONC document into a [`Value`].
///
/// A document that is empty or contains only comments and whitespace parses as `{}`.
pub fn parse_jsonc(text: &str) -> Result<Value, JsoncError> {
    let stripped = strip_jsonc(text)?;
    if stripped.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&stripped).map_err(|e| JsoncError {
        message: strip_position_suffix(&e.to_string()),
        line: e.line(),
        column: e.column(),
    })
}

/// Removes the " at line X column Y" suffix that serde_json appends; we report it separately.
fn strip_position_suffix(msg: &str) -> String {
    match msg.rfind(" at line ") {
        Some(i) => msg[..i].to_string(),
        None => msg.to_string(),
    }
}

/// Returns `text` with comments and trailing commas replaced by spaces, preserving byte
/// offsets and newlines.
pub fn strip_jsonc(text: &str) -> Result<String, JsoncError> {
    let src = text.as_bytes();
    let mut out = src.to_vec();
    let mut i = 0;
    // Position of the last comma seen outside strings/comments that has not yet been followed
    // by a significant character.
    let mut pending_comma: Option<usize> = None;

    while i < src.len() {
        match src[i] {
            b'"' => {
                pending_comma = None;
                i += 1;
                while i < src.len() {
                    match src[i] {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                if i >= src.len() {
                    // Unterminated string: let serde_json report it with its position.
                    break;
                }
                i += 1;
            }
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < src.len() && src[i] != b'\n' {
                    if src[i] != b'\r' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                let start = i;
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                loop {
                    if i >= src.len() {
                        let (line, column) = line_col(src, start);
                        return Err(JsoncError {
                            message: "unterminated block comment".to_string(),
                            line,
                            column,
                        });
                    }
                    if src[i] == b'*' && src.get(i + 1) == Some(&b'/') {
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        break;
                    }
                    if src[i] != b'\n' && src[i] != b'\r' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            b',' => {
                pending_comma = Some(i);
                i += 1;
            }
            b'}' | b']' => {
                if let Some(c) = pending_comma.take() {
                    out[c] = b' ';
                }
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => {
                pending_comma = None;
                i += 1;
            }
        }
    }

    // Only ASCII bytes were replaced by ASCII spaces, and multi-byte characters are only
    // ever blanked entirely (inside comments), so the result is valid UTF-8.
    String::from_utf8(out).map_err(|e| {
        let (line, column) = line_col(src, e.utf8_error().valid_up_to());
        JsoncError {
            message: "invalid UTF-8 after comment removal".to_string(),
            line,
            column,
        }
    })
}

fn line_col(src: &[u8], offset: usize) -> (usize, usize) {
    let before = &src[..offset.min(src.len())];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let column = offset
        - before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1)
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json() {
        assert_eq!(
            parse_jsonc(r#"{"a": [1, 2]}"#).unwrap(),
            json!({"a": [1, 2]})
        );
    }

    #[test]
    fn comments_and_trailing_commas() {
        let text = r#"
        // leading comment
        {
          /* block
             comment */
          "a": 1, // trailing
          "b": [1, 2, /* inner */ 3,],
          "c": {"d": true,},
        }
        "#;
        assert_eq!(
            parse_jsonc(text).unwrap(),
            json!({"a": 1, "b": [1, 2, 3], "c": {"d": true}})
        );
    }

    #[test]
    fn comment_markers_inside_strings_are_kept() {
        let text =
            r#"{"url": "https://example.com/*x*/", "s": "a // b", "e": "q\"//,}", "t": ",]"}"#;
        assert_eq!(
            parse_jsonc(text).unwrap(),
            json!({"url": "https://example.com/*x*/", "s": "a // b", "e": "q\"//,}", "t": ",]"})
        );
    }

    #[test]
    fn escaped_backslash_before_quote() {
        let text = r#"{"p": "C:\\", // comment
        "q": 1}"#;
        assert_eq!(parse_jsonc(text).unwrap(), json!({"p": "C:\\", "q": 1}));
    }

    #[test]
    fn non_ascii_in_comments_and_strings() {
        let text = "{\n  // héllo — ünïcode\n  \"k\": \"värde /* not */\" /* ✓ */\n}";
        assert_eq!(parse_jsonc(text).unwrap(), json!({"k": "värde /* not */"}));
    }

    #[test]
    fn empty_document_is_empty_object() {
        assert_eq!(parse_jsonc("  // only a comment\n").unwrap(), json!({}));
        assert_eq!(parse_jsonc("").unwrap(), json!({}));
    }

    #[test]
    fn comma_followed_by_comment_then_bracket() {
        assert_eq!(parse_jsonc("[1, // x\n]").unwrap(), json!([1]));
    }

    #[test]
    fn double_comma_is_still_an_error() {
        assert!(parse_jsonc("[1,,]").is_err());
    }

    #[test]
    fn error_position_points_at_original_text() {
        let text = "{\n  /* c */ \"a\": 1,\n  \"b\": [1 2]\n}";
        let err = parse_jsonc(text).unwrap_err();
        assert_eq!((err.line, err.column), (3, 11), "{err:?}");
    }

    #[test]
    fn unterminated_block_comment() {
        let err = parse_jsonc("{\n  \"a\": 1 /* oops\n}").unwrap_err();
        assert_eq!((err.line, err.column), (2, 10));
        assert!(err.message.contains("block comment"));
    }
}
