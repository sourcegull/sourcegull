//! Where the definition query needs to run.
//!
//! Running a query over a whole tree visits every node and costs as much as
//! the parse. Definitions sit at few places, though: [`Prune`] works out,
//! from the query and the grammar's `node-types.json`, at which node kinds
//! a pattern starts (its roots) and below which kinds a root can be, not
//! counting what is below a local (`(kind) @local`, whose definitions are
//! hidden anyway). [`extract`](crate::extract) walks the tree, runs the
//! query only at root nodes (with a start depth of 0), and goes down only
//! where a root can be: past expressions, literals and local function
//! bodies it does not go. It does not go into a definition that hides what
//! it holds either (a function).

use std::collections::{HashMap, HashSet};

use tree_sitter::{CaptureQuantifier, Language, Query};

pub(crate) struct Prune {
    /// Per node kind id: a pattern starts at this kind.
    pub(crate) roots: Vec<bool>,
    /// Per node kind id: a root can be below this kind.
    pub(crate) descend: Vec<bool>,
    /// Per node kind id: only a plain local pattern (`(kind) @local`)
    /// starts at this kind, so it matches without running the query.
    pub(crate) local: Vec<bool>,
}

impl Prune {
    pub(crate) fn new(
        language: &Language,
        query: &Query,
        source: &str,
        local: Option<u32>,
        node_types: &str,
    ) -> Prune {
        let count = language.node_kind_count();
        let everything = Prune {
            roots: vec![true; count],
            descend: vec![true; count],
            local: vec![false; count],
        };
        let mut roots = HashSet::new();
        let mut barriers = HashSet::new();
        // Roots of the patterns other than plain locals.
        let mut other_roots = HashSet::new();
        for p in 0..query.pattern_count() {
            let text = &source[query.start_byte_for_pattern(p)..query.end_byte_for_pattern(p)];
            let Some(kinds) = pattern_roots(text) else {
                return everything;
            };
            let is_local = local.is_some_and(|l| {
                query.capture_quantifiers(p)[l as usize] != CaptureQuantifier::Zero
            });
            if is_local && plain_local(text) {
                barriers.extend(kinds.iter().copied());
            } else {
                other_roots.extend(kinds.iter().copied());
            }
            roots.extend(kinds);
        }
        let Ok(types) = serde_json::from_str::<Vec<serde_json::Value>>(node_types) else {
            return everything;
        };

        // Named children per concrete kind, supertypes expanded.
        let mut subtypes: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for t in &types {
            let Some(name) = named(t) else { continue };
            if let Some(subs) = t["subtypes"].as_array() {
                subtypes.insert(name, subs.iter().filter_map(named).collect());
                continue;
            }
            let mut kids = Vec::new();
            let fields = t["fields"].as_object().into_iter().flat_map(|f| f.values());
            for list in fields.chain(t.get("children")) {
                kids.extend(
                    list["types"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(named),
                );
            }
            children.insert(name, kids);
        }
        fn expand<'a>(
            name: &'a str,
            subtypes: &HashMap<&str, Vec<&'a str>>,
            out: &mut Vec<&'a str>,
        ) {
            match subtypes.get(name) {
                Some(subs) => {
                    for s in subs {
                        if !out.contains(s) {
                            expand(s, subtypes, out);
                        }
                    }
                }
                None => out.push(name),
            }
        }
        let children: HashMap<&str, Vec<&str>> = children
            .into_iter()
            .map(|(k, kids)| {
                let mut out = Vec::new();
                for kid in kids {
                    expand(kid, &subtypes, &mut out);
                }
                (k, out)
            })
            .collect();

        // below(k): a root, or something with a root below it, is a child of
        // k (through no barrier).
        let mut below: HashSet<&str> = HashSet::new();
        loop {
            let mut changed = false;
            for (k, kids) in &children {
                if below.contains(k) || barriers.contains(k) {
                    continue;
                }
                if kids
                    .iter()
                    .any(|c| !barriers.contains(c) && (roots.contains(c) || below.contains(c)))
                {
                    below.insert(k);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut prune = Prune {
            roots: vec![false; count],
            descend: vec![false; count],
            local: vec![false; count],
        };
        for id in 0..count {
            let id16 = id as u16;
            if !language.node_kind_is_named(id16) {
                continue;
            }
            let Some(name) = language.node_kind_for_id(id16) else {
                continue;
            };
            prune.roots[id] = roots.contains(name);
            prune.local[id] = barriers.contains(name) && !other_roots.contains(name);
            prune.descend[id] = if barriers.contains(name) {
                false
            } else if children.contains_key(name) {
                below.contains(name)
            } else {
                // Not in node-types.json (ERROR, aliases): look inside.
                true
            };
        }
        prune
    }
}

/// The name of a named node type in `node-types.json`.
fn named(t: &serde_json::Value) -> Option<&str> {
    (t["named"] == true).then(|| t["type"].as_str()).flatten()
}

/// The identifier at the start of `s`, after whitespace; not `_`.
fn ident(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let name = &s[..end];
    (!name.is_empty() && name != "_").then_some(name)
}

/// The node kinds a pattern starts at, or `None` if it may start anywhere.
fn pattern_roots(text: &str) -> Option<Vec<&str>> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('(') {
        return Some(vec![ident(rest)?]);
    }
    let rest = text.strip_prefix('[')?;
    let mut kinds = Vec::new();
    let mut depth = 0;
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '(' => {
                if depth == 0 {
                    kinds.push(ident(&rest[i + 1..])?);
                }
                depth += 1;
            }
            ')' => depth -= 1,
            ']' if depth == 0 => return Some(kinds),
            // A capture name.
            '@' => {
                for (_, c) in chars.by_ref() {
                    if c.is_whitespace() {
                        break;
                    }
                }
            }
            // A bare string or wildcard alternative.
            '"' | '_' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Whether a local pattern is the root node and nothing more: `(kind)
/// @local` or `[(a) (b)] @local`.
fn plain_local(text: &str) -> bool {
    let Some(head) = text.trim().strip_suffix("@local") else {
        return false;
    };
    let head = head.trim();
    let inner = head
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(head);
    // Only `(kind)` groups, no fields, children or other captures.
    inner.split(')').all(|g| {
        g.trim().is_empty()
            || g.trim().strip_prefix('(').is_some_and(|k| {
                k.trim()
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_of_patterns() {
        assert_eq!(pattern_roots("(a b: (c) @name) @def.x"), Some(vec!["a"]));
        assert_eq!(
            pattern_roots("[\n (a) (b c: (d))\n] @local"),
            Some(vec!["a", "b"])
        );
        assert_eq!(pattern_roots("(_) @x"), None);
        assert_eq!(pattern_roots("[\"fn\" (a)] @x"), None);
        assert!(plain_local("(func_literal) @local"));
        assert!(plain_local("[\n (a) (b)\n] @local"));
        assert!(!plain_local("(f body: (_) @local)"));
        assert!(!plain_local("(f (g)) @local"));
    }
}
