//! sourcegull-symbols: symbol definitions from source files.
//!
//! [`extract`] parses a file with its tree-sitter grammar and runs the
//! language's definition query ([`queries`](crate::queries)): functions,
//! methods, types, fields, constants and so on, each with its kind (LSP
//! numbering, [`SymbolKind`]), the byte range of its name and the name of
//! its container (the enclosing type, module or `impl`). Functions directly
//! inside a class, struct, interface, enum or `impl` become methods.
//! Definitions inside function bodies are locals and are left out; to save
//! most of the parse, the bodies' insides are found by a light lexer
//! ([`skip`](crate::skip)) and not parsed at all (nor C attribute macros,
//! macro calls' arguments and parameter lists), and the query runs only at
//! nodes where a definition can start ([`walk`](crate::walk)).
//!
//! Compiled in: Rust, Go, Python, Java, JavaScript, TypeScript, TSX, C,
//! C++, shell, C#, Kotlin, Ruby, Scala, PHP, Swift, Lua, Zig and Elixir.
//! Symbols depend only on the content and the language, so callers cache
//! them by blob OID and [`VERSION`].
//!
//! Languages without a grammar here get no symbols; there is no
//! `universal-ctags` fallback.

mod queries;
mod skip;
mod walk;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use sourcegull_types::SymbolKind;
use tree_sitter::{Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

/// Kinds whose functions are methods.
fn holds_methods(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum
    )
}

/// The kind for a query capture `@def.<suffix>`.
fn kind_from_capture(suffix: &str) -> Option<SymbolKind> {
    SymbolKind::from_name(suffix).filter(|k| *k != SymbolKind::Unknown)
}

/// Extractor version; bump when output changes.
pub const VERSION: u32 = 3;

/// Larger files are not parsed (512 KiB).
pub const MAX_BYTES: usize = 512 << 10;

/// A symbol definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The enclosing definition's name, if any.
    pub container: Option<String>,
    /// Byte range of the name in the file.
    pub range: Range<u32>,
}

struct Grammar {
    language: Language,
    skip: Option<skip::Rules>,
    query: Query,
    name: u32,
    scope: Option<u32>,
    local: Option<u32>,
    container: Option<u32>,
    /// Per capture index, the kind of a `@def.<kind>` capture.
    kinds: Vec<Option<SymbolKind>>,
    prune: walk::Prune,
}

impl Grammar {
    fn new(
        id: &str,
        language: tree_sitter_language::LanguageFn,
        source: &str,
        node_types: &str,
    ) -> Grammar {
        let language: Language = language.into();
        let query = Query::new(&language, source)
            .unwrap_or_else(|e| panic!("symbol query for {id} does not compile: {e}"));
        let names = query.capture_names();
        let index = |n: &str| names.iter().position(|c| *c == n).map(|i| i as u32);
        let kinds = names
            .iter()
            .map(|c| {
                let suffix = c.strip_prefix("def.")?;
                let kind = kind_from_capture(suffix);
                assert!(kind.is_some(), "{id}: unknown kind @{c}");
                kind
            })
            .collect();
        let local = index("local");
        Grammar {
            skip: skip::Rules::for_lang(id),
            prune: walk::Prune::new(&language, &query, source, local, node_types),
            name: index("name").unwrap_or_else(|| panic!("{id}: no @name capture")),
            scope: index("scope"),
            local,
            container: index("container"),
            kinds,
            language,
            query,
        }
    }
}

/// The grammars, each set up on first use.
type LazyGrammar = LazyLock<Grammar, fn() -> Grammar>;

macro_rules! grammar {
    ($id:literal, $language:expr, $query:expr, $node_types:expr) => {
        (
            $id,
            LazyGrammar::new(|| Grammar::new($id, $language, &$query, $node_types)),
        )
    };
}

