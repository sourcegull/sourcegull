//! Scanner and recursive-descent parser.

use crate::{
    Node, OperatorKind, Parameter, ParseError, Pattern, PatternKind, PatternType, Query, Span,
    canonical_field,
};

/// Parses `input`. A `patterntype:` field in the query overrides `default`.
pub fn parse(input: &str, default: PatternType) -> Result<Query, ParseError> {
    let mut pattern_type = default;
    let tokens = Scanner::new(input, pattern_type, false).scan()?;
    if let Some(pt) = tokens.iter().rev().find_map(|t| match &t.kind {
        TokKind::Param(p) if p.field == "patterntype" => PatternType::from_name(&p.value),
        _ => None,
    }) {
        pattern_type = pt;
    }
    // Structural templates are code: parentheses and `and`/`or`/`not` are
    // part of them, not grouping and operators.
    let structural = pattern_type == PatternType::Structural;
    let tokens = if pattern_type == default && !structural {
        tokens
    } else {
        Scanner::new(input, pattern_type, structural).scan()?
    };
    // A `(` that is never closed, or a missing operand, may come from
    // parentheses meant as text (`if (x`, `foo( or bar(`): parse again with
    // literal parentheses, and report the first error if that fails too. A
    // `)` that closes no group is an error with no second try.
    let keyword = pattern_type == PatternType::Keyword;
    let (root, grouped) = match Parser::new(input, tokens, keyword).parse() {
        Err(e @ (ParseError::UnbalancedParens(_) | ParseError::MissingOperand { .. })) => {
            Parser::new(
                input,
                Scanner::new(input, pattern_type, true).scan()?,
                keyword,
            )
            .parse()
            .map_err(|_| e)?
        }
        result => result?,
    };
    // A query with no grouping parentheses may have its fields lifted out of
    // a top-level AND/OR of patterns.
    let root = match root {
        Some(node) if !grouped => Some(hoist(&node).unwrap_or(node)),
        root => root,
    };
    if matches!(pattern_type, PatternType::Standard | PatternType::Literal)
        && let Some(span) = root.as_ref().and_then(grouped_in_concat)
    {
        return Err(ParseError::UnclearParens(span));
    }
    Ok(Query { pattern_type, root })
}

/// The span of the first concat that holds a parenthesized AND/OR next to
/// another pattern, as in `a (b or c) d`. Standard and literal queries join
/// the terms of a concat into one phrase, so such a query has no clear
/// meaning and is rejected. A group that mixes fields and patterns
/// (`a (repo:x b)`) is an AND too. Fields and groups without patterns do not
/// count: they are separated from the patterns (`repo:x (a or b)` is fine).
fn grouped_in_concat(node: &Node) -> Option<Span> {
    let Node::Operator {
        kind,
        operands,
        span,
    } = node
    else {
        return None;
    };
    if *kind == OperatorKind::Concat {
        let patterns: Vec<&Node> = operands.iter().filter(|n| has_pattern(n)).collect();
        let and_or = |n: &&Node| match n {
            Node::Operator {
                kind: OperatorKind::And | OperatorKind::Or,
                ..
            } => true,
            Node::Operator {
                kind: OperatorKind::Concat,
                operands,
                ..
            } => operands.iter().any(|o| matches!(o, Node::Parameter(_))),
            _ => false,
        };
        if patterns.len() > 1 && patterns.iter().any(and_or) {
            return Some(*span);
        }
    }
    operands.iter().find_map(grouped_in_concat)
}

fn has_pattern(node: &Node) -> bool {
    match node {
        Node::Pattern(_) => true,
        Node::Parameter(_) => false,
        Node::Operator { operands, .. } => operands.iter().any(has_pattern),
    }
}

