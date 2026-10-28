//! SCIM filters (RFC 7644 §3.4.2.2): parsing, evaluation against a resource's JSON, and the
//! SQL pre-filter for the common identity provider lookups.
//!
//! Grammar (operators and attribute names case-insensitive; `not` binds tighter than `and`,
//! which binds tighter than `or`):
//!
//! ```text
//! FILTER    = attrExp / FILTER SP ("and" / "or") SP FILTER / ["not"] "(" FILTER ")" / valuePath
//! valuePath = attrPath "[" FILTER "]" [ "." subAttr SP compareOp SP compValue ]
//! attrExp   = attrPath SP "pr" / attrPath SP compareOp SP compValue
//! compareOp = "eq" / "ne" / "co" / "sw" / "ew" / "gt" / "lt" / "ge" / "le"
//! compValue = "false" / "null" / "true" / number / string
//! ```
//!
//! `emails[type eq "work"].value eq "x"` (sent by some providers although RFC 7644 has no such
//! form) is read as `emails[type eq "work" and value eq "x"]`.

use serde_json::Value;
use sourcegull_db::ScimNarrow;

use super::resource::{CORE_USER_SCHEMA, ENTERPRISE_USER_SCHEMA};

/// A parsed filter.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
    /// `attr pr`.
    Present(AttrPath),
    /// `attr op value`.
    Compare(AttrPath, CompareOp, Value),
    /// `attr[filter]`: some element of the multi-valued `attr` matches the inner filter,
    /// whose attribute paths are relative to the element.
    ValuePath(AttrPath, Box<Filter>),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Co,
    Sw,
    Ew,
    Gt,
    Lt,
    Ge,
    Le,
}

/// An attribute reference: `[urn:]attr[.sub]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrPath {
    /// A schema URN prefix (`urn:…:enterprise:2.0:User`), lowercased; `None` for the core
    /// schema or no prefix.
    pub extension: Option<String>,
    /// The attribute name, as written.
    pub attr: String,
    /// The sub-attribute, as written.
    pub sub: Option<String>,
}

impl AttrPath {
    /// Parses `[urn:…:]attr[.sub]`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (extension, rest) = split_urn(s)?;
        let (attr, sub) = match rest.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        let valid = |n: &str| {
            !n.is_empty()
                && n.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '$')
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '$')
        };
        if !valid(attr) || sub.is_some_and(|s| !valid(s)) {
            return Err(format!("invalid attribute path {s:?}"));
        }
        Ok(Self {
            extension,
            attr: attr.to_owned(),
            sub: sub.map(str::to_owned),
        })
    }

    /// Whether this names the top-level attribute `name` of the core schema.
    pub fn is(&self, name: &str) -> bool {
        self.extension.is_none() && self.attr.eq_ignore_ascii_case(name)
    }
}

/// Splits a schema URN prefix off an attribute path. The core User schema prefix is dropped.
fn split_urn(s: &str) -> Result<(Option<String>, &str), String> {
    if !s.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("urn:")) {
        return Ok((None, s));
    }
    let lower = s.to_ascii_lowercase();
    for schema in [CORE_USER_SCHEMA, ENTERPRISE_USER_SCHEMA] {
        let schema_l = schema.to_ascii_lowercase();
        if lower == schema_l {
            return Err(format!(
                "attribute path {s:?} names a schema, not an attribute"
            ));
        }
        if lower.starts_with(&schema_l) && lower.as_bytes().get(schema.len()) == Some(&b':') {
            let rest = &s[schema.len() + 1..];
            let ext = (schema != CORE_USER_SCHEMA).then_some(schema_l);
            return Ok((ext, rest));
        }
    }
    // An unknown schema: the attribute is whatever follows the last ':'.
    let idx = s.rfind(':').unwrap_or(0);
    Ok((Some(lower[..idx].to_owned()), &s[idx + 1..]))
}