static GRAMMARS: LazyLock<HashMap<&'static str, LazyGrammar>> = LazyLock::new(|| {
    use queries as q;
    HashMap::from([
        grammar!(
            "rust",
            tree_sitter_rust::LANGUAGE,
            q::RUST,
            tree_sitter_rust::NODE_TYPES
        ),
        grammar!(
            "go",
            tree_sitter_go::LANGUAGE,
            q::GO,
            tree_sitter_go::NODE_TYPES
        ),
        grammar!(
            "python",
            tree_sitter_python::LANGUAGE,
            q::PYTHON,
            tree_sitter_python::NODE_TYPES
        ),
        grammar!(
            "java",
            tree_sitter_java::LANGUAGE,
            q::JAVA,
            tree_sitter_java::NODE_TYPES
        ),
        grammar!(
            "javascript",
            tree_sitter_javascript::LANGUAGE,
            q::javascript(),
            tree_sitter_javascript::NODE_TYPES
        ),
        grammar!(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            q::typescript(),
            tree_sitter_typescript::TYPESCRIPT_NODE_TYPES
        ),
        grammar!(
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX,
            q::typescript(),
            tree_sitter_typescript::TSX_NODE_TYPES
        ),
        grammar!(
            "c",
            tree_sitter_c::LANGUAGE,
            q::c(),
            tree_sitter_c::NODE_TYPES
        ),
        grammar!(
            "cpp",
            tree_sitter_cpp::LANGUAGE,
            q::cpp(),
            tree_sitter_cpp::NODE_TYPES
        ),
        grammar!(
            "shell",
            tree_sitter_bash::LANGUAGE,
            q::SHELL,
            tree_sitter_bash::NODE_TYPES
        ),
        grammar!(
            "csharp",
            tree_sitter_c_sharp::LANGUAGE,
            q::CSHARP,
            tree_sitter_c_sharp::NODE_TYPES
        ),
        grammar!(
            "kotlin",
            tree_sitter_kotlin_sg::LANGUAGE,
            q::KOTLIN,
            tree_sitter_kotlin_sg::NODE_TYPES
        ),
        grammar!(
            "ruby",
            tree_sitter_ruby::LANGUAGE,
            q::RUBY,
            tree_sitter_ruby::NODE_TYPES
        ),
        grammar!(
            "scala",
            tree_sitter_scala::LANGUAGE,
            q::SCALA,
            tree_sitter_scala::NODE_TYPES
        ),
        grammar!(
            "php",
            tree_sitter_php::LANGUAGE_PHP,
            q::PHP,
            tree_sitter_php::PHP_NODE_TYPES
        ),
        grammar!(
            "swift",
            tree_sitter_swift::LANGUAGE,
            q::SWIFT,
            tree_sitter_swift::NODE_TYPES
        ),
        grammar!(
            "lua",
            tree_sitter_lua::LANGUAGE,
            q::LUA,
            tree_sitter_lua::NODE_TYPES
        ),
        grammar!(
            "zig",
            tree_sitter_zig::LANGUAGE,
            q::ZIG,
            tree_sitter_zig::NODE_TYPES
        ),
        grammar!(
            "elixir",
            tree_sitter_elixir::LANGUAGE,
            q::ELIXIR,
            tree_sitter_elixir::NODE_TYPES
        ),
    ])
});

/// Whether symbols are extracted for the language id (as in
/// `sourcegull_languages::Language::id`).
pub fn supported(lang: &str) -> bool {
    GRAMMARS.contains_key(lang)
}

thread_local! {
    static PARSERS: RefCell<HashMap<&'static str, Parser>> = RefCell::new(HashMap::new());
}

/// Parses `content`, leaving out what the lexer finds (function bodies, C
/// macros) where the result has no syntax errors from that: none at a
/// body, or none at all for bodies found from indentation.
fn parse(
    parser: &mut Parser,
    grammar: &Grammar,
    content: &[u8],
    skip: bool,
) -> (Option<Tree>, Parsed) {
    let parsed = match grammar.skip.filter(|_| skip) {
        None => Parsed::Full,
        Some(rules) => match skip::bodies(rules, content) {
            None => Parsed::LexerGaveUp,
            Some(b) if b.is_empty() => Parsed::NoBodies,
            Some(skipped) => {
                let ranges = included(content, &merge(&skipped.bodies, &skipped.hidden));
                let bodies = skipped.bodies;
                parser
                    .set_included_ranges(&ranges)
                    .expect("ranges are sorted and disjoint");
                let tree = parser.parse(content, None);
                parser
                    .set_included_ranges(&[])
                    .expect("the whole file is a valid range");
                match tree {
                    Some(tree)
                        if !(if rules.strict() {
                            tree.root_node().has_error()
                        } else {
                            blames_skipping(&tree, &bodies)
                        }) =>
                    {
                        return (Some(tree), Parsed::BodiesSkipped);
                    }
                    _ => Parsed::Retried,
                }
            }
        },
    };
    (parser.parse(content, None), parsed)
}

/// Two sorted lists of disjoint ranges, merged.
fn merge(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.iter().cloned().peekable(), b.iter().cloned().peekable());
    loop {
        let next = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x.start <= y.start => a.next(),
            (Some(_), Some(_)) => b.next(),
            (Some(_), None) => a.next(),
            (None, _) => b.next(),
        };
        match next {
            Some(r) => out.push(r),
            None => return out,
        }
    }
}

