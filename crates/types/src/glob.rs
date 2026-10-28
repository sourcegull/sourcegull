//! Glob patterns of code intelligence configuration policies: branch, tag and
//! repository names, with `gobwas/glob` syntax (without separators).
//!
//! - `*` (or `**`) matches any run of characters, `/` included;
//! - `?` matches one character;
//! - `[abc]`, `[a-z]` match one character of the class, `[!abc]` / `[^abc]` one not in it;
//! - `{a,b}` matches either alternative (alternatives may hold patterns, and nest);
//! - `\` escapes the next character.

use std::fmt;

/// Alternatives a pattern may expand to (`{a,b}{c,d}` is four).
const MAX_ALTERNATIVES: usize = 256;

/// A compiled glob.
#[derive(Clone, PartialEq, Eq)]
pub struct Glob {
    source: String,
    /// The pattern with alternations expanded.
    alternatives: Vec<Vec<Token>>,
}

impl fmt::Debug for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Glob").field(&self.source).finish()
    }
}

/// Why a pattern does not compile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid glob pattern {pattern:?}: {reason}")]
pub struct GlobError {
    pub pattern: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Char(char),
    Any,
    Star,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

/// A parsed node before alternations are expanded.
#[derive(Debug, Clone)]
enum Node {
    Token(Token),
    Alt(Vec<Vec<Node>>),
}

impl Glob {
    /// Compiles `pattern`.
    pub fn new(pattern: &str) -> Result<Self, GlobError> {
        let err = |reason| GlobError {
            pattern: pattern.to_owned(),
            reason,
        };
        let chars: Vec<char> = pattern.chars().collect();
        let mut pos = 0;
        let nodes = parse_seq(&chars, &mut pos, false).map_err(err)?;
        if pos != chars.len() {
            return Err(err("unmatched '}'"));
        }
        let alternatives = expand(&nodes).map_err(err)?;
        Ok(Self {
            source: pattern.to_owned(),
            alternatives,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether `name` matches the whole pattern.
    pub fn is_match(&self, name: &str) -> bool {
        let chars: Vec<char> = name.chars().collect();
        self.alternatives.iter().any(|t| match_tokens(t, &chars))
    }
}

/// Parses until the end, or (inside braces) until an unnested `,` or `}`.
fn parse_seq(chars: &[char], pos: &mut usize, in_alt: bool) -> Result<Vec<Node>, &'static str> {
    let mut out = Vec::new();
    while let Some(&c) = chars.get(*pos) {
        match c {
            ',' | '}' if in_alt => return Ok(out),
            '}' => return Err("unmatched '}'"),
            '\\' => {
                let next = *chars.get(*pos + 1).ok_or("trailing '\\'")?;
                out.push(Node::Token(Token::Char(next)));
                *pos += 2;
            }
            '*' => {
                while chars.get(*pos) == Some(&'*') {
                    *pos += 1;
                }
                out.push(Node::Token(Token::Star));
            }
            '?' => {
                out.push(Node::Token(Token::Any));
                *pos += 1;
            }
            '[' => {
                *pos += 1;
                out.push(Node::Token(parse_class(chars, pos)?));
            }
            '{' => {
                *pos += 1;
                let mut alts = Vec::new();
                loop {
                    alts.push(parse_seq(chars, pos, true)?);
                    match chars.get(*pos) {
                        Some(',') => *pos += 1,
                        Some('}') => {
                            *pos += 1;
                            break;
                        }
                        _ => return Err("unclosed '{'"),
                    }
                }
                out.push(Node::Alt(alts));
            }
            c => {
                out.push(Node::Token(Token::Char(c)));
                *pos += 1;
            }
        }
    }
    if in_alt {
        return Err("unclosed '{'");
    }
    Ok(out)
}

/// Parses a class after its `[`, through its `]`.
fn parse_class(chars: &[char], pos: &mut usize) -> Result<Token, &'static str> {
    let negated = matches!(chars.get(*pos), Some('!' | '^'));
    if negated {
        *pos += 1;
    }
    let mut ranges = Vec::new();
    loop {
        let c = match chars.get(*pos) {
            None => return Err("unclosed '['"),
            Some(']') if !ranges.is_empty() => {
                *pos += 1;
                return Ok(Token::Class { negated, ranges });
            }
            Some(']') => return Err("empty character class"),
            Some('\\') => {
                *pos += 1;
                *chars.get(*pos).ok_or("unclosed '['")?
            }
            Some(&c) => c,
        };
        *pos += 1;
        if chars.get(*pos) == Some(&'-') && chars.get(*pos + 1).is_some_and(|&e| e != ']') {
            let end = chars[*pos + 1];
            if end < c {
                return Err("invalid character range");
            }
            ranges.push((c, end));
            *pos += 2;
        } else {
            ranges.push((c, c));
        }
    }
}

/// Expands alternations into flat token lists.
fn expand(nodes: &[Node]) -> Result<Vec<Vec<Token>>, &'static str> {
    let mut acc: Vec<Vec<Token>> = vec![Vec::new()];
    for node in nodes {
        match node {
            Node::Token(t) => acc.iter_mut().for_each(|a| a.push(t.clone())),
            Node::Alt(alts) => {
                let mut options = Vec::new();
                for alt in alts {
                    options.extend(expand(alt)?);
                }
                if acc.len().saturating_mul(options.len()) > MAX_ALTERNATIVES {
                    return Err("too many alternatives");
                }
                acc = acc
                    .iter()
                    .flat_map(|a| {
                        options.iter().map(move |o| {
                            let mut v = a.clone();
                            v.extend(o.iter().cloned());
                            v
                        })
                    })
                    .collect();
            }
        }
    }
    Ok(acc)
}

/// Wildcard matching with single-star backtracking (linear in practice).
fn match_tokens(tokens: &[Token], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match tokens.get(t) {
            Some(Token::Star) => {
                star = Some((t, n));
                t += 1;
                continue;
            }
            Some(tok) if matches_one(tok, name[n]) => {
                t += 1;
                n += 1;
                continue;
            }
            _ => {}
        }
        match star {
            Some((st, sn)) => {
                t = st + 1;
                n = sn + 1;
                star = Some((st, sn + 1));
            }
            None => return false,
        }
    }
    tokens[t..].iter().all(|tok| *tok == Token::Star)
}

