//! tree-sitter highlighting for the compiled-in grammars.

use std::cell::RefCell;
use std::sync::LazyLock;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use sourcegull_languages::GrammarRegistry;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::kind::{CAPTURE_NAMES, SyntaxKind, kind_for_capture};
use crate::{Error, Limits};

struct Grammars {
    registry: GrammarRegistry<HighlightConfiguration>,
    markdown_inline: HighlightConfiguration,
    kinds: Vec<Option<SyntaxKind>>,
}

fn config(
    language: tree_sitter_language::LanguageFn,
    name: &str,
    highlights: &[&str],
    injections: &str,
    locals: &str,
) -> HighlightConfiguration {
    let mut c = HighlightConfiguration::new(
        language.into(),
        name,
        &highlights.join("\n"),
        injections,
        locals,
    )
    .unwrap_or_else(|e| panic!("highlight query for {name} does not compile: {e}"));
    c.configure(CAPTURE_NAMES);
    c
}

static GRAMMARS: LazyLock<Grammars> = LazyLock::new(|| {
    let js_highlights = tree_sitter_javascript::HIGHLIGHT_QUERY;
    let jsx = tree_sitter_javascript::JSX_HIGHLIGHT_QUERY;
    let ts_highlights = tree_sitter_typescript::HIGHLIGHTS_QUERY;
    let entries = [
        (
            "rust",
            config(
                tree_sitter_rust::LANGUAGE,
                "rust",
                &[tree_sitter_rust::HIGHLIGHTS_QUERY],
                tree_sitter_rust::INJECTIONS_QUERY,
                "",
            ),
        ),
        (
            "go",
            config(tree_sitter_go::LANGUAGE, "go", &[&go_highlights()], "", ""),
        ),
        (
            "javascript",
            config(
                tree_sitter_javascript::LANGUAGE,
                "javascript",
                &[js_highlights, jsx],
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            ),
        ),
        (
            "typescript",
            config(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                "typescript",
                &[js_highlights, ts_highlights],
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_typescript::LOCALS_QUERY,
            ),
        ),
        (
            "tsx",
            config(
                tree_sitter_typescript::LANGUAGE_TSX,
                "tsx",
                &[js_highlights, jsx, ts_highlights],
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_typescript::LOCALS_QUERY,
            ),
        ),
        (
            "python",
            config(
                tree_sitter_python::LANGUAGE,
                "python",
                &[tree_sitter_python::HIGHLIGHTS_QUERY],
                "",
                "",
            ),
        ),
        (
            "java",
            config(
                tree_sitter_java::LANGUAGE,
                "java",
                &[tree_sitter_java::HIGHLIGHTS_QUERY],
                "",
                "",
            ),
        ),
        (
            "c",
            config(
                tree_sitter_c::LANGUAGE,
                "c",
                &[tree_sitter_c::HIGHLIGHT_QUERY],
                "",
                "",
            ),
        ),
        (
            "cpp",
            config(
                tree_sitter_cpp::LANGUAGE,
                "cpp",
                &[
                    tree_sitter_cpp::HIGHLIGHT_QUERY,
                    tree_sitter_c::HIGHLIGHT_QUERY,
                ],
                "",
                "",
            ),
        ),
        (
            "json",
            config(
                tree_sitter_json::LANGUAGE,
                "json",
                &[tree_sitter_json::HIGHLIGHTS_QUERY],
                "",
                "",
            ),
        ),
        (
            "jsonc",
            config(
                tree_sitter_json::LANGUAGE,
                "jsonc",
                &[tree_sitter_json::HIGHLIGHTS_QUERY],
                "",
                "",
            ),
        ),
        (
            "yaml",
            config(
                tree_sitter_yaml::LANGUAGE,
                "yaml",
                &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
                "",
                "",
            ),
        ),
        (
            "markdown",
            config(
                tree_sitter_md::LANGUAGE,
                "markdown",
                &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
                tree_sitter_md::INJECTION_QUERY_BLOCK,
                "",
            ),
        ),
        (
            "shell",
            config(
                tree_sitter_bash::LANGUAGE,
                "shell",
                &[tree_sitter_bash::HIGHLIGHT_QUERY],
                "",
                "",
            ),
        ),
    ];
    let mut registry = GrammarRegistry::new();
    for (id, c) in entries {
        registry
            .register(id, c)
            .unwrap_or_else(|e| panic!("grammar registry: {e}"));
    }
    Grammars {
        registry,
        markdown_inline: config(
            tree_sitter_md::INLINE_LANGUAGE,
            "markdown_inline",
            &[tree_sitter_md::HIGHLIGHT_QUERY_INLINE],
            tree_sitter_md::INJECTION_QUERY_INLINE,
            "",
        ),
        kinds: CAPTURE_NAMES.iter().map(|n| kind_for_capture(n)).collect(),
    }
});

