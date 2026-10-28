//! sourcegull-languages: language detection and the grammar registry hook.
//!
//! [`detect`] picks a language from a path and, optionally, the first bytes
//! of the file: exact filename, then shebang, then extension, with content
//! heuristics for ambiguous extensions (`.h`, `.m`, `.pl`, `.v`, `.ts`).
//! The table ([`LANGUAGES`]) covers about 110 languages.
//!
//! Highlighting, symbols and syntactic code intel attach per-language data
//! (tree-sitter grammars and queries) through a [`GrammarRegistry`] keyed by
//! the stable [`Language::id`], so this crate does not depend on tree-sitter.

mod table;

use std::collections::HashMap;
use std::sync::LazyLock;

pub use table::LANGUAGES;

/// Broad category of a language (Linguist's `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageKind {
    /// General-purpose and scripting languages.
    Programming,
    /// Markup and styling (HTML, CSS, TeX).
    Markup,
    /// Data and configuration formats (JSON, YAML, SQL).
    Data,
    /// Prose (Markdown, plain text).
    Prose,
}

/// A language.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Language {
    /// Stable lowercase identifier (`rust`, `cpp`, `csharp`). Used as the key
    /// in caches, the registry and the `lang:` search filter.
    pub id: &'static str,
    /// Display name (`Rust`, `C++`, `C#`).
    pub name: &'static str,
    /// Category.
    pub kind: LanguageKind,
    /// File extensions without the dot. Lowercase unless case matters (`C`, `H`, `S`).
    pub extensions: &'static [&'static str],
    /// Exact file names (`Makefile`, `Dockerfile`).
    pub filenames: &'static [&'static str],
    /// Shebang interpreters (`python3`, `node`).
    pub interpreters: &'static [&'static str],
}

struct Index {
    by_id: HashMap<&'static str, &'static Language>,
    by_name: HashMap<String, &'static Language>,
    by_ext: HashMap<&'static str, Vec<&'static Language>>,
    by_filename: HashMap<&'static str, &'static Language>,
    by_interpreter: HashMap<&'static str, &'static Language>,
}

static INDEX: LazyLock<Index> = LazyLock::new(|| {
    let mut idx = Index {
        by_id: HashMap::new(),
        by_name: HashMap::new(),
        by_ext: HashMap::new(),
        by_filename: HashMap::new(),
        by_interpreter: HashMap::new(),
    };
    for lang in LANGUAGES {
        idx.by_id.insert(lang.id, lang);
        idx.by_name.insert(lang.name.to_lowercase(), lang);
        idx.by_name.insert(lang.id.to_owned(), lang);
        for ext in lang.extensions {
            idx.by_ext.entry(ext).or_default().push(lang);
        }
        for f in lang.filenames {
            idx.by_filename.entry(f).or_insert(lang);
        }
        for i in lang.interpreters {
            idx.by_interpreter.entry(i).or_insert(lang);
        }
    }
    idx
});

/// Looks up a language by its [`Language::id`].
pub fn by_id(id: &str) -> Option<&'static Language> {
    INDEX.by_id.get(id).copied()
}

/// Looks up a language by display name or id, case-insensitively
/// (`"c++"`, `"C++"`, `"cpp"`).
pub fn by_name(name: &str) -> Option<&'static Language> {
    INDEX.by_name.get(&name.to_lowercase()).copied()
}

/// All languages using the extension `ext` (without the dot), most likely
/// first. Exact case is tried first (`.C` is C++), then lowercase.
pub fn by_extension(ext: &str) -> &'static [&'static Language] {
    if let Some(v) = INDEX.by_ext.get(ext) {
        return v;
    }
    INDEX
        .by_ext
        .get(ext.to_ascii_lowercase().as_str())
        .map_or(&[], Vec::as_slice)
}

/// The language for an exact file name (`Makefile`).
pub fn by_filename(name: &str) -> Option<&'static Language> {
    INDEX.by_filename.get(name).copied()
}

/// The language named by a shebang line (`#!/usr/bin/env python3`).
pub fn by_shebang(content: &[u8]) -> Option<&'static Language> {
    let first = content.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(first.strip_prefix(b"#!")?).ok()?;
    let mut words = line.split_whitespace();
    let mut prog = words.next()?.rsplit('/').next()?;
    if prog == "env" {
        // Skip env options such as `-S`.
        prog = words.find(|w| !w.starts_with('-') && !w.contains('='))?;
    }
    if let Some(l) = INDEX.by_interpreter.get(prog) {
        return Some(l);
    }
    // `python3.12` → `python3` → `python`.
    let trimmed = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    INDEX.by_interpreter.get(trimmed).copied()
}