/// Whether parentheses in `s` are balanced.
pub(crate) fn balanced(s: &str) -> bool {
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return false,
            ')' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

#[derive(Debug)]
enum TokKind {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Param(Parameter),
    Pattern(Pattern),
}

#[derive(Debug)]
struct Tok {
    kind: TokKind,
    span: Span,
}

struct Scanner<'a> {
    input: &'a str,
    pos: usize,
    pattern_type: PatternType,
    /// Parentheses are ordinary characters (fallback for unbalanced input).
    literal_parens: bool,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a str, pattern_type: PatternType, literal_parens: bool) -> Self {
        Scanner {
            input,
            pos: 0,
            pattern_type,
            literal_parens,
        }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn scan(mut self) -> Result<Vec<Tok>, ParseError> {
        let mut tokens = Vec::new();
        loop {
            let trimmed = self.rest().trim_start();
            self.pos = self.input.len() - trimmed.len();
            if trimmed.is_empty() {
                return Ok(tokens);
            }
            tokens.push(self.next_token()?);
        }
    }

    fn next_token(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        let rest = self.rest();
        if !self.literal_parens {
            // Outside keyword queries, balanced parentheses that hold no
            // field or operator are pattern text: `copyright (c) 2005`,
            // `(\w+)Reader`, `(a b) c`, `()foo()`.
            if rest.starts_with('(')
                && self.pattern_type != PatternType::Keyword
                && let Some(len) = balanced_pattern_len(rest)
            {
                self.pos += len;
                return Ok(self.pattern(rest[..len].into(), PatternKind::Literal, start));
            }
            if rest.starts_with("()") {
                self.pos += 2;
                return Ok(self.pattern("()".into(), PatternKind::Literal, start));
            }
            if rest.starts_with('(') {
                self.pos += 1;
                return Ok(Tok {
                    kind: TokKind::LParen,
                    span: Span {
                        start,
                        end: self.pos,
                    },
                });
            }
            if rest.starts_with(')') {
                self.pos += 1;
                return Ok(Tok {
                    kind: TokKind::RParen,
                    span: Span {
                        start,
                        end: self.pos,
                    },
                });
            }
        }
        let structural = self.pattern_type == PatternType::Structural;
        if !structural && let Some((kind, len)) = self.keyword(rest) {
            self.pos += len;
            return Ok(Tok {
                kind,
                span: Span {
                    start,
                    end: self.pos,
                },
            });
        }
        if let Some(tok) = self.field()? {
            return Ok(tok);
        }
        if let Some(tok) = self.delimited_pattern(rest) {
            return Ok(tok);
        }
        // A word may hold balanced parentheses with whitespace inside
        // (`func(a, b)`). Otherwise it ends at whitespace; in regexp
        // queries also at a parenthesis (`foo(a or b)` is `foo` and a group).
        let len = if self.literal_parens || structural {
            whitespace_len(rest)
        } else if let Some(len) = balanced_pattern_len(rest) {
            len
        } else if self.pattern_type == PatternType::Regexp {
            regexp_word_len(rest)
        } else {
            whitespace_len(rest)
        };
        self.pos += len;
        Ok(self.pattern(rest[..len].into(), PatternKind::Literal, start))
    }

    /// `and`/`or`/`not` at the start of `rest`, where they are operators:
    /// `and` and `or` after whitespace or `)` (not as the first term) and
    /// before whitespace or `(`, `not` at the start, or after whitespace or
    /// a parenthesis, and before whitespace. Elsewhere they are patterns
    /// (`and b`, `a or`, `(a and)`).
    fn keyword(&self, rest: &str) -> Option<(TokKind, usize)> {
        let before = &self.input[..self.pos];
        let prev = before.chars().next_back();
        let is = |k: &str| {
            rest.get(..k.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(k))
        };
        let next = |k: &str| rest[k.len()..].chars().next();
        let space = |c: Option<char>| c.is_some_and(char::is_whitespace);
        if !before.trim().is_empty() && (space(prev) || prev == Some(')')) {
            for (k, kind) in [("and", TokKind::And), ("or", TokKind::Or)] {
                if is(k) && (space(next(k)) || next(k) == Some('(')) {
                    return Some((kind, k.len()));
                }
            }
        }
        let unary_prev = prev.is_none() || space(prev) || matches!(prev, Some('(' | ')'));
        (unary_prev && is("not") && space(next("not"))).then_some((TokKind::Not, 3))
    }

    /// A pattern between delimiters: `/regexp/` in keyword, standard and
    /// regexp queries, and a `"quoted"` or `'quoted'` literal in keyword and
    /// regexp queries. The closing delimiter must be followed by whitespace,
    /// `)` or the end; otherwise, or when it is missing, the delimiters are
    /// pattern text (`"foo"bar`, `"foo`). Empty delimiters are their own text: `//`, `""` and
    /// `''` are literals.
    fn delimited_pattern(&mut self, rest: &str) -> Option<Tok> {
        let delim = rest.chars().next()?;
        let allowed = match delim {
            '/' => matches!(
                self.pattern_type,
                PatternType::Keyword | PatternType::Standard | PatternType::Regexp
            ),
            '"' | '\'' => matches!(
                self.pattern_type,
                PatternType::Keyword | PatternType::Regexp
            ),
            _ => false,
        };
        if !allowed {
            return None;
        }
        let (value, len) = scan_delimited(rest)?;
        if rest[len..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && c != ')')
        {
            return None;
        }
        let start = self.pos;
        self.pos += len;
        Some(match delim {
            _ if value.is_empty() => {
                self.pattern(format!("{delim}{delim}"), PatternKind::Literal, start)
            }
            '/' => self.pattern(value, PatternKind::Regexp, start),
            _ => self.pattern(value, PatternKind::Quoted, start),
        })
    }

    fn pattern(&self, value: String, kind: PatternKind, start: usize) -> Tok {
        let span = Span {
            start,
            end: self.pos,
        };
        Tok {
            kind: TokKind::Pattern(Pattern {
                value,
                kind,
                negated: false,
                span,
            }),
            span,
        }
    }

    /// `[-]field:value` for a known field.
    fn field(&mut self) -> Result<Option<Tok>, ParseError> {
        let start = self.pos;
        let rest = self.rest();
        let negated = rest.starts_with('-');
        let name_start = usize::from(negated);
        let name_len = rest[name_start..]
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len() - name_start);
        let colon = name_start + name_len;
        if name_len == 0 || !rest[colon..].starts_with(':') {
            return Ok(None);
        }
        let Some(field) = canonical_field(&rest[name_start..colon]) else {
            return Ok(None);
        };
        let value_src = &rest[colon + 1..];
        let (value, quoted, len) = if value_src.starts_with(['"', '\'']) {
            let (value, len) =
                scan_quoted(value_src).ok_or(ParseError::UnterminatedQuote(Span {
                    start: start + colon + 1,
                    end: self.input.len(),
                }))?;
            (value, true, len)
        } else {
            let len = predicate_len(field, value_src).unwrap_or_else(|| self.word_len(value_src));
            (value_src[..len].to_string(), false, len)
        };
        self.pos += colon + 1 + len;
        let span = Span {
            start,
            end: self.pos,
        };
        Ok(Some(Tok {
            kind: TokKind::Param(Parameter {
                field,
                value,
                negated,
                quoted,
                span,
            }),
            span,
        }))
    }

    /// Length of a field value: up to whitespace, or an unmatched `)` unless
    /// parentheses are literal. Balanced parentheses inside the value are
    /// kept (`file:\.(ts|js)$`).
    fn word_len(&self, s: &str) -> usize {
        let mut depth = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                c if c.is_whitespace() => return i,
                _ if self.literal_parens => {}
                '(' => depth += 1,
                ')' if depth == 0 => return i,
                ')' => depth -= 1,
                _ => {}
            }
        }
        s.len()
    }
}

/// Length of a pattern with balanced parentheses at the start of `s`, which
/// may hold whitespace inside the parentheses: `(c)`, `func(a, b)`,
/// `(a b)`. It ends at whitespace outside parentheses or at an unmatched `)`.
/// `None` when the parentheses do not balance, or when a field or an
/// `and`/`or`/`not` keyword starts the input, follows a `(` or follows
/// whitespace inside the parentheses: those parentheses are grouping
/// (`(repo:x foo)`, `(a or b)`). A backslash escapes the next character.
fn balanced_pattern_len(s: &str) -> Option<usize> {
    // Whether the pattern may go on at `rest`.
    let ok = |rest: &str| {
        let keyword = ["and ", "or ", "not "].iter().any(|k| {
            rest.get(..k.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(k))
        });
        !keyword && !starts_with_field(rest)
    };
    if !ok(s) {
        return None;
    }
    let mut depth = 0usize;
    let mut end = s.len();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        let after = &s[i + c.len_utf8()..];
        match c {
            c if c.is_whitespace() && depth == 0 => {
                end = i;
                break;
            }
            c if c.is_whitespace() => {
                if !ok(after) {
                    return None;
                }
            }
            '(' => {
                if !ok(after) {
                    return None;
                }
                depth += 1;
            }
            ')' if depth == 0 => {
                end = i;
                break;
            }
            ')' => depth -= 1,
            '\\' => {
                chars.next();
            }
            _ => {}
        }
    }
    (depth == 0 && end > 0).then_some(end)
}

/// Length of the text up to whitespace.
fn whitespace_len(s: &str) -> usize {
    s.find(char::is_whitespace).unwrap_or(s.len())
}

/// Length of a regexp query's word: up to whitespace or a parenthesis that
/// is not escaped.
fn regexp_word_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            c if c.is_whitespace() || c == '(' || c == ')' => return i,
            _ => {}
        }
    }
    s.len()
}

/// Whether `s` starts with `[-]field:` for a known field.
fn starts_with_field(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let len = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    len > 0 && s[len..].starts_with(':') && canonical_field(&s[..len]).is_some()
}