/// Parses a filter expression.
pub fn parse(input: &str) -> Result<Filter, String> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let f = p.or()?;
    if p.pos != p.tokens.len() {
        return Err(format!("unexpected {:?}", p.tokens[p.pos]));
    }
    Ok(f)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Str(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    /// `.sub` right after `]`.
    SubAttr(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '[' => {
                out.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Token::RBracket);
                i += 1;
                if chars.get(i) == Some(&'.') {
                    let start = i + 1;
                    let mut j = start;
                    while j < chars.len() && !is_delim(chars[j]) {
                        j += 1;
                    }
                    out.push(Token::SubAttr(chars[start..j].iter().collect()));
                    i = j;
                }
            }
            '"' => {
                // A JSON string literal.
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated string".into());
                }
                i += 1;
                let lit: String = chars[start..i].iter().collect();
                let s: String =
                    serde_json::from_str(&lit).map_err(|_| format!("invalid string {lit}"))?;
                out.push(Token::Str(s));
            }
            _ => {
                let start = i;
                while i < chars.len() && !is_delim(chars[i]) {
                    i += 1;
                }
                out.push(Token::Word(chars[start..i].iter().collect()));
            }
        }
    }
    Ok(out)
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"')
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek_word(&self, w: &str) -> bool {
        matches!(self.tokens.get(self.pos), Some(Token::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: Token) -> Result<(), String> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            other => Err(format!("expected {t:?}, found {other:?}")),
        }
    }

    fn or(&mut self) -> Result<Filter, String> {
        let mut left = self.and()?;
        while self.peek_word("or") {
            self.pos += 1;
            let right = self.and()?;
            left = Filter::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Filter, String> {
        let mut left = self.unary()?;
        while self.peek_word("and") {
            self.pos += 1;
            let right = self.unary()?;
            left = Filter::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Filter, String> {
        if self.peek_word("not") && self.tokens.get(self.pos + 1) == Some(&Token::LParen) {
            self.pos += 1;
            return Ok(Filter::Not(Box::new(self.group()?)));
        }
        if self.tokens.get(self.pos) == Some(&Token::LParen) {
            return self.group();
        }
        self.attr_exp()
    }

    fn group(&mut self) -> Result<Filter, String> {
        self.expect(Token::LParen)?;
        let f = self.or()?;
        self.expect(Token::RParen)?;
        Ok(f)
    }

    fn attr_exp(&mut self) -> Result<Filter, String> {
        let Some(Token::Word(path)) = self.next() else {
            return Err("expected an attribute path".into());
        };
        let path = AttrPath::parse(&path)?;
        if self.tokens.get(self.pos) == Some(&Token::LBracket) {
            if path.sub.is_some() {
                return Err("a value filter must follow a multi-valued attribute".into());
            }
            self.pos += 1;
            let inner = self.or()?;
            self.expect(Token::RBracket)?;
            if let Some(Token::SubAttr(sub)) = self.tokens.get(self.pos).cloned() {
                self.pos += 1;
                let sub_path = AttrPath::parse(&sub)?;
                let cmp = self.comparison(sub_path)?;
                let inner = Filter::And(Box::new(inner), Box::new(cmp));
                return Ok(Filter::ValuePath(path, Box::new(inner)));
            }
            return Ok(Filter::ValuePath(path, Box::new(inner)));
        }
        self.comparison(path)
    }

    fn comparison(&mut self, path: AttrPath) -> Result<Filter, String> {
        let Some(Token::Word(op)) = self.next() else {
            return Err("expected an operator".into());
        };
        let op = match op.to_ascii_lowercase().as_str() {
            "pr" => return Ok(Filter::Present(path)),
            "eq" => CompareOp::Eq,
            "ne" => CompareOp::Ne,
            "co" => CompareOp::Co,
            "sw" => CompareOp::Sw,
            "ew" => CompareOp::Ew,
            "gt" => CompareOp::Gt,
            "lt" => CompareOp::Lt,
            "ge" => CompareOp::Ge,
            "le" => CompareOp::Le,
            other => return Err(format!("unknown operator {other:?}")),
        };
        let value = match self.next() {
            Some(Token::Str(s)) => Value::String(s),
            Some(Token::Word(w)) => match w.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => serde_json::from_str::<serde_json::Number>(&w)
                    .map(Value::Number)
                    .map_err(|_| format!("invalid comparison value {w:?}"))?,
            },
            other => return Err(format!("expected a comparison value, found {other:?}")),
        };
        if matches!(value, Value::Bool(_) | Value::Null)
            && !matches!(op, CompareOp::Eq | CompareOp::Ne)
        {
            return Err("booleans and null only support eq and ne".into());
        }
        Ok(Filter::Compare(path, op, value))
    }
}

/// Attributes compared case-sensitively (RFC 7643 `caseExact`).
fn case_exact(name: &str) -> bool {
    ["id", "externalId", "version"]
        .iter()
        .any(|a| a.eq_ignore_ascii_case(name))
}

/// Case-insensitive object key lookup.
pub(super) fn get_ci<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let map = obj.as_object()?;
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// The values `path` refers to in `ctx` (several for multi-valued attributes).
fn resolve<'a>(ctx: &'a Value, path: &AttrPath) -> Vec<&'a Value> {
    let base = match &path.extension {
        None => Some(ctx),
        Some(urn) => get_ci(ctx, urn),
    };
    let Some(top) = base.and_then(|b| get_ci(b, &path.attr)) else {
        return Vec::new();
    };
    let items: Vec<&Value> = match top {
        Value::Array(a) => a.iter().collect(),
        Value::Null => Vec::new(),
        v => vec![v],
    };
    match &path.sub {
        None => items
            .into_iter()
            // A complex multi-valued attribute compares by its `value` sub-attribute.
            .map(|v| {
                if v.is_object() {
                    get_ci(v, "value").unwrap_or(v)
                } else {
                    v
                }
            })
            .collect(),
        Some(sub) => items
            .into_iter()
            .filter_map(|v| get_ci(v, sub))
            .flat_map(|v| match v {
                Value::Array(a) => a.iter().collect(),
                v => vec![v],
            })
            .filter(|v| !v.is_null())
            .collect(),
    }
}