/// Detects the language of the file at `path` (repo-relative or bare name).
/// `content`, if given, should be the start of the file (a few KB is
/// enough); it enables shebang detection and disambiguation.
pub fn detect(path: &str, content: Option<&[u8]>) -> Option<&'static Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some(l) = by_filename(name) {
        return Some(l);
    }
    let ext = extension(name);
    let candidates = ext.map(by_extension).unwrap_or(&[]);
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }
    if let Some(content) = content {
        if candidates.is_empty() {
            if let Some(l) = by_shebang(content) {
                return Some(l);
            }
        } else if let Some(l) = disambiguate(ext.unwrap_or_default(), content) {
            return Some(l);
        }
    }
    if let Some(&first) = candidates.first() {
        return Some(first);
    }
    // Dockerfile.prod, Makefile.common, …
    let stem = name.split('.').next().unwrap_or(name);
    if stem != name {
        if let Some(l) = by_filename(stem) {
            return Some(l);
        }
    }
    None
}

/// The extension of a file name: the part after the last dot, unless the
/// name starts with its only dot (`.bashrc` has none).
fn extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then_some(ext)
}

/// Content heuristics for ambiguous extensions (a small subset of
/// Linguist's `heuristics.yml` ideas, written from scratch).
fn disambiguate(ext: &str, content: &[u8]) -> Option<&'static Language> {
    let head = &content[..content.len().min(16 * 1024)];
    let text = String::from_utf8_lossy(head);
    let has = |needle: &str| text.contains(needle);
    let id = match ext.to_ascii_lowercase().as_str() {
        "h" => {
            if has("@interface") || has("@protocol") || has("#import ") {
                "objective-c"
            } else if has("namespace ")
                || has("template <")
                || has("template<")
                || has("std::")
                || has("class ") && has("public:")
                || has("constexpr ")
            {
                "cpp"
            } else {
                "c"
            }
        }
        "m" => {
            if has("@interface") || has("@implementation") || has("#import") || has("#include") {
                "objective-c"
            } else if text.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with('%') || l.starts_with("function ") || l == "end"
            }) {
                "matlab"
            } else {
                "objective-c"
            }
        }
        "pl" => {
            if has(":- ") || text.lines().any(|l| l.trim_end().ends_with(":-")) {
                "prolog"
            } else {
                "perl"
            }
        }
        "v" => {
            if has("Proof.") || has("Qed.") || has("Theorem ") || has("Require Import") {
                "coq"
            } else {
                "verilog"
            }
        }
        "ts" => {
            if text.trim_start().starts_with("<?xml") || has("<!DOCTYPE TS") {
                "xml"
            } else {
                "typescript"
            }
        }
        "star" | "bzl" => "starlark",
        _ => return by_shebang(content),
    };
    by_id(id)
}

/// Error from [`GrammarRegistry::register`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown language id {0:?}")]
pub struct UnknownLanguage(pub String);

/// Per-language data registered by other crates (tree-sitter grammars and
/// queries for highlighting, symbols, syntactic code intel), keyed by
/// [`Language::id`].
#[derive(Debug, Clone)]
pub struct GrammarRegistry<G> {
    entries: HashMap<&'static str, G>,
}

impl<G> Default for GrammarRegistry<G> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<G> GrammarRegistry<G> {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `grammar` for the language `id`, replacing an earlier one.
    pub fn register(&mut self, id: &str, grammar: G) -> Result<(), UnknownLanguage> {
        let lang = by_id(id).ok_or_else(|| UnknownLanguage(id.to_owned()))?;
        self.entries.insert(lang.id, grammar);
        Ok(())
    }

    /// The grammar for `lang`.
    pub fn get(&self, lang: &Language) -> Option<&G> {
        self.entries.get(lang.id)
    }

    /// The grammar for the language id `id`.
    pub fn get_by_id(&self, id: &str) -> Option<&G> {
        self.entries.get(id)
    }

    /// Detects the language of `path` and returns it with its grammar, if registered.
    pub fn detect(&self, path: &str, content: Option<&[u8]>) -> Option<(&'static Language, &G)> {
        let lang = detect(path, content)?;
        Some((lang, self.entries.get(lang.id)?))
    }

    /// Ids of languages with a registered grammar.
    pub fn languages(&self) -> impl Iterator<Item = &'static Language> + '_ {
        self.entries.keys().filter_map(|id| by_id(id))
    }