fn matches_one(token: &Token, c: char) -> bool {
    match token {
        Token::Char(x) => *x == c,
        Token::Any => true,
        Token::Star => false,
        Token::Class { negated, ranges } => {
            ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Glob::new(p).unwrap().is_match(s)
    }

    #[test]
    fn wildcards() {
        assert!(m("*", ""));
        assert!(m("*", "feature/x/y"));
        assert!(m("release/*", "release/1.0"));
        assert!(!m("release/*", "main"));
        assert!(m("*-stable", "6.1-stable"));
        assert!(m("v?.*", "v1.2"));
        assert!(!m("v?.*", "v12.2"));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("**/x", "a/b/x"));
        assert!(m("main", "main"));
        assert!(!m("main", "mainline"));
    }

    #[test]
    fn classes_and_alternatives() {
        assert!(m("v[0-9].*", "v3.1"));
        assert!(!m("v[0-9].*", "vx.1"));
        assert!(m("[!m]*", "dev"));
        assert!(!m("[!m]*", "main"));
        assert!(m("{main,master}", "master"));
        assert!(!m("{main,master}", "mast"));
        assert!(m("{release/*,v{1,2}.*}", "v2.0"));
        assert!(m("{release/*,v{1,2}.*}", "release/x"));
        assert!(m("a\\*", "a*"));
        assert!(!m("a\\*", "ab"));
        assert!(m("{,x}y", "y"));
    }

    #[test]
    fn errors() {
        for p in ["[", "[]", "{a,b", "a}", "\\", "[z-a]"] {
            assert!(Glob::new(p).is_err(), "{p}");
        }
        assert!(Glob::new(&"{a,b}".repeat(9)).is_err());
    }
}
