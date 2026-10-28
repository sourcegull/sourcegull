//! Identifiers under a position and conversions between byte and UTF-16 columns.
//!
//! The API speaks UTF-16 columns (as LSP's `character`), search results
//! and git content speak bytes.

/// An identifier on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    /// Byte columns, end exclusive.
    pub start_byte: usize,
    pub end_byte: usize,
    /// UTF-16 columns, end exclusive.
    pub start: u32,
    pub end: u32,
}

/// Whether `c` can be part of an identifier in language `lang` (a `sourcegull-languages` id).
pub fn is_ident_char(c: char, lang: Option<&str>) -> bool {
    c == '_'
        || c.is_alphanumeric()
        || (c == '$'
            && matches!(
                lang,
                Some("javascript" | "typescript" | "tsx" | "jsx" | "php")
            ))
}

/// The byte offset of UTF-16 column `col` in `line` (`None` past the end or inside a
/// surrogate pair).
pub fn utf16_to_byte(line: &str, col: u32) -> Option<usize> {
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units == col {
            return Some(i);
        }
        units += u32::try_from(c.len_utf16()).unwrap_or(2);
        if units > col {
            return None;
        }
    }
    (units == col).then_some(line.len())
}

/// The UTF-16 column of byte offset `byte` in `line` (clamped to the line).
pub fn byte_to_utf16(line: &str, byte: usize) -> u32 {
    let mut end = byte.min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end]
        .chars()
        .map(|c| u32::try_from(c.len_utf16()).unwrap_or(2))
        .sum()
}

/// The identifier covering UTF-16 column `col` of `line`, if any. Identifiers starting with
/// a digit (numbers) are not tokens.
pub fn token_at(line: &str, col: u32, lang: Option<&str>) -> Option<Token> {
    let at = utf16_to_byte(line, col)?;
    let c = line[at..].chars().next()?;
    if !is_ident_char(c, lang) {
        return None;
    }
    let start_byte = line[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident_char(c, lang))
        .last()
        .map_or(at, |(i, _)| i);
    let end_byte = line[at..]
        .char_indices()
        .find(|&(_, c)| !is_ident_char(c, lang))
        .map_or(line.len(), |(i, _)| at + i);
    let text = &line[start_byte..end_byte];
    if text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(Token {
        text: text.to_owned(),
        start_byte,
        end_byte,
        start: byte_to_utf16(line, start_byte),
        end: byte_to_utf16(line, end_byte),
    })
}

/// Line `n` (0-based) of `text`, without its line terminator.
pub fn line_of(text: &str, n: u32) -> Option<&str> {
    let line = text.split('\n').nth(usize::try_from(n).ok()?)?;
    Some(line.strip_suffix('\r').unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let line = "    let fooBar = baz_2(x);";
        let t = token_at(line, 9, Some("rust")).unwrap();
        assert_eq!((t.text.as_str(), t.start, t.end), ("fooBar", 8, 14));
        assert_eq!(token_at(line, 8, None).unwrap().text, "fooBar");
        assert_eq!(token_at(line, 13, None).unwrap().text, "fooBar");
        assert_eq!(token_at(line, 14, None), None);
        assert_eq!(token_at(line, 17, None).unwrap().text, "baz_2");
        assert_eq!(token_at("x = 42", 4, None), None);
        assert_eq!(token_at("", 0, None), None);
        assert_eq!(token_at("abc", 9, None), None);
        // `$` only where it is an identifier character.
        assert_eq!(
            token_at("$el.x", 1, Some("javascript")).unwrap().text,
            "$el"
        );
        assert_eq!(token_at("$el.x", 1, Some("go")).unwrap().text, "el");
    }

    #[test]
    fn utf16_columns() {
        // "é" is 2 bytes / 1 unit, "😀" is 4 bytes / 2 units.
        let line = "é😀ab";
        assert_eq!(utf16_to_byte(line, 0), Some(0));
        assert_eq!(utf16_to_byte(line, 1), Some(2));
        assert_eq!(utf16_to_byte(line, 2), None);
        assert_eq!(utf16_to_byte(line, 3), Some(6));
        assert_eq!(utf16_to_byte(line, 5), Some(8));
        assert_eq!(utf16_to_byte(line, 6), None);
        assert_eq!(byte_to_utf16(line, 6), 3);
        assert_eq!(byte_to_utf16(line, 3), 1);
        assert_eq!(byte_to_utf16(line, 99), 5);
        let t = token_at("😀 naïve()", 4, None).unwrap();
        assert_eq!((t.text.as_str(), t.start, t.end), ("naïve", 3, 8));
    }

    #[test]
    fn lines() {
        assert_eq!(line_of("a\r\nb\nc", 0), Some("a"));
        assert_eq!(line_of("a\r\nb\nc", 2), Some("c"));
        assert_eq!(line_of("a\n", 1), Some(""));
        assert_eq!(line_of("a", 1), None);
    }
}
