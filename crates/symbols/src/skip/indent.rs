//! Function bodies of Python and Ruby, found from indentation.
//!
//! Neither language brackets its bodies, so a body is the lines indented
//! deeper than its header, up to the next line at the header's indentation
//! (Python) or its `end` at the same indentation (Ruby, which is how Ruby is
//! written). The lexer tracks only what can make a line not code: strings,
//! comments, brackets and line continuations in Python; heredocs,
//! `=begin` blocks and a line's own literals in Ruby. Anything it cannot
//! place gives `None` or leaves that body in, and the caller parses the
//! file again in full when the parse with bodies left out has any syntax
//! error.
//!
//! What is left out are whole lines, from a line start to a line start, so
//! the parser sees the indentation of every line it reads:
//! - Python keeps a body's first statement, a block needs one (and the first
//!   statement of each block it opens, `if x:` needs its own), and leaves
//!   out the rest.
//! - Ruby keeps `def …` and `end`, and the first and last lines of the
//!   `do … end` and `{ … }` that span lines (blocks are locals, and hash
//!   literals hold no definitions; array literals can, `A = [B = 1]`).

use std::ops::Range;

/// Columns of a line's indentation (tabs to the next multiple of 8), and
/// the offset of its first other byte.
fn indentation(line: &[u8]) -> (usize, usize) {
    let mut col = 0;
    for (i, &c) in line.iter().enumerate() {
        match c {
            b' ' => col += 1,
            b'\t' => col = (col / 8 + 1) * 8,
            b'\x0c' | b'\r' => {}
            _ => return (col, i),
        }
    }
    (col, line.len())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// The line ending at the first `\n` at or after `at`, up to and with the
/// newline.
fn line_end(src: &[u8], at: usize) -> usize {
    memchr::memchr(b'\n', &src[at..]).map_or(src.len(), |n| at + n + 1)
}

/// A Python logical line (a statement or a block header).
struct Logical {
    /// Where its first physical line starts.
    start: usize,
    indent: usize,
    /// Ends with `:` (a block header).
    colon: bool,
    /// Starts with `def` or `async def`.
    def: bool,
    /// Starts with `try` (`try:` needs its `except`).
    try_: bool,
}

/// Python's logical lines, or `None` if the lexer gave up.
fn python_lines(src: &[u8]) -> Option<Vec<Logical>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let line_start = i;
        let (indent, text) = indentation(&src[i..line_end(src, i)]);
        i += text;
        if i >= src.len() || matches!(src[i], b'\n' | b'#') {
            // A blank or comment line.
            i = line_end(src, i);
            continue;
        }
        let mut line = Logical {
            start: line_start,
            indent,
            colon: false,
            def: false,
            try_: false,
        };
        let mut words = 0;
        let mut first: &[u8] = b"";
        let mut depth = 0usize;
        while let Some(&c) = src.get(i) {
            match c {
                b'\n' => {
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                b' ' | b'\t' | b'\r' | b'\x0c' => i += 1,
                b'#' => i = memchr::memchr(b'\n', &src[i..]).map_or(src.len(), |n| i + n),
                b'\\' if matches!(src.get(i + 1), Some(b'\n')) => i += 2,
                b'\\' if src[i + 1..].starts_with(b"\r\n") => i += 3,
                b'"' | b'\'' => {
                    i = python_string(src, i)?;
                    line.colon = false;
                }
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    i += 1;
                    line.colon = false;
                }
                b')' | b']' | b'}' => {
                    depth = depth.checked_sub(1)?;
                    i += 1;
                    line.colon = false;
                }
                _ if is_ident_byte(c) => {
                    let s = i;
                    while i < src.len() && is_ident_byte(src[i]) {
                        i += 1;
                    }
                    let w = &src[s..i];
                    // A string prefix: `r"…"`, `f'…'`, `rb"""…"""`.
                    if matches!(src.get(i), Some(b'"' | b'\''))
                        && w.len() <= 2
                        && w.iter().all(|c| b"rRbBuUfF".contains(c))
                    {
                        i = python_string(src, i)?;
                    } else {
                        // The first word, or the second after `async`.
                        if words == 0 || (words == 1 && first == b"async") {
                            first = w;
                        }
                        words += 1;
                    }
                    line.colon = false;
                }
                b':' => {
                    i += 1;
                    line.colon = depth == 0;
                }
                _ => {
                    i += 1;
                    line.colon = false;
                }
            }
        }
        line.def = first == b"def";
        line.try_ = first == b"try";
        out.push(line);
    }
    Some(out)
}

