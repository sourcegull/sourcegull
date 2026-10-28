//! The functions templates can call: Go `text/template`'s builtins and the bulk change spec
//! functions `join`, `split`, `replace`, `join_if` and `matches`.

use std::cmp::Ordering;

use crate::glob::Glob;
use crate::gofmt;
use crate::value::Value;

/// The bulk change spec functions added to Go's builtins.
pub const SRC_CLI_FUNCTIONS: &[&str] = &["join", "split", "replace", "join_if", "matches"];

/// Go's builtin template functions that are implemented.
pub const GO_BUILTINS: &[&str] = &[
    "and", "or", "not", "len", "index", "slice", "print", "printf", "println", "html", "js",
    "urlquery", "eq", "ne", "lt", "le", "gt", "ge",
];

/// Go builtins that are not implemented (there are no function values to `call`).
pub const UNSUPPORTED_BUILTINS: &[&str] = &["call"];

pub(crate) enum Arity {
    Exact(usize),
    AtLeast(usize),
}

pub(crate) fn arity(name: &str) -> Option<Arity> {
    Some(match name {
        "and" | "or" | "eq" | "index" | "slice" | "printf" | "join_if" => Arity::AtLeast(1),
        "print" | "println" | "html" | "js" | "urlquery" => Arity::AtLeast(0),
        "not" | "len" => Arity::Exact(1),
        "ne" | "lt" | "le" | "gt" | "ge" | "join" | "split" | "matches" => Arity::Exact(2),
        "replace" => Arity::Exact(3),
        _ => return None,
    })
}

pub(crate) fn is_builtin(name: &str) -> bool {
    arity(name).is_some()
}

/// Checks the argument count the way Go reports it.
pub(crate) fn check_arity(name: &str, got: usize) -> Result<(), String> {
    match arity(name) {
        Some(Arity::Exact(n)) if got != n => Err(format!(
            "wrong number of args for {name}: want {n} got {got}"
        )),
        Some(Arity::AtLeast(n)) if got < n => Err(format!(
            "wrong number of args for {name}: want at least {n} got {got}"
        )),
        _ => Ok(()),
    }
}

/// A function failure: `Type` errors are reported as is (Go checks argument types before
/// calling), `Call` errors as `error calling <name>: …`.
pub(crate) enum CallError {
    Type(String),
    Call(String),
}

fn string_arg(v: &Value) -> Result<String, CallError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Nil => Err(CallError::Type("invalid value; expected string".to_owned())),
        other => Err(CallError::Type(format!(
            "wrong type for value; expected string; got {}",
            other.go_type()
        ))),
    }
}

fn string_list_arg(v: &Value) -> Result<Vec<String>, CallError> {
    v.as_string_list().ok_or_else(|| {
        CallError::Type(format!(
            "wrong type for value; expected []string; got {}",
            v.go_type()
        ))
    })
}

/// Calls builtin `name` (not `and`/`or`, which the executor short-circuits) with
/// evaluated arguments whose count has been checked.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Value, CallError> {
    match name {
        "not" => Ok(Value::Bool(!args[0].is_true())),
        "len" => len(&args[0]).map(Value::Int).map_err(CallError::Call),
        "index" => index(&args[0], &args[1..]).map_err(CallError::Call),
        "slice" => slice(&args[0], &args[1..]).map_err(CallError::Call),
        "print" => Ok(Value::String(gofmt::sprint(args))),
        "println" => Ok(Value::String(gofmt::sprintln(args))),
        "printf" => {
            let f = string_arg(&args[0])?;
            Ok(Value::String(gofmt::sprintf(&f, &args[1..])))
        }
        "html" => Ok(Value::String(html_escape(&eval_args(args)))),
        "js" => Ok(Value::String(js_escape(&eval_args(args)))),
        "urlquery" => Ok(Value::String(query_escape(&eval_args(args)))),
        "eq" => eq(&args[0], &args[1..])
            .map(Value::Bool)
            .map_err(CallError::Call),
        "ne" => eq(&args[0], &args[1..2])
            .map(|b| Value::Bool(!b))
            .map_err(CallError::Call),
        "lt" => lt(&args[0], &args[1])
            .map(Value::Bool)
            .map_err(CallError::Call),
        "le" => le(&args[0], &args[1])
            .map(Value::Bool)
            .map_err(CallError::Call),
        "gt" => le(&args[0], &args[1])
            .map(|b| Value::Bool(!b))
            .map_err(CallError::Call),
        "ge" => lt(&args[0], &args[1])
            .map(|b| Value::Bool(!b))
            .map_err(CallError::Call),
        "join" => {
            let elems = string_list_arg(&args[0])?;
            let sep = string_arg(&args[1])?;
            Ok(Value::String(elems.join(&sep)))
        }
        "split" => {
            let s = string_arg(&args[0])?;
            let sep = string_arg(&args[1])?;
            Ok(Value::strings(split(&s, &sep)))
        }
        "replace" => {
            let s = string_arg(&args[0])?;
            let old = string_arg(&args[1])?;
            let new = string_arg(&args[2])?;
            Ok(Value::String(replace_all(&s, &old, &new)))
        }
        "join_if" => {
            let sep = string_arg(&args[0])?;
            let elems = args[1..]
                .iter()
                .map(string_arg)
                .collect::<Result<Vec<_>, _>>()?;
            let non_blank: Vec<String> = elems.into_iter().filter(|e| !e.is_empty()).collect();
            Ok(Value::String(non_blank.join(&sep)))
        }
        "matches" => {
            let input = string_arg(&args[0])?;
            let pattern = string_arg(&args[1])?;
            let g = Glob::compile(&pattern).map_err(CallError::Call)?;
            Ok(Value::Bool(g.matches(&input)))
        }
        other => Err(CallError::Call(format!("function {other:?} not defined"))),
    }
}