/// tree-sitter-highlight lets the *last* matching pattern win for a node.
/// tree-sitter-go's query was written for first-wins and lists its generic
/// identifier patterns after the specific ones, so they would override
/// function names; move them to the top.
fn go_highlights() -> String {
    const GENERIC: [&str; 3] = [
        "(type_identifier) @type",
        "(field_identifier) @property",
        "(identifier) @variable",
    ];
    let mut rest = tree_sitter_go::HIGHLIGHTS_QUERY.to_owned();
    for g in GENERIC {
        rest = rest.replacen(g, "", 1);
    }
    format!("{}\n{rest}", GENERIC.join("\n"))
}

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

/// Whether a tree-sitter grammar is compiled in for the language `id`.
pub(crate) fn supports(id: &str) -> bool {
    GRAMMARS.registry.get_by_id(id).is_some()
}

/// Ids of languages with a tree-sitter grammar.
pub(crate) fn languages() -> Vec<&'static str> {
    let mut v: Vec<_> = GRAMMARS.registry.languages().map(|l| l.id).collect();
    v.sort_unstable();
    v
}

/// Resolves an injection language name (a code fence info string, `bash`,
/// `markdown_inline`, …) to a configuration.
fn injection(name: &str) -> Option<&'static HighlightConfiguration> {
    let g = &*GRAMMARS;
    if name == "markdown_inline" {
        return Some(&g.markdown_inline);
    }
    let lang = sourcegull_languages::by_name(name)
        .or_else(|| sourcegull_languages::by_extension(name).first().copied())
        .or_else(|| match name {
            "bash" | "sh" | "zsh" => sourcegull_languages::by_id("shell"),
            "js" => sourcegull_languages::by_id("javascript"),
            "ts" => sourcegull_languages::by_id("typescript"),
            "py" => sourcegull_languages::by_id("python"),
            "c++" => sourcegull_languages::by_id("cpp"),
            _ => None,
        })?;
    g.registry.get(lang)
}

/// Highlights `source` with the grammar for `lang_id`, returning sorted,
/// non-overlapping byte spans (innermost capture wins).
pub(crate) fn highlight(
    lang_id: &str,
    source: &[u8],
    limits: &Limits,
    cancel: Option<&AtomicUsize>,
) -> Result<Vec<(usize, usize, SyntaxKind)>, Error> {
    let g = &*GRAMMARS;
    let config = g
        .registry
        .get_by_id(lang_id)
        .ok_or_else(|| Error::NoGrammar(lang_id.to_owned()))?;
    let start = Instant::now();
    HIGHLIGHTER.with(|h| {
        let mut h = h.borrow_mut();
        let events = h
            .highlight(config, source, None, cancel, |name| {
                injection(name).map(|c| c as &HighlightConfiguration)
            })
            .map_err(map_error)?;
        let mut stack: Vec<Option<SyntaxKind>> = Vec::new();
        let mut spans = Vec::new();
        for (i, event) in events.enumerate() {
            if i % 1024 == 0 && start.elapsed() > limits.timeout {
                return Err(Error::Timeout);
            }
            match event.map_err(map_error)? {
                HighlightEvent::HighlightStart(hl) => {
                    stack.push(g.kinds.get(hl.0).copied().flatten());
                }
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    // Innermost capture that maps to a kind.
                    if let Some(mut kind) = stack.iter().rev().find_map(|k| *k) {
                        // Some grammars (Rust) capture numeric literals as
                        // constant.builtin; normalize so numbers look alike
                        // across languages.
                        if kind == SyntaxKind::ConstantBuiltin
                            && source.get(start).is_some_and(u8::is_ascii_digit)
                        {
                            kind = SyntaxKind::Number;
                        }
                        spans.push((start, end, kind));
                    }
                }
            }
        }
        Ok(spans)
    })
}

fn map_error(e: tree_sitter_highlight::Error) -> Error {
    match e {
        tree_sitter_highlight::Error::Cancelled => Error::Timeout,
        e => Error::Highlight(e.to_string()),
    }
}
