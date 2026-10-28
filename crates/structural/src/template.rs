//! Templates: the pattern side of structural search.
//!
//! A template is text with holes:
//!
//! - `:[x]` and `...`: anything, lazily, as long as delimiters inside it
//!   are balanced (it may be empty and span lines);
//! - `:[[x]]`: an identifier (letters, digits, `_`);
//! - `:[x.]`: a run of non-space characters other than delimiters;
//! - `:[x:e]`: an expression: characters and balanced groups up to
//!   whitespace, e.g. `a.b(c, d)[0]`;
//! - `:[x\n]`: the rest of the line, newline included;
//! - `:[ x]`: spaces and tabs;
//! - `:[x~regex]`: what `regex` matches at that point (`^` and `$` also
//!   match at line boundaries).
//!
//! Whitespace in a template matches one or more whitespace characters or
//! comments. Delimiters `()`, `[]`, `{}` that pair up in the template
//! must match a pair in the source. Holes with the same name (other than
//! `_` and names starting with `_`) must match the same text.

use regex::bytes::Regex;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unterminated hole at byte {0}")]
    Unterminated(usize),
    #[error("invalid hole name {0:?}")]
    BadName(String),
    #[error("invalid regular expression in hole {name:?}: {message}")]
    Regex { name: String, message: String },
    #[error("empty template")]
    Empty,
}

#[derive(Debug, Clone)]
pub enum HoleKind {
    Everything,
    Identifier,
    NonSpace,
    Expression,
    Line,
    Blank,
    Regex(Regex),
}

#[derive(Debug, Clone)]
pub struct Hole {
    pub name: String,
    pub kind: HoleKind,
}

impl Hole {
    /// Whether every hole with this name must match the same text.
    pub fn named(&self) -> bool {
        !self.name.is_empty() && !self.name.starts_with('_')
    }
}

#[derive(Debug, Clone)]
pub enum Node {
    /// Text without whitespace, matched exactly.
    Lit(Vec<u8>),
    /// Whitespace.
    Space,
    Hole(Hole),
    /// A delimiter pair and what is between them.
    Group {
        open: u8,
        close: u8,
        body: Vec<Node>,
    },
}

/// A parsed template.
#[derive(Debug, Clone)]
pub struct Template {
    pub(crate) nodes: Vec<Node>,
}

impl Template {
    pub fn parse(text: &str) -> Result<Template, TemplateError> {
        let bytes = text.trim().as_bytes();
        if bytes.is_empty() {
            return Err(TemplateError::Empty);
        }
        let pairs = pair_delimiters(bytes);
        let nodes = parse_seq(bytes, 0, bytes.len(), &pairs)?;
        Ok(Template { nodes })
    }

    /// The literal parts every match contains, for prefiltering.
    pub fn literals(&self) -> Vec<String> {
        fn walk(nodes: &[Node], out: &mut Vec<String>) {
            for n in nodes {
                match n {
                    Node::Lit(l) => out.push(String::from_utf8_lossy(l).into_owned()),
                    Node::Group { body, .. } => walk(body, out),
                    Node::Space | Node::Hole(_) => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.nodes, &mut out);
        out
    }
}

/// For each opening delimiter in the template, the index of its closer.
fn pair_delimiters(b: &[u8]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; b.len()];
    let mut stack: Vec<(usize, u8)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // Holes are not delimiters.
        if b[i..].starts_with(b":[") {
            match hole_end(b, i) {
                Some(e) => {
                    i = e;
                    continue;
                }
                None => break,
            }
        }
        if let Some(close) = crate::scan::is_open(b[i]) {
            stack.push((i, close));
        } else if crate::scan::is_close(b[i])
            && let Some(at) = stack.iter().rposition(|&(_, c)| c == b[i])
        {
            pairs[stack[at].0] = Some(i);
            stack.truncate(at);
        }
        i += 1;
    }
    pairs
}

/// The end of the hole starting at `i` (`:[`), honoring brackets in regexps.
fn hole_end(b: &[u8], i: usize) -> Option<usize> {
    if b[i..].starts_with(b":[[") {
        let close = memchr::memmem::find(&b[i + 3..], b"]]")?;
        return Some(i + 3 + close + 2);
    }
    let mut depth = 0usize;
    let mut j = i + 2;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' if depth == 0 => return Some(j + 1),
            b']' => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    None
}

