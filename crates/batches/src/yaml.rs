//! YAML to JSON with source positions.
//!
//! Bulk change specs are YAML, but they are validated against a JSON Schema and stored as JSON. This
//! loader converts the first YAML document to a [`serde_json::Value`]: the YAML 1.2 core schema
//! (`on`, `yes` and `no` are strings), scalar keys stringified, merge keys (`<<`) and aliases
//! expanded, duplicate keys rejected. Alongside the value it records where every node starts, keyed
//! by JSON pointer, so that schema and semantic errors can be reported with a line and column.
//!
//! The loader never panics and bounds its work: nesting depth and the number of nodes (after
//! alias expansion) are limited.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};
use serde_json::{Map, Number, Value};

/// A position in the source text; both numbers start at 1. The column counts characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// A YAML syntax or conversion error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlError {
    pub message: String,
    pub location: Option<Location>,
}

/// Maximum nesting of sequences and mappings.
pub const MAX_DEPTH: usize = 128;
/// Maximum number of nodes in the converted document, counting alias expansions.
pub const MAX_NODES: usize = 1_000_000;

/// Where a node of the document starts: its key (for mapping values) and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePosition {
    pub key: Option<Location>,
    pub value: Location,
}

/// Positions of the nodes of a document by JSON pointer (`""` is the root, `/on/0` the first
/// entry of `on`). Nodes copied in through aliases and merge keys have no entry of their own;
/// [`Positions::find`] then answers with the closest ancestor.
#[derive(Debug, Clone, Default)]
pub struct Positions {
    map: HashMap<String, NodePosition>,
}

impl Positions {
    /// The position of the node at `pointer`, or of its closest recorded ancestor.
    pub fn find(&self, pointer: &str) -> Option<NodePosition> {
        let mut p = pointer;
        loop {
            if let Some(pos) = self.map.get(p) {
                return Some(*pos);
            }
            let cut = p.rfind('/')?;
            p = &p[..cut];
        }
    }

    /// Where the value at `pointer` starts.
    pub fn value(&self, pointer: &str) -> Option<Location> {
        self.find(pointer).map(|p| p.value)
    }

    /// Where the key of the mapping entry at `pointer` starts (the value when it has no key).
    pub fn key(&self, pointer: &str) -> Option<Location> {
        self.find(pointer).map(|p| p.key.unwrap_or(p.value))
    }
}