/// The end of the Python string whose quote is at `at`.
fn python_string(src: &[u8], at: usize) -> Option<usize> {
    let q = src[at];
    let triple = src.len() >= at + 3 && src[at + 1] == q && src[at + 2] == q;
    let mut i = at + if triple { 3 } else { 1 };
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\n' if !triple => return None,
            c if c == q => {
                if !triple {
                    return Some(i + 1);
                }
                if src.len() >= i + 3 && src[i + 1] == q && src[i + 2] == q {
                    return Some(i + 3);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The lines of Python function bodies to leave out.
pub(super) fn python(src: &[u8]) -> Option<Vec<Range<usize>>> {
    let lines = python_lines(src)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let header = &lines[i];
        i += 1;
        if !(header.def && header.colon) {
            continue;
        }
        // The body: the lines after the header indented deeper.
        let end = lines[i..]
            .iter()
            .position(|l| l.indent <= header.indent)
            .map_or(lines.len(), |n| i + n);
        if end == i {
            continue;
        }
        // Kept: the first statement, and of each block it opens.
        let mut kept = i;
        let mut ok = true;
        while lines[kept].colon {
            if lines[kept].try_ || kept + 1 >= end || lines[kept + 1].indent <= lines[kept].indent {
                ok = false;
                break;
            }
            kept += 1;
        }
        if !ok {
            // Its nested functions' bodies can still be left out.
            continue;
        }
        if kept + 1 < end {
            let stop = lines.get(end).map_or(src.len(), |l| l.start);
            out.push(lines[kept + 1].start..stop);
        }
        i = end;
    }
    Some(out)
}

/// What a Ruby line opens, if its body can be left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opens {
    /// `def name(…)`, closed by `end`.
    Def,
    /// A trailing `do` (`do |x|`), closed by `end`.
    Do,
    /// A trailing `{` (`{ |x|`), closed by `}`.
    Brace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Code, starting at `text`.
    Code,
    /// Blank or a comment.
    Blank,
    /// A heredoc's or an `=begin` comment's text.
    Text,
}

struct Line {
    start: usize,
    indent: usize,
    /// The first byte after the indentation.
    text: usize,
    kind: Kind,
    opens: Option<Opens>,
}

/// The lines of Ruby method bodies and multi-line blocks and literals to
/// leave out.
pub(super) fn ruby(src: &[u8]) -> Option<Vec<Range<usize>>> {
    let lines = ruby_lines(src)?;
    let word = |l: &Line| {
        let rest = &src[l.text..];
        let n = rest.iter().take_while(|&&c| is_ident_byte(c)).count();
        &rest[..n]
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        i += 1;
        let Some(opens) = line.opens else { continue };
        let mut close = None;
        for (j, l) in lines.iter().enumerate().skip(i) {
            if l.kind != Kind::Code || l.indent > line.indent {
                continue;
            }
            let w = word(l);
            let first = src[l.text];
            let closes = match opens {
                Opens::Def | Opens::Do => w == b"end",
                Opens::Brace => first == b'}',
            };
            if l.indent == line.indent && closes {
                close = Some(j);
            } else if l.indent == line.indent
                && opens == Opens::Def
                && matches!(w, b"rescue" | b"ensure" | b"else")
            {
                continue;
            }
            break;
        }
        let Some(close) = close else { continue };
        // Text (heredocs, literals) at the opener's indentation that reads
        // like `def` or `end` may be code the lexer misread: unsure.
        let suspect = lines[i..close].iter().any(|l| {
            if l.kind != Kind::Text {
                return false;
            }
            let text = &src[l.start..line_end(src, l.start)];
            let (indent, at) = indentation(text);
            let w = &text[at..];
            let w = &w[..w.iter().take_while(|&&c| is_ident_byte(c)).count()];
            indent == line.indent && matches!(w, b"def" | b"end")
        });
        if suspect {
            continue;
        }
        if close > i {
            out.push(lines[i].start..lines[close].start);
        }
        // The closing line may open the next body (`end.each do |x|`).
        i = close;
    }
    Some(out)
}

/// Ruby's lines, or `None` if the lexer gave up.
fn ruby_lines(src: &[u8]) -> Option<Vec<Line>> {
    let mut out: Vec<Line> = Vec::new();
    let text_line = |start| Line {
        start,
        indent: 0,
        text: start,
        kind: Kind::Text,
        opens: None,
    };
    let mut i = 0;
    // Heredocs started on the current line: (name, `~` or `-`).
    let mut heredocs: Vec<(&[u8], bool)> = Vec::new();
    // A literal that goes on past the line it started on.
    let mut open: Option<Lit> = None;
    while i < src.len() {
        let end = line_end(src, i);
        let line = &src[i..end];
        if let Some(mut lit) = open.take() {
            // The literal's text, then maybe code.
            if let Some(at) = lit_body(line, 0, &mut lit)? {
                scan_ruby_line(&line[at..], Prev::Value, &mut heredocs, &mut open)?;
            } else {
                open = Some(lit);
            }
            out.push(text_line(i));
            i = end;
            if !heredocs.is_empty() && open.is_some() {
                return None;
            }
            i = heredoc_texts(src, i, &mut heredocs, &mut out)?;
            continue;
        }
        let (indent, text) = indentation(line);
        let body = &line[text..];
        let at_word = |w: &[u8]| {
            text == 0
                && body.starts_with(w)
                && !body.get(w.len()).is_some_and(|&c| is_ident_byte(c))
        };
        if at_word(b"=begin") {
            // Up to and with the `=end` line.
            out.push(text_line(i));
            i = end;
            loop {
                if i >= src.len() {
                    return None;
                }
                out.push(text_line(i));
                let done = src[i..].starts_with(b"=end");
                i = line_end(src, i);
                if done {
                    break;
                }
            }
            continue;
        }
        if at_word(b"__END__") {
            break;
        }
        let mut l = Line {
            start: i,
            indent,
            text: i + text,
            kind: Kind::Code,
            opens: None,
        };
        if body.iter().all(u8::is_ascii_whitespace) || body.first() == Some(&b'#') {
            l.kind = Kind::Blank;
        } else {
            l.opens = scan_ruby_line(body, Prev::Op, &mut heredocs, &mut open)?;
        }
        out.push(l);
        i = end;
        if !heredocs.is_empty() && open.is_some() {
            return None;
        }
        i = heredoc_texts(src, i, &mut heredocs, &mut out)?;
    }
    // A literal or heredoc left open.
    open.is_none().then_some(out)
}

/// The texts of `heredocs`, from line `i` on, which follow the line that
/// starts them; the line after them.
fn heredoc_texts(
    src: &[u8],
    mut i: usize,
    heredocs: &mut Vec<(&[u8], bool)>,
    out: &mut Vec<Line>,
) -> Option<usize> {
    for (name, indented) in heredocs.drain(..) {
        loop {
            if i >= src.len() {
                return None;
            }
            let e = line_end(src, i);
            let text = &src[i..e];
            let t = if indented {
                &text[indentation(text).1..]
            } else {
                text
            };
            let t = t.strip_suffix(b"\n").unwrap_or(t);
            let t = t.strip_suffix(b"\r").unwrap_or(t);
            out.push(Line {
                start: i,
                indent: 0,
                text: i,
                kind: Kind::Text,
                opens: None,
            });
            i = e;
            if t == name {
                break;
            }
        }
    }
    Some(i)
}

/// A Ruby string, percent literal or regular expression being lexed.
#[derive(Debug, Clone, Copy)]
struct Lit {
    open: u8,
    close: u8,
    /// Nested `open`s (`%w[a [b]]`).
    depth: usize,
    /// `#{…}` is code.
    interpolates: bool,
}

impl Lit {
    fn new(open: u8, interpolates: bool) -> Lit {
        let close = match open {
            b'(' => b')',
            b'[' => b']',
            b'{' => b'}',
            b'<' => b'>',
            c => c,
        };
        Lit {
            open,
            close,
            depth: 0,
            interpolates,
        }
    }
}

/// Lexes the text of `lit` on `line` from `i`: past its end, or `None`
/// (inside) if it goes on past the line. `None` outside if an
/// interpolation does.
fn lit_body(line: &[u8], mut i: usize, lit: &mut Lit) -> Option<Option<usize>> {
    while i < line.len() {
        let c = line[i];
        if c == b'\\' {
            i += 2;
            continue;
        }
        if lit.interpolates && c == b'#' && line.get(i + 1) == Some(&b'{') {
            // Code up to the matching `}`, with strings in it.
            let mut depth = 0usize;
            i += 1;
            loop {
                match *line.get(i)? {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    q @ (b'"' | b'\'') => {
                        let mut inner = Lit::new(q, q == b'"');
                        i = lit_body(line, i + 1, &mut inner)?? - 1;
                    }
                    _ => {}
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == lit.close {
            if lit.depth == 0 {
                return Some(Some(i + 1));
            }
            lit.depth -= 1;
        } else if c == lit.open && lit.open != lit.close {
            lit.depth += 1;
        }
        i += 1;
    }
    Some(None)
}

/// What the previous token on a Ruby line was, to tell `/`, `%` and `?`
/// literals from operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// An operator, a keyword, or nothing: a literal can follow.
    Op,
    /// A value: an operator follows.
    Value,
}

/// Lexes one line of Ruby code (from its first byte after the
/// indentation): what it opens, and the heredocs it starts. `None` if a
/// literal does not end on the line.
fn scan_ruby_line<'a>(
    line: &'a [u8],
    mut prev: Prev,
    heredocs: &mut Vec<(&'a [u8], bool)>,
    open: &mut Option<Lit>,
) -> Option<Option<Opens>> {
    let mut i = 0;
    // Starts a literal at `$at` (its first byte inside): goes on after it,
    // or leaves the line if it does not end on it.
    macro_rules! literal {
        ($lit:expr, $at:expr) => {{
            let mut lit = $lit;
            match lit_body(line, $at, &mut lit)? {
                Some(end) => {
                    i = end;
                    prev = Prev::Value;
                }
                None => {
                    *open = Some(lit);
                    return Some(None);
                }
            }
        }};
    }
    // A `do`, `{` or `[` with nothing after it so far but block parameters.
    let mut trailing: Option<Opens> = None;
    let mut first: &[u8] = b"";
    let mut after_def = None;
    while i < line.len() {
        let c = line[i];
        if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
            continue;
        }
        if c == b'#' {
            break;
        }
        let mut opens = None;
        match c {
            b'"' | b'\'' | b'`' => literal!(Lit::new(c, c != b'\''), i + 1),
            b'<' if line[i..].starts_with(b"<<")
                && matches!(
                    line.get(i + 2),
                    Some(b'~' | b'-' | b'A'..=b'Z' | b'_' | b'"' | b'\'')
                ) =>
            {
                let mut j = i + 2;
                let indented = matches!(line[j], b'~' | b'-');
                j += usize::from(indented);
                let quote = matches!(line.get(j), Some(b'"' | b'\'' | b'`'));
                j += usize::from(quote);
                let s = j;
                while j < line.len() && is_ident_byte(line[j]) {
                    j += 1;
                }
                if j == s {
                    // `x <<-1`: not a heredoc.
                    i += 2;
                    prev = Prev::Op;
                    continue;
                }
                heredocs.push((&line[s..j], indented));
                i = j + usize::from(quote);
                prev = Prev::Value;
            }
            b'%' if prev == Prev::Op || arg_start(line, i) => {
                let mut j = i + 1;
                let kind = line.get(j).copied().filter(|c| b"qQwWiIrsx".contains(c));
                if kind.is_some() {
                    j += 1;
                }
                let delim = *line.get(j)?;
                if delim.is_ascii_alphanumeric() || delim.is_ascii_whitespace() {
                    i += 1;
                    continue;
                }
                let interpolates = !matches!(kind, Some(b'q' | b'w' | b'i' | b's'));
                literal!(Lit::new(delim, interpolates), j + 1)
            }
            b'/' if prev == Prev::Op => literal!(Lit::new(b'/', true), i + 1),
            // `foo /x/`: a regular expression if it ends on the line.
            b'/' if arg_start(line, i)
                && lit_body(line, i + 1, &mut Lit::new(b'/', true))
                    .is_some_and(|e| e.is_some()) =>
            {
                literal!(Lit::new(b'/', true), i + 1)
            }
            b'?' if prev == Prev::Op
                && line.get(i + 1).is_some_and(|c| !c.is_ascii_whitespace()) =>
            {
                // A character literal: `?a`, `?\n`.
                i += if line[i + 1] == b'\\' { 3 } else { 2 };
                prev = Prev::Value;
            }
            b'|' if matches!(trailing, Some(Opens::Do | Opens::Brace)) => {
                // Block parameters: `do |a, b|`.
                let n = line[i + 1..].iter().position(|&c| c == b'|')?;
                i += n + 2;
                continue;
            }
            _ if is_ident_byte(c) || c == b'@' || c == b'$' => {
                let s = i;
                i += 1;
                while i < line.len() && is_ident_byte(line[i]) {
                    i += 1;
                }
                if matches!(line.get(i), Some(b'?' | b'!')) && line.get(i + 1) != Some(&b'=') {
                    i += 1;
                }
                let w = &line[s..i];
                // `do:` is a hash key.
                let key = line.get(i) == Some(&b':') && line.get(i + 1) != Some(&b':');
                if first.is_empty() {
                    first = w;
                    if w == b"def" {
                        after_def = Some(i);
                    }
                }
                if w == b"do" && !key {
                    opens = Some(Opens::Do);
                }
                prev = if !key
                    && matches!(
                        w,
                        b"if"
                            | b"unless"
                            | b"while"
                            | b"until"
                            | b"and"
                            | b"or"
                            | b"not"
                            | b"when"
                            | b"in"
                            | b"then"
                            | b"else"
                            | b"elsif"
                            | b"return"
                            | b"do"
                    ) {
                    Prev::Op
                } else {
                    Prev::Value
                };
            }
            b'{' => {
                i += 1;
                opens = Some(Opens::Brace);
                prev = Prev::Op;
            }
            b')' | b']' | b'}' => {
                i += 1;
                prev = Prev::Value;
            }
            _ => {
                i += 1;
                prev = Prev::Op;
            }
        }
        trailing = opens;
    }
    if trailing.is_some() {
        return Some(trailing);
    }
    let def = first == b"def" && after_def.is_some_and(|at| def_header(&line[at..]));
    Some(def.then_some(Opens::Def))
}

/// Whether the byte at `i` starts a method's argument after a value, as
/// Ruby reads `foo %w[a]` and `foo /x/`: a space before it, none after.
fn arg_start(line: &[u8], i: usize) -> bool {
    i > 0
        && matches!(line[i - 1], b' ' | b'\t')
        && line
            .get(i + 1)
            .is_some_and(|&c| !c.is_ascii_whitespace() && c != b'=')
}

/// Whether what follows `def` is only a name and parameters in parentheses
/// (not `def f = x`, `def f; end`, or parameters without parentheses or
/// over several lines).
fn def_header(rest: &[u8]) -> bool {
    let rest = rest.trim_ascii_start();
    let name = rest
        .iter()
        .take_while(|&&c| !matches!(c, b'(' | b' ' | b'\t' | b'\r' | b'\n' | b'#' | b';'))
        .count();
    if name == 0 {
        return false;
    }
    let mut rest = rest[name..].trim_ascii_start();
    if rest.first() == Some(&b'(') {
        let mut depth = 0usize;
        let mut end = None;
        for (i, &c) in rest.iter().enumerate() {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                b'"' | b'\'' | b'#' | b';' => return false,
                _ => {}
            }
        }
        let Some(end) = end else { return false };
        rest = rest[end..].trim_ascii_start();
    }
    rest.is_empty() || rest[0] == b'#'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn left_out(f: fn(&[u8]) -> Option<Vec<Range<usize>>>, src: &str) -> Option<Vec<&str>> {
        f(src.as_bytes()).map(|v| v.into_iter().map(|r| &src[r]).collect())
    }

    #[test]
    fn python_bodies() {
        let src = "import os\n\n@dec\ndef f(a,\n      b) -> int:\n    \"\"\"Doc.\n\n  more\"\"\"\n    x = (1,\n2)\n    return x\n\nclass C:\n    y = 1\n    async def m(self):\n        if self:\n            return 1\n        return 2\n    def one(self): return 3\n    def short(self):\n        pass\n\ndef g():\n    try:\n        pass\n    except E:\n        pass\n";
        assert_eq!(
            left_out(python, src).unwrap(),
            ["    x = (1,\n2)\n    return x\n\n", "        return 2\n",]
        );
        assert!(python(b"s = 'unterminated\n").is_none());
    }

    #[test]
    fn ruby_bodies() {
        let src = "class A < B\n  def f(a, b = 1)\n    x = <<~SQL\n      end\n    SQL\n    y\n  rescue E\n    z\n  end\n\n  def g = 1\n  def h; end\n\n  scope :s, -> { where(x: 1) }\n  included do |base|\n    def hidden; end\n  end\n  CONFIG = {\n    a: 1,\n  }.freeze\n  ROLES = [\n    A = 1,\n  ]\n  TYPES = %w[\n    a b\n  ].freeze\n  def k(x)\n    \"\n  end\"\n  end\n  def r\n    single %r{\n  (?# it's)\n    }x\n  end\n  def s = 1 % 2\nend\n=begin\n  def x\n  end\n=end\n";
        assert_eq!(
            left_out(ruby, src).unwrap(),
            [
                "    x = <<~SQL\n      end\n    SQL\n    y\n  rescue E\n    z\n",
                "    def hidden; end\n",
                "    a: 1,\n",
                // Not `k`: text reads like its `end`.
                "    single %r{\n  (?# it's)\n    }x\n",
            ]
        );
    }
}