fn valid_name(name: &str) -> bool {
    name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn parse_hole(b: &[u8], start: usize, end: usize) -> Result<Hole, TemplateError> {
    let text = String::from_utf8_lossy(&b[start..end]).into_owned();
    if let Some(name) = text.strip_prefix(":[[").and_then(|t| t.strip_suffix("]]")) {
        if !valid_name(name) {
            return Err(TemplateError::BadName(name.to_owned()));
        }
        return Ok(Hole {
            name: name.to_owned(),
            kind: HoleKind::Identifier,
        });
    }
    let inner = &text[2..text.len() - 1];
    if let Some((name, re)) = inner.split_once('~') {
        if !valid_name(name) {
            return Err(TemplateError::BadName(name.to_owned()));
        }
        let regex = Regex::new(&format!("^(?m:{re})")).map_err(|e| TemplateError::Regex {
            name: name.to_owned(),
            message: e.to_string(),
        })?;
        return Ok(Hole {
            name: name.to_owned(),
            kind: HoleKind::Regex(regex),
        });
    }
    let (name, kind) = if let Some(n) = inner.strip_prefix(' ') {
        (n, HoleKind::Blank)
    } else if let Some(n) = inner.strip_suffix(":e") {
        (n, HoleKind::Expression)
    } else if let Some(n) = inner.strip_suffix('.') {
        (n, HoleKind::NonSpace)
    } else if let Some(n) = inner.strip_suffix("\\n") {
        (n, HoleKind::Line)
    } else {
        (inner, HoleKind::Everything)
    };
    if !valid_name(name) {
        return Err(TemplateError::BadName(name.to_owned()));
    }
    Ok(Hole {
        name: name.to_owned(),
        kind,
    })
}

fn parse_seq(
    b: &[u8],
    mut i: usize,
    end: usize,
    pairs: &[Option<usize>],
) -> Result<Vec<Node>, TemplateError> {
    let mut nodes = Vec::new();
    let mut lit = Vec::new();
    let flush = |lit: &mut Vec<u8>, nodes: &mut Vec<Node>| {
        if !lit.is_empty() {
            nodes.push(Node::Lit(std::mem::take(lit)));
        }
    };
    while i < end {
        let c = b[i];
        if b[i..end].starts_with(b":[") {
            let e = hole_end(b, i).filter(|&e| e <= end);
            let e = e.ok_or(TemplateError::Unterminated(i))?;
            flush(&mut lit, &mut nodes);
            nodes.push(Node::Hole(parse_hole(b, i, e)?));
            i = e;
        } else if b[i..end].starts_with(b"...") {
            flush(&mut lit, &mut nodes);
            nodes.push(Node::Hole(Hole {
                name: String::new(),
                kind: HoleKind::Everything,
            }));
            i += 3;
        } else if c.is_ascii_whitespace() {
            flush(&mut lit, &mut nodes);
            while i < end && b[i].is_ascii_whitespace() {
                i += 1;
            }
            nodes.push(Node::Space);
        } else if let (Some(close), Some(j)) = (crate::scan::is_open(c), pairs[i]) {
            flush(&mut lit, &mut nodes);
            let body = parse_seq(b, i + 1, j, pairs)?;
            nodes.push(Node::Group {
                open: c,
                close,
                body,
            });
            i = j + 1;
        } else {
            lit.push(c);
            i += 1;
        }
    }
    flush(&mut lit, &mut nodes);
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(nodes: &[Node]) -> String {
        nodes
            .iter()
            .map(|n| match n {
                Node::Lit(l) => format!("'{}'", String::from_utf8_lossy(l)),
                Node::Space => "_".to_owned(),
                Node::Hole(h) => format!(
                    "{}:{}",
                    match h.kind {
                        HoleKind::Everything => "any",
                        HoleKind::Identifier => "id",
                        HoleKind::NonSpace => "ns",
                        HoleKind::Expression => "expr",
                        HoleKind::Line => "line",
                        HoleKind::Blank => "blank",
                        HoleKind::Regex(_) => "re",
                    },
                    h.name
                ),
                Node::Group { open, close, body } => {
                    format!("{}{}{}", *open as char, shape(body), *close as char)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn parse(t: &str) -> String {
        shape(&Template::parse(t).unwrap().nodes)
    }

    #[test]
    fn holes_groups_and_space() {
        assert_eq!(parse("fmt.Println(:[x])"), "'fmt.Println' (any:x)");
        assert_eq!(
            parse("if  :[[c]] {\n :[body] }"),
            "'if' _ id:c _ {_ any:body _}"
        );
        assert_eq!(
            parse(":[x.] :[y\\n]:[ z]... :[n~[a-z]+]"),
            "ns:x _ line:y blank:z any: _ re:n"
        );
        // Unpaired delimiters are literal text.
        assert_eq!(parse("a) (b"), "'a)' _ '(b'");
        assert_eq!(parse("f([:[x]])"), "'f' ([any:x])");
    }

    #[test]
    fn literals_and_errors() {
        let t = Template::parse("foo.bar(:[x], baz)").unwrap();
        assert_eq!(t.literals(), ["foo.bar", ",", "baz"]);
        assert!(matches!(
            Template::parse("f(:[x"),
            Err(TemplateError::Unterminated(_))
        ));
        assert!(matches!(
            Template::parse(":[a-b]"),
            Err(TemplateError::BadName(_))
        ));
        assert!(matches!(
            Template::parse(":[x~(]"),
            Err(TemplateError::Regex { .. })
        ));
        assert_eq!(Template::parse("  ").unwrap_err(), TemplateError::Empty);
        assert!(
            Hole {
                name: "x".into(),
                kind: HoleKind::Everything
            }
            .named()
        );
        assert!(
            !Hole {
                name: "_x".into(),
                kind: HoleKind::Everything
            }
            .named()
        );
    }
}
