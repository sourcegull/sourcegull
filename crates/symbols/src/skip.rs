//! Function bodies found by a light lexer, so the parser can skip them.
//!
//! Definitions inside function bodies are locals and are not reported, yet
//! bodies are most of a file and most of the parse. [`bodies`] tokenizes a
//! file (comments, strings, character and raw literals, template literals,
//! regular expressions, preprocessor lines) and tracks brackets to find the
//! `{ … }` of each function body outside other bodies (and of initializer
//! values in Go, C and C++, whose definitions are locals as well: the
//! queries mark `literal_value` and `initializer_list` `@local`).
//! [`extract`] then parses only the rest, with the bodies' insides excluded
//! through tree-sitter's included ranges, so `{` and `}` stay and the tree
//! is well formed. Brace languages are lexed here (Go, C, C++, Java, C#,
//! Rust, JavaScript, TypeScript, Kotlin, Swift, Scala, PHP), Python and
//! Ruby from indentation ([`indent`]).
//!
//! Outside bodies, C and C++ also leave out what holds no definitions and
//! that the grammar does not know, where a full parse spends most of its
//! time in error recovery: attribute macros (`static int __init f(void)`,
//! [`is_attribute_macro`]), the arguments of macro calls that start a
//! declaration at the file level (`static DEFINE_PER_CPU(struct x *, y);`
//! keeps `y`), and the parameters of function definitions and C prototypes.
//! A full parse often recovers from these with made-up names (`__packed` as
//! a field, `__init` as a variable) or loses the definition; left out, the
//! rest parses cleanly.
//!
//! The rules are per language and deliberately simple. Anything the lexer
//! does not understand (an unterminated string, unbalanced brackets, as
//! from `#if` branches) gives `None`, and a parse with bodies left out that
//! has errors is redone in full, so a wrong guess costs time, not symbols.
//!
//! [`extract`]: crate::extract

use std::ops::Range;

mod indent;

/// Body rules for a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rules {
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    Rust,
    Js,
    Ts,
    Kotlin,
    Swift,
    Scala,
    Php,
    Python,
    Ruby,
}

impl Rules {
    pub(crate) fn for_lang(id: &str) -> Option<Rules> {
        Some(match id {
            "go" => Rules::Go,
            "c" => Rules::C,
            "cpp" => Rules::Cpp,
            "java" => Rules::Java,
            "csharp" => Rules::CSharp,
            "rust" => Rules::Rust,
            "javascript" => Rules::Js,
            "typescript" | "tsx" => Rules::Ts,
            "kotlin" => Rules::Kotlin,
            "swift" => Rules::Swift,
            "scala" => Rules::Scala,
            "php" => Rules::Php,
            "python" => Rules::Python,
            "ruby" => Rules::Ruby,
            _ => return None,
        })
    }

    /// Whether a parse with bodies left out is redone in full on any syntax
    /// error, not only one at a body: bodies found from indentation are
    /// guesses.
    pub(crate) fn strict(self) -> bool {
        matches!(self, Rules::Python | Rules::Ruby)
    }

    /// Lines starting with `#` are preprocessor directives.
    fn c_like(self) -> bool {
        matches!(self, Rules::C | Rules::Cpp | Rules::CSharp)
    }

    fn js_like(self) -> bool {
        matches!(self, Rules::Js | Rules::Ts)
    }

    /// Whether a plain string may span lines.
    fn multiline_strings(self) -> bool {
        matches!(self, Rules::Rust | Rules::Php)
    }

    /// Whether block comments nest.
    fn nested_comments(self) -> bool {
        matches!(
            self,
            Rules::Rust | Rules::Kotlin | Rules::Swift | Rules::Scala
        )
    }

    /// Whether a newline after a token that can end a statement ends it.
    fn newline_ends_statements(self) -> bool {
        matches!(
            self,
            Rules::Go | Rules::Kotlin | Rules::Swift | Rules::Scala
        )
    }

    /// Whether `w` starts a function whose body is the next `{` of the
    /// statement.
    fn is_function_keyword(self, w: &[u8]) -> bool {
        match self {
            Rules::Go => w == b"func",
            Rules::Rust => w == b"fn",
            Rules::Js | Rules::Ts | Rules::Php => w == b"function",
            Rules::Kotlin => w == b"fun",
            Rules::Swift => matches!(w, b"func" | b"init" | b"deinit" | b"subscript"),
            Rules::Scala => w == b"def",
            Rules::C | Rules::Cpp | Rules::Java | Rules::CSharp | Rules::Python | Rules::Ruby => {
                false
            }
        }
    }

    /// Keywords after which a `{` opens a type, not a body.
    fn is_type_keyword(self, w: &[u8]) -> bool {
        match self {
            Rules::Java => matches!(w, b"class" | b"interface" | b"enum" | b"record"),
            Rules::CSharp => matches!(
                w,
                b"class" | b"struct" | b"interface" | b"enum" | b"record" | b"namespace"
            ),
            Rules::Js | Rules::Ts => matches!(
                w,
                b"class" | b"interface" | b"enum" | b"namespace" | b"module" | b"global"
            ),
            // `fun interface I { … }`.
            Rules::Kotlin => matches!(w, b"class" | b"interface" | b"object"),
            Rules::Swift => matches!(
                w,
                b"class" | b"struct" | b"enum" | b"protocol" | b"extension" | b"actor"
            ),
            Rules::Scala => matches!(w, b"class" | b"object" | b"trait" | b"enum"),
            Rules::Php => matches!(w, b"class" | b"interface" | b"trait" | b"enum"),
            _ => false,
        }
    }
}

/// The previous significant token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    None,
    Ident(usize, usize),
    Literal,
    Punct(u8),
    CloseParen,
    CloseBracket,
    CloseBrace,
    /// `=>`.
    FatArrow,
}

