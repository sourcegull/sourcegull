//! The parser: Go `text/template/parse`'s grammar (actions, pipelines, `if`/`else if`/
//! `range`/`with`/`break`/`continue`, variables and their scopes). Function names are
//! checked separately ([`crate::Template::check_functions`]) because the context decides
//! which names exist.

use crate::ast::{Command, Control, Node, Operand, Pipe};
use crate::error::{ErrorKind, RawError};
use crate::lex::{Keyword, Tok, Token};
use crate::value::Value;

/// Deeper nesting (parentheses, control structures) is rejected rather than risking the
/// stack.
const MAX_DEPTH: usize = 100;

pub(crate) struct Parser<'a> {
    toks: &'a [Token],
    i: usize,
    vars: Vec<String>,
    range_depth: usize,
    depth: usize,
}

type Result<T> = std::result::Result<T, RawError>;

/// What ends an item list.
enum Ending {
    End,
    Else { pos: usize },
    Eof,
}

pub(crate) fn parse(toks: &[Token]) -> Result<Vec<Node>> {
    let mut p = Parser {
        toks,
        i: 0,
        vars: vec!["$".to_owned()],
        range_depth: 0,
        depth: 0,
    };
    let (nodes, ending) = p.item_list(true)?;
    match ending {
        Ending::Eof => Ok(nodes),
        Ending::End => Err(RawError::syntax(p.prev_pos(), "unexpected {{end}}")),
        Ending::Else { pos } => Err(RawError::syntax(pos, "unexpected {{else}}")),
    }
}

