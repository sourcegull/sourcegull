//! The search-based tier: definitions from symbol search, references from
//! whole-word text search, hover text from the definition's line and its doc comment.
//!
//! Definitions use `type:symbol ^name$` and references `\bname\b` with `case:yes`, both
//! limited to the file's language family.

use sourcegull_highlight::{Occurrence, SyntaxKind};

use crate::token;

/// The `lang:`/`file:` filter for files that can define or use a symbol of a file in `lang`.
/// TypeScript and JavaScript see each other, as do C and C++.
pub fn lang_filter(lang: Option<&str>) -> Option<String> {
    let family = match lang? {
        "typescript" | "tsx" | "javascript" | "jsx" => r"file:\.(ts|tsx|mts|cts|js|jsx|mjs|cjs)$",
        "c" | "cpp" | "objective-c" | "objective-cpp" => {
            r"file:\.(c|h|cc|cpp|cxx|c\+\+|hh|hpp|hxx|h\+\+|m|mm|ipp|inl)$"
        }
        id => return Some(format!("lang:{id}")),
    };
    Some(family.to_owned())
}

/// `repo:^<name>$[@rev]`.
pub fn repo_filter(name: &str, rev: Option<&str>) -> String {
    match rev {
        Some(rev) => format!("repo:^{}$@{rev}", regex::escape(name)),
        None => format!("repo:^{}$", regex::escape(name)),
    }
}

/// `-repo:^<name>$`.
pub fn exclude_repo_filter(name: &str) -> String {
    format!("-repo:^{}$", regex::escape(name))
}

/// A regexp query (pattern type `regexp`) for the symbols named `token`.
pub fn definitions_query(scope: &str, lang: Option<&str>, token: &str, count: usize) -> String {
    let lang = lang_filter(lang)
        .map(|f| format!(" {f}"))
        .unwrap_or_default();
    format!(
        "{scope} type:symbol case:yes count:{count}{lang} ^{}$",
        regex::escape(token)
    )
}

/// A regexp query (pattern type `regexp`) for whole-word occurrences of `token`.
pub fn references_query(scope: &str, lang: Option<&str>, token: &str, count: usize) -> String {
    let lang = lang_filter(lang)
        .map(|f| format!(" {f}"))
        .unwrap_or_default();
    let word = |c: Option<char>| c.is_some_and(|c| c == '_' || c.is_alphanumeric());
    let left = if word(token.chars().next()) {
        r"\b"
    } else {
        ""
    };
    let right = if word(token.chars().last()) {
        r"\b"
    } else {
        ""
    };
    format!(
        "{scope} type:file case:yes count:{count}{lang} {left}{}{right}",
        regex::escape(token)
    )
}

/// How close two paths are: the number of leading directories they share, plus one more
/// when they are the same file.
pub fn proximity(from: &str, to: &str) -> usize {
    if from == to {
        return usize::MAX;
    }
    let a: Vec<&str> = from.split('/').collect();
    let b: Vec<&str> = to.split('/').collect();
    a[..a.len() - 1]
        .iter()
        .zip(&b[..b.len() - 1])
        .take_while(|(x, y)| x == y)
        .count()
}

/// Whether the range `[start, end)` (UTF-16) on `line` lies in a comment or string.
pub fn in_comment_or_string(occ: &[Occurrence], line: u32, start: u32, end: u32) -> bool {
    let from = occ.partition_point(|o| o.line < line);
    occ[from..].iter().take_while(|o| o.line == line).any(|o| {
        matches!(
            o.kind,
            SyntaxKind::Comment | SyntaxKind::String | SyntaxKind::StringEscape
        ) && o.start_col <= start
            && end <= o.end_col
    })
}

/// The highlight kind at UTF-16 column `col` of `line`, if any.
pub fn kind_at(occ: &[Occurrence], line: u32, col: u32) -> Option<SyntaxKind> {
    let from = occ.partition_point(|o| o.line < line);
    occ[from..]
        .iter()
        .take_while(|o| o.line == line)
        .find(|o| o.start_col <= col && col < o.end_col)
        .map(|o| o.kind)
}

/// Kinds that never name a symbol to navigate to.
pub fn is_inert(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Comment
            | SyntaxKind::Keyword
            | SyntaxKind::String
            | SyntaxKind::StringEscape
            | SyntaxKind::Number
            | SyntaxKind::Regex
            | SyntaxKind::ConstantBuiltin
            | SyntaxKind::TypeBuiltin
            | SyntaxKind::Operator
    )
}