/// What the current statement at a nesting level has seen.
#[derive(Debug, Clone, Copy, Default)]
struct Stmt {
    /// An assignment `=`.
    eq: bool,
    /// The function keyword.
    func: bool,
    /// A type keyword (`class`, `record`, …).
    typ: bool,
    /// `enum` (Java).
    enum_kw: bool,
    /// `throws` (Java).
    throws: bool,
    /// A `)` closing a group at this level.
    close_paren: bool,
    /// A `:` after such a `)` (TypeScript return types).
    paren_colon: bool,
    /// `->` after such a `)` (C++ trailing return types).
    arrow: bool,
    /// The insides of the last group at this level that may be a C
    /// function's parameter list.
    params: Option<(usize, usize, bool)>,
}

#[derive(Debug, Clone, Copy)]
struct Level {
    /// The closing byte: `)`, `]`, `}`, or `` ` `` for a template
    /// literal's `${`.
    close: u8,
    /// A function body, opened at this byte.
    body: Option<usize>,
    /// Inside an initializer (`= { … }`): no bodies in C, C++ and Java.
    init: bool,
    /// A Go `struct { … }` or `interface { … }`: the statement goes on.
    keep: bool,
    stmt: Stmt,
    /// A C macro call's arguments, left out: the lengths of the output's
    /// lists when it opened (what was found inside is dropped).
    args: Option<(usize, usize)>,
    /// May be a C function's parameter list: where it opened and the
    /// lengths of the output's lists then.
    params: Option<(usize, usize, usize, bool)>,
    /// An interpolation in a string: the string goes on after the closing
    /// byte.
    resume: Option<Str>,
    /// A C macro call's group (`LOCKS_REQUIRED(x)`), not parameters.
    macro_call: bool,
    /// A Java enum's constants, up to the first `;`: `A(1) { … }` is a
    /// constant's class body.
    constants: bool,
}

/// A string with interpolations, resumed after each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Str {
    /// `"…"` with escapes and `${…}` (Kotlin, Scala with an interpolator).
    Dollar,
    /// `"""…"""` without escapes, with `${…}` if interpolated (Kotlin
    /// always, Scala with an interpolator).
    Triple { interpolated: bool },
    /// Swift: `"…"` or `"""…"""` with escapes and `\(…)`, raw with as many
    /// `#` around it as `hashes`.
    Swift { triple: bool, hashes: usize },
}

impl Level {
    fn new(close: u8, init: bool) -> Level {
        Level {
            close,
            body: None,
            init,
            keep: false,
            stmt: Stmt::default(),
            args: None,
            params: None,
            resume: None,
            constants: false,
            macro_call: false,
        }
    }
}

/// What the parser can leave out of a file, each list in order.
#[derive(Debug, Default)]
pub(crate) struct Skipped {
    /// The insides of the function bodies that are not nested in other
    /// bodies.
    pub bodies: Vec<Range<usize>>,
    /// Outside bodies: C attribute macros (`__init`, `__printf(1, 2)`), the
    /// insides of macro calls' argument lists (`DEFINE_PER_CPU(…)`) and of
    /// parameter lists.
    pub hidden: Vec<Range<usize>>,
}

impl Skipped {
    pub(crate) fn is_empty(&self) -> bool {
        self.bodies.is_empty() && self.hidden.is_empty()
    }
}

/// The bodies and macros in `src` the parser can leave out, or `None` if
/// the lexer gave up.
pub(crate) fn bodies(rules: Rules, src: &[u8]) -> Option<Skipped> {
    let indented = match rules {
        Rules::Python => Some(indent::python(src)),
        Rules::Ruby => Some(indent::ruby(src)),
        _ => None,
    };
    if let Some(bodies) = indented {
        return bodies.map(|bodies| Skipped {
            bodies,
            hidden: Vec::new(),
        });
    }
    Lexer {
        rules,
        src,
        i: 0,
        prev: Tok::None,
        levels: vec![Level::new(0, false)],
        in_body: 0,
        line_start: true,
        macro_args: false,
        out: Skipped::default(),
    }
    .run()
}

struct Lexer<'a> {
    rules: Rules,
    src: &'a [u8],
    i: usize,
    prev: Tok,
    /// Open brackets; the first is the file.
    levels: Vec<Level>,
    /// Open bodies.
    in_body: usize,
    /// Only whitespace since the last newline (C preprocessor lines).
    line_start: bool,
    /// The next `(` opens a macro call's arguments.
    macro_args: bool,
    out: Skipped,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