/// Whether a syntax error in `tree`, parsed with `bodies` left out, may
/// come from leaving them out: an error that holds one brace of a body but
/// not the other, or a missing token at a body. Errors elsewhere (C macros
/// the grammar does not know, say) are in the full parse too.
fn blames_skipping(tree: &Tree, bodies: &[Range<usize>]) -> bool {
    let root = tree.root_node();
    if !root.has_error() {
        return false;
    }
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            let (s, e) = (node.start_byte(), node.end_byte());
            // Bodies whose `}` is at or after the error's start.
            let first = bodies.partition_point(|b| b.end < s);
            for b in bodies[first..].iter().take_while(|b| b.start - 1 <= e) {
                let (open, close) = (b.start - 1, b.end);
                let holds = |at: usize| s <= at && at < e;
                if holds(open) != holds(close) || (s == e && open <= s && s <= close + 1) {
                    return true;
                }
            }
        }
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return false;
            }
        }
    }
}

/// The ranges of `content` outside `bodies` (sorted and disjoint), for
/// [`Parser::set_included_ranges`].
fn included(content: &[u8], bodies: &[Range<usize>]) -> Vec<tree_sitter::Range> {
    // Byte offsets to points, scanning forward once.
    let mut row = 0;
    let mut line_start = 0;
    let mut scanned = 0;
    let mut point = |at: usize| {
        for nl in memchr::memchr_iter(b'\n', &content[scanned..at]) {
            row += 1;
            line_start = scanned + nl + 1;
        }
        scanned = at;
        Point::new(row, at - line_start)
    };
    let mut out = Vec::with_capacity(bodies.len() + 1);
    let (mut start, mut start_point) = (0, Point::new(0, 0));
    for b in bodies {
        let end_point = point(b.start);
        out.push(tree_sitter::Range {
            start_byte: start,
            end_byte: b.start,
            start_point,
            end_point,
        });
        start = b.end;
        start_point = point(b.end);
    }
    out.push(tree_sitter::Range {
        start_byte: start,
        end_byte: content.len(),
        start_point,
        end_point: point(content.len()),
    });
    out
}

/// What a query match found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefKind {
    /// A definition.
    Symbol(SymbolKind),
    /// A node that only names a container (`@scope`).
    Scope,
    /// A node whose definitions are locals (`@local`).
    Local,
}

impl DefKind {
    /// Whether nothing nested in this is reported: function bodies are
    /// local.
    fn hides(self) -> bool {
        matches!(
            self,
            DefKind::Local
                | DefKind::Symbol(
                    SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
                )
        )
    }
}

/// A definition found by the query, before nesting.
struct Def<'t> {
    node: Node<'t>,
    /// `None` for a local.
    name: Option<Range<usize>>,
    kind: DefKind,
    container: Option<Range<usize>>,
    pattern: usize,
}

/// The symbols defined in `content`, a file in language `lang`, in the
/// order of their definitions. Empty for unsupported languages, files over
/// [`MAX_BYTES`] and files that fail to parse.
pub fn extract(lang: &str, content: &[u8]) -> Vec<Symbol> {
    extract_traced(lang, content, true).0
}

/// How [`extract_traced`] parsed a file.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Parsed {
    /// Not parsed: no grammar, too large, or the parser gave up.
    No,
    /// Function bodies (or C macros, parameters) were left out.
    BodiesSkipped,
    /// In full: no body rules for the language, or skipping not asked for.
    Full,
    /// In full: the lexer found nothing to leave out.
    NoBodies,
    /// In full: the body lexer gave up.
    LexerGaveUp,
    /// In full, again: leaving out the bodies gave syntax errors.
    Retried,
}

/// [`extract`], reporting how the file was parsed; `skip: false` always
/// parses in full (benchmarks and tests compare the two).
#[doc(hidden)]
pub fn extract_traced(lang: &str, content: &[u8], skip: bool) -> (Vec<Symbol>, Parsed) {
    let Some((&id, grammar)) = GRAMMARS.get_key_value(lang) else {
        return (Vec::new(), Parsed::No);
    };
    if content.len() > MAX_BYTES {
        return (Vec::new(), Parsed::No);
    }
    let grammar: &Grammar = grammar;
    let (tree, parsed) = PARSERS.with(|p| {
        let mut parsers = p.borrow_mut();
        let parser = parsers.entry(id).or_insert_with(|| {
            let mut p = Parser::new();
            p.set_language(&grammar.language)
                .expect("grammar version is supported");
            p
        });
        parse(parser, grammar, content, skip)
    });
    let Some(tree) = tree else {
        return (Vec::new(), Parsed::No);
    };
    let defs = find_defs(grammar, &tree, content);
    (nest(defs, content), parsed)
}

/// Definitions by node id; the first pattern matching a node wins.
type Defs<'t> = rustc_hash::FxHashMap<usize, Def<'t>>;