/// Escapes a mapping key as a JSON pointer segment (RFC 6901).
pub fn escape_pointer_segment(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Converts the first document of `text` to JSON. An empty stream is `null`.
pub fn load(text: &str) -> Result<(Value, Positions), YamlError> {
    let first = match Loader::new(text).run() {
        Err(e) if e.message == QUOTED_INDENT => e,
        r => return r,
    };
    // libyaml does not check the indentation of the continuation lines of
    // quoted scalars, so specs like
    //
    //     description: "first line
    //     second line"
    //
    // load there. Leading white space of continuation lines is not part of a quoted scalar's
    // value, so indenting them changes nothing but makes the text valid YAML 1.2. Each round
    // fixes the scalar the parser stopped at; lines and the positions of nodes before the
    // scalar's end stay the same.
    let mut fixed = text.to_owned();
    let mut at = first.location;
    for _ in 0..MAX_QUOTED_FIXES {
        let Some(next) = at.and_then(|l| indent_quoted_scalar(&fixed, l)) else {
            break;
        };
        fixed = next;
        match Loader::new(&fixed).run() {
            Err(e) if e.message == QUOTED_INDENT && e.location != at => at = e.location,
            Err(e) if e.message == QUOTED_INDENT => break,
            r => return r,
        }
    }
    Err(first)
}

/// saphyr's error for a quoted scalar continuation line indented less than its block.
const QUOTED_INDENT: &str = "invalid indentation in quoted scalar";
/// How many quoted scalars [`load`] re-indents before giving up.
const MAX_QUOTED_FIXES: usize = 1000;

/// `text` with the continuation lines of the quoted scalar starting at `at` indented to the
/// scalar's column, or `None` if no quoted scalar starts there or nothing changes. Lines that
/// are document markers (`---`, `...`) are left alone, as libyaml rejects those too.
fn indent_quoted_scalar(text: &str, at: Location) -> Option<String> {
    // The byte offset of `at`.
    let line_start = if at.line <= 1 {
        0
    } else {
        text.match_indices('\n').nth(at.line - 2)?.0 + 1
    };
    let start = line_start
        + text[line_start..]
            .char_indices()
            .nth(at.column - 1)
            .map(|(i, _)| i)?;
    let quote = text[start..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let width = at.column - 1;
    // Find the closing quote and the starts of the lines in between.
    let mut lines = Vec::new();
    let mut chars = text[start + 1..].char_indices().peekable();
    let mut closed = false;
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if quote == '"' => {
                if let Some(&(j, '\n')) = chars.peek() {
                    lines.push(start + 1 + j + 1);
                }
                chars.next();
            }
            '\'' if quote == '\'' && chars.peek().is_some_and(|&(_, n)| n == '\'') => {
                chars.next();
            }
            c if c == quote => {
                closed = true;
                break;
            }
            '\n' => lines.push(start + 1 + i + 1),
            _ => {}
        }
    }
    if !closed {
        return None;
    }
    let mut out = String::with_capacity(text.len() + lines.len() * width);
    let mut last = 0;
    let mut changed = false;
    for l in lines {
        let rest = &text[l..];
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        let marker = (rest.starts_with("---") || rest.starts_with("..."))
            && rest[3..]
                .chars()
                .next()
                .is_none_or(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
        if spaces >= width || marker {
            continue;
        }
        out.push_str(&text[last..l]);
        out.extend(std::iter::repeat_n(' ', width - spaces));
        last = l;
        changed = true;
    }
    out.push_str(&text[last..]);
    changed.then_some(out)
}

/// Turns the parser's character offsets into lines and columns. Offsets mostly increase, so a
/// cursor walks forward from the last answer (and restarts when asked about an earlier offset).
struct Cursor<'a> {
    text: &'a str,
    chars: usize,
    byte: usize,
    line: usize,
    column: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Cursor {
            text,
            chars: 0,
            byte: 0,
            line: 1,
            column: 1,
        }
    }

    fn locate(&mut self, char_index: usize) -> Location {
        if char_index < self.chars {
            *self = Cursor::new(self.text);
        }
        let mut rest = self.text[self.byte..].chars();
        while self.chars < char_index {
            let Some(c) = rest.next() else { break };
            self.chars += 1;
            self.byte += c.len_utf8();
            if c == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        Location {
            line: self.line,
            column: self.column,
        }
    }
}

enum Frame {
    Seq {
        items: Vec<Value>,
        pointer: String,
        anchor: usize,
        location: Location,
        size: usize,
        depth: usize,
    },
    Map {
        map: Map<String, Value>,
        /// Key waiting for its value: name, location, whether it is a merge key.
        pending: Option<(String, Location, bool)>,
        keys: HashMap<String, Location>,
        merges: Vec<(Value, Location)>,
        pointer: String,
        anchor: usize,
        location: Location,
        size: usize,
        depth: usize,
    },
}

struct Anchored {
    value: Value,
    size: usize,
    depth: usize,
}

struct Loader<'a> {
    parser: Parser<'a, saphyr_parser::StrInput<'a>>,
    cursor: Cursor<'a>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, Anchored>,
    positions: Positions,
    nodes: usize,
    root: Option<Value>,
}

/// A finished node on its way into its parent.
struct Done {
    value: Value,
    location: Location,
    anchor: usize,
    size: usize,
    /// Depth of the deepest container inside (0 for scalars).
    depth: usize,
    /// The text of an untagged plain scalar (to recognise the merge key `<<`).
    plain: Option<String>,
    /// The mapping key this node names when it is used as a key.
    key: String,
    is_container: bool,
    /// Whether the position was recorded when the node started.
    recorded: bool,
}

fn err(message: impl Into<String>, location: Location) -> YamlError {
    YamlError {
        message: message.into(),
        location: Some(location),
    }
}

impl<'a> Loader<'a> {
    fn new(text: &'a str) -> Self {
        Loader {
            parser: Parser::new_from_str(text),
            cursor: Cursor::new(text),
            stack: Vec::new(),
            anchors: HashMap::new(),
            positions: Positions::default(),
            nodes: 0,
            root: None,
        }
    }

