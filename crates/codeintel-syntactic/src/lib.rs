//! Syntactic code intelligence: local definitions and references
//! resolved through tree-sitter scopes, in the shape of SCIP occurrences with `local N`
//! symbols.
//!
//! A query per language ([`queries`]) marks scopes, definitions and references; the
//! resolver here is language independent. A reference resolves to the nearest enclosing
//! scope that has a definition of its name visible at the reference. Only locals are
//! reported: names defined in the file's top scope, and names that resolve nowhere, are left
//! to search-based navigation.

mod queries;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

/// Bumped when results change, so caches drop old entries.
pub const VERSION: u32 = 1;

/// Files larger than this are not analyzed.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// One occurrence of a local symbol on one line. Columns count UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Occurrence {
    /// 0-based line.
    pub line: u32,
    pub start: u32,
    /// Exclusive.
    pub end: u32,
    /// The symbol: `local {symbol}` in SCIP terms, numbered in order of first definition.
    pub symbol: u32,
    pub definition: bool,
}

/// The local symbols of a file, occurrences sorted by position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Locals {
    pub occurrences: Vec<Occurrence>,
}

impl Locals {
    /// The occurrence covering UTF-16 column `col` of `line`.
    pub fn at(&self, line: u32, col: u32) -> Option<&Occurrence> {
        let from = self.occurrences.partition_point(|o| o.line < line);
        self.occurrences[from..]
            .iter()
            .take_while(|o| o.line == line)
            .find(|o| o.start <= col && col < o.end)
    }

    /// Every occurrence of `symbol`, in order.
    pub fn of(&self, symbol: u32) -> impl Iterator<Item = &Occurrence> {
        self.occurrences.iter().filter(move |o| o.symbol == symbol)
    }
}

/// The SCIP symbol string of a local.
pub fn scip_symbol(symbol: u32) -> String {
    format!("local {symbol}")
}

struct Grammar {
    language: Language,
    query: Query,
    scope: Option<u32>,
    definition: Option<u32>,
    shadow: Option<u32>,
    hoisted: Option<u32>,
    parent: Option<u32>,
    after: Option<u32>,
    reference: Option<u32>,
    ignore: Option<u32>,
}

impl Grammar {
    fn new(id: &str, language: impl Into<Language>, source: &str) -> Grammar {
        let language = language.into();
        let query = Query::new(&language, source)
            .unwrap_or_else(|e| panic!("{id}: invalid scope query: {e}"));
        let index = |n: &str| query.capture_index_for_name(n);
        Grammar {
            scope: index("scope"),
            definition: index("definition"),
            shadow: index("definition.shadow"),
            hoisted: index("definition.hoisted"),
            parent: index("definition.parent"),
            after: index("after"),
            reference: index("reference"),
            ignore: index("ignore"),
            language,
            query,
        }
    }
}

static GRAMMARS: LazyLock<HashMap<&'static str, Grammar>> = LazyLock::new(|| {
    let ts = queries::typescript();
    HashMap::from([
        (
            "go",
            Grammar::new("go", tree_sitter_go::LANGUAGE, queries::GO),
        ),
        (
            "rust",
            Grammar::new("rust", tree_sitter_rust::LANGUAGE, queries::RUST),
        ),
        (
            "python",
            Grammar::new("python", tree_sitter_python::LANGUAGE, queries::PYTHON),
        ),
        (
            "java",
            Grammar::new("java", tree_sitter_java::LANGUAGE, queries::JAVA),
        ),
        (
            "javascript",
            Grammar::new(
                "javascript",
                tree_sitter_javascript::LANGUAGE,
                &queries::javascript(),
            ),
        ),
        (
            "typescript",
            Grammar::new(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                &ts,
            ),
        ),
        (
            "tsx",
            Grammar::new("tsx", tree_sitter_typescript::LANGUAGE_TSX, &ts),
        ),
        (
            "c",
            Grammar::new("c", tree_sitter_c::LANGUAGE, &queries::c()),
        ),
        (
            "cpp",
            Grammar::new("cpp", tree_sitter_cpp::LANGUAGE, &queries::cpp()),
        ),
    ])
});

/// Whether locals are resolved for the language id (`sourcegull_languages::Language::id`).
pub fn supported(lang: &str) -> bool {
    GRAMMARS.contains_key(lang)
}

/// The language ids with scope queries.
pub fn languages() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = GRAMMARS.keys().copied().collect();
    v.sort_unstable();
    v
}

thread_local! {
    static PARSERS: RefCell<HashMap<&'static str, Parser>> = RefCell::new(HashMap::new());
}

/// How a definition was captured.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DefKind {
    Merge,
    Shadow,
    Parent,
}

/// A definition in a scope: visible from a byte, as a symbol.
type Visible = (usize, usize, DefKind);

struct Def {
    start: usize,
    end: usize,
    kind: DefKind,
    /// Visible to references at or after this byte (0 for the whole scope).
    visible_from: usize,
}

/// Resolves the locals of `source` in language `lang`; `None` without a scope query, for
/// files over [`MAX_BYTES`] or when parsing fails.
pub fn analyze(lang: &str, source: &[u8]) -> Option<Locals> {
    let (&id, grammar) = GRAMMARS.get_key_value(lang)?;
    if source.len() > MAX_BYTES {
        return None;
    }
    let tree = PARSERS.with(|p| {
        let mut parsers = p.borrow_mut();
        let parser = parsers.entry(id).or_insert_with(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&grammar.language)
                .expect("grammar version matches tree-sitter");
            parser
        });
        parser.parse(source, None)
    })?;
    Some(resolve(grammar, tree.root_node(), source))
}