/// Length of a predicate call at the start of `s` (`has.file(path:a content:b)`): its
/// name, then balanced parentheses that may hold whitespace and quoted strings. `None`
/// when `s` does not start with a predicate of `field` or its parentheses do not close.
fn predicate_len(field: &str, s: &str) -> Option<usize> {
    let open = s.find('(')?;
    let name = &s[..open];
    if name.is_empty() || !crate::predicate::is_predicate(field, name) {
        return None;
    }
    let mut depth = 0usize;
    let mut chars = s[open..].char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' | '\'' => {
                // Skip to the closing quote.
                loop {
                    match chars.next()? {
                        (_, '\\') => {
                            chars.next();
                        }
                        (_, q) if q == c => break,
                        _ => {}
                    }
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// A string delimited by its first character (`"` or `'`). The delimiter and
/// backslash can be escaped with a backslash; other escapes are kept as is.
/// Returns the unescaped value and the length consumed.
fn scan_quoted(s: &str) -> Option<(String, usize)> {
    let delim = s.chars().next()?;
    let mut value = String::new();
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((_, e)) if e == delim || e == '\\' => value.push(e),
                Some((_, e)) => {
                    value.push('\\');
                    value.push(e);
                }
                None => return None,
            },
            c if c == delim => return Some((value, i + c.len_utf8())),
            c => value.push(c),
        }
    }
    None
}

/// A string between delimiters, its first character (`"`, `'` or `/`).
/// A backslash escapes the delimiter; `\n`, `\r` and `\t` are a newline, a
/// carriage return and a tab; in quotes `\\` is a backslash. Any other escape,
/// and `\\` in a regexp, is kept as is (`\d`). Returns the value and the
/// length consumed, or `None` when the delimiter is not closed.
fn scan_delimited(s: &str) -> Option<(String, usize)> {
    let delim = s.chars().next()?;
    let mut value = String::new();
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next()?.1 {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                e if e == delim || (e == '\\' && delim != '/') => value.push(e),
                e => {
                    value.push('\\');
                    value.push(e);
                }
            },
            c if c == delim => return Some((value, i + c.len_utf8())),
            c => value.push(c),
        }
    }
    None
}

/// A `)` that closes no group.
fn unsupported(keyword: bool, span: Span) -> ParseError {
    ParseError::UnsupportedParens {
        span,
        quotes: keyword,
    }
}

struct Parser<'a> {
    input: &'a str,
    tokens: std::iter::Peekable<std::vec::IntoIter<Tok>>,
    /// Open groups.
    depth: usize,
    /// Keyword query (picks the hint of [`ParseError::UnsupportedParens`]).
    keyword: bool,
    /// Parentheses grouped some terms.
    grouped: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, tokens: Vec<Tok>, keyword: bool) -> Self {
        Parser {
            input,
            tokens: tokens.into_iter().peekable(),
            depth: 0,
            keyword,
            grouped: false,
        }
    }

    /// The tree, and whether parentheses grouped any terms.
    fn parse(mut self) -> Result<(Option<Node>, bool), ParseError> {
        if self.tokens.peek().is_none() {
            return Ok((None, false));
        }
        let node = self.or()?;
        match self.tokens.next() {
            None => Ok((Some(node), self.grouped)),
            // Only a `)` that closes no group is left.
            Some(tok) => Err(unsupported(self.keyword, tok.span)),
        }
    }

    fn or(&mut self) -> Result<Node, ParseError> {
        self.binary(OperatorKind::Or, |p| p.and())
    }

    fn and(&mut self) -> Result<Node, ParseError> {
        self.binary(OperatorKind::And, |p| p.concat())
    }

    fn binary(
        &mut self,
        kind: OperatorKind,
        mut operand: impl FnMut(&mut Self) -> Result<Node, ParseError>,
    ) -> Result<Node, ParseError> {
        let mut operands = vec![operand(self)?];
        while let Some(tok) = self.tokens.next_if(|t| match kind {
            OperatorKind::Or => matches!(t.kind, TokKind::Or),
            _ => matches!(t.kind, TokKind::And),
        }) {
            match self.tokens.peek() {
                Some(Tok {
                    kind: TokKind::RParen,
                    span,
                }) if self.depth == 0 => return Err(unsupported(self.keyword, *span)),
                // `(a or )`: the empty operand is the pattern `()`.
                Some(Tok {
                    kind: TokKind::RParen,
                    ..
                }) => {
                    operands.push(Node::Pattern(Pattern {
                        value: "()".into(),
                        kind: PatternKind::Literal,
                        negated: false,
                        span: tok.span,
                    }));
                    continue;
                }
                None => {
                    let op = if kind == OperatorKind::Or {
                        "OR"
                    } else {
                        "AND"
                    };
                    return Err(ParseError::MissingOperand {
                        op,
                        side: "right",
                        span: tok.span,
                    });
                }
                Some(_) => {}
            }
            operands.push(operand(self)?);
        }
        Ok(operator(kind, operands))
    }

    fn concat(&mut self) -> Result<Node, ParseError> {
        let mut operands = Vec::new();
        loop {
            match self.tokens.peek() {
                Some(Tok {
                    kind: TokKind::LParen | TokKind::Not | TokKind::Param(_) | TokKind::Pattern(_),
                    ..
                }) => operands.push(self.unary()?),
                Some(Tok {
                    kind: kind @ (TokKind::And | TokKind::Or),
                    span,
                }) if operands.is_empty() => {
                    let op = if matches!(kind, TokKind::Or) {
                        "OR"
                    } else {
                        "AND"
                    };
                    return Err(ParseError::MissingOperand {
                        op,
                        side: "left",
                        span: *span,
                    });
                }
                // A `)` that closes no group.
                Some(Tok { span, .. }) if operands.is_empty() => {
                    return Err(unsupported(self.keyword, *span));
                }
                _ => return Ok(operator(OperatorKind::Concat, operands)),
            }
        }
    }

    fn unary(&mut self) -> Result<Node, ParseError> {
        let tok = self.tokens.next().expect("peeked");
        match tok.kind {
            TokKind::Not => match self.tokens.next() {
                // `NOT (a b)`, even where `(a b)` is pattern text.
                Some(Tok {
                    kind: TokKind::LParen,
                    ..
                }) => Err(ParseError::NegatedExpression(tok.span)),
                Some(Tok {
                    kind: TokKind::Pattern(p),
                    ..
                }) if p.kind == PatternKind::Literal && p.value.starts_with('(') => {
                    Err(ParseError::NegatedExpression(tok.span))
                }
                // `NOT -file:x` is an error, not a double negation.
                Some(Tok {
                    kind: TokKind::Param(p),
                    span,
                }) if p.negated => Err(ParseError::NegatedParameter {
                    text: self.input[span.start..span.end].to_string(),
                    span: tok.span.join(span),
                }),
                Some(Tok {
                    kind: TokKind::Param(mut p),
                    ..
                }) => {
                    p.negated = true;
                    p.span = p.span.join(tok.span);
                    Ok(Node::Parameter(p))
                }
                Some(Tok {
                    kind: TokKind::Pattern(mut p),
                    ..
                }) => {
                    p.negated = true;
                    p.span = p.span.join(tok.span);
                    Ok(Node::Pattern(p))
                }
                _ => Err(ParseError::MissingOperand {
                    op: "NOT",
                    side: "right",
                    span: tok.span,
                }),
            },
            TokKind::LParen => {
                self.grouped = true;
                // An empty group, `(  )`, is the pattern `()`.
                if let Some(close) = self.tokens.next_if(|t| matches!(t.kind, TokKind::RParen)) {
                    let span = tok.span.join(close.span);
                    return Ok(Node::Pattern(Pattern {
                        value: "()".into(),
                        kind: PatternKind::Literal,
                        negated: false,
                        span,
                    }));
                }
                self.depth += 1;
                let node = self.or()?;
                self.depth -= 1;
                match self.tokens.next() {
                    Some(Tok {
                        kind: TokKind::RParen,
                        span,
                    }) => Ok(match node {
                        Node::Operator { kind, operands, .. } => Node::Operator {
                            kind,
                            operands,
                            span: tok.span.join(span),
                        },
                        leaf => leaf,
                    }),
                    _ => Err(ParseError::UnbalancedParens(tok.span)),
                }
            }
            TokKind::Param(p) => Ok(Node::Parameter(p)),
            TokKind::Pattern(p) => Ok(Node::Pattern(p)),
            TokKind::And | TokKind::Or | TokKind::RParen => unreachable!("not an atom start"),
        }
    }
}