impl Lexer<'_> {
    fn at(&self, i: usize) -> u8 {
        self.src.get(i).copied().unwrap_or(0)
    }

    fn stmt(&mut self) -> &mut Stmt {
        &mut self.levels.last_mut().expect("the file level").stmt
    }

    fn run(mut self) -> Option<Skipped> {
        self.lex()?;
        self.out.hidden.sort_unstable_by_key(|r| r.start);
        (self.levels.len() == 1).then_some(self.out)
    }

    fn lex(&mut self) -> Option<()> {
        let src = self.src;
        if src.starts_with(b"#!") {
            self.i = memchr::memchr(b'\n', src).unwrap_or(src.len());
        }
        if self.rules == Rules::Php {
            // Only files that are PHP from the start (no HTML around).
            while self.at(self.i).is_ascii_whitespace() {
                self.i += 1;
            }
            if !src[self.i..].starts_with(b"<?php") {
                return None;
            }
            self.i += 5;
        }
        while self.i < src.len() {
            let b = src[self.i];
            match b {
                b'\n' => {
                    self.newline();
                    self.i += 1;
                    self.line_start = true;
                    continue;
                }
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let line_start = std::mem::replace(&mut self.line_start, false);
            match b {
                b'#' if line_start && self.rules.c_like() => self.preprocessor()?,
                b'#' if self.rules == Rules::Php && self.at(self.i + 1) != b'[' => {
                    self.line_comment()?
                }
                b'#' if self.rules == Rules::Swift => {
                    let hashes = src[self.i..].iter().take_while(|&&c| c == b'#').count();
                    if self.at(self.i + hashes) == b'"' {
                        self.i += hashes;
                        self.swift_string(hashes)?;
                    } else {
                        self.punct(b)?;
                    }
                }
                b'/' if self.at(self.i + 1) == b'/' => self.line_comment()?,
                b'/' if self.at(self.i + 1) == b'*' => self.block_comment()?,
                b'/' if self.rules.js_like() && self.regex_allowed() => {
                    self.regex()?;
                    self.prev = Tok::Literal;
                }
                b'"' if self.rules == Rules::Swift => self.swift_string(0)?,
                b'"' if matches!(self.rules, Rules::Kotlin | Rules::Scala) => {
                    self.dollar_string()?
                }
                b'"' => {
                    self.string()?;
                    self.prev = Tok::Literal;
                }
                b'\'' if self.rules == Rules::Swift => self.punct(b)?,
                b'\'' => self.quote()?,
                b'`' if matches!(self.rules, Rules::Kotlin | Rules::Swift | Rules::Scala) => {
                    // A quoted name.
                    let start = self.i;
                    let line = &src[self.i + 1..];
                    let end = memchr::memchr2(b'`', b'\n', line).filter(|&n| line[n] == b'`')?;
                    self.i += end + 2;
                    self.prev = Tok::Ident(start, self.i);
                }
                b'`' if self.rules == Rules::Php => {
                    self.quoted(b'`')?;
                    self.prev = Tok::Literal;
                }
                b'<' if self.rules == Rules::Php && src[self.i..].starts_with(b"<<<") => {
                    self.heredoc()?;
                    self.prev = Tok::Literal;
                }
                b'?' if self.rules == Rules::Php && self.at(self.i + 1) == b'>' => {
                    // The end of the PHP code: nothing may follow.
                    if !src[self.i + 2..].iter().all(u8::is_ascii_whitespace) {
                        return None;
                    }
                    break;
                }
                b'`' if self.rules == Rules::Go => {
                    let end = memchr::memchr(b'`', &src[self.i + 1..])?;
                    self.i += end + 2;
                    self.prev = Tok::Literal;
                }
                b'`' if self.rules.js_like() => {
                    self.i += 1;
                    self.template()?;
                }
                b'0'..=b'9' => self.number(),
                _ if is_ident_byte(b) => self.ident()?,
                _ => self.punct(b)?,
            }
        }
        Some(())
    }

    /// Go, Kotlin, Swift and Scala end a statement at a newline after a
    /// token that can end one.
    fn newline(&mut self) {
        if self.rules.newline_ends_statements()
            && self
                .levels
                .last()
                .is_some_and(|l| l.close == b'}' || l.close == 0)
            && matches!(
                self.prev,
                Tok::Ident(..)
                    | Tok::Literal
                    | Tok::CloseParen
                    | Tok::CloseBracket
                    | Tok::CloseBrace
            )
        {
            *self.stmt() = Stmt::default();
        }
    }

    fn line_comment(&mut self) -> Option<()> {
        let end = memchr::memchr(b'\n', &self.src[self.i..]).map_or(self.src.len(), |n| self.i + n);
        // `?>` ends PHP code even in a line comment.
        if self.rules == Rules::Php && memchr::memmem::find(&self.src[self.i..end], b"?>").is_some()
        {
            return None;
        }
        self.i = end;
        Some(())
    }

    fn block_comment(&mut self) -> Option<()> {
        let mut depth = 0usize;
        while self.i < self.src.len() {
            match (self.src[self.i], self.at(self.i + 1)) {
                (b'/', b'*') => {
                    depth += 1;
                    self.i += 2;
                }
                (b'*', b'/') => {
                    depth -= 1;
                    self.i += 2;
                    if depth == 0 || !self.rules.nested_comments() {
                        return Some(());
                    }
                }
                _ => self.i += 1,
            }
        }
        None
    }

    /// A C preprocessor line with its continuations.
    fn preprocessor(&mut self) -> Option<()> {
        while self.i < self.src.len() {
            match self.src[self.i] {
                b'\n' => return Some(()),
                b'\\' => self.i += 2,
                b'/' if self.at(self.i + 1) == b'/' => return self.line_comment(),
                b'/' if self.at(self.i + 1) == b'*' => self.block_comment()?,
                b'"' | b'\'' => {
                    // `#error don't` is fine: a quote that does not close on
                    // the line ends the directive's literal.
                    let q = self.src[self.i];
                    self.i += 1;
                    while self.i < self.src.len() && !matches!(self.src[self.i], b'\n') {
                        match self.src[self.i] {
                            b'\\' => self.i += 2,
                            c if c == q => break,
                            _ => self.i += 1,
                        }
                    }
                    if self.at(self.i) == q {
                        self.i += 1;
                    }
                }
                _ => self.i += 1,
            }
        }
        Some(())
    }

    /// A `"` string, the quote at `self.i`; Java text blocks too.
    fn string(&mut self) -> Option<()> {
        let src = self.src;
        if matches!(self.rules, Rules::Java | Rules::CSharp) && src[self.i..].starts_with(b"\"\"\"")
        {
            self.i += 3;
            while self.i < src.len() {
                match src[self.i] {
                    b'\\' => self.i += 2,
                    b'"' if src[self.i..].starts_with(b"\"\"\"") => {
                        self.i += 3;
                        return Some(());
                    }
                    _ => self.i += 1,
                }
            }
            return None;
        }
        self.quoted(b'"')
    }

    /// A C# verbatim string, `@"…"` with `""` for a quote, the quote at
    /// `self.i`.
    fn verbatim(&mut self) -> Option<()> {
        let src = self.src;
        self.i += 1;
        while self.i < src.len() {
            if src[self.i] == b'"' {
                if self.at(self.i + 1) != b'"' {
                    self.i += 1;
                    self.prev = Tok::Literal;
                    return Some(());
                }
                self.i += 1;
            }
            self.i += 1;
        }
        None
    }

    /// A Kotlin or Scala `"` or `"""` string, the quote at `self.i`.
    fn dollar_string(&mut self) -> Option<()> {
        // Scala interpolates only after an interpolator: `s"…"`.
        let interpolated =
            self.rules == Rules::Kotlin || matches!(self.prev, Tok::Ident(_, e) if e == self.i);
        if self.src[self.i..].starts_with(b"\"\"\"") {
            self.i += 3;
            return self.str_body(Str::Triple { interpolated });
        }
        if interpolated {
            self.i += 1;
            return self.str_body(Str::Dollar);
        }
        self.quoted(b'"')?;
        self.prev = Tok::Literal;
        Some(())
    }

    /// A Swift string after its `hashes` `#`s, the quote at `self.i`.
    fn swift_string(&mut self, hashes: usize) -> Option<()> {
        let triple = self.src[self.i..].starts_with(b"\"\"\"");
        self.i += if triple { 3 } else { 1 };
        self.str_body(Str::Swift { triple, hashes })
    }

    /// The rest of a string of `kind`, from `self.i`: up to its end, or up
    /// to an interpolation, which is lexed as code on a new level.
    fn str_body(&mut self, kind: Str) -> Option<()> {
        let src = self.src;
        while self.i < src.len() {
            let c = src[self.i];
            match kind {
                Str::Dollar => match c {
                    b'\\' => self.i += 2,
                    b'\n' => return None,
                    b'"' => {
                        self.i += 1;
                        break;
                    }
                    b'$' if self.at(self.i + 1) == b'{' => {
                        return self.interpolation(2, b'}', kind);
                    }
                    _ => self.i += 1,
                },
                Str::Triple { interpolated } => match c {
                    b'"' if src[self.i..].starts_with(b"\"\"\"") => {
                        // The last three of a run of quotes close it.
                        self.i += src[self.i..].iter().take_while(|&&q| q == b'"').count();
                        break;
                    }
                    b'$' if interpolated && self.at(self.i + 1) == b'{' => {
                        return self.interpolation(2, b'}', kind);
                    }
                    _ => self.i += 1,
                },
                Str::Swift { triple, hashes } => {
                    let hashes_at = |at: usize| {
                        src.len() >= at + hashes && src[at..at + hashes].iter().all(|&h| h == b'#')
                    };
                    match c {
                        b'\\' if hashes_at(self.i + 1) => {
                            if self.at(self.i + 1 + hashes) == b'(' {
                                return self.interpolation(2 + hashes, b')', kind);
                            }
                            self.i += 2 + hashes;
                        }
                        b'\n' if !triple => return None,
                        b'"' if (!triple || src[self.i..].starts_with(b"\"\"\""))
                            && hashes_at(self.i + if triple { 3 } else { 1 }) =>
                        {
                            self.i += hashes + if triple { 3 } else { 1 };
                            break;
                        }
                        _ => self.i += 1,
                    }
                }
            }
        }
        self.prev = Tok::Literal;
        Some(())
    }

    /// An interpolation of `kind` opening with `open` bytes at `self.i`,
    /// closed by `close`.
    fn interpolation(&mut self, open: usize, close: u8, kind: Str) -> Option<()> {
        self.i += open;
        let init = self.levels.last().is_some_and(|l| l.init);
        let mut level = Level::new(close, init);
        level.resume = Some(kind);
        self.levels.push(level);
        self.prev = Tok::Punct(b'(');
        Some(())
    }

    /// A PHP heredoc or nowdoc, `<<<` at `self.i`: up to the line that
    /// starts with its name.
    fn heredoc(&mut self) -> Option<()> {
        let src = self.src;
        let mut i = self.i + 3;
        while matches!(src.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let quoted = matches!(src.get(i), Some(b'\'' | b'"'));
        if quoted {
            i += 1;
        }
        let start = i;
        while i < src.len() && is_ident_byte(src[i]) {
            i += 1;
        }
        let name = &src[start..i];
        if name.is_empty() {
            return None;
        }
        let mut line = memchr::memchr(b'\n', &src[i..])? + i + 1;
        loop {
            let text = &src[line..];
            let indent = text
                .iter()
                .take_while(|&&c| c == b' ' || c == b'\t')
                .count();
            if text[indent..].starts_with(name)
                && !text
                    .get(indent + name.len())
                    .is_some_and(|&c| is_ident_byte(c))
            {
                self.i = line + indent + name.len();
                return Some(());
            }
            line += memchr::memchr(b'\n', text)? + 1;
        }
    }

    /// A literal closed by `q` with backslash escapes, the opening quote at
    /// `self.i`.
    fn quoted(&mut self, q: u8) -> Option<()> {
        let src = self.src;
        let multiline = self.rules.multiline_strings();
        self.i += 1;
        while self.i < src.len() {
            match src[self.i] {
                b'\\' => self.i += 2,
                b'\n' if !multiline => return None,
                c if c == q => {
                    self.i += 1;
                    return Some(());
                }
                _ => self.i += 1,
            }
        }
        None
    }

    /// `'`: a character, rune or string literal, or a Rust lifetime or
    /// Scala symbol.
    fn quote(&mut self) -> Option<()> {
        if matches!(self.rules, Rules::Rust | Rules::Scala) && self.at(self.i + 1) != b'\\' {
            // `'x'` is a character; `'a` (no closing quote after one
            // character) a lifetime or label.
            let c = self.at(self.i + 1);
            let len = match c {
                0xf0.. => 4,
                0xe0.. => 3,
                0xc0.. => 2,
                _ => 1,
            };
            if self.at(self.i + 1 + len) == b'\'' {
                self.i += 2 + len;
                self.prev = Tok::Literal;
            } else {
                self.i += 1;
                self.prev = Tok::Punct(b'\'');
            }
            return Some(());
        }
        self.quoted(b'\'')?;
        self.prev = Tok::Literal;
        Some(())
    }

    /// A template literal's text after `` ` `` or after the `}` of a `${`.
    fn template(&mut self) -> Option<()> {
        let src = self.src;
        while self.i < src.len() {
            match src[self.i] {
                b'\\' => self.i += 2,
                b'`' => {
                    self.i += 1;
                    self.prev = Tok::Literal;
                    return Some(());
                }
                b'$' if self.at(self.i + 1) == b'{' => {
                    self.i += 2;
                    let init = self.levels.last().is_some_and(|l| l.init);
                    self.levels.push(Level::new(b'`', init));
                    self.prev = Tok::Punct(b'{');
                    return Some(());
                }
                _ => self.i += 1,
            }
        }
        None
    }

    /// Whether a `/` here starts a regular expression rather than a division.
    fn regex_allowed(&self) -> bool {
        match self.prev {
            // `</` closes a JSX element.
            Tok::Punct(b'<') => false,
            Tok::None | Tok::Punct(_) | Tok::FatArrow => true,
            Tok::Ident(s, e) => matches!(
                &self.src[s..e],
                b"return"
                    | b"typeof"
                    | b"instanceof"
                    | b"in"
                    | b"of"
                    | b"new"
                    | b"delete"
                    | b"void"
                    | b"throw"
                    | b"case"
                    | b"do"
                    | b"else"
                    | b"yield"
                    | b"await"
            ),
            Tok::Literal | Tok::CloseParen | Tok::CloseBracket | Tok::CloseBrace => false,
        }
    }

    fn regex(&mut self) -> Option<()> {
        let src = self.src;
        self.i += 1;
        let mut class = false;
        while self.i < src.len() {
            match src[self.i] {
                b'\\' => self.i += 2,
                b'\n' => return None,
                b'[' => {
                    class = true;
                    self.i += 1;
                }
                b']' => {
                    class = false;
                    self.i += 1;
                }
                b'/' if !class => {
                    self.i += 1;
                    while self.i < src.len() && is_ident_byte(src[self.i]) {
                        self.i += 1;
                    }
                    return Some(());
                }
                _ => self.i += 1,
            }
        }
        None
    }

    fn number(&mut self) {
        let src = self.src;
        self.i += 1;
        while self.i < src.len() {
            let b = src[self.i];
            // C++14 digit separators: `1'000`.
            if is_ident_byte(b) || b == b'.' || (b == b'\'' && self.rules == Rules::Cpp) {
                self.i += 1;
            } else {
                break;
            }
        }
        self.prev = Tok::Literal;
    }

    fn ident(&mut self) -> Option<()> {
        let src = self.src;
        let start = self.i;
        while self.i < src.len() && is_ident_byte(src[self.i]) {
            self.i += 1;
        }
        let w = &src[start..self.i];
        let next = self.at(self.i);
        // Raw strings: Rust `r#"…"#`, C++ `R"d(…)d"`.
        if self.rules == Rules::Rust
            && matches!(w, b"r" | b"br" | b"cr")
            && (next == b'"' || next == b'#')
        {
            let hashes = src[self.i..].iter().take_while(|&&b| b == b'#').count();
            if self.at(self.i + hashes) == b'"' {
                self.i += hashes + 1;
                loop {
                    let n = memchr::memchr(b'"', &src[self.i..])?;
                    self.i += n + 1;
                    if src[self.i..].iter().take_while(|&&b| b == b'#').count() >= hashes {
                        self.i += hashes;
                        break;
                    }
                }
                self.prev = Tok::Literal;
                return Some(());
            }
        }
        if self.rules == Rules::Cpp
            && next == b'"'
            && matches!(w, b"R" | b"LR" | b"uR" | b"UR" | b"u8R")
        {
            let open = memchr::memchr(b'(', &src[self.i..])?;
            let delim = &src[self.i + 1..self.i + open];
            let mut close = Vec::with_capacity(delim.len() + 2);
            close.push(b')');
            close.extend_from_slice(delim);
            close.push(b'"');
            let body = &src[self.i + open..];
            let end = body
                .windows(close.len())
                .position(|w| w == close.as_slice())?;
            self.i += open + end + close.len();
            self.prev = Tok::Literal;
            return Some(());
        }
        // C# `$@"…"`.
        if self.rules == Rules::CSharp && w == b"$" && next == b'@' && self.at(self.i + 1) == b'"' {
            self.i += 1;
            return self.verbatim();
        }
        if self.in_body == 0 && matches!(self.rules, Rules::C | Rules::Cpp) {
            if is_attribute_macro(w) {
                // Left out with its arguments, and invisible to the rules.
                let mut end = self.i;
                while matches!(self.at(end), b' ' | b'\t') {
                    end += 1;
                }
                if self.at(end) == b'(' {
                    end = self.close_paren(end)?;
                } else {
                    end = self.i;
                }
                self.out.hidden.push(start..end);
                self.i = end;
                return Some(());
            }
            // A macro call at the file level that starts a declaration.
            let starts = match self.prev {
                Tok::None | Tok::Punct(b';' | b'{') | Tok::CloseBrace => true,
                Tok::Ident(s, e) => matches!(&src[s..e], b"static" | b"extern" | b"const"),
                _ => false,
            };
            if next == b'(' && starts && self.levels.len() == 1 && is_macro_name(w) {
                self.macro_args = true;
            }
        }
        if self.in_body == 0 {
            let rules = self.rules;
            let stmt = self.stmt();
            if rules.is_function_keyword(w) {
                stmt.func = true;
                // `class func f() { … }` is a function.
                if rules == Rules::Swift {
                    stmt.typ = false;
                }
            } else if rules.is_type_keyword(w) {
                stmt.typ = true;
                stmt.enum_kw |= w == b"enum";
            } else if rules == Rules::Java && w == b"throws" {
                stmt.throws = true;
            }
        }
        self.prev = Tok::Ident(start, self.i);
        Some(())
    }

    fn punct(&mut self, b: u8) -> Option<()> {
        let next = self.at(self.i + 1);
        match b {
            b'(' | b'[' => {
                let init = self.levels.last().is_some_and(|l| l.init);
                let mut level = Level::new(if b == b'(' { b')' } else { b']' }, init);
                let lens = (self.out.bodies.len(), self.out.hidden.len());
                if b == b'(' && std::mem::take(&mut self.macro_args) {
                    level.args = Some(lens);
                    level.body = Some(self.i);
                } else if b == b'('
                    && self.in_body == 0
                    && matches!(self.rules, Rules::C | Rules::Cpp)
                    && matches!(self.prev, Tok::Ident(..) | Tok::CloseParen)
                    && self
                        .levels
                        .last()
                        .is_some_and(|l| matches!(l.close, 0 | b'}') && !l.init)
                {
                    // In a struct, a prototype is a function pointer's: `(*f)(…);`.
                    let proto = self.levels.len() == 1 || self.prev == Tok::CloseParen;
                    // `f(int a) LOCKS_REQUIRED(x) {`: the macro's arguments
                    // are not the parameters.
                    let macro_call =
                        matches!(self.prev, Tok::Ident(s, e) if is_macro_name(&self.src[s..e]));
                    level.params = (!macro_call).then_some((self.i, lens.0, lens.1, proto));
                    level.macro_call = macro_call;
                }
                self.levels.push(level);
            }
            b')' | b']' => {
                let level = self.levels.pop()?;
                if level.close != b || self.levels.is_empty() {
                    return None;
                }
                if let Some(kind) = level.resume {
                    self.i += 1;
                    return self.str_body(kind);
                }
                if let (Some((bodies, hidden)), Some(open)) = (level.args, level.body) {
                    self.out.bodies.truncate(bodies);
                    self.out.hidden.truncate(hidden);
                    // A last argument that is a name stays: `static
                    // DEFINE_PER_CPU(int, x);` parses as a declaration of `x`
                    // in parentheses, `static DEFINE_PER_CPU();` does not.
                    let src = self.src;
                    let close = self.i;
                    let mut end = close;
                    while end > open + 1 && src[end - 1].is_ascii_whitespace() {
                        end -= 1;
                    }
                    let mut start = end;
                    while start > open + 1 && is_ident_byte(src[start - 1]) {
                        start -= 1;
                    }
                    let mut before = start;
                    while before > open + 1 && src[before - 1].is_ascii_whitespace() {
                        before -= 1;
                    }
                    let last_name = start < end
                        && !src[start].is_ascii_digit()
                        && (before == open + 1 || src[before - 1] == b',');
                    let hide = if last_name {
                        open + 1..start
                    } else {
                        open + 1..close
                    };
                    if !hide.is_empty() {
                        self.out.hidden.push(hide);
                    }
                }
                if b == b')' {
                    let lens = (self.out.bodies.len(), self.out.hidden.len());
                    let macro_call = level.macro_call;
                    let params = level
                        .params
                        .filter(|&(open, b, h, _)| {
                            (b, h) == lens && looks_like_params(&self.src[open + 1..self.i])
                        })
                        .map(|(open, _, _, proto)| (open + 1, self.i, proto));
                    if !macro_call {
                        self.stmt().params = params;
                    }
                    self.stmt().close_paren = true;
                    self.prev = Tok::CloseParen;
                } else {
                    self.prev = Tok::CloseBracket;
                }
                self.i += 1;
                return Some(());
            }
            b'@' if self.rules == Rules::CSharp && next == b'"' => {
                self.i += 1;
                return self.verbatim();
            }
            b'{' => {
                self.open_brace();
                self.i += 1;
                return Some(());
            }
            b'}' => {
                let level = self.levels.pop()?;
                if self.levels.is_empty() {
                    return None;
                }
                self.i += 1;
                match level.close {
                    b'`' => return self.template(),
                    b'}' => {}
                    _ => return None,
                }
                if let Some(kind) = level.resume {
                    return self.str_body(kind);
                }
                if let Some(start) = level.body {
                    self.in_body -= 1;
                    if self.in_body == 0 && start + 1 < self.i - 1 {
                        self.out.bodies.push(start + 1..self.i - 1);
                    }
                }
                if !level.keep {
                    *self.stmt() = Stmt::default();
                }
                self.prev = Tok::CloseBrace;
                return Some(());
            }
            b';' => {
                // A C prototype's parameters.
                if self.rules == Rules::C
                    && self.in_body == 0
                    && self.prev == Tok::CloseParen
                    && let Some((s, e, true)) = self.stmt().params
                {
                    self.out.hidden.push(s..e);
                }
                *self.stmt() = Stmt::default();
                if let Some(level) = self.levels.last_mut() {
                    level.constants = false;
                }
            }
            b'=' if next == b'>' => {
                self.i += 2;
                self.prev = Tok::FatArrow;
                return Some(());
            }
            b'=' if next == b'=' => {
                self.i += 2;
                self.prev = Tok::Punct(b'=');
                return Some(());
            }
            b'=' if !matches!(self.prev, Tok::Punct(b'<' | b'>' | b'!')) => {
                self.stmt().eq = true;
            }
            b'-' if next == b'>' => {
                let stmt = self.stmt();
                if stmt.close_paren {
                    stmt.arrow = true;
                }
                self.i += 2;
                self.prev = Tok::Punct(b'>');
                return Some(());
            }
            b':' if next == b':' => {
                self.i += 2;
                self.prev = Tok::Punct(b':');
                return Some(());
            }
            b':' => {
                let stmt = self.stmt();
                if stmt.close_paren {
                    stmt.paren_colon = true;
                }
            }
            _ => {}
        }
        self.i += 1;
        self.prev = Tok::Punct(b);
        Some(())
    }

    /// The end of the parenthesized group opening at `open`, past its `)`.
    fn close_paren(&self, open: usize) -> Option<usize> {
        let src = self.src;
        let mut depth = 0usize;
        let mut i = open;
        while i < src.len() {
            match src[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                q @ (b'"' | b'\'') => {
                    i += 1;
                    while i < src.len() && src[i] != q {
                        if src[i] == b'\n' {
                            return None;
                        }
                        i += if src[i] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\n' | b'{' | b'}' | b';' => return None,
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn open_brace(&mut self) {
        let parent = *self.levels.last().expect("the file level");
        let s = parent.stmt;
        let prev_word = match self.prev {
            Tok::Ident(a, e) => &self.src[a..e],
            _ => b"",
        };
        let body = self.in_body == 0
            && match self.rules {
                // A composite literal's value (`= T{…}`) is skipped too.
                Rules::Go => (s.func || s.eq) && !matches!(prev_word, b"struct" | b"interface"),
                Rules::Rust => s.func,
                // So is an initializer (`= { … }`).
                Rules::C | Rules::Cpp => {
                    !parent.init
                        && (s.eq
                            || self.prev == Tok::CloseParen
                            || (s.close_paren
                                && (matches!(
                                    prev_word,
                                    b"const" | b"override" | b"final" | b"noexcept" | b"volatile"
                                ) || self.prev == Tok::Punct(b'&')
                                    || (self.rules == Rules::Cpp && s.arrow))))
                }
                Rules::Java | Rules::CSharp => {
                    !parent.init
                        && !parent.constants
                        && !s.eq
                        && !s.typ
                        && (self.prev == Tok::CloseParen || s.throws)
                }
                Rules::Kotlin | Rules::Swift | Rules::Scala | Rules::Php => s.func && !s.typ,
                // Not lexed here.
                Rules::Python | Rules::Ruby => false,
                Rules::Js | Rules::Ts => {
                    self.prev == Tok::FatArrow
                        || (s.func && !s.typ)
                        || (!s.typ
                            && (self.prev == Tok::CloseParen
                                || (self.rules == Rules::Ts && s.paren_colon)))
                }
            };
        let mut level = Level::new(b'}', parent.init || (s.eq && !body));
        level.keep = self.rules == Rules::Go && matches!(prev_word, b"struct" | b"interface");
        level.constants = self.rules == Rules::Java && s.enum_kw;
        if body {
            // A C or C++ function's parameters.
            if matches!(self.rules, Rules::C | Rules::Cpp)
                && !s.eq
                && let Some((s, e, _)) = s.params
            {
                self.out.hidden.push(s..e);
            }
            level.body = Some(self.i);
            level.init = false;
            self.in_body += 1;
        }
        self.levels.push(level);
        self.prev = Tok::Punct(b'{');
    }
}

/// C attribute macros, which the grammar does not know: left out of the
/// parse, where they are syntax errors (and error recovery is slow).
/// Linux's, as the most common C code base.
fn is_attribute_macro(w: &[u8]) -> bool {
    matches!(
        w,
        b"__attribute__"
            | b"__attribute"
            | b"__init"
            | b"__exit"
            | b"__initdata"
            | b"__initconst"
            | b"__exitdata"
            | b"__init_memblock"
            | b"__initdata_memblock"
            | b"__meminit"
            | b"__meminitdata"
            | b"__ref"
            | b"__refdata"
            | b"__refconst"
            | b"__iomem"
            | b"__user"
            | b"__kernel"
            | b"__force"
            | b"__rcu"
            | b"__percpu"
            | b"__bitwise"
            | b"__safe"
            | b"__nocast"
            | b"__maybe_unused"
            | b"__always_unused"
            | b"__used"
            | b"__always_inline"
            | b"__inline"
            | b"noinline"
            | b"__noinline"
            | b"__cold"
            | b"__hot"
            | b"__pure"
            | b"__weak"
            | b"__visible"
            | b"__noreturn"
            | b"__must_check"
            | b"__deprecated"
            | b"__read_mostly"
            | b"__ro_after_init"
            | b"__sched"
            | b"__kprobes"
            | b"__lockfunc"
            | b"__latent_entropy"
            | b"__randomize_layout"
            | b"__no_randomize_layout"
            | b"__packed"
            | b"__attribute_const__"
            | b"__malloc"
            | b"__flatten"
            | b"__naked"
            | b"__noclone"
            | b"__nocfi"
            | b"__noscs"
            | b"__no_kcsan"
            | b"__no_sanitize_address"
            | b"__net_init"
            | b"__net_exit"
            | b"__net_initdata"
            | b"__cacheline_aligned"
            | b"__cacheline_aligned_in_smp"
            | b"____cacheline_aligned"
            | b"____cacheline_aligned_in_smp"
            | b"____cacheline_internodealigned_in_smp"
            | b"notrace"
            | b"asmlinkage"
            | b"__aligned"
            | b"__printf"
            | b"__scanf"
            | b"__section"
            | b"__acquires"
            | b"__releases"
            | b"__must_hold"
            | b"__cond_lock"
            | b"__counted_by"
            | b"__alloc_size"
            | b"__nonstring"
    )
}

/// Whether the insides of a group may be C parameters: not a declarator in
/// parentheses (`(*f)`), no literals (`SEC(".maps")`, `ALIGN(void *p, 8)`).
fn looks_like_params(inside: &[u8]) -> bool {
    let first = inside.iter().find(|c| !c.is_ascii_whitespace());
    if matches!(first, None | Some(b'*' | b'^' | b'&')) {
        return false;
    }
    let mut arg_start = true;
    for &c in inside {
        match c {
            b'"' | b'\'' => return false,
            b'0'..=b'9' if arg_start => return false,
            b',' => arg_start = true,
            c if c.is_ascii_whitespace() => {}
            _ => arg_start = false,
        }
    }
    true
}

/// A macro by convention: all capitals, digits and `_`, with a letter.
fn is_macro_name(w: &[u8]) -> bool {
    w.len() > 1
        && w.iter().any(u8::is_ascii_uppercase)
        && w.iter()
            .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bodies' insides as text.
    fn found(rules: Rules, src: &str) -> Option<Vec<&str>> {
        bodies(rules, src.as_bytes()).map(|v| v.bodies.into_iter().map(|r| &src[r]).collect())
    }

    #[test]
    fn go() {
        let src = "package p\n\
            func f() struct{ x int } { return x{} }\n\
            type T struct { a int }\n\
            func (t *T) M(s string) (int, error) {\n\tif s == \"}\" { return 0, nil }\n\treturn `\n}`, nil\n}\n\
            func asm()\n\
            var x = 1\n\
            var m = map[string]struct{ a int }{\"k\": {1}}\n";
        assert_eq!(
            found(Rules::Go, src).unwrap(),
            [
                " return x{} ",
                "\n\tif s == \"}\" { return 0, nil }\n\treturn `\n}`, nil\n",
                "\"k\": {1}"
            ]
        );
    }

    #[test]
    fn c_and_cpp() {
        let src = "#define F(x) { x }\n\
            struct s { int (*f)(void); };\n\
            static int a[] = { 1, 2 };\n\
            int main(void) { char c = '}'; return 0; }\n\
            void g(void) __attribute__((x)) { /* } */ }\n";
        assert_eq!(
            found(Rules::C, src).unwrap(),
            [" 1, 2 ", " char c = '}'; return 0; ", " /* } */ "]
        );
        let cpp = "namespace n {\nclass A {\n  int f() const { return 1'000; }\n  auto g() -> int { return R\"x(})x\"[0]; }\n  A() : v{1} { }\n};\nauto l = [](int x) { return x; };\n}\n";
        assert_eq!(
            found(Rules::Cpp, cpp).unwrap(),
            [" return 1'000; ", " return R\"x(})x\"[0]; ", " return x; "]
        );
        // Unbalanced through `#if` branches.
        assert!(
            found(
                Rules::C,
                "#if A\nvoid f(int a) {\n#else\nvoid f(void) {\n#endif\n}\n"
            )
            .is_none()
        );
    }

    #[test]
    fn java() {
        let src = "class A extends B {\n  enum K { X(1) { void m() { } }, Y; void n() { } }\n  void f() throws E { g(); }\n  record P(int x) { P { } }\n  Runnable r = new Runnable() { public void run() { } };\n}\n";
        assert_eq!(found(Rules::Java, src).unwrap(), [" ", " ", " g(); "]);
    }

    #[test]
    fn csharp() {
        let src = "#region R\nnamespace N {\n  record P(int X) { }\n  class C : B {\n    string s = @\"C:\\\";\n    public C() : base(1) { var t = $@\"{s}\\\"; }\n    public string Name { get { return s; } }\n    void M<T>() where T : new() { }\n  }\n}\n#endregion\n";
        assert_eq!(
            found(Rules::CSharp, src).unwrap(),
            [" var t = $@\"{s}\\\"; ", " "]
        );
    }

    #[test]
    fn rust() {
        let src = "impl<'a> X<'a> {\n    fn f(&self) -> Result<(), E> where E: Fmt { let c = '}'; let s = r#\"}\"#; /* /* } */ */ }\n    fn g();\n}\nfn main() { let l: &'static str = \"\n}\"; }\n";
        assert_eq!(
            found(Rules::Rust, src).unwrap(),
            [
                " let c = '}'; let s = r#\"}\"#; /* /* } */ */ ",
                " let l: &'static str = \"\n}\"; "
            ]
        );
    }

    #[test]
    fn javascript_and_typescript() {
        let src = "const re = /[}]/g;\nconst f = (x) => { return `${x}}` };\nclass A extends mix(B) {\n  m(a) { return a / 2 }\n}\ndescribe('x', function () { it('y', () => {}) });\n";
        assert_eq!(
            found(Rules::Js, src).unwrap(),
            [" return `${x}}` ", " return a / 2 ", " it('y', () => {}) "]
        );
        let ts = "interface I { m(): void }\nclass C { m(): string { return '' } }\n";
        assert_eq!(found(Rules::Ts, ts).unwrap(), [" return '' "]);
        assert!(found(Rules::Js, "const s = 'unterminated\n").is_none());
    }

    #[test]
    fn kotlin() {
        let src = "package p\nfun interface F { fun f() }\nclass A(val x: Int) {\n    fun m(s: String): String { return \"${s.map { \"}\" }} $x\" + \"\"\"raw } ${x}\"\"\" }\n    val p: Int get() { return 1 }\n    /* /* nested } */ */\n    fun `quoted name`() { }\n}\nfun top() = when (1) { else -> { } }\n";
        assert_eq!(
            found(Rules::Kotlin, src).unwrap(),
            [
                " return \"${s.map { \"}\" }} $x\" + \"\"\"raw } ${x}\"\"\" ",
                " ",
                " else -> { } "
            ]
        );
    }

    #[test]
    fn swift() {
        let src = "class C {\n    class func f() -> Int { return \"\\(g(\"}\"))\".count }\n    init(x: Int) { }\n    var v: Int { 1 }\n    let r = #\"raw \"} \\#(x)\"#\n}\nfunc top() { let s = \"\"\"\n} \\(1)\n\"\"\" }\n";
        assert_eq!(
            found(Rules::Swift, src).unwrap(),
            [
                " return \"\\(g(\"}\"))\".count ",
                " ",
                " let s = \"\"\"\n} \\(1)\n\"\"\" "
            ]
        );
    }

    #[test]
    fn scala() {
        let src = "object O {\n  def f(x: Int): Int = { val s = s\"${x}}\" ; 'c' ; x }\n  def g = 1\n  val h = new A { def i = { 2 } }\n  /* /* } */ */\n}\n";
        assert_eq!(
            found(Rules::Scala, src).unwrap(),
            [" val s = s\"${x}}\" ; 'c' ; x ", " 2 "]
        );
    }

    #[test]
    fn php() {
        let src = "<?php\nnamespace N;\nclass A {\n    public function f($x) { return \"{$x['}']}\" . '}'; }\n    abstract function g();\n}\nfunction h() { $s = <<<EOT\n}\nEOT;\n}\n# comment }\n$c = function () { };\n";
        assert_eq!(
            found(Rules::Php, src).unwrap(),
            [
                " return \"{$x['}']}\" . '}'; ",
                " $s = <<<EOT\n}\nEOT;\n",
                " "
            ]
        );
        // HTML around the code, or after `?>`.
        assert!(found(Rules::Php, "<html><?php function f() { } ?>\n").is_none());
        assert!(found(Rules::Php, "<?php function f() { } ?>\n<p>x</p>\n").is_none());
        assert!(found(Rules::Php, "<?php function f() { }\n?>\n").is_some());
    }
}