const COMMENT_PREFIXES: [&str; 6] = ["//", "/*", "*", "#", "--", ";;"];

/// Whether line `n` is entirely comment: by the highlighting when there is some, by its
/// leading marker otherwise.
fn is_comment_line(text: &str, n: u32, occ: Option<&[Occurrence]>) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    match occ {
        Some(occ) => {
            let lead = token::byte_to_utf16(text, text.len() - text.trim_start().len());
            let tail = token::byte_to_utf16(text, text.trim_end().len());
            kind_at(occ, n, lead) == Some(SyntaxKind::Comment)
                && kind_at(occ, n, tail.saturating_sub(1)) == Some(SyntaxKind::Comment)
        }
        None => COMMENT_PREFIXES.iter().any(|p| trimmed.starts_with(p)),
    }
}

/// Attribute and decorator lines between a doc comment and its definition.
fn is_attribute_line(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("#[") || t.starts_with('@')
}

/// Strips comment markers from one doc comment line.
fn strip_markers(line: &str) -> String {
    let mut t = line.trim();
    for p in ["/**", "/*!", "///", "//!", "/*", "//", "#", "--", ";;"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest;
            break;
        }
    }
    t = t.strip_suffix("*/").unwrap_or(t).trim_end();
    if let Some(rest) = t.trim_start().strip_prefix('*')
        && !rest.starts_with('/')
    {
        t = rest;
    }
    t.strip_prefix(' ').unwrap_or(t).trim_end().to_owned()
}

/// The doc comment of the definition on line `def_line` of `content`: the comment lines
/// right above it (attributes in between are skipped), or a Python docstring right below.
pub fn doc_comment(
    content: &str,
    def_line: u32,
    lang: Option<&str>,
    occ: Option<&[Occurrence]>,
) -> Vec<String> {
    let lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let Ok(def) = usize::try_from(def_line) else {
        return Vec::new();
    };
    if def >= lines.len() {
        return Vec::new();
    }
    if lang == Some("python") {
        return docstring(&lines[def + 1..]);
    }
    let mut above = Vec::new();
    let mut i = def;
    while i > 0 && def - i < 200 {
        i -= 1;
        let n = u32::try_from(i).unwrap_or(u32::MAX);
        if is_comment_line(lines[i], n, occ) {
            above.push(strip_markers(lines[i]));
        } else if above.is_empty() && is_attribute_line(lines[i]) {
            continue;
        } else {
            break;
        }
    }
    above.reverse();
    trim_blank(above)
}

/// The docstring at the start of a Python body.
fn docstring(body: &[&str]) -> Vec<String> {
    let Some(first) = body.iter().find(|l| !l.trim().is_empty()) else {
        return Vec::new();
    };
    let t = first.trim();
    let Some(quote) = ["\"\"\"", "'''"].into_iter().find(|q| t.starts_with(q)) else {
        return Vec::new();
    };
    let start = body.iter().position(|l| l.trim() == t).unwrap_or(0);
    let mut out = Vec::new();
    for (i, line) in body[start..].iter().enumerate().take(200) {
        let mut l = line.trim();
        if i == 0 {
            l = &l[quote.len()..];
        }
        if let Some(end) = l.find(quote) {
            out.push(l[..end].trim_end().to_owned());
            break;
        }
        out.push(l.to_owned());
    }
    trim_blank(out)
}

fn trim_blank(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let lead = lines.iter().take_while(|l| l.is_empty()).count();
    lines.drain(..lead);
    lines
}

/// The longest run of backticks in `s`, so a code fence around it can be longer.
fn fence_for(s: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat((longest + 1).max(3))
}