/// `strings.Split`: an empty separator splits into characters.
fn split(s: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        return s.chars().map(String::from).collect();
    }
    s.split(sep).map(str::to_owned).collect()
}

/// `strings.ReplaceAll`: an empty `old` inserts `new` around every character.
fn replace_all(s: &str, old: &str, new: &str) -> String {
    if !old.is_empty() {
        return s.replace(old, new);
    }
    let mut out = String::with_capacity(s.len() + new.len() * (s.chars().count() + 1));
    out.push_str(new);
    for c in s.chars() {
        out.push(c);
        out.push_str(new);
    }
    out
}

fn len(v: &Value) -> Result<i64, String> {
    Ok(match v {
        Value::String(s) => s.len() as i64,
        Value::List(l) => l.len() as i64,
        Value::PathList(l) => l.len() as i64,
        Value::Map(m) => m.len() as i64,
        Value::Nil => return Err("len of nil pointer".to_owned()),
        other => return Err(format!("len of type {}", other.go_type())),
    })
}

fn int_index(i: &Value) -> Result<i64, String> {
    match i {
        Value::Int(i) => Ok(*i),
        Value::Nil => Err("cannot index slice/array with nil".to_owned()),
        other => Err(format!(
            "cannot index slice/array with type {}",
            other.go_type()
        )),
    }
}

fn index(item: &Value, indexes: &[Value]) -> Result<Value, String> {
    if matches!(item, Value::Nil) {
        return Err("index of untyped nil".to_owned());
    }
    let mut cur = item.clone();
    for ix in indexes {
        cur = match &cur {
            Value::Nil => return Err("index of nil pointer".to_owned()),
            Value::List(l) => {
                let i = int_index(ix)?;
                usize::try_from(i)
                    .ok()
                    .and_then(|i| l.get(i))
                    .cloned()
                    .ok_or_else(|| format!("index out of range: {i}"))?
            }
            Value::PathList(l) => {
                let i = int_index(ix)?;
                usize::try_from(i)
                    .ok()
                    .and_then(|i| l.get(i))
                    .map(|s| Value::String(s.clone()))
                    .ok_or_else(|| format!("index out of range: {i}"))?
            }
            Value::String(s) => {
                let i = int_index(ix)?;
                usize::try_from(i)
                    .ok()
                    .and_then(|i| s.as_bytes().get(i))
                    .map(|b| Value::Int(*b as i64))
                    .ok_or_else(|| format!("index out of range: {i}"))?
            }
            Value::Map(m) => match ix {
                Value::String(k) => m.get(k).cloned().unwrap_or(Value::Nil),
                Value::Nil => Value::Nil,
                other => {
                    return Err(format!(
                        "value has type {}; should be string",
                        other.go_type()
                    ));
                }
            },
            other => return Err(format!("can't index item of type {}", other.go_type())),
        };
    }
    Ok(cur)
}