/// Whether `resource` (or a multi-valued element, inside value filters) matches `f`.
pub fn matches(f: &Filter, ctx: &Value) -> bool {
    match f {
        Filter::And(a, b) => matches(a, ctx) && matches(b, ctx),
        Filter::Or(a, b) => matches(a, ctx) || matches(b, ctx),
        Filter::Not(a) => !matches(a, ctx),
        Filter::Present(p) => resolve(ctx, p).iter().any(|v| match v {
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Null => false,
            _ => true,
        }),
        Filter::Compare(p, op, value) => {
            let values = resolve(ctx, p);
            let name = p.sub.as_deref().unwrap_or(&p.attr);
            if value.is_null() {
                let present = !values.is_empty();
                return if *op == CompareOp::Eq {
                    !present
                } else {
                    present
                };
            }
            let exact = case_exact(name);
            if *op == CompareOp::Ne {
                // `ne` matches when no value equals (or the attribute is absent).
                values
                    .iter()
                    .all(|v| compare(v, CompareOp::Ne, value, exact))
            } else {
                values.iter().any(|v| compare(v, *op, value, exact))
            }
        }
        Filter::ValuePath(p, inner) => {
            let base = match &p.extension {
                None => Some(ctx),
                Some(urn) => get_ci(ctx, urn),
            };
            match base.and_then(|b| get_ci(b, &p.attr)) {
                Some(Value::Array(items)) => items.iter().any(|item| matches(inner, item)),
                Some(v @ Value::Object(_)) => matches(inner, v),
                _ => false,
            }
        }
    }
}

fn compare(actual: &Value, op: CompareOp, expected: &Value, exact: bool) -> bool {
    match (actual, expected) {
        (Value::String(a), Value::String(e)) => {
            let (a, e) = if exact {
                (a.clone(), e.clone())
            } else {
                (a.to_lowercase(), e.to_lowercase())
            };
            match op {
                CompareOp::Eq => a == e,
                CompareOp::Ne => a != e,
                CompareOp::Co => a.contains(&e),
                CompareOp::Sw => a.starts_with(&e),
                CompareOp::Ew => a.ends_with(&e),
                CompareOp::Gt => a > e,
                CompareOp::Lt => a < e,
                CompareOp::Ge => a >= e,
                CompareOp::Le => a <= e,
            }
        }
        (Value::Number(a), Value::Number(e)) => {
            let (Some(a), Some(e)) = (a.as_f64(), e.as_f64()) else {
                return false;
            };
            match op {
                CompareOp::Eq => a == e,
                CompareOp::Ne => a != e,
                CompareOp::Gt => a > e,
                CompareOp::Lt => a < e,
                CompareOp::Ge => a >= e,
                CompareOp::Le => a <= e,
                CompareOp::Co | CompareOp::Sw | CompareOp::Ew => false,
            }
        }
        (Value::Bool(a), Value::Bool(e)) => match op {
            CompareOp::Eq => a == e,
            CompareOp::Ne => a != e,
            _ => false,
        },
        // Mismatched types never compare equal.
        _ => op == CompareOp::Ne,
    }
}

/// The SQL pre-filter implied by `f`: set when every matching user must satisfy it.
pub fn narrow(f: &Filter) -> ScimNarrow {
    let mut n = ScimNarrow::default();
    collect(f, &mut n);
    n
}