/// The hover in Markdown: the definition line in a code fence, then its doc comment.
pub fn hover_markdown(lang: Option<&str>, def_line: &str, docs: &[String]) -> String {
    let mut sig = def_line.trim().to_owned();
    if sig.len() > 300 {
        let mut cut = 300;
        while !sig.is_char_boundary(cut) {
            cut -= 1;
        }
        sig.truncate(cut);
        sig.push('…');
    }
    let fence = fence_for(&sig);
    let mut md = format!("{fence}{}\n{sig}\n{fence}", lang.unwrap_or(""));
    if !docs.is_empty() {
        md.push_str("\n\n---\n\n");
        md.push_str(&docs.join("\n"));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries() {
        let scope = repo_filter("gitlab.example.com/a/b", Some("abc123"));
        assert_eq!(scope, r"repo:^gitlab\.example\.com/a/b$@abc123");
        assert_eq!(
            definitions_query(&scope, Some("go"), "Foo", 50),
            r"repo:^gitlab\.example\.com/a/b$@abc123 type:symbol case:yes count:50 lang:go ^Foo$"
        );
        assert_eq!(
            references_query("repo:^x$", Some("tsx"), "$el", 500),
            r"repo:^x$ type:file case:yes count:500 file:\.(ts|tsx|mts|cts|js|jsx|mjs|cjs)$ \$el\b"
        );
        assert_eq!(
            references_query("repo:^x$", None, "run", 10),
            r"repo:^x$ type:file case:yes count:10 \brun\b"
        );
        assert_eq!(exclude_repo_filter("a.b"), r"-repo:^a\.b$");
    }

    #[test]
    fn path_proximity() {
        assert_eq!(proximity("a/b/c.go", "a/b/c.go"), usize::MAX);
        assert_eq!(proximity("a/b/c.go", "a/b/d.go"), 2);
        assert_eq!(proximity("a/b/c.go", "a/x/d.go"), 1);
        assert_eq!(proximity("c.go", "d.go"), 0);
        assert_eq!(proximity("a/c.go", "a"), 0);
    }

    fn occ(line: u32, start_col: u32, end_col: u32, kind: SyntaxKind) -> Occurrence {
        Occurrence {
            line,
            start_col,
            end_col,
            kind,
        }
    }

    #[test]
    fn doc_comments_from_highlighting() {
        let src = "use x;\n\n/// Adds two numbers.\n///\n/// Returns the sum.\n#[inline]\npub fn add(a: i32) -> i32 {\n";
        let o = [
            occ(0, 0, 3, SyntaxKind::Keyword),
            occ(2, 0, 21, SyntaxKind::Comment),
            occ(3, 0, 3, SyntaxKind::Comment),
            occ(4, 0, 20, SyntaxKind::Comment),
            occ(5, 0, 9, SyntaxKind::Attribute),
            occ(6, 0, 3, SyntaxKind::Keyword),
        ];
        assert_eq!(
            doc_comment(src, 6, Some("rust"), Some(&o)),
            ["Adds two numbers.", "", "Returns the sum."]
        );
        // A trailing comment after code is not a doc comment line.
        let src = "x = 1 // one\nfunc f() {\n";
        let o = [occ(0, 6, 12, SyntaxKind::Comment)];
        assert!(doc_comment(src, 1, Some("go"), Some(&o)).is_empty());
    }

    #[test]
    fn doc_comments_without_highlighting() {
        let src = "/**\n * Greets.\n * @param n name\n */\nfunction greet(n) {}\n";
        assert_eq!(
            doc_comment(src, 4, Some("javascript"), None),
            ["Greets.", "@param n name"]
        );
        let src = "def f(x):\n    \"\"\"Does f.\n\n    More.\n    \"\"\"\n    return x\n";
        assert_eq!(
            doc_comment(src, 0, Some("python"), None),
            ["Does f.", "", "More."]
        );
        let src = "def g():\n    '''One line.'''\n";
        assert_eq!(doc_comment(src, 0, Some("python"), None), ["One line."]);
        assert!(doc_comment("def h():\n    pass\n", 0, Some("python"), None).is_empty());
    }

    #[test]
    fn hover() {
        assert_eq!(
            hover_markdown(Some("go"), "\tfunc F() {", &["Does F.".into()]),
            "```go\nfunc F() {\n```\n\n---\n\nDoes F."
        );
        assert_eq!(
            hover_markdown(None, "x ```` y", &[]),
            "`````\nx ```` y\n`````"
        );
    }

    #[test]
    fn comment_and_string_ranges() {
        let o = [
            occ(1, 0, 4, SyntaxKind::Keyword),
            occ(1, 10, 20, SyntaxKind::String),
            occ(2, 0, 30, SyntaxKind::Comment),
        ];
        assert!(in_comment_or_string(&o, 1, 12, 15));
        assert!(!in_comment_or_string(&o, 1, 5, 8));
        assert!(in_comment_or_string(&o, 2, 3, 8));
        assert!(!in_comment_or_string(&o, 0, 3, 8));
        assert_eq!(kind_at(&o, 1, 2), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&o, 1, 4), None);
    }
}