fn slice(item: &Value, indexes: &[Value]) -> Result<Value, String> {
    if matches!(item, Value::Nil) {
        return Err("slice of untyped nil".to_owned());
    }
    if indexes.len() > 3 {
        return Err(format!("too many slice indexes: {}", indexes.len()));
    }
    let len = match item {
        Value::String(s) => {
            if indexes.len() == 3 {
                return Err("cannot 3-index slice a string".to_owned());
            }
            s.len()
        }
        Value::List(l) => l.len(),
        Value::PathList(l) => l.len(),
        other => return Err(format!("can't slice item of type {}", other.go_type())),
    };
    let mut idx = [0usize, len, len];
    for (i, v) in indexes.iter().enumerate() {
        let x = int_index(v)?;
        let x = usize::try_from(x)
            .ok()
            .filter(|&x| x <= len)
            .ok_or_else(|| format!("index out of range: {x}"))?;
        idx[i] = x;
    }
    if idx[0] > idx[1] {
        return Err(format!("invalid slice index: {} > {}", idx[0], idx[1]));
    }
    if indexes.len() == 3 && idx[1] > idx[2] {
        return Err(format!("invalid slice index: {} > {}", idx[1], idx[2]));
    }
    let (lo, hi) = (idx[0], idx[1]);
    Ok(match item {
        Value::String(s) => match s.get(lo..hi) {
            Some(sub) => Value::String(sub.to_owned()),
            None => Value::String(String::from_utf8_lossy(&s.as_bytes()[lo..hi]).into_owned()),
        },
        Value::List(l) => Value::List(l[lo..hi].to_vec()),
        Value::PathList(l) => Value::PathList(l[lo..hi].to_vec()),
        _ => Value::Nil,
    })
}

#[derive(PartialEq)]
enum Kind {
    Bool,
    Int,
    Float,
    String,
    Other,
}

fn kind(v: &Value) -> Kind {
    match v {
        Value::Bool(_) => Kind::Bool,
        Value::Int(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::String(_) => Kind::String,
        _ => Kind::Other,
    }
}

const BAD_COMPARISON: &str = "incompatible types for comparison";
const BAD_COMPARISON_TYPE: &str = "invalid type for comparison";

/// Go's `eq arg1 arg2…`: true if `arg1` equals any of the others.
fn eq(a: &Value, others: &[Value]) -> Result<bool, String> {
    if others.is_empty() {
        return Err("missing argument for comparison".to_owned());
    }
    for b in others {
        let truth = match (a, b) {
            (Value::Nil, Value::Nil) => true,
            (Value::Nil, _) | (_, Value::Nil) => false,
            _ if kind(a) != kind(b) => return Err(BAD_COMPARISON.to_owned()),
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::String(x), Value::String(y)) => x == y,
            (x, _) => return Err(format!("non-comparable type {x}: {}", x.go_type())),
        };
        if truth {
            return Ok(true);
        }
    }
    Ok(false)
}

fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>, String> {
    for v in [a, b] {
        if matches!(kind(v), Kind::Other) {
            return Err(BAD_COMPARISON_TYPE.to_owned());
        }
    }
    if kind(a) != kind(b) {
        return Err(BAD_COMPARISON.to_owned());
    }
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Some(x.cmp(y))),
        (Value::Float(x), Value::Float(y)) => Ok(x.partial_cmp(y)),
        (Value::String(x), Value::String(y)) => Ok(Some(x.as_bytes().cmp(y.as_bytes()))),
        _ => Err(BAD_COMPARISON_TYPE.to_owned()),
    }
}

fn lt(a: &Value, b: &Value) -> Result<bool, String> {
    Ok(compare(a, b)? == Some(Ordering::Less))
}

fn le(a: &Value, b: &Value) -> Result<bool, String> {
    if lt(a, b)? {
        return Ok(true);
    }
    eq(a, std::slice::from_ref(b))
}

/// The text `html`, `js` and `urlquery` escape: a single string as is, else `fmt.Sprint`
/// of the arguments with nil printed as `<no value>` (Go's `evalArgs`).
fn eval_args(args: &[Value]) -> String {
    if let [Value::String(s)] = args {
        return s.clone();
    }
    let printable: Vec<Value> = args
        .iter()
        .map(|a| match a {
            Value::Nil => Value::from("<no value>"),
            other => other.clone(),
        })
        .collect();
    gofmt::sprint(&printable)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\0' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            '`' => out.push_str("\\u0060"),
            c if (c as u32) < 0x20 || c.is_control() => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
