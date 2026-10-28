//! Highlighting per language, limits and output forms.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use sourcegull_highlight::{
    Engine, Highlighted, Limits, Occurrence, PlainReason, Request, SyntaxKind, highlight, to_html,
    unpack,
};

fn hl(path: &str, src: &str) -> Highlighted {
    highlight(&Request::new(path, src.as_bytes()))
}

/// The text of each occurrence, with its kind.
fn tokens(src: &str, h: &Highlighted) -> Vec<(String, SyntaxKind)> {
    let lines: Vec<&str> = src.split('\n').collect();
    h.occurrences
        .iter()
        .map(|o| {
            let units: Vec<u16> = lines[o.line as usize].encode_utf16().collect();
            let text = String::from_utf16_lossy(&units[o.start_col as usize..o.end_col as usize]);
            (text, o.kind)
        })
        .collect()
}

fn has(src: &str, h: &Highlighted, text: &str, kind: SyntaxKind) -> bool {
    tokens(src, h).iter().any(|(t, k)| t == text && *k == kind)
}

#[track_caller]
fn assert_tokens(path: &str, src: &str, expected: &[(&str, SyntaxKind)]) {
    let h = hl(path, src);
    assert_eq!(h.engine, Engine::TreeSitter, "{path}: {h:?}");
    for (text, kind) in expected {
        assert!(
            has(src, &h, text, *kind),
            "{path}: expected {text:?} as {kind:?}, got {:?}",
            tokens(src, &h)
        );
    }
    // Sorted and non-overlapping.
    for w in h.occurrences.windows(2) {
        assert!(
            (w[0].line, w[0].end_col) <= (w[1].line, w[1].start_col),
            "{w:?}"
        );
    }
}

#[test]
fn core_languages_use_tree_sitter() {
    use SyntaxKind as K;
    assert_tokens(
        "main.rs",
        "// hi\nfn main() {\n    let s = \"x\\n\";\n    println!(\"{}\", 42);\n}\n",
        &[
            ("// hi", K::Comment),
            ("fn", K::Keyword),
            ("main", K::Function),
            ("42", K::Number),
            ("println!", K::Macro),
        ],
    );
    assert_tokens(
        "main.go",
        "package main\n\nfunc main() {\n\tx := \"s\" // c\n}\n",
        &[
            ("package", K::Keyword),
            ("func", K::Keyword),
            ("main", K::Function),
            ("\"s\"", K::String),
            ("// c", K::Comment),
        ],
    );
    assert_tokens(
        "a.ts",
        "interface A { x: number }\nexport function f(a: A): string { return `t`; }\n",
        &[
            ("interface", K::Keyword),
            ("f", K::Function),
            ("number", K::TypeBuiltin),
        ],
    );
    assert_tokens(
        "a.tsx",
        "export const C = () => <div className=\"x\">hi</div>;\n",
        &[("const", K::Keyword), ("div", K::Tag)],
    );
    assert_tokens(
        "a.js",
        "const x = 1; // one\nfunction g() { return null; }\n",
        &[
            ("const", K::Keyword),
            ("1", K::Number),
            ("// one", K::Comment),
            ("g", K::Function),
        ],
    );
    assert_tokens(
        "a.py",
        "def f(x):\n    \"\"\"doc\"\"\"\n    return None  # c\n",
        &[
            ("def", K::Keyword),
            ("f", K::Function),
            ("None", K::ConstantBuiltin),
            ("# c", K::Comment),
        ],
    );
    assert_tokens(
        "A.java",
        "public class A { int f() { return 1; } }\n",
        &[
            ("public", K::Keyword),
            ("class", K::Keyword),
            ("1", K::Number),
        ],
    );
    assert_tokens(
        "a.c",
        "#include <stdio.h>\nint main(void) { return 0; }\n",
        &[
            ("#include", K::Keyword),
            ("return", K::Keyword),
            ("0", K::Number),
        ],
    );
    assert_tokens(
        "a.cpp",
        "namespace n { class C { public: void f(); }; }\n",
        &[("namespace", K::Keyword), ("class", K::Keyword)],
    );
    assert_tokens(
        "a.json",
        "{\"k\": [1, true, null]}\n",
        &[
            ("\"k\"", K::String),
            ("1", K::Number),
            ("true", K::ConstantBuiltin),
        ],
    );
    assert_tokens(
        "a.yaml",
        "key: value # c\nn: 3\n",
        &[("# c", K::Comment), ("3", K::Number)],
    );
    assert_tokens(
        "run.sh",
        "#!/bin/sh\nif [ -n \"$x\" ]; then echo hi; fi # c\n",
        &[("if", K::Keyword), ("# c", K::Comment)],
    );
    // Markdown: headings from the block grammar, code from the inline injection,
    // and fenced code highlighted with the fence's language.
    let md = "# Title\n\nSome `code` here.\n\n```rust\nfn x() {}\n```\n";
    let h = hl("README.md", md);
    assert_eq!(h.engine, Engine::TreeSitter);
    let t = tokens(md, &h);
    assert!(
        t.iter()
            .any(|(s, k)| s.contains("Title") && *k == K::Heading),
        "{t:?}"
    );
    assert!(
        t.iter().any(|(s, k)| s == "fn" && *k == K::Keyword),
        "{t:?}"
    );
}