    fn loc(&mut self, span: &Span) -> Location {
        self.cursor.locate(span.start.index())
    }

    fn run(mut self) -> Result<(Value, Positions), YamlError> {
        let mut in_document = false;
        while let Some(next) = self.parser.next_event() {
            let (event, span) = match next {
                Ok(ok) => ok,
                Err(e) => {
                    return Err(YamlError {
                        message: e.info().to_string(),
                        location: Some(self.cursor.locate(e.marker().index())),
                    });
                }
            };
            let location = self.loc(&span);
            match event {
                Event::Nothing | Event::StreamStart => {}
                Event::StreamEnd => break,
                Event::DocumentStart(_) => in_document = true,
                Event::DocumentEnd => {
                    if in_document {
                        // Only the first document counts.
                        break;
                    }
                }
                Event::Alias(id) => {
                    let Some(anchored) = self.anchors.get(&id) else {
                        return Err(err("unknown anchor", location));
                    };
                    let (size, depth) = (anchored.size, anchored.depth);
                    if self.stack.len() + depth > MAX_DEPTH {
                        return Err(err("document is nested too deeply", location));
                    }
                    self.count(size, location)?;
                    let value = self.anchors[&id].value.clone();
                    let is_container = value.is_array() || value.is_object();
                    let key = if is_container {
                        String::new()
                    } else {
                        key_name(&value)
                    };
                    self.finish(Done {
                        value,
                        location,
                        anchor: 0,
                        size,
                        depth,
                        plain: None,
                        key,
                        is_container,
                        recorded: false,
                    })?;
                }
                Event::Scalar(text, style, anchor, tag) => {
                    self.count(1, location)?;
                    let value = resolve_scalar(&text, style, tag.as_deref())
                        .map_err(|m| err(m, location))?;
                    let plain =
                        (style == ScalarStyle::Plain && tag.is_none()).then(|| text.to_string());
                    let key = key_name(&value);
                    self.finish(Done {
                        value,
                        location,
                        anchor,
                        size: 1,
                        depth: 0,
                        plain,
                        key,
                        is_container: false,
                        recorded: false,
                    })?;
                }
                Event::SequenceStart(anchor, _) => {
                    self.open(location)?;
                    let pointer = self.child_pointer();
                    self.record(&pointer, location);
                    let depth = self.stack.len() + 1;
                    self.stack.push(Frame::Seq {
                        items: Vec::new(),
                        pointer,
                        anchor,
                        location,
                        size: 1,
                        depth,
                    });
                }
                Event::MappingStart(anchor, _) => {
                    self.open(location)?;
                    let pointer = self.child_pointer();
                    self.record(&pointer, location);
                    let depth = self.stack.len() + 1;
                    self.stack.push(Frame::Map {
                        map: Map::new(),
                        pending: None,
                        keys: HashMap::new(),
                        merges: Vec::new(),
                        pointer,
                        anchor,
                        location,
                        size: 1,
                        depth,
                    });
                }
                Event::SequenceEnd => {
                    let Some(Frame::Seq {
                        items,
                        anchor,
                        location,
                        size,
                        depth,
                        ..
                    }) = self.stack.pop()
                    else {
                        return Err(err("unbalanced sequence", location));
                    };
                    let below = self.stack.len();
                    self.finish(Done {
                        value: Value::Array(items),
                        location,
                        anchor,
                        size,
                        depth: depth - below,
                        plain: None,
                        key: String::new(),
                        is_container: true,
                        recorded: true,
                    })?;
                }
                Event::MappingEnd => {
                    let Some(Frame::Map {
                        mut map,
                        merges,
                        anchor,
                        location,
                        size,
                        depth,
                        ..
                    }) = self.stack.pop()
                    else {
                        return Err(err("unbalanced mapping", location));
                    };
                    // Merge keys: explicit keys win, then earlier merge sources win.
                    for (source, at) in merges {
                        let sources = match source {
                            Value::Object(m) => vec![m],
                            Value::Array(items) => {
                                let mut maps = Vec::new();
                                for item in items {
                                    match item {
                                        Value::Object(m) => maps.push(m),
                                        _ => {
                                            return Err(err(
                                                "map merge requires a mapping or a list of mappings",
                                                at,
                                            ));
                                        }
                                    }
                                }
                                maps
                            }
                            _ => {
                                return Err(err(
                                    "map merge requires a mapping or a list of mappings",
                                    at,
                                ));
                            }
                        };
                        for m in sources {
                            for (k, v) in m {
                                map.entry(k).or_insert(v);
                            }
                        }
                    }
                    let below = self.stack.len();
                    self.finish(Done {
                        value: Value::Object(map),
                        location,
                        anchor,
                        size,
                        depth: depth - below,
                        plain: None,
                        key: String::new(),
                        is_container: true,
                        recorded: true,
                    })?;
                }
            }
        }
        if !self.stack.is_empty() {
            return Err(YamlError {
                message: "unexpected end of document".into(),
                location: None,
            });
        }
        let root = self.root.unwrap_or(Value::Null);
        if !self.positions.map.contains_key("") {
            self.positions.map.insert(
                String::new(),
                NodePosition {
                    key: None,
                    value: Location { line: 1, column: 1 },
                },
            );
        }
        Ok((root, self.positions))
    }

