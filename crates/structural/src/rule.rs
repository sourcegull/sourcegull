//! `rule:` constraints on matches:
//!
//! ```text
//! where <cond>, <cond>, ...
//! cond  := expr == expr | expr != expr | true | false
//!        | match expr { | "template" -> cond, cond, ... | ... }
//! expr  := :[name] | "text with :[holes]"
//! ```
//!
//! A match is kept when every condition holds. `match` tries its cases in
//! order against the whole value and takes the first that matches (false
//! when none does); that case's conditions decide, and they see the holes
//! of the case's template besides the match's. Rewrite rules belong to
//! compute and are rejected here.

use crate::matcher::{Env, trimmed};
use crate::syntax::GENERIC;
use crate::{StructuralError, Template};

#[derive(Debug, Clone)]
enum Expr {
    /// Text with holes, substituted from the match.
    Text(String),
}

#[derive(Debug, Clone)]
enum Cond {
    Const(bool),
    Eq(Expr, Expr, bool),
    Match(Expr, Vec<(Template, Vec<Cond>)>),
}

/// A parsed rule.
#[derive(Debug, Clone)]
pub struct Rule {
    conds: Vec<Cond>,
}

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn ws(&mut self) {
        let rest = &self.s[self.pos..];
        self.pos += rest.len() - rest.trim_start().len();
    }

    fn eat(&mut self, tok: &str) -> bool {
        self.ws();
        if self.s[self.pos..].starts_with(tok) {
            self.pos += tok.len();
            true
        } else {
            false
        }
    }

    fn keyword(&mut self, word: &str) -> bool {
        self.ws();
        let rest = &self.s[self.pos..];
        let boundary = rest[word.len().min(rest.len())..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if rest.starts_with(word) && boundary {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn done(&mut self) -> bool {
        self.ws();
        self.pos == self.s.len()
    }

    fn err(&self, what: &str) -> StructuralError {
        StructuralError::Rule(format!("{what} at offset {}", self.pos))
    }

    /// `:[name]` or a quoted string.
    fn expr(&mut self) -> Result<Expr, StructuralError> {
        self.ws();
        let rest = &self.s[self.pos..];
        if rest.starts_with(":[") {
            let end = if rest.starts_with(":[[") {
                rest.find("]]").map(|e| e + 1)
            } else {
                rest.find(']')
            };
            let end = end.ok_or_else(|| self.err("unterminated hole"))?;
            let text = rest[..=end].to_owned();
            self.pos += end + 1;
            return Ok(Expr::Text(text));
        }
        Ok(Expr::Text(self.string()?))
    }

    fn string(&mut self) -> Result<String, StructuralError> {
        self.ws();
        let rest = &self.s[self.pos..];
        let quote = rest
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(|| self.err("expected a hole or a quoted string"))?;
        let mut out = String::new();
        let mut chars = rest[1..].char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                // `\"` and `\\` escape; other backslashes are kept (regexps).
                '\\' => match chars.next() {
                    Some((_, n)) if n == quote || n == '\\' => out.push(n),
                    Some((_, n)) => {
                        out.push('\\');
                        out.push(n);
                    }
                    None => out.push('\\'),
                },
                c if c == quote => {
                    self.pos += 1 + i + 1;
                    return Ok(out);
                }
                c => out.push(c),
            }
        }
        Err(self.err("unterminated string"))
    }

    fn cond(&mut self) -> Result<Cond, StructuralError> {
        if self.keyword("true") {
            return Ok(Cond::Const(true));
        }
        if self.keyword("false") {
            return Ok(Cond::Const(false));
        }
        if self.keyword("rewrite") {
            return Err(StructuralError::Rule(
                "rewrite rules are not supported in search".to_owned(),
            ));
        }
        if self.keyword("match") {
            let value = self.expr()?;
            if !self.eat("{") {
                return Err(self.err("expected `{`"));
            }
            let mut cases = Vec::new();
            while self.eat("|") {
                let template = Template::parse(&self.string()?)?;
                if !self.eat("->") {
                    return Err(self.err("expected `->`"));
                }
                let mut body = vec![self.cond()?];
                while self.eat(",") {
                    body.push(self.cond()?);
                }
                cases.push((template, body));
            }
            if !self.eat("}") {
                return Err(self.err("expected `}`"));
            }
            return Ok(Cond::Match(value, cases));
        }
        let left = self.expr()?;
        let equal = if self.eat("==") {
            true
        } else if self.eat("!=") {
            false
        } else {
            return Err(self.err("expected `==` or `!=`"));
        };
        Ok(Cond::Eq(left, self.expr()?, equal))
    }
}

