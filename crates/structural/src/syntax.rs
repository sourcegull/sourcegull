//! Per-language syntax: strings and comments.
//!
//! Each language has delimiters, string literals with their escape
//! character, and line and block comments. Delimiters are `()`, `[]` and
//! `{}` for every language. Languages are keyed by `sourcegull-languages` ids;
//! unknown ones get [`GENERIC`], which knows only double-quoted strings.

/// A string literal form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringSyntax {
    pub open: &'static str,
    pub close: &'static str,
    /// `\` escapes the next byte.
    pub escapes: bool,
    /// May span lines; otherwise an unterminated literal on its line is
    /// not a string.
    pub multiline: bool,
}

/// What the matcher needs to know about a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub strings: &'static [StringSyntax],
    /// `'x'` and `'\n'` are character literals; other `'` are plain
    /// characters (lifetimes, labels).
    pub char_literals: bool,
    pub line_comments: &'static [&'static str],
    pub block_comments: &'static [(&'static str, &'static str)],
    /// Block comments nest (Rust, OCaml, Haskell, Swift).
    pub nested_comments: bool,
}

const fn s(
    open: &'static str,
    close: &'static str,
    escapes: bool,
    multiline: bool,
) -> StringSyntax {
    StringSyntax {
        open,
        close,
        escapes,
        multiline,
    }
}

const DQ: StringSyntax = s("\"", "\"", true, false);
const SQ: StringSyntax = s("'", "'", true, false);
const BACKTICK: StringSyntax = s("`", "`", true, true);
const C_BLOCK: &[(&str, &str)] = &[("/*", "*/")];

pub const GENERIC: Syntax = Syntax {
    strings: &[DQ],
    char_literals: false,
    line_comments: &[],
    block_comments: &[],
    nested_comments: false,
};

const C_LIKE: Syntax = Syntax {
    strings: &[DQ],
    char_literals: true,
    line_comments: &["//"],
    block_comments: C_BLOCK,
    nested_comments: false,
};

const GO: Syntax = Syntax {
    strings: &[DQ, s("`", "`", false, true)],
    ..C_LIKE
};

const RUST: Syntax = Syntax {
    strings: &[s("\"", "\"", true, true)],
    nested_comments: true,
    ..C_LIKE
};

const SWIFT: Syntax = Syntax {
    strings: &[s("\"\"\"", "\"\"\"", true, true), DQ],
    nested_comments: true,
    ..C_LIKE
};

const KOTLIN_SCALA: Syntax = Syntax {
    strings: &[s("\"\"\"", "\"\"\"", false, true), DQ],
    ..C_LIKE
};

const CSHARP: Syntax = Syntax {
    strings: &[s("@\"", "\"", false, true), DQ],
    ..C_LIKE
};

const JS: Syntax = Syntax {
    strings: &[DQ, SQ, BACKTICK],
    char_literals: false,
    ..C_LIKE
};

const PHP: Syntax = Syntax {
    strings: &[DQ, SQ],
    char_literals: false,
    line_comments: &["//", "#"],
    ..C_LIKE
};

const DART: Syntax = Syntax {
    strings: &[
        s("\"\"\"", "\"\"\"", true, true),
        s("'''", "'''", true, true),
        DQ,
        SQ,
    ],
    char_literals: false,
    ..C_LIKE
};

const PYTHON: Syntax = Syntax {
    strings: &[
        s("\"\"\"", "\"\"\"", true, true),
        s("'''", "'''", true, true),
        DQ,
        SQ,
    ],
    char_literals: false,
    line_comments: &["#"],
    block_comments: &[],
    nested_comments: false,
};

/// Shell, Ruby, Perl, R, YAML, TOML and other `#`-comment languages.
const HASH: Syntax = Syntax {
    strings: &[DQ, SQ],
    char_literals: false,
    line_comments: &["#"],
    block_comments: &[],
    nested_comments: false,
};

const ELIXIR: Syntax = Syntax {
    strings: &[s("\"\"\"", "\"\"\"", true, true), DQ],
    ..HASH
};