    fn count(&mut self, n: usize, location: Location) -> Result<(), YamlError> {
        self.nodes = self.nodes.saturating_add(n);
        if self.nodes > MAX_NODES {
            return Err(err(
                format!("document has more than {MAX_NODES} nodes (after expanding aliases)"),
                location,
            ));
        }
        Ok(())
    }

    fn open(&mut self, location: Location) -> Result<(), YamlError> {
        self.count(1, location)?;
        if self.stack.len() >= MAX_DEPTH {
            return Err(err("document is nested too deeply", location));
        }
        if self.root.is_some() && self.stack.is_empty() {
            return Err(err("unexpected node after the document root", location));
        }
        Ok(())
    }

    /// The pointer of the node that is about to start under the top of the stack.
    fn child_pointer(&self) -> String {
        match self.stack.last() {
            None => String::new(),
            Some(Frame::Seq { items, pointer, .. }) => format!("{pointer}/{}", items.len()),
            Some(Frame::Map {
                pending: Some((key, _, _)),
                pointer,
                ..
            }) => format!("{pointer}/{}", escape_pointer_segment(key)),
            // A container used as a key; it is rejected when it ends.
            Some(Frame::Map { pointer, .. }) => format!("{pointer}/?"),
        }
    }

    fn record(&mut self, pointer: &str, location: Location) {
        let key = match self.stack.last() {
            Some(Frame::Map {
                pending: Some((_, at, _)),
                ..
            }) => Some(*at),
            _ => None,
        };
        self.positions
            .map
            .entry(pointer.to_string())
            .or_insert(NodePosition {
                key,
                value: location,
            });
    }

    fn finish(&mut self, done: Done) -> Result<(), YamlError> {
        if done.anchor != 0 {
            self.anchors.insert(
                done.anchor,
                Anchored {
                    value: done.value.clone(),
                    size: done.size,
                    depth: done.depth,
                },
            );
        }
        if !done.recorded {
            let pointer = self.child_pointer();
            let is_key = matches!(self.stack.last(), Some(Frame::Map { pending: None, .. }));
            if !is_key {
                self.record(&pointer, done.location);
            }
        }
        match self.stack.last_mut() {
            None => {
                if self.root.is_some() {
                    return Err(err(
                        "unexpected node after the document root",
                        done.location,
                    ));
                }
                self.root = Some(done.value);
            }
            Some(Frame::Seq { items, size, .. }) => {
                *size += done.size;
                items.push(done.value);
            }
            Some(Frame::Map {
                map,
                pending,
                keys,
                merges,
                size,
                ..
            }) => {
                *size += done.size;
                match pending.take() {
                    None => {
                        if done.is_container {
                            return Err(err("mapping keys must be scalars", done.location));
                        }
                        let is_merge = done.plain.as_deref() == Some("<<") && done.anchor == 0;
                        let name = done.key;
                        if !is_merge {
                            if let Some(first) = keys.get(&name) {
                                return Err(err(
                                    format!(
                                        "mapping key {name:?} already defined at line {}",
                                        first.line
                                    ),
                                    done.location,
                                ));
                            }
                            keys.insert(name.clone(), done.location);
                        }
                        *pending = Some((name, done.location, is_merge));
                    }
                    Some((_, _, true)) => {
                        merges.push((done.value, done.location));
                    }
                    Some((name, _, false)) => {
                        map.insert(name, done.value);
                    }
                }
            }
        }
        Ok(())
    }
}