impl Rule {
    pub fn parse(text: &str) -> Result<Rule, StructuralError> {
        let mut lx = Lexer { s: text, pos: 0 };
        if !lx.keyword("where") {
            return Err(lx.err("expected `where`"));
        }
        let mut conds = vec![lx.cond()?];
        while lx.eat(",") {
            conds.push(lx.cond()?);
        }
        if !lx.done() {
            return Err(lx.err("unexpected text"));
        }
        Ok(Rule { conds })
    }

    /// Text every match contains: the right side of `:[x] == "text"` (or
    /// the left of `"text" == :[x]`) when `x` is a hole of the template and
    /// the text has no holes.
    pub(crate) fn literals(&self) -> Vec<String> {
        let hole = |t: &str| t.starts_with(":[") && t.ends_with(']');
        self.conds
            .iter()
            .filter_map(|c| match c {
                Cond::Eq(Expr::Text(a), Expr::Text(b), true) => match (hole(a), hole(b)) {
                    (true, false) if !b.contains(":[") => Some(b.clone()),
                    (false, true) if !a.contains(":[") => Some(a.clone()),
                    _ => None,
                },
                _ => None,
            })
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.trim().to_owned())
            .collect()
    }

    /// The text the rule requires hole `name` to equal, if it does.
    pub(crate) fn literal_for(&self, name: &str) -> Option<String> {
        let target = format!(":[{name}]");
        let alt = format!(":[[{name}]]");
        let is = |t: &str| t == target || t == alt;
        self.conds
            .iter()
            .find_map(|c| match c {
                Cond::Eq(Expr::Text(a), Expr::Text(b), true) if is(a) && !b.contains(":[") => {
                    Some(b.trim().to_owned())
                }
                Cond::Eq(Expr::Text(a), Expr::Text(b), true) if is(b) && !a.contains(":[") => {
                    Some(a.trim().to_owned())
                }
                _ => None,
            })
            .filter(|t| !t.is_empty())
    }

    /// Whether a match with bindings `env` in `src` satisfies the rule.
    pub(crate) fn holds(&self, src: &[u8], env: &Env) -> bool {
        let lookup = |name: &str| {
            env.iter()
                .find(|(n, _)| n == name)
                .map(|(_, r)| String::from_utf8_lossy(trimmed(src, r)).into_owned())
        };
        all_hold(&self.conds, &lookup)
    }
}

fn all_hold(conds: &[Cond], lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    conds.iter().all(|c| match c {
        Cond::Const(b) => *b,
        Cond::Eq(a, b, equal) => (value(a, lookup) == value(b, lookup)) == *equal,
        Cond::Match(e, cases) => {
            let v = value(e, lookup);
            cases
                .iter()
                .find_map(|(t, body)| {
                    crate::matches_whole(t, v.as_bytes(), &GENERIC).map(|bound| (bound, body))
                })
                .is_some_and(|(bound, body)| {
                    let inner = |name: &str| {
                        bound
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, v)| v.clone())
                            .or_else(|| lookup(name))
                    };
                    all_hold(body, &inner)
                })
        }
    })
}

/// An expression's text, holes replaced by what they matched.
fn value(e: &Expr, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let Expr::Text(t) = e;
    let mut out = String::new();
    let mut rest = t.as_str();
    while let Some(start) = rest.find(":[") {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find(']') else {
            break;
        };
        let name = rest[start + 2..start + len]
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[start..=start + len]),
        }
        rest = &rest[start + len + 1..];
        // `:[[x]]` closes with two brackets.
        rest = rest.strip_prefix(']').unwrap_or(rest);
    }
    out.push_str(rest);
    out
}