    /// Number of registered grammars.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: &str, content: Option<&str>) -> Option<&'static str> {
        detect(path, content.map(str::as_bytes)).map(|l| l.id)
    }

    #[test]
    fn table_is_consistent() {
        assert!(LANGUAGES.len() >= 100, "{}", LANGUAGES.len());
        let mut ids = std::collections::HashSet::new();
        for l in LANGUAGES {
            assert!(ids.insert(l.id), "duplicate id {}", l.id);
            assert_eq!(l.id, l.id.to_lowercase());
            for e in l.extensions {
                assert!(!e.starts_with('.') && !e.is_empty(), "{}: {e}", l.id);
            }
        }
    }

    #[test]
    fn detects_by_extension_and_filename() {
        assert_eq!(id("src/main.rs", None), Some("rust"));
        assert_eq!(id("a/b/c.go", None), Some("go"));
        assert_eq!(id("App.TSX", None), Some("tsx"));
        assert_eq!(id("index.mjs", None), Some("javascript"));
        assert_eq!(id("x.hpp", None), Some("cpp"));
        assert_eq!(id("x.C", None), Some("cpp"));
        assert_eq!(id("x.c", None), Some("c"));
        assert_eq!(id("Makefile", None), Some("makefile"));
        assert_eq!(id("deploy/Dockerfile", None), Some("dockerfile"));
        assert_eq!(id("Dockerfile.prod", None), Some("dockerfile"));
        assert_eq!(id("CMakeLists.txt", None), Some("cmake"));
        assert_eq!(id("notes.txt", None), Some("text"));
        assert_eq!(id("BUILD.bazel", None), Some("starlark"));
        assert_eq!(id("defs.bzl", None), Some("starlark"));
        assert_eq!(id("Cargo.lock", None), Some("toml"));
        assert_eq!(id(".bashrc", None), Some("shell"));
        assert_eq!(id(".gitignore", None), Some("ignore-list"));
        assert_eq!(id("README.md", None), Some("markdown"));
        assert_eq!(id("conf.yml", None), Some("yaml"));
        assert_eq!(id("noext", None), None);
        assert_eq!(id("weird.zzzz", None), None);
    }

    #[test]
    fn detects_shebangs() {
        assert_eq!(
            id("bin/tool", Some("#!/usr/bin/env python3\nprint()")),
            Some("python")
        );
        assert_eq!(id("run", Some("#!/bin/bash\necho")), Some("shell"));
        assert_eq!(
            id("run", Some("#!/usr/bin/env -S node --flag\n")),
            Some("javascript")
        );
        assert_eq!(
            id("run", Some("#!/usr/local/bin/python3.12\n")),
            Some("python")
        );
        assert_eq!(id("run", Some("#!/usr/bin/env ruby\n")), Some("ruby"));
        assert_eq!(id("run", Some("no shebang")), None);
        // Extension wins over shebang when unambiguous.
        assert_eq!(id("x.rb", Some("#!/bin/sh\n")), Some("ruby"));
    }

    #[test]
    fn disambiguates_by_content() {
        assert_eq!(id("x.h", None), Some("c"));
        assert_eq!(id("x.h", Some("int f(void);")), Some("c"));
        assert_eq!(id("x.h", Some("namespace a { class B {}; }")), Some("cpp"));
        assert_eq!(
            id(
                "x.h",
                Some("#import <Foundation/Foundation.h>\n@interface A\n")
            ),
            Some("objective-c")
        );
        assert_eq!(
            id("x.m", Some("@implementation A\n@end\n")),
            Some("objective-c")
        );
        assert_eq!(
            id("x.m", Some("function y = f(x)\n  y = x;\nend\n")),
            Some("matlab")
        );
        assert_eq!(id("x.pl", Some("use strict;\nmy $x = 1;\n")), Some("perl"));
        assert_eq!(
            id("x.pl", Some("parent(a, b).\nanc(X,Y) :- parent(X,Y).\n")),
            Some("prolog")
        );
        assert_eq!(
            id("x.v", Some("module top(input clk);\nendmodule\n")),
            Some("verilog")
        );
        assert_eq!(
            id("x.v", Some("Theorem t : True.\nProof. trivial. Qed.\n")),
            Some("coq")
        );
        assert_eq!(
            id("x.ts", Some("<?xml version=\"1.0\"?>\n<!DOCTYPE TS>")),
            Some("xml")
        );
        assert_eq!(id("x.ts", Some("export const a = 1;")), Some("typescript"));
    }

    #[test]
    fn lookups_by_name() {
        assert_eq!(by_name("C++").unwrap().id, "cpp");
        assert_eq!(by_name("cpp").unwrap().name, "C++");
        assert_eq!(by_name("javascript").unwrap().name, "JavaScript");
        assert_eq!(by_name("c#").unwrap().id, "csharp");
        assert!(by_name("klingon").is_none());
        assert_eq!(by_extension("h").len(), 2);
        assert!(by_extension("nope").is_empty());
    }

    #[test]
    fn grammar_registry() {
        let mut reg = GrammarRegistry::new();
        assert!(reg.is_empty());
        reg.register("rust", "rust-grammar").unwrap();
        reg.register("go", "go-grammar").unwrap();
        assert_eq!(
            reg.register("klingon", "x"),
            Err(UnknownLanguage("klingon".into()))
        );
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get(by_id("rust").unwrap()), Some(&"rust-grammar"));
        assert_eq!(reg.get_by_id("go"), Some(&"go-grammar"));
        let (lang, g) = reg.detect("main.go", None).unwrap();
        assert_eq!((lang.id, *g), ("go", "go-grammar"));
        assert!(reg.detect("x.py", None).is_none());
        let mut ids: Vec<_> = reg.languages().map(|l| l.id).collect();
        ids.sort();
        assert_eq!(ids, ["go", "rust"]);
    }
}