#[test]
fn utf16_columns_for_codemirror() {
    let src = "let s = \"é😀\"; // ok\n";
    let h = hl("a.rs", src);
    let comment = h
        .occurrences
        .iter()
        .find(|o| o.kind == SyntaxKind::Comment)
        .unwrap();
    // `let s = "é😀"; ` is 15 UTF-16 units ("😀" counts 2), so the comment starts at 15.
    assert_eq!(comment.start_col, 15);
    assert_eq!(comment.end_col, 20);
}

#[test]
fn multiline_ranges_are_split_per_line() {
    let src = "/* a\nb\nc */ fn x() {}\n";
    let h = hl("a.rs", src);
    let comments: Vec<&Occurrence> = h
        .occurrences
        .iter()
        .filter(|o| o.kind == SyntaxKind::Comment)
        .collect();
    assert_eq!(comments.len(), 3);
    assert_eq!(
        comments.iter().map(|o| o.line).collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

#[cfg(feature = "syntect")]
#[test]
fn long_tail_languages_fall_back_to_syntect() {
    let src = "# comment\ndef hello\n  puts \"hi\"\nend\n";
    let h = hl("x.rb", src);
    assert_eq!(h.engine, Engine::Syntect);
    assert_eq!(h.language.as_deref(), Some("ruby"));
    assert!(
        has(src, &h, "# comment", SyntaxKind::Comment),
        "{:?}",
        tokens(src, &h)
    );
    assert!(
        has(src, &h, "def", SyntaxKind::Keyword),
        "{:?}",
        tokens(src, &h)
    );
    let sql = "SELECT 1 FROM t; -- c\n";
    let h = hl("q.sql", sql);
    assert_eq!(h.engine, Engine::Syntect);
    assert!(h.occurrences.iter().any(|o| o.kind == SyntaxKind::Keyword));
}

#[test]
fn limits_and_plain_text() {
    let big = "x".repeat(2 * 1024 * 1024);
    let h = hl("a.rs", &big);
    assert_eq!(
        (h.engine, h.plain_reason),
        (Engine::PlainText, Some(PlainReason::TooLarge))
    );
    let many = "a\n".repeat(30_000);
    assert_eq!(
        hl("a.rs", &many).plain_reason,
        Some(PlainReason::TooManyLines)
    );
    let bin = highlight(&Request::new("a.rs", b"fn\0x"));
    assert_eq!(bin.plain_reason, Some(PlainReason::Binary));
    let unknown = hl("file.unknownext", "hello");
    assert_eq!(unknown.plain_reason, Some(PlainReason::NoGrammar));
    assert_eq!(unknown.language, None);

    // A zero timeout trips before any work is done.
    let src = "fn main() {}\n".repeat(2000);
    let mut req = Request::new("a.rs", src.as_bytes());
    req.limits = Limits {
        timeout: Duration::ZERO,
        ..Limits::default()
    };
    assert_eq!(highlight(&req).plain_reason, Some(PlainReason::Timeout));
    // So does cancellation.
    let cancel = AtomicUsize::new(1);
    let mut req = Request::new("a.rs", src.as_bytes());
    req.cancel = Some(&cancel);
    assert_eq!(highlight(&req).plain_reason, Some(PlainReason::Timeout));
    // Language override.
    let mut req = Request::new("noext", b"fn main() {}");
    req.language = Some("Rust");
    assert_eq!(highlight(&req).engine, Engine::TreeSitter);
}

#[test]
fn packed_and_html_forms() {
    let src = "fn main() { let x = 1; }\n";
    let h = hl("a.rs", src);
    let packed = h.packed();
    assert_eq!(packed.len(), h.occurrences.len() * 4);
    assert_eq!(unpack(&packed).unwrap(), h.occurrences);
    let html = to_html(src, &h.occurrences);
    assert!(
        html.contains("<span class=\"hl-keyword\">fn</span>"),
        "{html}"
    );
    // JSON form for caching/API.
    let json = serde_json::to_string(&h).unwrap();
    let back: Highlighted = serde_json::from_str(&json).unwrap();
    assert_eq!(back, h);
}

#[test]
fn compact_output_is_much_smaller_than_html() {
    let src = include_str!("../src/encode.rs");
    let h = hl("encode.rs", src);
    let packed_bytes = h.packed().len() * 4;
    let html_bytes = to_html(src, &h.occurrences).len();
    assert!(
        packed_bytes * 3 < html_bytes,
        "packed {packed_bytes} vs html {html_bytes}"
    );
}