impl Parser<'_> {
    fn next(&mut self) -> &Token {
        let t = &self.toks[self.i.min(self.toks.len() - 1)];
        if self.i < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn peek(&self) -> &Token {
        &self.toks[self.i.min(self.toks.len() - 1)]
    }

    fn backup(&mut self) {
        self.i = self.i.saturating_sub(1);
    }

    fn next_non_space(&mut self) -> Token {
        loop {
            let t = self.next().clone();
            if t.tok != Tok::Space {
                return t;
            }
        }
    }

    fn peek_non_space(&mut self) -> Token {
        let t = self.next_non_space();
        self.backup();
        t
    }

    fn prev_pos(&self) -> usize {
        self.toks[self.i.saturating_sub(1).min(self.toks.len() - 1)].pos
    }

    fn unexpected(&self, t: &Token, context: &str) -> RawError {
        let msg = match &t.tok {
            Tok::Eof => format!("unexpected EOF in {context}"),
            Tok::RightDelim => format!("unexpected \"}}}}\" in {context}"),
            other => format!("unexpected {} in {context}", other.describe()),
        };
        RawError::syntax(t.pos, msg)
    }

    fn enter(&mut self, pos: usize) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(RawError {
                kind: ErrorKind::Limit,
                pos,
                message: "template nesting too deep".to_owned(),
            });
        }
        Ok(())
    }

    /// Parses nodes up to `{{end}}`, `{{else}}` or the end of input.
    fn item_list(&mut self, root: bool) -> Result<(Vec<Node>, Ending)> {
        let mut nodes = Vec::new();
        loop {
            let t = self.next().clone();
            match t.tok {
                Tok::Eof => {
                    if !root {
                        return Err(RawError::syntax(t.pos, "unexpected EOF"));
                    }
                    return Ok((nodes, Ending::Eof));
                }
                Tok::Text(text) => nodes.push(Node::Text { pos: t.pos, text }),
                Tok::LeftDelim => {
                    let start = t.pos;
                    let k = self.peek_non_space();
                    match k.tok {
                        Tok::Keyword(Keyword::End) => {
                            self.next_non_space();
                            self.expect_right_delim("end")?;
                            return Ok((nodes, Ending::End));
                        }
                        Tok::Keyword(Keyword::Else) => {
                            self.next_non_space();
                            // `else if` / `else with`: the keyword stays for the caller.
                            let after = self.peek_non_space();
                            if !matches!(
                                after.tok,
                                Tok::Keyword(Keyword::If) | Tok::Keyword(Keyword::With)
                            ) {
                                self.expect_right_delim("else")?;
                            }
                            return Ok((nodes, Ending::Else { pos: start }));
                        }
                        _ => nodes.push(self.action(start)?),
                    }
                }
                _ => return Err(self.unexpected(&t, "input")),
            }
        }
    }

    fn expect_right_delim(&mut self, context: &str) -> Result<()> {
        let t = self.next_non_space();
        if t.tok != Tok::RightDelim {
            return Err(self.unexpected(&t, context));
        }
        Ok(())
    }

    /// After a left delimiter.
    fn action(&mut self, start: usize) -> Result<Node> {
        let t = self.peek_non_space();
        if let Tok::Keyword(k) = t.tok {
            self.next_non_space();
            return match k {
                Keyword::If => Ok(Node::If(self.control("if", start)?)),
                Keyword::Range => Ok(Node::Range(self.control("range", start)?)),
                Keyword::With => Ok(Node::With(self.control("with", start)?)),
                Keyword::Break | Keyword::Continue => {
                    if self.range_depth == 0 {
                        return Err(RawError::syntax(
                            t.pos,
                            format!("{{{{{}}}}} outside {{{{range}}}}", k.as_str()),
                        ));
                    }
                    self.expect_right_delim(k.as_str())?;
                    Ok(if k == Keyword::Break {
                        Node::Break { pos: start }
                    } else {
                        Node::Continue { pos: start }
                    })
                }
                Keyword::Define | Keyword::Template | Keyword::Block => Err(RawError {
                    kind: ErrorKind::Unsupported(k.as_str().to_owned()),
                    pos: t.pos,
                    message: format!(
                        "{{{{{}}}}} actions are not supported in bulk change spec templates",
                        k.as_str()
                    ),
                }),
                Keyword::End | Keyword::Else => Err(self.unexpected(&t, "command")),
            };
        }
        let pipe = self.pipeline("command", false)?;
        Ok(Node::Action { pos: start, pipe })
    }

    fn control(&mut self, context: &'static str, start: usize) -> Result<Control> {
        self.enter(start)?;
        let saved_vars = self.vars.len();
        let pipe = self.pipeline(context, false)?;
        if context == "range" {
            self.range_depth += 1;
        }
        let listed = self.item_list(false);
        if context == "range" {
            self.range_depth -= 1;
        }
        let (list, ending) = listed?;
        let else_list = match ending {
            Ending::End => None,
            Ending::Eof => return Err(RawError::syntax(start, "unexpected EOF")),
            Ending::Else { pos } => {
                let t = self.peek_non_space();
                let chained = match (context, &t.tok) {
                    ("if", Tok::Keyword(Keyword::If)) => Some("if"),
                    ("with", Tok::Keyword(Keyword::With)) => Some("with"),
                    _ => None,
                };
                if let Some(kw) = chained {
                    // `{{else if …}}` is an `if` inside the else branch sharing one `{{end}}`.
                    self.next_non_space();
                    let inner = self.control(kw, pos)?;
                    Some(vec![if kw == "if" {
                        Node::If(inner)
                    } else {
                        Node::With(inner)
                    }])
                } else {
                    if matches!(t.tok, Tok::Keyword(Keyword::If | Keyword::With)) {
                        return Err(self.unexpected(&t, context));
                    }
                    let (list, ending) = self.item_list(false)?;
                    match ending {
                        Ending::End => {}
                        Ending::Else { pos } => {
                            return Err(RawError::syntax(pos, "expected end; found {{else}}"));
                        }
                        Ending::Eof => return Err(RawError::syntax(start, "unexpected EOF")),
                    }
                    Some(list)
                }
            }
        };
        self.vars.truncate(saved_vars);
        self.depth -= 1;
        Ok(Control {
            pos: start,
            pipe,
            list,
            else_list,
        })
    }

    /// A pipeline up to a right delimiter (or a right paren when `paren`).
    fn pipeline(&mut self, context: &str, paren: bool) -> Result<Pipe> {
        let start = self.peek_non_space().pos;
        let mut pipe = Pipe {
            pos: start,
            is_assign: false,
            decl: Vec::new(),
            cmds: Vec::new(),
        };
        // Declarations.
        loop {
            let v = self.peek_non_space();
            let Tok::Variable(name) = &v.tok else { break };
            let before = self.i;
            self.next_non_space();
            let next = self.peek_non_space();
            match next.tok {
                Tok::Assign | Tok::Declare => {
                    pipe.is_assign = next.tok == Tok::Assign;
                    self.next_non_space();
                    pipe.decl.push((v.pos, name.clone()));
                    self.vars.push(name.clone());
                    break;
                }
                Tok::Char(',') => {
                    self.next_non_space();
                    pipe.decl.push((v.pos, name.clone()));
                    self.vars.push(name.clone());
                    if context == "range" && pipe.decl.len() < 2 {
                        let t = self.peek_non_space();
                        if matches!(t.tok, Tok::Variable(_) | Tok::RightDelim | Tok::RightParen) {
                            continue;
                        }
                        return Err(RawError::syntax(
                            t.pos,
                            "range can only initialize variables",
                        ));
                    }
                    return Err(RawError::syntax(
                        next.pos,
                        format!("too many declarations in {context}"),
                    ));
                }
                _ => {
                    self.i = before;
                    break;
                }
            }
        }
        let end = if paren {
            Tok::RightParen
        } else {
            Tok::RightDelim
        };
        loop {
            let t = self.next_non_space();
            if t.tok == end {
                self.check_pipeline(&pipe, context)?;
                return Ok(pipe);
            }
            match t.tok {
                Tok::Bool(_)
                | Tok::CharConst { .. }
                | Tok::Dot
                | Tok::Field(_)
                | Tok::Ident(_)
                | Tok::Number(_)
                | Tok::Nil
                | Tok::Str { .. }
                | Tok::Variable(_)
                | Tok::LeftParen => {
                    self.backup();
                    let cmd = self.command()?;
                    pipe.cmds.push(cmd);
                }
                _ => return Err(self.unexpected(&t, context)),
            }
        }
    }

    fn check_pipeline(&self, pipe: &Pipe, context: &str) -> Result<()> {
        if pipe.cmds.is_empty() {
            return Err(RawError::syntax(
                pipe.pos,
                format!("missing value for {context}"),
            ));
        }
        for (i, c) in pipe.cmds.iter().enumerate().skip(1) {
            if matches!(
                c.args[0],
                Operand::Bool { .. }
                    | Operand::Dot { .. }
                    | Operand::Nil { .. }
                    | Operand::Number { .. }
                    | Operand::String { .. }
            ) {
                return Err(RawError::syntax(
                    c.pos,
                    format!("non executable command in pipeline stage {}", i + 1),
                ));
            }
        }
        Ok(())
    }

    fn command(&mut self) -> Result<Command> {
        let pos = self.peek_non_space().pos;
        let mut cmd = Command {
            pos,
            args: Vec::new(),
        };
        loop {
            self.peek_non_space();
            if let Some(op) = self.operand()? {
                cmd.args.push(op);
            }
            let t = self.next().clone();
            match t.tok {
                Tok::Space => continue,
                Tok::RightDelim | Tok::RightParen => self.backup(),
                Tok::Pipe => {}
                _ => return Err(self.unexpected(&t, "operand")),
            }
            break;
        }
        if cmd.args.is_empty() {
            return Err(RawError::syntax(pos, "empty command"));
        }
        Ok(cmd)
    }

    fn operand(&mut self) -> Result<Option<Operand>> {
        let Some(node) = self.term()? else {
            return Ok(None);
        };
        let mut fields = Vec::new();
        while let Tok::Field(f) = &self.peek().tok {
            fields.push(f.clone());
            self.next();
        }
        if fields.is_empty() {
            return Ok(Some(node));
        }
        Ok(Some(match node {
            Operand::Field { pos, mut names } => {
                names.extend(fields);
                Operand::Field { pos, names }
            }
            Operand::Variable { pos, mut names } => {
                names.extend(fields);
                Operand::Variable { pos, names }
            }
            Operand::Bool { .. }
            | Operand::String { .. }
            | Operand::Number { .. }
            | Operand::Nil { .. }
            | Operand::Dot { .. } => {
                return Err(RawError::syntax(
                    node.pos(),
                    format!("unexpected . after term {:?}", node.to_string()),
                ));
            }
            other => Operand::Chain {
                pos: other.pos(),
                node: Box::new(other),
                fields,
            },
        }))
    }

    fn term(&mut self) -> Result<Option<Operand>> {
        let t = self.next_non_space();
        let pos = t.pos;
        Ok(Some(match t.tok {
            Tok::Ident(name) => Operand::Identifier { pos, name },
            Tok::Dot => Operand::Dot { pos },
            Tok::Nil => Operand::Nil { pos },
            Tok::Variable(name) => {
                if !self.vars.contains(&name) {
                    return Err(RawError::syntax(
                        pos,
                        format!("undefined variable {name:?}"),
                    ));
                }
                Operand::Variable {
                    pos,
                    names: vec![name],
                }
            }
            Tok::Field(name) => Operand::Field {
                pos,
                names: vec![name],
            },
            Tok::Bool(value) => Operand::Bool { pos, value },
            Tok::CharConst { quoted, value } => Operand::Number {
                pos,
                text: quoted,
                value: Value::Int(value as i64),
            },
            Tok::Number(text) => {
                let value = number_value(&text).map_err(|(kind, msg)| RawError {
                    kind,
                    pos,
                    message: msg,
                })?;
                Operand::Number { pos, text, value }
            }
            Tok::LeftParen => {
                self.enter(pos)?;
                let pipe = self.pipeline("parenthesized pipeline", true)?;
                self.depth -= 1;
                Operand::Pipe {
                    pos,
                    pipe: Box::new(pipe),
                }
            }
            Tok::Str { quoted, value } => Operand::String { pos, quoted, value },
            _ => {
                self.backup();
                return Ok(None);
            }
        }))
    }
}