const SQL: Syntax = Syntax {
    strings: &[SQ, DQ],
    char_literals: false,
    line_comments: &["--"],
    block_comments: C_BLOCK,
    nested_comments: false,
};

const LUA: Syntax = Syntax {
    strings: &[s("[[", "]]", false, true), DQ, SQ],
    char_literals: false,
    line_comments: &["--"],
    block_comments: &[("--[[", "]]")],
    nested_comments: false,
};

const HASKELL: Syntax = Syntax {
    strings: &[DQ],
    char_literals: true,
    line_comments: &["--"],
    block_comments: &[("{-", "-}")],
    nested_comments: true,
};

const OCAML: Syntax = Syntax {
    strings: &[DQ],
    char_literals: true,
    line_comments: &[],
    block_comments: &[("(*", "*)")],
    nested_comments: true,
};

const LISP: Syntax = Syntax {
    strings: &[DQ],
    char_literals: false,
    line_comments: &[";"],
    block_comments: &[],
    nested_comments: false,
};

const ERLANG: Syntax = Syntax {
    strings: &[DQ],
    char_literals: false,
    line_comments: &["%"],
    block_comments: &[],
    nested_comments: false,
};

const MARKUP: Syntax = Syntax {
    strings: &[DQ, SQ],
    char_literals: false,
    line_comments: &[],
    block_comments: &[("<!--", "-->")],
    nested_comments: false,
};

const CSS: Syntax = Syntax {
    strings: &[DQ, SQ],
    char_literals: false,
    line_comments: &[],
    block_comments: C_BLOCK,
    nested_comments: false,
};

const SCSS: Syntax = Syntax {
    line_comments: &["//"],
    ..CSS
};

const FORTRAN: Syntax = Syntax {
    strings: &[DQ, SQ],
    char_literals: false,
    line_comments: &["!"],
    block_comments: &[],
    nested_comments: false,
};

const TEX: Syntax = Syntax {
    strings: &[],
    char_literals: false,
    line_comments: &["%"],
    block_comments: &[],
    nested_comments: false,
};

/// The syntax for a `sourcegull-languages` id; [`GENERIC`] when unknown.
pub fn syntax_for(lang: &str) -> &'static Syntax {
    match lang {
        "c" | "cpp" | "objective-c" | "objective-cpp" | "java" | "groovy" | "cuda" | "d"
        | "zig" | "vlang" | "solidity" | "protobuf" | "thrift" | "jsonc" | "json5" | "hcl"
        | "verilog" | "systemverilog" | "jsonnet" | "cue" | "bicep" => &C_LIKE,
        "go" => &GO,
        "rust" => &RUST,
        "swift" => &SWIFT,
        "kotlin" | "scala" => &KOTLIN_SCALA,
        "csharp" | "fsharp" => &CSHARP,
        "javascript" | "typescript" | "tsx" | "coffeescript" | "vue" | "svelte" | "astro" => &JS,
        "php" => &PHP,
        "dart" => &DART,
        "python" | "starlark" => &PYTHON,
        "shell" | "fish" | "powershell" | "ruby" | "perl" | "raku" | "r" | "julia" | "tcl"
        | "yaml" | "toml" | "makefile" | "cmake" | "dockerfile" | "nginx" | "nix" | "awk"
        | "crystal" | "graphql" | "ini" | "dotenv" | "git-config" | "ignore-list" | "meson"
        | "just" | "rego" => &HASH,
        "elixir" => &ELIXIR,
        "sql" | "ada" | "vhdl" => &SQL,
        "lua" => &LUA,
        "haskell" | "elm" | "purescript" => &HASKELL,
        "ocaml" | "coq" => &OCAML,
        "clojure" | "scheme" | "racket" | "common-lisp" | "emacs-lisp" => &LISP,
        "erlang" | "prolog" | "matlab" => &ERLANG,
        "html" | "xml" | "markdown" | "handlebars" | "jinja" | "erb" => &MARKUP,
        "css" | "less" => &CSS,
        "scss" | "sass" => &SCSS,
        "fortran" => &FORTRAN,
        "tex" => &TEX,
        _ => &GENERIC,
    }
}
