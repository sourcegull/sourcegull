//! The lexer: Go `text/template`'s lexing rules with the delimiters `${{` and `}}`,
//! including trim markers (`${{- ` and ` -}}`) and comments (`${{/* … */}}`).

pub(crate) const LEFT_DELIM: &str = "${{";
pub(crate) const RIGHT_DELIM: &str = "}}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keyword {
    If,
    Else,
    End,
    Range,
    With,
    Break,
    Continue,
    Define,
    Template,
    Block,
}

impl Keyword {
    fn from_word(w: &str) -> Option<Keyword> {
        Some(match w {
            "if" => Keyword::If,
            "else" => Keyword::Else,
            "end" => Keyword::End,
            "range" => Keyword::Range,
            "with" => Keyword::With,
            "break" => Keyword::Break,
            "continue" => Keyword::Continue,
            "define" => Keyword::Define,
            "template" => Keyword::Template,
            "block" => Keyword::Block,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Keyword::If => "if",
            Keyword::Else => "else",
            Keyword::End => "end",
            Keyword::Range => "range",
            Keyword::With => "with",
            Keyword::Break => "break",
            Keyword::Continue => "continue",
            Keyword::Define => "define",
            Keyword::Template => "template",
            Keyword::Block => "block",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Text(String),
    LeftDelim,
    RightDelim,
    Space,
    Pipe,
    LeftParen,
    RightParen,
    Declare,
    Assign,
    /// Any other printable ASCII character (`,` in `range $i, $v := …`).
    Char(char),
    /// A quoted or raw string: the source text with quotes, and the value.
    Str {
        quoted: String,
        value: String,
    },
    CharConst {
        quoted: String,
        value: char,
    },
    Number(String),
    Bool(bool),
    Nil,
    Dot,
    /// `.name` (without the dot).
    Field(String),
    /// `$name` (with the `$`; `$` alone is the root variable).
    Variable(String),
    Ident(String),
    Keyword(Keyword),
    Eof,
}

impl Tok {
    /// How the parser names the token in "unexpected …" errors.
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Text(t) => format!("{t:?}"),
            Tok::LeftDelim => format!("\"{LEFT_DELIM}\""),
            Tok::RightDelim => format!("\"{RIGHT_DELIM}\""),
            Tok::Space => "space".to_owned(),
            Tok::Pipe => "\"|\"".to_owned(),
            Tok::LeftParen => "\"(\"".to_owned(),
            Tok::RightParen => "\")\"".to_owned(),
            Tok::Declare => "\":=\"".to_owned(),
            Tok::Assign => "\"=\"".to_owned(),
            Tok::Char(c) => format!("{c:?}"),
            Tok::Str { quoted, .. } | Tok::CharConst { quoted, .. } => quoted.clone(),
            Tok::Number(n) => n.clone(),
            Tok::Bool(b) => b.to_string(),
            Tok::Nil => "nil".to_owned(),
            Tok::Dot => "\".\"".to_owned(),
            Tok::Field(f) => format!(".{f}"),
            Tok::Variable(v) => v.clone(),
            Tok::Ident(i) => i.clone(),
            Tok::Keyword(k) => format!("<{}>", k.as_str()),
            Tok::Eof => "EOF".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub tok: Tok,
    /// Byte offset in the template.
    pub pos: usize,
}

/// A lexing error at a byte offset.
pub(crate) type LexError = (usize, String);

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn is_alnum(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

fn has_left_trim(s: &str) -> bool {
    let mut it = s.chars();
    it.next() == Some('-') && it.next().is_some_and(is_space)
}

/// Whether `s` starts with the right delimiter, and whether that has a trim marker.
fn at_right_delim(s: &str) -> (bool, bool) {
    let mut it = s.chars();
    if let (Some(a), Some('-')) = (it.next(), it.next()) {
        if is_space(a) && s[a.len_utf8() + 1..].starts_with(RIGHT_DELIM) {
            return (true, true);
        }
    }
    (s.starts_with(RIGHT_DELIM), false)
}

struct Lexer<'a> {
    src: &'a str,
    toks: Vec<Token>,
    paren_depth: usize,
}

pub(crate) fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let mut l = Lexer {
        src,
        toks: Vec::new(),
        paren_depth: 0,
    };
    l.run()?;
    Ok(l.toks)
}

impl Lexer<'_> {
    fn emit(&mut self, tok: Tok, pos: usize) {
        self.toks.push(Token { tok, pos });
    }

    fn skip_spaces(&self, mut p: usize) -> usize {
        while let Some(c) = self.src[p..].chars().next() {
            if !is_space(c) {
                break;
            }
            p += c.len_utf8();
        }
        p
    }

    fn run(&mut self) -> Result<(), LexError> {
        let src = self.src;
        let mut pos = 0;
        loop {
            let rest = &src[pos..];
            let Some(i) = rest.find(LEFT_DELIM) else {
                if !rest.is_empty() {
                    self.emit(Tok::Text(rest.to_owned()), pos);
                }
                break;
            };
            let delim = pos + i;
            let after = delim + LEFT_DELIM.len();
            let trim = has_left_trim(&src[after..]);
            let mut text = &src[pos..delim];
            if trim {
                text = text.trim_end_matches(is_space);
            }
            if !text.is_empty() {
                self.emit(Tok::Text(text.to_owned()), pos);
            }
            let p = after + if trim { 2 } else { 0 };
            if src[p..].starts_with("/*") {
                let Some(end) = src[p + 2..].find("*/") else {
                    return Err((delim, "unclosed comment".to_owned()));
                };
                let p = p + 2 + end + 2;
                let (at, trim) = at_right_delim(&src[p..]);
                if !at {
                    return Err((delim, "comment ends before closing delimiter".to_owned()));
                }
                let p = p + if trim { 2 } else { 0 } + RIGHT_DELIM.len();
                pos = if trim { self.skip_spaces(p) } else { p };
                continue;
            }
            self.emit(Tok::LeftDelim, delim);
            pos = self.inside_action(p, delim)?;
        }
        self.emit(Tok::Eof, src.len());
        Ok(())
    }

    /// Lexes one action from `p` up to and including its right delimiter; returns the
    /// offset where the following text starts.
    fn inside_action(&mut self, mut p: usize, start: usize) -> Result<usize, LexError> {
        let src = self.src;
        loop {
            let s = &src[p..];
            let (at, trim) = at_right_delim(s);
            if at {
                if self.paren_depth > 0 {
                    return Err((p, "unclosed left paren".to_owned()));
                }
                let delim_pos = p + if trim { 1 } else { 0 };
                self.emit(Tok::RightDelim, delim_pos);
                let p = p + if trim { 2 } else { 0 } + RIGHT_DELIM.len();
                return Ok(if trim { self.skip_spaces(p) } else { p });
            }
            let Some(c) = s.chars().next() else {
                return Err((start, "unclosed action".to_owned()));
            };
            match c {
                c if is_space(c) => {
                    let mut q = p;
                    let mut n = 0;
                    while let Some(c) = src[q..].chars().next() {
                        if !is_space(c) {
                            break;
                        }
                        q += c.len_utf8();
                        n += 1;
                    }
                    // A space that belongs to a trim-marked closing delimiter (` -}}`).
                    let prev = src[..q].chars().next_back().map_or(1, char::len_utf8);
                    if src[q..].starts_with('-') && src[q + 1..].starts_with(RIGHT_DELIM) {
                        q -= prev;
                        if n == 1 {
                            p = q;
                            continue;
                        }
                    }
                    self.emit(Tok::Space, p);
                    p = q;
                }
                '=' => {
                    self.emit(Tok::Assign, p);
                    p += 1;
                }
                ':' => {
                    if !s[1..].starts_with('=') {
                        return Err((p, "expected :=".to_owned()));
                    }
                    self.emit(Tok::Declare, p);
                    p += 2;
                }
                '|' => {
                    self.emit(Tok::Pipe, p);
                    p += 1;
                }
                '"' => p = self.quoted(p)?,
                '`' => p = self.raw_quoted(p)?,
                '\'' => p = self.char_const(p)?,
                '$' => p = self.field_or_variable(p, '$')?,
                '.' if !s[1..].starts_with(|c: char| c.is_ascii_digit()) => {
                    p = self.field_or_variable(p, '.')?
                }
                '.' | '+' | '-' | '0'..='9' => p = self.number(p)?,
                c if is_alnum(c) => p = self.identifier(p)?,
                '(' => {
                    self.paren_depth += 1;
                    self.emit(Tok::LeftParen, p);
                    p += 1;
                }
                ')' => {
                    if self.paren_depth == 0 {
                        return Err((p, "unexpected right paren".to_owned()));
                    }
                    self.paren_depth -= 1;
                    self.emit(Tok::RightParen, p);
                    p += 1;
                }
                c if c.is_ascii_graphic() => {
                    self.emit(Tok::Char(c), p);
                    p += 1;
                }
                c => {
                    return Err((
                        p,
                        format!("unrecognized character in action: U+{:04X}", c as u32),
                    ));
                }
            }
        }
    }

    fn at_terminator(&self, p: usize) -> bool {
        let s = &self.src[p..];
        match s.chars().next() {
            None => true,
            Some(c) => is_space(c) || matches!(c, '.' | ',' | '|' | ':' | ')' | '(' | '}'),
        }
    }

    fn bad_char(&self, p: usize) -> LexError {
        let c = self.src[p..].chars().next().unwrap_or(' ');
        (p, format!("bad character U+{:04X} '{c}'", c as u32))
    }

    fn identifier(&mut self, p: usize) -> Result<usize, LexError> {
        let src = self.src;
        let end = src[p..]
            .char_indices()
            .find(|&(_, c)| !is_alnum(c))
            .map_or(src.len(), |(i, _)| p + i);
        if !self.at_terminator(end) {
            return Err(self.bad_char(end));
        }
        let word = &src[p..end];
        let tok = match word {
            "true" => Tok::Bool(true),
            "false" => Tok::Bool(false),
            "nil" => Tok::Nil,
            w => match Keyword::from_word(w) {
                Some(k) => Tok::Keyword(k),
                None => Tok::Ident(w.to_owned()),
            },
        };
        self.emit(tok, p);
        Ok(end)
    }

    fn field_or_variable(&mut self, p: usize, marker: char) -> Result<usize, LexError> {
        let src = self.src;
        let start = p + 1;
        if self.at_terminator(start) {
            self.emit(
                if marker == '.' {
                    Tok::Dot
                } else {
                    Tok::Variable("$".to_owned())
                },
                p,
            );
            return Ok(start);
        }
        let end = src[start..]
            .char_indices()
            .find(|&(_, c)| !is_alnum(c))
            .map_or(src.len(), |(i, _)| start + i);
        if end == start || !self.at_terminator(end) {
            return Err(self.bad_char(end));
        }
        let name = &src[start..end];
        self.emit(
            if marker == '.' {
                Tok::Field(name.to_owned())
            } else {
                Tok::Variable(format!("${name}"))
            },
            p,
        );
        Ok(end)
    }

    /// Go's `scanNumber`; the value is checked by the parser.
    fn number(&mut self, p: usize) -> Result<usize, LexError> {
        let b = self.src.as_bytes();
        let mut q = p;
        let accept = |q: &mut usize, set: &[u8]| -> bool {
            if *q < b.len() && set.contains(&b[*q]) {
                *q += 1;
                true
            } else {
                false
            }
        };
        let accept_run = |q: &mut usize, set: &[u8]| {
            while *q < b.len() && set.contains(&b[*q]) {
                *q += 1;
            }
        };
        accept(&mut q, b"+-");
        let mut digits: &[u8] = b"0123456789_";
        if accept(&mut q, b"0") {
            if accept(&mut q, b"xX") {
                digits = b"0123456789abcdefABCDEF_";
            } else if accept(&mut q, b"oO") {
                digits = b"01234567_";
            } else if accept(&mut q, b"bB") {
                digits = b"01_";
            }
        }
        accept_run(&mut q, digits);
        if accept(&mut q, b".") {
            accept_run(&mut q, digits);
        }
        if digits.len() == 11 && accept(&mut q, b"eE") {
            accept(&mut q, b"+-");
            accept_run(&mut q, b"0123456789_");
        }
        if digits.len() == 23 && accept(&mut q, b"pP") {
            accept(&mut q, b"+-");
            accept_run(&mut q, b"0123456789_");
        }
        accept(&mut q, b"i");
        if self.src[q..].chars().next().is_some_and(is_alnum) {
            let end = self.src[q..]
                .char_indices()
                .nth(1)
                .map_or(self.src.len(), |(i, _)| q + i);
            return Err((p, format!("bad number syntax: {:?}", &self.src[p..end])));
        }
        self.emit(Tok::Number(self.src[p..q].to_owned()), p);
        Ok(q)
    }

    fn quoted(&mut self, p: usize) -> Result<usize, LexError> {
        let src = self.src;
        let mut it = src[p + 1..].char_indices();
        loop {
            match it.next() {
                None | Some((_, '\n')) => {
                    return Err((p, "unterminated quoted string".to_owned()));
                }
                Some((_, '\\')) => match it.next() {
                    None | Some((_, '\n')) => {
                        return Err((p, "unterminated quoted string".to_owned()));
                    }
                    _ => {}
                },
                Some((i, '"')) => {
                    let end = p + 1 + i + 1;
                    let quoted = &src[p..end];
                    let value = unquote(&quoted[1..quoted.len() - 1], '"')
                        .map_err(|e| (p, format!("{e} in {quoted}")))?;
                    self.emit(
                        Tok::Str {
                            quoted: quoted.to_owned(),
                            value,
                        },
                        p,
                    );
                    return Ok(end);
                }
                _ => {}
            }
        }
    }

    fn raw_quoted(&mut self, p: usize) -> Result<usize, LexError> {
        let src = self.src;
        let Some(i) = src[p + 1..].find('`') else {
            return Err((p, "unterminated raw quoted string".to_owned()));
        };
        let end = p + 1 + i + 1;
        let quoted = &src[p..end];
        self.emit(
            Tok::Str {
                quoted: quoted.to_owned(),
                value: quoted[1..quoted.len() - 1].replace('\r', ""),
            },
            p,
        );
        Ok(end)
    }

    fn char_const(&mut self, p: usize) -> Result<usize, LexError> {
        let src = self.src;
        let mut it = src[p + 1..].char_indices();
        loop {
            match it.next() {
                None | Some((_, '\n')) => {
                    return Err((p, "unterminated character constant".to_owned()));
                }
                Some((_, '\\')) => match it.next() {
                    None | Some((_, '\n')) => {
                        return Err((p, "unterminated character constant".to_owned()));
                    }
                    _ => {}
                },
                Some((i, '\'')) => {
                    let end = p + 1 + i + 1;
                    let quoted = &src[p..end];
                    let value = unquote(&quoted[1..quoted.len() - 1], '\'')
                        .ok()
                        .and_then(|s| {
                            let mut cs = s.chars();
                            match (cs.next(), cs.next()) {
                                (Some(c), None) => Some(c),
                                _ => None,
                            }
                        })
                        .ok_or_else(|| (p, format!("malformed character constant: {quoted}")))?;
                    self.emit(
                        Tok::CharConst {
                            quoted: quoted.to_owned(),
                            value,
                        },
                        p,
                    );
                    return Ok(end);
                }
                _ => {}
            }
        }
    }
}

/// Go's `strconv.Unquote` for the inside of a `"…"` string or `'…'` character constant.
fn unquote(s: &str, q: char) -> Result<String, &'static str> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut it = s.chars().peekable();
    let mut buf = [0u8; 4];
    while let Some(c) = it.next() {
        if c == q {
            return Err("invalid syntax");
        }
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(e) = it.next() else {
            return Err("invalid syntax");
        };
        let simple = match e {
            'a' => Some(0x07),
            'b' => Some(0x08),
            'f' => Some(0x0c),
            'n' => Some(b'\n'),
            'r' => Some(b'\r'),
            't' => Some(b'\t'),
            'v' => Some(0x0b),
            '\\' => Some(b'\\'),
            '\'' | '"' if e == q => Some(e as u8),
            _ => None,
        };
        if let Some(b) = simple {
            out.push(b);
            continue;
        }
        let hex = |it: &mut std::iter::Peekable<std::str::Chars<'_>>, n: usize| {
            let mut v: u32 = 0;
            for _ in 0..n {
                let d = it
                    .next()
                    .and_then(|c| c.to_digit(16))
                    .ok_or("invalid syntax")?;
                v = v * 16 + d;
            }
            Ok::<u32, &'static str>(v)
        };
        match e {
            'x' => out.push(hex(&mut it, 2)? as u8),
            'u' | 'U' => {
                let v = hex(&mut it, if e == 'u' { 4 } else { 8 })?;
                let c = char::from_u32(v).ok_or("invalid syntax")?;
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            '0'..='7' => {
                let mut v = e.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    let d = it
                        .next()
                        .and_then(|c| c.to_digit(8))
                        .ok_or("invalid syntax")?;
                    v = v * 8 + d;
                }
                if v > 255 {
                    return Err("invalid syntax");
                }
                out.push(v as u8);
            }
            _ => return Err("invalid syntax"),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