/// The value of a number literal, following Go's rules: integers (with `0x`, `0o`, `0b`
/// and leading-`0` octal prefixes and `_` separators) are `int`; anything with a `.` or an
/// exponent is `float64`.
fn number_value(text: &str) -> std::result::Result<Value, (ErrorKind, String)> {
    let illegal = || {
        (
            ErrorKind::Syntax,
            format!("illegal number syntax: {text:?}"),
        )
    };
    if text.ends_with('i') {
        return Err((
            ErrorKind::Unsupported("complex".to_owned()),
            format!("complex number {text} is not supported"),
        ));
    }
    let (neg, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return Err(illegal());
    }
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d)
    } else if lower.len() > 1
        && lower.starts_with('0')
        && lower.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    let digits = digits.trim_start_matches('_').replace('_', "");
    if !digits.is_empty() {
        if let Ok(u) = u64::from_str_radix(&digits, radix) {
            let i = if neg {
                (u <= i64::MAX as u64 + 1).then(|| (u as i64).wrapping_neg())
            } else {
                i64::try_from(u).ok()
            };
            return i
                .map(Value::Int)
                .ok_or_else(|| (ErrorKind::Exec, format!("{text} overflows int")));
        }
    }
    if radix == 16 {
        if lower.contains('p') || lower.contains('.') {
            return Err((
                ErrorKind::Unsupported("hexadecimal float".to_owned()),
                format!("hexadecimal floating-point number {text} is not supported"),
            ));
        }
        return Err(illegal());
    }
    if radix != 10 && radix != 8 {
        return Err(illegal());
    }
    let cleaned = body.replace('_', "");
    if cleaned.contains(['.', 'e', 'E'])
        && cleaned
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        if let Ok(f) = cleaned.parse::<f64>() {
            if f.is_finite() {
                return Ok(Value::Float(if neg { -f } else { f }));
            }
        }
    }
    Err(illegal())
}

#[cfg(test)]
mod tests {
    use super::number_value;
    use crate::value::Value;

    #[test]
    fn numbers() {
        assert_eq!(number_value("42").unwrap(), Value::Int(42));
        assert_eq!(number_value("-0x1F").unwrap(), Value::Int(-31));
        assert_eq!(number_value("0o17").unwrap(), Value::Int(15));
        assert_eq!(number_value("017").unwrap(), Value::Int(15));
        assert_eq!(number_value("0b101").unwrap(), Value::Int(5));
        assert_eq!(number_value("1_000").unwrap(), Value::Int(1000));
        assert_eq!(number_value("1.5").unwrap(), Value::Float(1.5));
        assert_eq!(number_value("1e3").unwrap(), Value::Float(1000.0));
        assert_eq!(number_value(".5").unwrap(), Value::Float(0.5));
        assert!(number_value("1.2.3").is_err());
        assert!(number_value("-").is_err());
        assert!(number_value("2i").is_err());
        assert!(number_value("09").is_err());
    }
}