/// Runs the query at the root nodes of `tree` that [`walk::Prune`] says
/// may hold definitions.
fn find_defs<'t>(grammar: &Grammar, tree: &'t Tree, content: &[u8]) -> Defs<'t> {
    let mut defs = Defs::default();
    let mut query = QueryCursor::new();
    query.set_max_start_depth(Some(0));
    let prune = &grammar.prune;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let kind = usize::from(node.kind_id());
        let mut down = prune.descend.get(kind).copied().unwrap_or(true) || node.has_error();
        if prune.local.get(kind).copied().unwrap_or(false) {
            // What the query would find: the node, a local.
            defs.insert(
                node.id(),
                Def {
                    node,
                    name: None,
                    kind: DefKind::Local,
                    container: None,
                    pattern: usize::MAX,
                },
            );
            down = false;
        } else if prune.roots.get(kind).copied().unwrap_or(true) {
            add_matches(grammar, &mut query, node, content, &mut defs);
            // Nothing in a function body or a local is reported.
            if defs.get(&node.id()).is_some_and(|d| d.kind.hides()) {
                down = false;
            }
        }
        if down && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return defs;
            }
        }
    }
}

/// Adds the query's matches at `node` (all of them for the root node with a
/// cursor without a start depth limit) to `defs`.
fn add_matches<'t>(
    grammar: &Grammar,
    cursor: &mut QueryCursor,
    node: Node<'t>,
    content: &[u8],
    defs: &mut Defs<'t>,
) {
    let mut matches = cursor.matches(&grammar.query, node, content);
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut def = None;
        let mut container = None;
        for c in m.captures() {
            if c.index == grammar.name {
                name = Some(c.node.byte_range());
            }
            if Some(c.index) == grammar.container {
                container = Some(c.node.byte_range());
            }
            if Some(c.index) == grammar.scope {
                def = Some((c.node, DefKind::Scope));
            } else if Some(c.index) == grammar.local {
                def = Some((c.node, DefKind::Local));
            } else if let Some(kind) = grammar.kinds[c.index as usize] {
                def = Some((c.node, DefKind::Symbol(kind)));
            }
        }
        let Some((node, kind)) = def else {
            continue;
        };
        if name.is_none() && kind != DefKind::Local {
            continue;
        }
        let entry = Def {
            node,
            name,
            kind,
            container,
            pattern: m.pattern_index,
        };
        match defs.get(&node.id()) {
            Some(d) if d.pattern <= entry.pattern => {}
            _ => {
                defs.insert(node.id(), entry);
            }
        }
    }
}

/// Nests definitions into their containers; drops what function bodies and
/// locals hold.
fn nest(defs: Defs<'_>, content: &[u8]) -> Vec<Symbol> {
    let mut defs: Vec<Def<'_>> = defs.into_values().collect();
    defs.sort_by_key(|d| (d.node.start_byte(), std::cmp::Reverse(d.node.end_byte())));
    let text = |r: &Range<usize>| String::from_utf8_lossy(&content[r.clone()]).into_owned();
    let mut out = Vec::new();
    // Enclosing definitions: (end byte, name, kind).
    let mut stack: Vec<(usize, String, DefKind)> = Vec::new();
    // How many entries of `stack` hide what they hold.
    let mut hiding = 0;
    for d in defs {
        while stack
            .last()
            .is_some_and(|(end, ..)| *end <= d.node.start_byte())
        {
            if stack.pop().is_some_and(|(_, _, k)| k.hides()) {
                hiding -= 1;
            }
        }
        if hiding > 0 {
            // Inside a function body or a local: neither reported nor a
            // container, but it still hides what it holds.
            if d.kind.hides() {
                hiding += 1;
                stack.push((d.node.end_byte(), String::new(), d.kind));
            }
            continue;
        }
        let name = d.name.as_ref().map(text).unwrap_or_default();
        let parent = stack.last();
        if let DefKind::Symbol(kind) = d.kind
            && let Some(range) = &d.name
        {
            let in_type = parent.is_some_and(|(_, _, k)| match k {
                DefKind::Scope => true,
                DefKind::Symbol(k) => holds_methods(*k),
                DefKind::Local => false,
            });
            let kind = if kind == SymbolKind::Function && in_type {
                SymbolKind::Method
            } else {
                kind
            };
            let container = match &d.container {
                Some(r) => Some(text(r)),
                None => parent.map(|(_, n, _)| n.clone()),
            };
            out.push(Symbol {
                name: name.clone(),
                kind,
                container,
                range: range.start as u32..range.end as u32,
            });
        }
        if d.kind.hides() {
            hiding += 1;
        }
        stack.push((d.node.end_byte(), name, d.kind));
    }
    out
}

#[cfg(test)]
mod tests;