/// Builds an operator node, collapsing single operands and flattening nested
/// AND/OR of the same kind.
fn operator(kind: OperatorKind, operands: Vec<Node>) -> Node {
    let mut flat = Vec::with_capacity(operands.len());
    for node in operands {
        match node {
            Node::Operator {
                kind: k,
                operands: inner,
                ..
            } if k == kind && kind != OperatorKind::Concat => flat.extend(inner),
            node => flat.push(node),
        }
    }
    if flat.len() == 1 {
        return flat.pop().expect("one operand");
    }
    let span = flat
        .iter()
        .map(Node::span)
        .reduce(Span::join)
        .unwrap_or_default();
    Node::Operator {
        kind,
        operands: flat,
        span,
    }
}

/// Lifts fields out of a top-level AND/OR of patterns in queries without
/// grouping parentheses: `repo:foo a or b file:bar` is
/// `repo:foo file:bar (a or b)`, not `(repo:foo a) or (b file:bar)`. It
/// applies only when fields come first in the first operand, last in the
/// last one, and nowhere in between; `None` leaves the tree as it is.
fn hoist(root: &Node) -> Option<Node> {
    let Node::Operator {
        kind: kind @ (OperatorKind::And | OperatorKind::Or),
        operands,
        ..
    } = root
    else {
        return None;
    };
    let n = operands.len();
    let mut parameters = Vec::new();
    let mut patterns = Vec::with_capacity(n);
    for (i, node) in operands.iter().enumerate() {
        if i == 0 || i == n - 1 {
            let (params, pattern) = partition(node)?;
            if !naturally_ordered(node, i != 0) {
                return None;
            }
            parameters.extend(params);
            patterns.push(pattern);
        } else if has_parameter(node) {
            return None;
        } else {
            patterns.push(node.clone());
        }
    }
    let mut nodes: Vec<Node> = parameters.into_iter().map(Node::Parameter).collect();
    nodes.push(operator(*kind, patterns));
    Some(operator(OperatorKind::Concat, nodes))
}

/// Splits an operand into its fields and one pattern expression: an AND or
/// concat may mix fields and patterns (an AND's operands may be such
/// concats), an OR may not, and there must be a pattern.
fn partition(node: &Node) -> Option<(Vec<Parameter>, Node)> {
    fn split(node: &Node, nested: bool, params: &mut Vec<Parameter>) -> Option<Option<Node>> {
        match node {
            Node::Parameter(p) => {
                params.push(p.clone());
                Some(None)
            }
            node if !has_parameter(node) => Some(Some(node.clone())),
            Node::Operator {
                kind: kind @ (OperatorKind::And | OperatorKind::Concat),
                operands,
                ..
            } if !nested || *kind == OperatorKind::Concat => {
                let mut patterns = Vec::new();
                for operand in operands {
                    patterns.extend(split(operand, true, params)?);
                }
                Some((!patterns.is_empty()).then(|| operator(*kind, patterns)))
            }
            _ => None,
        }
    }
    let mut params = Vec::new();
    let pattern = split(node, false, &mut params)??;
    Some((params, pattern))
}

/// Whether, reading left to right (right to left when `reverse`), the
/// patterns of `node` all come after its fields. It starts from position 0,
/// so a pattern at the very start of the query counts as following a field:
/// `a or b file:x` is not hoisted.
fn naturally_ordered(node: &Node, reverse: bool) -> bool {
    fn walk(node: &Node, b: &mut [usize; 4]) {
        match node {
            Node::Parameter(p) => {
                b[0] = b[0].max(p.span.start);
                b[2] = b[2].min(p.span.start);
            }
            Node::Pattern(p) => {
                b[1] = b[1].max(p.span.start);
                b[3] = b[3].min(p.span.start);
            }
            Node::Operator { operands, .. } => operands.iter().for_each(|n| walk(n, b)),
        }
    }
    // Rightmost field, rightmost pattern, leftmost field, leftmost pattern.
    let mut b = [0, 0, usize::MAX, usize::MAX];
    walk(node, &mut b);
    if reverse { b[2] > b[1] } else { b[0] < b[3] }
}