fn resolve(g: &Grammar, root: Node<'_>, source: &[u8]) -> Locals {
    let mut scopes: Vec<(usize, usize)> = Vec::new();
    let mut defs: BTreeMap<(usize, usize), Def> = BTreeMap::new();
    let mut refs: HashSet<(usize, usize)> = HashSet::new();
    let mut ignored: HashSet<(usize, usize)> = HashSet::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&g.query, root, source);
    while let Some(m) = matches.next() {
        let after = m
            .captures()
            .iter()
            .find(|c| Some(c.index) == g.after)
            .map(|c| c.node.end_byte());
        for c in m.captures() {
            let r = (c.node.start_byte(), c.node.end_byte());
            let i = Some(c.index);
            let kind = if i == g.definition || i == g.hoisted {
                Some(DefKind::Merge)
            } else if i == g.shadow {
                Some(DefKind::Shadow)
            } else if i == g.parent {
                Some(DefKind::Parent)
            } else {
                None
            };
            if let Some(kind) = kind {
                let visible_from = if kind == DefKind::Parent || i == g.hoisted {
                    0
                } else {
                    after.unwrap_or(r.0)
                };
                defs.entry(r)
                    .and_modify(|d| d.visible_from = d.visible_from.max(visible_from))
                    .or_insert(Def {
                        start: r.0,
                        end: r.1,
                        kind,
                        visible_from,
                    });
            } else if i == g.scope {
                scopes.push(r);
            } else if i == g.reference {
                refs.insert(r);
            } else if i == g.ignore {
                ignored.insert(r);
            }
        }
    }

    // Scope tree: index 0 is the file. Sorted by start, outer before inner.
    scopes.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    scopes.dedup();
    let mut tree: Vec<(usize, usize, usize)> = vec![(0, source.len(), 0)]; // (start, end, parent)
    let mut stack = vec![0usize];
    for &(s, e) in &scopes {
        while stack.len() > 1 && tree[*stack.last().expect("non-empty")].1 <= s {
            stack.pop();
        }
        let parent = *stack.last().expect("root");
        tree.push((s, e, parent));
        stack.push(tree.len() - 1);
    }
    let starts: Vec<usize> = tree.iter().map(|t| t.0).collect();
    // The innermost scope containing [s, e): an ancestor of the last scope starting at or
    // before `s`, since scopes nest.
    let scope_of = |s: usize, e: usize| -> usize {
        let mut i = starts.partition_point(|&x| x <= s).saturating_sub(1);
        while i != 0 && !(tree[i].0 <= s && e <= tree[i].1) {
            i = tree[i].2;
        }
        i
    };

    // name → scope → definitions
    let mut table: HashMap<&[u8], HashMap<usize, Vec<Visible>>> = HashMap::new();
    let mut def_symbol: Vec<((usize, usize), usize)> = Vec::new();
    let mut next = 0usize;
    for d in defs.values() {
        let mut scope = scope_of(d.start, d.end);
        if d.kind == DefKind::Parent {
            scope = tree[scope].2;
        }
        if scope == 0 {
            continue;
        }
        let name = &source[d.start..d.end];
        let entries = table.entry(name).or_default().entry(scope).or_default();
        let symbol = match (d.kind, entries.iter().find(|e| e.2 == DefKind::Merge)) {
            (DefKind::Merge, Some(e)) => e.1,
            _ => {
                next += 1;
                next - 1
            }
        };
        entries.push((d.visible_from, symbol, d.kind));
        def_symbol.push(((d.start, d.end), symbol));
    }

    let mut found: Vec<((usize, usize), usize, bool)> =
        def_symbol.iter().map(|&(r, s)| (r, s, true)).collect();
    for &r in &refs {
        if defs.contains_key(&r) || ignored.contains(&r) {
            continue;
        }
        let name = &source[r.0..r.1];
        let Some(by_scope) = table.get(name) else {
            continue;
        };
        let mut scope = scope_of(r.0, r.1);
        while scope != 0 {
            if let Some(entries) = by_scope.get(&scope) {
                let best = entries
                    .iter()
                    .filter(|e| e.0 <= r.0)
                    .max_by_key(|e| e.0)
                    .map(|e| e.1);
                if let Some(symbol) = best {
                    found.push((r, symbol, false));
                    break;
                }
            }
            scope = tree[scope].2;
        }
    }

    // Number symbols by first definition, and convert to lines and UTF-16 columns.
    found.sort_by_key(|f| f.0);
    let mut renumber: HashMap<usize, u32> = HashMap::new();
    for f in found.iter().filter(|f| f.2) {
        let n = u32::try_from(renumber.len()).unwrap_or(u32::MAX);
        renumber.entry(f.1).or_insert(n);
    }
    let lines = LineIndex::new(source);
    let occurrences = found
        .into_iter()
        .filter_map(|((s, e), sym, definition)| {
            let (line, start) = lines.position(source, s);
            let (end_line, end) = lines.position(source, e);
            (line == end_line).then(|| Occurrence {
                line,
                start,
                end,
                symbol: renumber[&sym],
                definition,
            })
        })
        .collect();
    Locals { occurrences }
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { starts }
    }

    /// (line, UTF-16 column) of byte `at`.
    fn position(&self, source: &[u8], at: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|&s| s <= at) - 1;
        let prefix = String::from_utf8_lossy(&source[self.starts[line]..at]);
        let col: usize = prefix.chars().map(char::len_utf16).sum();
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(col).unwrap_or(u32::MAX),
        )
    }
}

#[cfg(test)]
mod tests;