/// The mapping key a scalar names (non-string keys are stringified).
fn key_name(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Resolves a scalar with the YAML 1.2 core schema.
fn resolve_scalar(text: &str, style: ScalarStyle, tag: Option<&Tag>) -> Result<Value, String> {
    if let Some(tag) = tag {
        let core = tag.is_yaml_core_schema() || tag.handle == "!!";
        if core {
            return match tag.suffix.as_str() {
                "str" | "binary" | "timestamp" => Ok(Value::String(text.to_string())),
                "null" => Ok(Value::Null),
                "bool" => match resolve_plain(text) {
                    v @ Value::Bool(_) => Ok(v),
                    _ => Err(format!("cannot decode {text:?} as a !!bool")),
                },
                "int" => match resolve_plain(text) {
                    Value::Number(n) if !n.is_f64() => Ok(Value::Number(n)),
                    _ => Err(format!("cannot decode {text:?} as an !!int")),
                },
                "float" => match resolve_plain(text) {
                    Value::Number(n) => Ok(Value::Number(n)),
                    _ => Err(format!("cannot decode {text:?} as a !!float")),
                },
                "map" | "seq" => Err(format!("cannot decode a scalar as a !!{}", tag.suffix)),
                _ => Ok(resolve_style(text, style)),
            };
        }
    }
    let v = resolve_style(text, style);
    if let Value::String(s) = &v {
        if style == ScalarStyle::Plain && is_special_float(s) {
            return Err(format!("{s} cannot be represented in JSON"));
        }
    }
    Ok(v)
}

fn resolve_style(text: &str, style: ScalarStyle) -> Value {
    if style == ScalarStyle::Plain {
        resolve_plain(text)
    } else {
        Value::String(text.to_string())
    }
}

fn is_special_float(s: &str) -> bool {
    matches!(
        s,
        ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    )
}

fn resolve_plain(text: &str) -> Value {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = parse_int(text) {
        return Value::Number(n);
    }
    if is_float(text) {
        if let Ok(f) = text.parse::<f64>() {
            if let Some(n) = Number::from_f64(f) {
                return Value::Number(n);
            }
        }
    }
    Value::String(text.to_string())
}

fn parse_int(text: &str) -> Option<Number> {
    let (neg, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, body) = if let Some(r) = digits.strip_prefix("0x") {
        (16, r)
    } else if let Some(r) = digits.strip_prefix("0o") {
        (8, r)
    } else if let Some(r) = digits.strip_prefix("0b") {
        (2, r)
    } else {
        (10, digits)
    };
    if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let magnitude = u64::from_str_radix(body, radix).ok();
    match (neg, magnitude) {
        (false, Some(m)) => Some(Number::from(m)),
        (true, Some(m)) if m <= i64::MAX as u64 + 1 => {
            Some(Number::from(0i64.wrapping_sub_unsigned(m)))
        }
        // Too large for 64 bits: fall back to a float.
        _ if radix == 10 => text.parse::<f64>().ok().and_then(Number::from_f64),
        _ => None,
    }
}

fn is_float(text: &str) -> bool {
    // [-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?
    let b = text.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - s;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let s = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn value(text: &str) -> Value {
        load(text).unwrap().0
    }

    #[test]
    fn core_schema_scalars() {
        assert_eq!(
            value(
                "a: 1\nb: -2\nc: 1.5\nd: true\ne: ~\nf: yes\ng: on\nh: '1'\ni: 0x1f\nj: 1e3\nk:\n"
            ),
            json!({"a": 1, "b": -2, "c": 1.5, "d": true, "e": null, "f": "yes", "g": "on",
                   "h": "1", "i": 31, "j": 1000.0, "k": null})
        );
        assert_eq!(
            value("on: x\n1: y\ntrue: z\n"),
            json!({"on": "x", "1": "y", "true": "z"})
        );
        assert_eq!(
            value("a: !!str 12\nb: !!int '7'\n"),
            json!({"a": "12", "b": 7})
        );
    }

    #[test]
    fn positions() {
        let (_, pos) = load("name: x\non:\n  - repository: a\n    branch: b\n").unwrap();
        assert_eq!(pos.value("/name"), Some(Location { line: 1, column: 7 }));
        assert_eq!(pos.key("/name"), Some(Location { line: 1, column: 1 }));
        assert_eq!(
            pos.value("/on/0/branch"),
            Some(Location {
                line: 4,
                column: 13
            })
        );
        assert_eq!(
            pos.key("/on/0/branch"),
            Some(Location { line: 4, column: 5 })
        );
        assert_eq!(pos.value("/on/0/missing/deeper"), pos.value("/on/0"));
        // Columns count characters.
        let (_, pos) = load("é: ü\nx: y\n").unwrap();
        assert_eq!(pos.value("/é"), Some(Location { line: 1, column: 4 }));
        assert_eq!(pos.value("/x"), Some(Location { line: 2, column: 4 }));
    }

    #[test]
    fn aliases_and_merges() {
        assert_eq!(
            value("base: &b {x: 1, y: 2}\nother:\n  <<: *b\n  y: 3\nlist: [*b]\n"),
            json!({"base": {"x": 1, "y": 2}, "other": {"x": 1, "y": 3}, "list": [{"x": 1, "y": 2}]})
        );
    }

    #[test]
    fn errors() {
        let e = load("a: 1\nb: 2\na: 3\n").unwrap_err();
        assert_eq!(e.message, "mapping key \"a\" already defined at line 1");
        assert_eq!(e.location, Some(Location { line: 3, column: 1 }));
        assert!(load("a: [1, 2\n").is_err());
        assert!(load("? [a]\n: b\n").is_err());
        assert!(load("a: .inf\n").is_err());
        let laughs = "a: &a [x, x, x, x, x, x, x, x, x, x]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nc: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nd: &d [*c, *c, *c, *c, *c, *c, *c, *c, *c, *c]\ne: &e [*d, *d, *d, *d, *d, *d, *d, *d, *d, *d]\nf: &f [*e, *e, *e, *e, *e, *e, *e, *e, *e, *e]\ng: [*f, *f, *f, *f, *f, *f, *f, *f, *f, *f]\n";
        assert!(load(laughs).unwrap_err().message.contains("nodes"));
        let deep = "[".repeat(500) + &"]".repeat(500);
        assert!(load(&deep).is_err());
    }

    #[test]
    fn unindented_quoted_continuation_lines() {
        // As libyaml: continuation lines of quoted scalars may be indented less than their
        // block; their leading white space is not part of the value either way.
        let text = "name: x\ndescription: \"one\n\ntwo \\\"q\\\"\nthree\"\nb: 'it''s\nok'\nc:\n  d: \"x\ny\"\n  e: 1\n";
        let (v, pos) = load(text).unwrap();
        assert_eq!(
            v,
            json!({"name": "x", "description": "one\ntwo \"q\" three", "b": "it's ok", "c": {"d": "x y", "e": 1}})
        );
        assert_eq!(pos.value("/b"), Some(Location { line: 6, column: 4 }));
        assert_eq!(
            pos.key("/c/e"),
            Some(Location {
                line: 11,
                column: 3
            })
        );
        // An escaped line break.
        assert_eq!(value("a: \"x\\\ny\"\n"), json!({"a": "xy"}));
        // Document markers still end the stream, and other errors stay.
        assert!(load("a: \"x\n---\ny\"\n").is_err());
        let e = load("a: \"x\n").unwrap_err();
        assert!(e.message.contains("end of stream"), "{e:?}");
    }

    #[test]
    fn first_document_only() {
        assert_eq!(value("a: 1\n---\nb: 2\n"), json!({"a": 1}));
        assert_eq!(value(""), Value::Null);
        assert_eq!(value("# just a comment\n"), Value::Null);
    }
}