fn has_parameter(node: &Node) -> bool {
    match node {
        Node::Parameter(_) => true,
        Node::Pattern(_) => false,
        Node::Operator { operands, .. } => operands.iter().any(has_parameter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(input: &str, pt: PatternType) -> String {
        parse(input, pt).unwrap().to_string()
    }

    fn kw(input: &str) -> String {
        canon(input, PatternType::Keyword)
    }

    fn std(input: &str) -> String {
        canon(input, PatternType::Standard)
    }

    #[test]
    fn empty() {
        assert_eq!(parse("   ", PatternType::Keyword).unwrap().root, None);
    }

    #[test]
    fn fields_and_aliases() {
        let q = parse(
            r#"r:^gitlab\.com/ f:\.rs$ -lang:go message:"fix bug" foo"#,
            PatternType::Keyword,
        )
        .unwrap();
        let params = q.parameters();
        assert_eq!(params.len(), 4);
        assert_eq!(
            (params[0].field, params[0].value.as_str()),
            ("repo", r"^gitlab\.com/")
        );
        assert_eq!(params[1].field, "file");
        assert!(params[2].negated);
        assert_eq!(
            (params[3].field, params[3].value.as_str(), params[3].quoted),
            ("message", "fix bug", true)
        );
        assert_eq!(
            q.to_string(),
            r#"repo:^gitlab\.com/ file:\.rs$ -lang:go message:"fix bug" foo"#
        );
    }

    #[test]
    fn unknown_field_is_pattern() {
        let q = parse("http://example.com", PatternType::Keyword).unwrap();
        assert!(matches!(q.root, Some(Node::Pattern(_))));
    }

    #[test]
    fn precedence() {
        assert_eq!(kw("a or b and c"), "a OR b AND c");
        assert_eq!(kw("(x or y) and z"), "(x OR y) AND z");
        assert_eq!(kw("(a or b) c"), "(a OR b) c");
        assert_eq!(kw("a b and c d"), "a b AND c d");
        assert_eq!(kw("a OR (b OR c)"), "a OR b OR c");
        let q = parse("a or b and c", PatternType::Keyword).unwrap();
        let Some(Node::Operator {
            kind: OperatorKind::Or,
            operands,
            ..
        }) = q.root
        else {
            panic!()
        };
        assert!(matches!(
            operands[1],
            Node::Operator {
                kind: OperatorKind::And,
                ..
            }
        ));
    }

    #[test]
    fn not() {
        assert_eq!(kw("foo NOT bar"), "foo NOT bar");
        assert_eq!(kw("NOT repo:x"), "-repo:x");
        // `NOT -field:value` is an error.
        let err = parse("a not -file:y", PatternType::Keyword).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"unexpected NOT before "-file:y". Remove NOT and try again"#
        );
        assert_eq!(err.span(), Span { start: 2, end: 13 });
        assert_eq!(
            parse("NOT -f:y", PatternType::Standard)
                .unwrap_err()
                .to_string(),
            r#"unexpected NOT before "-f:y". Remove NOT and try again"#
        );
        assert!(matches!(
            parse("NOT (a b)", PatternType::Keyword),
            Err(ParseError::NegatedExpression(_))
        ));
        // `not` at the end is a pattern.
        assert_eq!(kw("a NOT"), "a NOT");
    }

    #[test]
    fn missing_operands() {
        for q in ["a or or b", "x or and y", "a and and b"] {
            assert!(
                matches!(
                    parse(q, PatternType::Keyword),
                    Err(ParseError::MissingOperand { .. })
                ),
                "{q}"
            );
        }
    }

    /// `and`/`or` are operators only between terms, `not` only before one;
    /// elsewhere they are patterns.
    #[test]
    fn operator_boundaries() {
        let k = |q: &str| tree(q, PatternType::Keyword);
        assert_eq!(k("and x"), r#"(concat "and" "x")"#);
        assert_eq!(k("   and x"), r#"(concat "and" "x")"#);
        assert_eq!(k("or y"), r#"(concat "or" "y")"#);
        assert_eq!(k("y and"), r#"(concat "y" "and")"#);
        assert_eq!(k("x andnot y"), r#"(concat "x" "andnot" "y")"#);
        assert_eq!(k("x oror y"), r#"(concat "x" "oror" "y")"#);
        assert_eq!(k("xANDy"), r#""xANDy""#);
        assert_eq!(k("(x or y)and z"), r#"(and (or "x" "y") "z")"#);
        assert_eq!(k("z and(x or y)"), r#"(and "z" (or "x" "y"))"#);
        assert_eq!(
            k("(w or x)and(y or z)"),
            r#"(and (or "w" "x") (or "y" "z"))"#
        );
        assert_eq!(k("(x and y)or z"), r#"(or (and "x" "y") "z")"#);
        assert_eq!(k("(x and)or z"), r#"(or (concat "x" "and") "z")"#);
        assert_eq!(k("x or(y and z)"), r#"(or "x" (and "y" "z"))"#);
        assert_eq!(k("(x or y)not z"), r#"(concat (or "x" "y") (not "z"))"#);
        assert_eq!(
            k("(x not y)not z"),
            r#"(concat (concat "x" (not "y")) (not "z"))"#
        );
        assert_eq!(k("  not y"), r#"(not "y")"#);
        assert_eq!(k("x or (yandz)"), r#"(or "x" "yandz")"#);
        assert_eq!(k("(a or)"), r#"(concat "a" "or")"#);
        assert_eq!(tree("x or", PatternType::Literal), r#"(concat "x" "or")"#);
        // An operand missing before `)` is the pattern `()`.
        assert_eq!(tree("(a or )", PatternType::Regexp), r#"(or "a" "()")"#);
    }

    #[test]
    fn quoted_phrase_in_keyword() {
        let q = parse(r#""foo bar" baz"#, PatternType::Keyword).unwrap();
        let Some(Node::Operator { operands, .. }) = &q.root else {
            panic!()
        };
        assert!(
            matches!(&operands[0], Node::Pattern(p) if p.kind == PatternKind::Quoted && p.value == "foo bar")
        );
        // Literal searches keep the quotes.
        assert_eq!(canon(r#""foo""#, PatternType::Literal), r#""foo""#);
        // A field value must close its quote.
        assert!(matches!(
            parse(r#"repo:"foo"#, PatternType::Keyword),
            Err(ParseError::UnterminatedQuote(_))
        ));
    }

    fn leaf(input: &str, pt: PatternType) -> Pattern {
        match parse(input, pt).unwrap().root {
            Some(Node::Pattern(p)) => p,
            root => panic!("{input}: {root:?}"),
        }
    }

    /// Keyword and regexp queries read `"..."` and `'...'` as quoted
    /// literals, and keyword, standard and regexp queries read `/.../` as a
    /// regexp, when the closing delimiter is followed by whitespace, `)` or
    /// the end.
    #[test]
    fn delimited_patterns() {
        use PatternKind::{Literal, Quoted, Regexp};
        for pt in [PatternType::Regexp, PatternType::Keyword] {
            let l = |q: &str| {
                let p = leaf(q, pt);
                (p.value, p.kind)
            };
            assert_eq!(l(r#""foo bar""#), ("foo bar".into(), Quoted));
            assert_eq!(l("'foo bar'"), ("foo bar".into(), Quoted));
            assert_eq!(l(r#"'foo "bar"'"#), (r#"foo "bar""#.into(), Quoted));
            assert_eq!(l(r#""foo \"bar\"""#), (r#"foo "bar""#.into(), Quoted));
            assert_eq!(l(r#""(foo bar)""#), ("(foo bar)".into(), Quoted));
            assert_eq!(l(r#""a\\b\d\tc""#), ("a\\b\\d\tc".into(), Quoted));
            assert_eq!(l(r#""""#), (r#""""#.into(), Literal));
            assert_eq!(l("''"), ("''".into(), Literal));
            // Unclosed, or followed by more text: the quotes are text.
            for q in [
                r#"""#,
                r#"""""#,
                r#""""""#,
                r#"""abc""#,
                r#"""abc"""#,
                r#""abc"def"ghi""#,
                r#""abc"def"#,
                r#"abc"def""#,
                r#"\"trento\""#,
            ] {
                assert_eq!(l(q), (q.into(), Literal), "{q}");
            }
            // A quote closed before `)`.
            assert_eq!(tree(r#"("a b" or c)"#, pt), r#"(or "a b" "c")"#);
            assert_eq!(
                tree(r#"repo:"foo bar" 'foo bar'"#, pt),
                r#"(concat "repo:foo bar" "foo bar")"#
            );
            assert_eq!(tree("repo:foo' bar'", pt), r#"(concat "repo:foo'" "bar'")"#);
            assert_eq!(
                tree(r#""(abc def)" ghi"#, pt),
                r#"(concat "(abc def)" "ghi")"#
            );
        }
        for pt in [
            PatternType::Regexp,
            PatternType::Keyword,
            PatternType::Standard,
        ] {
            let l = |q: &str| {
                let p = leaf(q, pt);
                (p.value, p.kind)
            };
            assert_eq!(l(r"/b\/ar/"), ("b/ar".into(), Regexp));
            assert_eq!(l(r"/\s+b\d+ar/ "), (r"\s+b\d+ar".into(), Regexp));
            assert_eq!(l("/foo bar/"), ("foo bar".into(), Regexp));
            assert_eq!(l("//"), ("//".into(), Literal));
            for q in ["/a/file/path", "/a/file/path/", "/usr/bin"] {
                assert_eq!(l(q), (q.into(), Literal), "{q}");
            }
            assert_eq!(
                tree("/a/ /another/path/", pt),
                r#"(concat "a" "/another/path/")"#
            );
        }
        assert_eq!(
            tree("(abc /de.*/) ghi", PatternType::Keyword),
            r#"(concat (concat "abc" "de.*") "ghi")"#
        );
        // Standard and literal queries keep quotes as text, literal queries
        // slashes too.
        for pt in [PatternType::Standard, PatternType::Literal] {
            for q in [r#""trento""#, r#"".*""#, "'", r#""""#] {
                assert_eq!(leaf(q, pt).kind, Literal, "{q}");
                assert_eq!(leaf(q, pt).value, q, "{q}");
            }
            assert_eq!(tree(r#""x foo:bar"#, pt), r#"(concat "\"x" "foo:bar")"#);
        }
        assert_eq!(leaf("/a/", PatternType::Literal).value, "/a/");
        // The printer writes the values back so they parse the same.
        for q in [
            r#""a \"b\" \\ \d""#,
            r"/a\/b\\\/c\d/",
            "'x\\ty'",
            r"/x\\/",
            r"/C:\\Users\\\n/",
        ] {
            let first = leaf(q, PatternType::Regexp);
            let printed = canon(q, PatternType::Regexp);
            let again = leaf(&printed, PatternType::Regexp);
            assert_eq!(
                (first.value, first.kind),
                (again.value, again.kind),
                "{q} -> {printed}"
            );
        }
    }

    #[test]
    fn regexp_delimiters_keep_escaped_backslashes() {
        use PatternKind::Regexp;
        for (q, value) in [
            (r"/\\n/", r"\\n"),
            (r"/foo\\d/", r"foo\\d"),
            (r"/a\\/", r"a\\"),
            (r"/C:\\Users/", r"C:\\Users"),
            (r"/a\/b/", "a/b"),
            (r"/a\nb/", "a\nb"),
        ] {
            let p = leaf(q, PatternType::Keyword);
            assert_eq!((p.value.as_str(), p.kind), (value, Regexp), "{q}");
        }
        // Quotes still unescape a backslash.
        assert_eq!(leaf(r#""a\\b""#, PatternType::Keyword).value, r"a\b");
    }

    #[test]
    fn regexp_in_standard() {
        let q = parse(r"/foo\/b.r/ baz", PatternType::Standard).unwrap();
        let Some(Node::Operator { operands, .. }) = &q.root else {
            panic!()
        };
        assert!(
            matches!(&operands[0], Node::Pattern(p) if p.kind == PatternKind::Regexp && p.value == "foo/b.r")
        );
        assert_eq!(std("/usr/bin"), "/usr/bin");
        assert!(
            matches!(parse("/usr/bin", PatternType::Standard).unwrap().root, Some(Node::Pattern(p)) if p.kind == PatternKind::Literal)
        );
    }

    #[test]
    fn parens_in_words_and_values() {
        assert_eq!(kw("foo(bar)"), "foo(bar)");
        assert_eq!(kw("()"), "()");
        assert_eq!(
            kw("(repo:has.file(path:go.mod) x)"),
            "repo:has.file(path:go.mod) x"
        );
        let q = parse("repo:has.file(path:go.mod)", PatternType::Keyword).unwrap();
        assert_eq!(q.parameters()[0].value, "has.file(path:go.mod)");
        // Predicate arguments may hold whitespace, quoted parentheses and escapes.
        let q = parse(
            "repo:has.file(path:go.mod content:\"a )b\") rev:at.time(2024-01-01, main) x",
            PatternType::Keyword,
        )
        .unwrap();
        let params = q.parameters();
        assert_eq!(params[0].value, "has.file(path:go.mod content:\"a )b\")");
        assert_eq!(params[1].value, "at.time(2024-01-01, main)");
        // Unclosed: a plain word up to whitespace.
        let q = parse("repo:has.path(x y", PatternType::Keyword).unwrap();
        assert_eq!(q.parameters()[0].value, "has.path(x");
    }

    /// A `(` that is never closed makes the parentheses text; a `)` that
    /// closes no group is an error.
    #[test]
    fn unbalanced_parens() {
        for pt in [
            PatternType::Keyword,
            PatternType::Standard,
            PatternType::Literal,
            PatternType::Regexp,
        ] {
            let t = |q: &str| tree(q, pt);
            assert_eq!(t("("), r#""(""#, "{pt:?}");
            assert_eq!(t("(x"), r#""(x""#);
            assert_eq!(t("(()"), r#""(()""#);
            assert_eq!(t("foo("), r#""foo(""#);
            assert_eq!(t("repo:foo (x"), r#"(concat "repo:foo" "(x")"#);
            assert_eq!(t("x or (x"), r#"(or "x" "(x")"#);
            assert_eq!(t("(y or (z"), r#"(or "(y" "(z")"#);
            assert_eq!(t("((x or bar("), r#"(or "((x" "bar(")"#);
            assert_eq!(t("foo( and bar("), r#"(and "foo(" "bar(")"#);
            assert_eq!(t("foo( or bar("), r#"(or "foo(" "bar(")"#);
            // Fields are hoisted out of the OR.
            assert_eq!(
                t("repo:foo foo( or bar("),
                r#"(concat "repo:foo" (or "foo(" "bar("))"#
            );
            assert_eq!(
                t("repo:foo (lisp or lisp()"),
                r#"(concat "repo:foo" (or "(lisp" "lisp()"))"#
            );
            assert_eq!(t("(lisp or lisp()"), r#"(or "(lisp" "lisp()")"#);
            assert_eq!(
                t(r"bar and (foo or x\) ()"),
                r#"(or (and "bar" "(foo") (concat "x\\)" "()"))"#
            );
            if pt != PatternType::Regexp {
                assert_eq!(
                    t("repo:foo (bar( and baz())"),
                    r#"(concat "repo:foo" (and "bar(" "baz()"))"#
                );
                assert!(matches!(
                    parse("x()(y or z)", pt),
                    Err(ParseError::UnsupportedParens { .. })
                ));
            }
            for q in [
                "a)",
                "x) or y",
                "())",
                "x )or y",
                ")(())(",
                "(a or (b and )) or d)",
                "repo:foo )foo(",
                "repo:foo ) main( or (lisp    lisp)",
                "repo:foo )))) main( or (lisp    lisp) and )))",
                "repo:foo Args or main)",
                "repo:foo Args) and main",
                "repo:foo bar and baz)",
                "repo:foo bar)) and baz",
                "repo:foo (bar and baz))",
                "repo:foo (bar and (baz)))",
                ")(0 )0",
                "((R:)0))0",
            ] {
                let e = parse(q, pt).unwrap_err();
                assert!(
                    matches!(e, ParseError::UnsupportedParens { .. }),
                    "{q} {pt:?}: {e:?}"
                );
                let hint = if pt == PatternType::Keyword {
                    "in double quotes"
                } else {
                    "Try using the content: filter"
                };
                assert!(e.to_string().starts_with("unsupported expression"), "{e}");
                assert!(e.to_string().contains(hint), "{e}");
            }
        }
        let e = parse("repo:foo Args or main)", PatternType::Standard).unwrap_err();
        assert_eq!(e.span(), Span { start: 21, end: 22 });
        // Empty parentheses are the pattern `()`.
        let k = |q: &str| tree(q, PatternType::Keyword);
        assert_eq!(k("()"), r#""()""#);
        assert_eq!(k("(())"), r#""()""#);
        assert_eq!(k("(   )"), r#""()""#);
        assert_eq!(k("() -> {}"), r#"(concat "()" "->" "{}")"#);
        // Regexp queries end a word at a parenthesis, so a group may follow.
        assert_eq!(
            tree("foo(a or b)", PatternType::Regexp),
            r#"(concat "foo" (or "a" "b"))"#
        );
        assert_eq!(
            tree("(bar( and baz())", PatternType::Regexp),
            r#"(and "(bar(" "baz())")"#
        );
        assert_eq!(
            tree("x()(y or z)", PatternType::Regexp),
            r#"(concat "x" "()" (or "y" "z"))"#
        );
        assert_eq!(
            tree("func( or func(.*)", PatternType::Regexp),
            r#"(or "func(" "func(.*)")"#
        );
        assert_eq!(
            tree("(0(F)(:())(:())(<0)0()", PatternType::Regexp),
            r#""(0(F)(:())(:())(<0)0()""#
        );
        // Structural templates keep every parenthesis.
        assert!(parse("foo(a))", PatternType::Structural).is_ok());
        let q = parse("if (x", PatternType::Standard).unwrap();
        let Some(Node::Operator {
            kind: OperatorKind::Concat,
            operands,
            ..
        }) = &q.root
        else {
            panic!()
        };
        assert!(matches!(&operands[1], Node::Pattern(p) if p.value == "(x"));
    }

    /// The tree as the reference's test printer writes it:
    /// `(concat "a" "b")`, `(or "a" (and "b" "c"))`.
    fn sexp(node: &Node) -> String {
        match node {
            Node::Pattern(p) if p.negated => format!("(not {:?})", p.value),
            Node::Pattern(p) => format!("{:?}", p.value),
            Node::Parameter(p) => format!("\"{}:{}\"", p.field, p.value),
            Node::Operator { kind, operands, .. } => {
                let op = match kind {
                    OperatorKind::And => "and",
                    OperatorKind::Or => "or",
                    OperatorKind::Concat => "concat",
                };
                let args: Vec<String> = operands.iter().map(sexp).collect();
                format!("({op} {})", args.join(" "))
            }
        }
    }

    fn tree(input: &str, pt: PatternType) -> String {
        sexp(&parse(input, pt).unwrap().root.unwrap())
    }

    /// Fields are lifted out of a top-level AND/OR of patterns (parsed as
    /// regexp); the hoisted fields and the pattern expression form a concat.
    /// Where hoisting fails, the tree is left as parsed.
    #[test]
    fn hoist_fields() {
        let t = |q: &str| tree(q, PatternType::Regexp);
        for (q, want) in [
            ("repo:foo a or b", r#"(concat "repo:foo" (or "a" "b"))"#),
            (
                "repo:foo file:bar a and b or c",
                r#"(concat "repo:foo" "file:bar" (or (and "a" "b") "c"))"#,
            ),
            (
                "repo:foo bar { and baz {",
                r#"(concat "repo:foo" (and (concat "bar" "{") (concat "baz" "{")))"#,
            ),
            (
                "repo:foo bar { and baz { and qux {",
                r#"(concat "repo:foo" (and (concat "bar" "{") (concat "baz" "{") (concat "qux" "{")))"#,
            ),
            (
                "repo:foo a or b file:bar",
                r#"(concat "repo:foo" "file:bar" (or "a" "b"))"#,
            ),
            (
                "repo:foo a or b or c file:bar",
                r#"(concat "repo:foo" "file:bar" (or "a" "b" "c"))"#,
            ),
            (
                "repo:foo a and b or c and d or e file:bar",
                r#"(concat "repo:foo" "file:bar" (or (and "a" "b") (and "c" "d") "e"))"#,
            ),
            // Not hoisted: a field between patterns.
            ("a repo:foo or b", r#"(or (concat "a" "repo:foo") "b")"#),
            (
                "a repo:foo q or b",
                r#"(or (concat "a" "repo:foo" "q") "b")"#,
            ),
            (
                "repo:bar a repo:foo or b",
                r#"(or (concat "repo:bar" "a" "repo:foo") "b")"#,
            ),
            (
                "a repo:foo or b or file:bar c",
                r#"(or (concat "a" "repo:foo") "b" (concat "file:bar" "c"))"#,
            ),
            // Not hoisted: an operand without a pattern.
            ("repo:foo or a", r#"(or "repo:foo" "a")"#),
            ("a or repo:foo", r#"(or "a" "repo:foo")"#),
            ("repo:foo or repo:bar", r#"(or "repo:foo" "repo:bar")"#),
            // Not hoisted: no AND/OR at the top.
            ("a b", r#"(concat "a" "b")"#),
            // Not hoisted: fields in an inner operand.
            (
                "repo:foo a or repo:foobar b or c file:bar",
                r#"(or (concat "repo:foo" "a") (concat "repo:foobar" "b") (concat "c" "file:bar"))"#,
            ),
            // Not hoisted: a pattern at the very start counts as following a
            // field.
            ("a or b file:bar", r#"(or "a" (concat "b" "file:bar"))"#),
            (" a or b file:bar", r#"(concat "file:bar" (or "a" "b"))"#),
            // Not hoisted: parentheses group terms.
            (
                "repo:foo a or (b or c)",
                r#"(or (concat "repo:foo" "a") "b" "c")"#,
            ),
        ] {
            assert_eq!(t(q), want, "{q}");
        }
        // Keyword and standard queries hoist too, and the result prints
        // with the group.
        assert_eq!(kw("repo:foo a or b"), "repo:foo (a OR b)");
        assert_eq!(std("repo:foo a b or c"), "repo:foo (a b OR c)");
        assert_eq!(kw("repo:foo NOT a or b"), "repo:foo (NOT a OR b)");
    }

    /// Balanced parentheses that do not hold an operator or a field are
    /// pattern text, not grouping, in every pattern type but keyword.
    #[test]
    fn balanced_parens_are_patterns() {
        for pt in [
            PatternType::Standard,
            PatternType::Literal,
            PatternType::Regexp,
        ] {
            let t = |q: &str| tree(q, pt);
            assert_eq!(
                t("copyright (c) 2005"),
                r#"(concat "copyright" "(c)" "2005")"#
            );
            assert_eq!(t("foo (bar)"), r#"(concat "foo" "(bar)")"#);
            assert_eq!(t("(foo or bar)"), r#"(or "foo" "bar")"#);
            assert_eq!(t("foo()"), r#""foo()""#);
            assert_eq!(t("foo()bar"), r#""foo()bar""#);
            assert_eq!(t("foo(   )bar"), r#""foo(   )bar""#);
            assert_eq!(t("(x())"), r#""(x())""#);
            assert_eq!(t("(x or y) and z"), r#"(and (or "x" "y") "z")"#);
            assert_eq!(t("((a or b)) and c"), r#"(and (or "a" "b") "c")"#);
            assert_eq!(t("(a b) c d"), r#"(concat "(a b)" "c" "d")"#);
            assert_eq!(t("a b (c d)"), r#"(concat "a" "b" "(c d)")"#);
            assert_eq!(t("(a b) and (d e)"), r#"(and "(a b)" "(d e)")"#);
            assert_eq!(t("(((a b c))) and d"), r#"(and "(((a b c)))" "d")"#);
            assert_eq!(t("(f(x)oo((a|b))bar)"), r#""(f(x)oo((a|b))bar)""#);
            assert_eq!(t("(x and regex(s)?)"), r#"(and "x" "regex(s)?")"#);
            assert_eq!(t("a or (b c)"), r#"(or "a" "(b c)")"#);
            assert_eq!(t("func(a, b) x"), r#"(concat "func(a, b)" "x")"#);
            assert_eq!(t(r"\( \) ()"), r#"(concat "\\(" "\\)" "()")"#);
            assert_eq!(t(r"()search\(()"), r#""()search\\(()""#);
            // A field or an operator inside makes them grouping. (Parameters
            // stay in the concat here; the planner separates them.)
            assert_eq!(t("(repo:foo bar)"), r#"(concat "repo:foo" "bar")"#);
            assert_eq!(t("(a not b)"), r#"(concat "a" (not "b"))"#);
            assert_eq!(t("00\u{a0}(000)"), r#"(concat "00" "(000)")"#);
            assert!(matches!(
                parse("a NOT (b c)", pt),
                Err(ParseError::NegatedExpression(_))
            ));
            // Unbalanced input still falls back to literal parentheses.
            assert_eq!(t("(foo) (bar"), r#"(concat "(foo)" "(bar")"#);
        }
        // Keyword queries group with parentheses; balanced ones inside a
        // word are still part of it.
        let k = |q: &str| tree(q, PatternType::Keyword);
        assert_eq!(
            k("copyright (c) 2005"),
            r#"(concat "copyright" "c" "2005")"#
        );
        assert_eq!(k("foo(a b) x"), r#"(concat "foo(a b)" "x")"#);
        assert_eq!(k("(x or y) and z"), r#"(and (or "x" "y") "z")"#);
    }

    /// A grouped AND/OR among the terms of a standard or literal query has
    /// no clear meaning and is an error.
    #[test]
    fn grouped_operator_in_concat() {
        for pt in [PatternType::Standard, PatternType::Literal] {
            for q in [
                "a (b or c) d",
                "a (b and c) d",
                "(a or b) c",
                "a b (repo:foo c d)",
                "a b (c d repo:foo)",
                "a repo:b repo:c (d repo:e repo:f)",
                "a repo:b repo:c (repo:e repo:f (repo:g repo:h b))",
                "(repo:foo a (repo:bar b (repo:qux c)))",
                "a repo:b repo:c (d repo:e repo:f e)",
                "x (y or z) and w",
            ] {
                assert!(
                    matches!(parse(q, pt), Err(ParseError::UnclearParens(_))),
                    "{q} {pt:?}"
                );
            }
            let t = |q: &str| tree(q, pt);
            // Groups of fields only, and groups next to fields, are fine.
            assert!(parse("a repo:b repo:c (repo:e repo:f (repo:g repo:h)) b", pt).is_ok());
            assert_eq!(t("repo:x (a or b)"), r#"(concat "repo:x" (or "a" "b"))"#);
            assert_eq!(
                t("(a or b) and (c or d)"),
                r#"(and (or "a" "b") (or "c" "d"))"#
            );
            assert_eq!(t("a b or c d"), r#"(or (concat "a" "b") (concat "c" "d"))"#);
            assert_eq!(t("a (b c) d"), r#"(concat "a" "(b c)" "d")"#);
            assert_eq!(t("a (b not c)"), r#"(concat "a" (concat "b" (not "c")))"#);
        }
        // Keyword queries AND the terms; regexp queries join them in order.
        for pt in [PatternType::Keyword, PatternType::Regexp] {
            assert_eq!(tree("a (b or c) d", pt), r#"(concat "a" (or "b" "c") "d")"#);
        }
        let e = parse("a (b or c) d", PatternType::Standard).unwrap_err();
        assert_eq!(e.span(), Span { start: 0, end: 12 });
        assert!(e.to_string().contains("trouble understanding that query"));
    }

    #[test]
    fn patterntype_override() {
        let q = parse("patterntype:standard /a+/", PatternType::Keyword).unwrap();
        assert_eq!(q.pattern_type, PatternType::Standard);
        assert!(q.to_string().ends_with("/a+/"));
    }

    #[test]
    fn spans() {
        let input = "foo  -repo:bar NOT baz";
        let q = parse(input, PatternType::Keyword).unwrap();
        let Some(Node::Operator { operands, span, .. }) = &q.root else {
            panic!()
        };
        assert_eq!(
            *span,
            Span {
                start: 0,
                end: input.len()
            }
        );
        let slice = |n: &Node| &input[n.span().start..n.span().end];
        assert_eq!(slice(&operands[0]), "foo");
        assert_eq!(slice(&operands[1]), "-repo:bar");
        assert_eq!(slice(&operands[2]), "NOT baz");
    }

    #[test]
    fn printer_round_trips() {
        let queries = [
            r#"repo:"a b" file:'x"y' (foo or "bar baz") and not qux"#,
            "repo: lang:rust count:all",
            r#"message:"a\\b" content:'it''s'"#,
            "(a or b) (c or d) and e",
            "patterntype:standard /x(y)/ z",
            "café ünï",
        ];
        for q in queries {
            let first = match parse(q, PatternType::Keyword) {
                Ok(first) => first,
                Err(_) => continue,
            };
            let printed = first.to_string();
            let second = parse(&printed, PatternType::Keyword).unwrap();
            assert_eq!(second.to_string(), printed, "{q}");
        }
    }
}