fn collect(f: &Filter, n: &mut ScimNarrow) {
    match f {
        Filter::And(a, b) => {
            collect(a, n);
            collect(b, n);
        }
        Filter::Compare(p, CompareOp::Eq, Value::String(s)) if p.sub.is_none() => {
            if p.is("userName") {
                n.user_name.get_or_insert_with(|| s.clone());
            } else if p.is("externalId") {
                n.external_id.get_or_insert_with(|| s.clone());
            } else if p.is("emails") {
                n.email.get_or_insert_with(|| s.clone());
            }
        }
        Filter::Compare(p, CompareOp::Eq, Value::String(s))
            if p.is("emails")
                && p.sub
                    .as_deref()
                    .is_some_and(|x| x.eq_ignore_ascii_case("value")) =>
        {
            n.email.get_or_insert_with(|| s.clone());
        }
        Filter::ValuePath(p, inner) if p.is("emails") => {
            if let Some(email) = value_eq(inner) {
                n.email.get_or_insert(email);
            }
        }
        _ => {}
    }
}

/// `value eq "x"` within an `and` chain of a value filter.
fn value_eq(f: &Filter) -> Option<String> {
    match f {
        Filter::And(a, b) => value_eq(a).or_else(|| value_eq(b)),
        Filter::Compare(p, CompareOp::Eq, Value::String(s)) if p.is("value") => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user() -> Value {
        json!({
            "id": "42",
            "userName": "Alice@Example.com",
            "externalId": "00uABC",
            "active": true,
            "name": {"givenName": "Alice", "familyName": "Liddell"},
            "emails": [
                {"value": "alice@example.com", "type": "work", "primary": true},
                {"value": "alice@home.example", "type": "home"}
            ],
            "meta": {"lastModified": "2026-10-01T10:00:00Z"},
            "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User": {
                "employeeNumber": "701984", "department": "Eng"
            }
        })
    }

    fn m(f: &str) -> bool {
        matches(&parse(f).unwrap(), &user())
    }

    #[test]
    fn identity_provider_lookups() {
        assert!(
            m(r#"userName eq "alice@example.com""#),
            "userName is case-insensitive"
        );
        assert!(m(r#"USERNAME Eq "ALICE@EXAMPLE.COM""#));
        assert!(!m(r#"userName eq "bob""#));
        assert!(m(r#"externalId eq "00uABC""#));
        assert!(!m(r#"externalId eq "00uabc""#), "externalId is case-exact");
        assert!(m(r#"emails[type eq "work"].value eq "ALICE@example.com""#));
        assert!(!m(r#"emails[type eq "home"].value eq "alice@example.com""#));
        assert!(m(r#"emails[type eq "work" and value co "alice"]"#));
        assert!(m(r#"emails.value eq "alice@home.example""#));
        assert!(m(r#"emails eq "alice@home.example""#));
        assert!(m(
            r#"urn:ietf:params:scim:schemas:core:2.0:User:userName sw "alice""#
        ));
        assert!(m(
            r#"urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:employeeNumber eq "701984""#
        ));
    }

    #[test]
    fn logic_and_operators() {
        assert!(m(r#"userName eq "x" or externalId eq "00uABC""#));
        assert!(!m(r#"userName eq "x" and externalId eq "00uABC""#));
        assert!(m(r#"not (userName eq "x") and active eq true"#));
        assert!(m(
            r#"(userName eq "x" or name.givenName eq "alice") and active eq true"#
        ));
        assert!(m("title pr or name.familyName pr"));
        assert!(!m("title pr"));
        assert!(m("title eq null"));
        assert!(m(r#"userName ne "bob""#));
        assert!(m(r#"meta.lastModified gt "2026-01-01T00:00:00Z""#));
        assert!(!m(r#"meta.lastModified lt "2026-01-01T00:00:00Z""#));
        assert!(m(r#"userName ew "example.com""#));
        // `and` binds tighter than `or`.
        assert!(m(r#"active eq false and userName eq "x" or id eq "42""#));
    }

    #[test]
    fn rejects_bad_filters() {
        for bad in [
            "",
            "userName",
            r#"userName xx "a""#,
            r#"userName eq "a"#,
            r#"userName eq "a" and"#,
            "active gt true",
            r#"(userName eq "a""#,
            r#"emails[type eq "work""#,
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn narrows_common_lookups() {
        let n = narrow(&parse(r#"userName eq "a" and externalId eq "b""#).unwrap());
        assert_eq!(n.user_name.as_deref(), Some("a"));
        assert_eq!(n.external_id.as_deref(), Some("b"));
        let n = narrow(&parse(r#"emails[type eq "work"].value eq "c@d.e""#).unwrap());
        assert_eq!(n.email.as_deref(), Some("c@d.e"));
        assert_eq!(
            narrow(&parse(r#"userName eq "a" or userName eq "b""#).unwrap()),
            ScimNarrow::default(),
            "or cannot be narrowed"
        );
    }
}
