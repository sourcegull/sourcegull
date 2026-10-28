//! Go's `fmt` printing rules for template values: `%v` (how actions print their result),
//! `fmt.Sprint`/`Sprintln` (`print`, `println`) and `fmt.Sprintf` (`printf`), including Go's
//! `%!verb(type=value)` markers for mismatched verbs.

use crate::value::Value;

/// How an action prints its result: Go's `<no value>` for nil, `%v` otherwise.
pub(crate) fn print_top(v: &Value) -> String {
    match v {
        Value::Nil => "<no value>".to_owned(),
        Value::String(s) => s.clone(),
        other => {
            let mut s = String::new();
            write_v(other, &mut s);
            s
        }
    }
}

/// Go's `%v`.
pub(crate) fn write_v(v: &Value, out: &mut String) {
    match v {
        Value::Nil => out.push_str("<nil>"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&format_g_shortest(*f, false)),
        Value::String(s) => out.push_str(s),
        Value::PathList(l) => out.push_str(&l.join(" ")),
        Value::List(l) => {
            out.push('[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_v(e, out);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push_str("map[");
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(k);
                out.push(':');
                write_v(e, out);
            }
            out.push(']');
        }
    }
}

/// `fmt.Sprint`: operands are separated by a space when neither side is a string.
pub(crate) fn sprint(args: &[Value]) -> String {
    let mut out = String::new();
    let mut prev_string = false;
    for (i, a) in args.iter().enumerate() {
        let is_string = matches!(a, Value::String(_));
        if i > 0 && !is_string && !prev_string {
            out.push(' ');
        }
        write_v(a, &mut out);
        prev_string = is_string;
    }
    out
}

/// `fmt.Sprintln`: always space-separated, with a trailing newline.
pub(crate) fn sprintln(args: &[Value]) -> String {
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_v(a, &mut out);
    }
    out.push('\n');
    out
}

/// Decimal digits of a float: `0.d1d2d3… × 10^dp`.
struct Digits {
    neg: bool,
    d: Vec<u8>,
    dp: i32,
}

impl Digits {
    /// Parses Rust's `{:e}` output (`-1.2345e-7`).
    fn from_sci(s: &str) -> Digits {
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let mut d: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
        while d.len() > 1 && d.last() == Some(&b'0') {
            d.pop();
        }
        if d == *b"0" {
            return Digits {
                neg,
                d: Vec::new(),
                dp: 0,
            };
        }
        Digits {
            neg,
            d,
            dp: exp + 1,
        }
    }

    fn shortest(f: f64) -> Digits {
        Digits::from_sci(&format!("{f:e}"))
    }

    /// Rounded to `n` (≥ 1) significant digits.
    fn significant(f: f64, n: usize) -> Digits {
        Digits::from_sci(&format!("{:.*e}", n.saturating_sub(1), f))
    }

    fn nd(&self) -> i32 {
        self.d.len() as i32
    }

    fn digit(&self, i: i32) -> u8 {
        if i >= 0 && (i as usize) < self.d.len() {
            self.d[i as usize]
        } else {
            b'0'
        }
    }

    /// `%e` with `prec` digits after the point (strconv's fmtE).
    fn fmt_e(&self, prec: i32, upper: bool) -> String {
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        s.push(self.digit(0) as char);
        if prec > 0 {
            s.push('.');
            for i in 1..=prec {
                s.push(self.digit(i) as char);
            }
        }
        s.push(if upper { 'E' } else { 'e' });
        let exp = if self.d.is_empty() { 0 } else { self.dp - 1 };
        s.push(if exp < 0 { '-' } else { '+' });
        let a = exp.unsigned_abs();
        if a < 10 {
            s.push('0');
        }
        s.push_str(&a.to_string());
        s
    }

    /// `%f` with `prec` digits after the point (strconv's fmtF).
    fn fmt_f(&self, prec: i32) -> String {
        let mut s = String::new();
        if self.neg {
            s.push('-');
        }
        if self.dp > 0 {
            for i in 0..self.dp {
                s.push(self.digit(i) as char);
            }
        } else {
            s.push('0');
        }
        if prec > 0 {
            s.push('.');
            for i in 1..=prec {
                s.push(self.digit(self.dp + i - 1) as char);
            }
        }
        s
    }
}

fn special_float(f: f64) -> Option<String> {
    if f.is_nan() {
        Some("NaN".to_owned())
    } else if f.is_infinite() {
        Some(if f > 0.0 { "+Inf" } else { "-Inf" }.to_owned())
    } else {
        None
    }
}

/// `%v` / `%g` without a precision: the shortest representation that round-trips, in
/// exponent form when the decimal exponent is < -4 or ≥ 6 (so `1e+06`, but `123456`).
pub(crate) fn format_g_shortest(f: f64, upper: bool) -> String {
    if let Some(s) = special_float(f) {
        return s;
    }
    let digs = Digits::shortest(f);
    let prec = digs.nd();
    let eprec = 6;
    let exp = digs.dp - 1;
    if !digs.d.is_empty() && (exp < -4 || exp >= eprec) {
        return digs.fmt_e(prec - 1, upper);
    }
    digs.fmt_f((prec - digs.dp).max(0))
}

/// `%g` with an explicit precision.
fn format_g_prec(f: f64, prec: usize, upper: bool) -> String {
    if let Some(s) = special_float(f) {
        return s;
    }
    let prec = prec.max(1);
    let digs = Digits::significant(f, prec);
    let mut prec = prec as i32;
    let mut eprec = prec;
    if eprec > digs.nd() && digs.nd() >= digs.dp {
        eprec = digs.nd();
    }
    let exp = digs.dp - 1;
    if !digs.d.is_empty() && (exp < -4 || exp >= eprec) {
        if prec > digs.nd() {
            prec = digs.nd();
        }
        return digs.fmt_e(prec - 1, upper);
    }
    if prec > digs.dp {
        prec = digs.nd();
    }
    digs.fmt_f((prec - digs.dp).max(0))
}

fn format_e(f: f64, prec: usize, upper: bool) -> String {
    if let Some(s) = special_float(f) {
        return s;
    }
    let digs = Digits::significant(f, prec + 1);
    digs.fmt_e(prec as i32, upper)
}

fn format_fixed(f: f64, prec: usize) -> String {
    if let Some(s) = special_float(f) {
        return s;
    }
    let s = format!("{f:.prec$}");
    // Rust drops the sign of a negative zero that rounds to zero; Go keeps it.
    if f.is_sign_negative() && !s.starts_with('-') {
        format!("-{s}")
    } else {
        s
    }
}

/// `strconv.Quote`.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        quote_char_into(c, '"', &mut out);
    }
    out.push('"');
    out
}

fn quote_char_into(c: char, q: char, out: &mut String) {
    match c {
        '\x07' => out.push_str("\\a"),
        '\x08' => out.push_str("\\b"),
        '\x0c' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\x0b' => out.push_str("\\v"),
        '\\' => out.push_str("\\\\"),
        c if c == q => {
            out.push('\\');
            out.push(c);
        }
        c if (c as u32) < 0x20 || c as u32 == 0x7f => {
            out.push_str(&format!("\\x{:02x}", c as u32));
        }
        c if is_go_printable(c) => out.push(c),
        c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
        c => out.push_str(&format!("\\U{:08x}", c as u32)),
    }
}

/// An approximation of Go's `unicode.IsPrint` for characters outside ASCII.
fn is_go_printable(c: char) -> bool {
    !c.is_control()
        && !matches!(
            c as u32,
            0x00ad | 0x2028 | 0x2029 | 0xfeff | 0xfff9..=0xfffb
        )
        && !(0xe000..=0xf8ff).contains(&(c as u32))
}

#[derive(Default, Clone, Copy)]
struct Spec {
    minus: bool,
    plus: bool,
    sharp: bool,
    zero: bool,
    space: bool,
    width: Option<usize>,
    prec: Option<usize>,
}

/// Go's limit on widths and precisions (`fmt` reports `%!(BADWIDTH)` above it).
const MAX_WIDTH: usize = 1_000_000;

fn pad(s: &str, spec: &Spec, out: &mut String) {
    let w = spec.width.unwrap_or(0);
    let n = s.chars().count();
    if n >= w {
        out.push_str(s);
        return;
    }
    let fill = w - n;
    if spec.minus {
        out.push_str(s);
        out.extend(std::iter::repeat_n(' ', fill));
    } else {
        let c = if spec.zero { '0' } else { ' ' };
        out.extend(std::iter::repeat_n(c, fill));
        out.push_str(s);
    }
}

/// Pads a number, putting zero padding after the sign.
fn pad_number(s: &str, spec: &Spec, out: &mut String) {
    if spec.zero && !spec.minus {
        if let Some(w) = spec.width {
            let n = s.chars().count();
            if n < w {
                let (sign, rest) = match s.chars().next() {
                    Some(c @ ('-' | '+' | ' ')) => (Some(c), &s[1..]),
                    _ => (None, s),
                };
                if let Some(c) = sign {
                    out.push(c);
                }
                out.extend(std::iter::repeat_n('0', w - n));
                out.push_str(rest);
                return;
            }
        }
    }
    let no_zero = Spec {
        zero: false,
        ..*spec
    };
    pad(s, &no_zero, out);
}

fn sign_prefix(neg: bool, spec: &Spec) -> &'static str {
    if neg {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

fn fmt_integer(i: i64, base: u32, verb: char, spec: &Spec, out: &mut String) {
    let neg = i < 0;
    let mag = i.unsigned_abs();
    let mut digits = match base {
        2 => format!("{mag:b}"),
        8 => format!("{mag:o}"),
        16 if verb == 'X' => format!("{mag:X}"),
        16 => format!("{mag:x}"),
        _ => mag.to_string(),
    };
    if let Some(p) = spec.prec {
        if p == 0 && mag == 0 {
            digits.clear();
        }
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
    let prefix = match (base, verb) {
        (8, 'O') => "0o",
        (2, _) if spec.sharp => "0b",
        (8, _) if spec.sharp && !digits.starts_with('0') => "0",
        (16, 'x') if spec.sharp => "0x",
        (16, 'X') if spec.sharp => "0X",
        _ => "",
    };
    let s = format!("{}{prefix}{digits}", sign_prefix(neg, spec));
    if spec.prec.is_some() {
        let no_zero = Spec {
            zero: false,
            ..*spec
        };
        pad(&s, &no_zero, out);
    } else {
        pad_number(&s, spec, out);
    }
}

fn fmt_float(f: f64, verb: char, spec: &Spec, out: &mut String) {
    let body = match verb {
        'v' | 'g' => match spec.prec {
            Some(p) => format_g_prec(f, p, false),
            None => format_g_shortest(f, false),
        },
        'G' => match spec.prec {
            Some(p) => format_g_prec(f, p, true),
            None => format_g_shortest(f, true),
        },
        'e' | 'E' => format_e(f, spec.prec.unwrap_or(6), verb == 'E'),
        _ => format_fixed(f, spec.prec.unwrap_or(6)),
    };
    let s = if body.starts_with('-') || body.starts_with('+') {
        if body == "+Inf" && !spec.plus {
            if spec.space { " Inf".to_owned() } else { body }
        } else {
            body
        }
    } else {
        format!("{}{body}", sign_prefix(false, spec))
    };
    if f.is_finite() {
        pad_number(&s, spec, out);
    } else {
        let no_zero = Spec {
            zero: false,
            ..*spec
        };
        pad(&s, &no_zero, out);
    }
}

fn fmt_string(s: &str, verb: char, spec: &Spec, out: &mut String) {
    let truncated: String = match spec.prec {
        Some(p) => s.chars().take(p).collect(),
        None => s.to_owned(),
    };
    match verb {
        'q' => {
            let q = if spec.sharp && !truncated.contains('`') && !truncated.contains('\n') {
                format!("`{truncated}`")
            } else {
                quote(&truncated)
            };
            pad(&q, spec, out);
        }
        'x' | 'X' => {
            let bytes: Vec<u8> = match spec.prec {
                Some(p) => s.bytes().take(p).collect(),
                None => s.bytes().collect(),
            };
            let mut h = String::new();
            for (i, b) in bytes.iter().enumerate() {
                if spec.space && i > 0 {
                    h.push(' ');
                }
                if spec.sharp && (spec.space || i == 0) {
                    h.push_str(if verb == 'x' { "0x" } else { "0X" });
                }
                if verb == 'x' {
                    h.push_str(&format!("{b:02x}"));
                } else {
                    h.push_str(&format!("{b:02X}"));
                }
            }
            pad(&h, spec, out);
        }
        _ => pad(&truncated, spec, out),
    }
}

fn bad_verb(v: &Value, verb: char, out: &mut String) {
    out.push_str("%!");
    out.push(verb);
    out.push('(');
    match v {
        Value::Nil => out.push_str("<nil>"),
        other => {
            out.push_str(other.go_type());
            out.push('=');
            write_v(other, out);
        }
    }
    out.push(')');
}

fn fmt_value(v: &Value, verb: char, spec: &Spec, out: &mut String) {
    if verb == 'T' {
        pad(
            match v {
                Value::Nil => "<nil>",
                other => other.go_type(),
            },
            spec,
            out,
        );
        return;
    }
    match v {
        Value::Nil => match verb {
            'v' => pad("<nil>", spec, out),
            _ => bad_verb(v, verb, out),
        },
        Value::Bool(b) => match verb {
            'v' | 't' => pad(if *b { "true" } else { "false" }, spec, out),
            _ => bad_verb(v, verb, out),
        },
        Value::Int(i) => match verb {
            'v' | 'd' => fmt_integer(*i, 10, verb, spec, out),
            'b' => fmt_integer(*i, 2, verb, spec, out),
            'o' | 'O' => fmt_integer(*i, 8, verb, spec, out),
            'x' | 'X' => fmt_integer(*i, 16, verb, spec, out),
            'c' => pad(&rune_of(*i).to_string(), spec, out),
            'q' => {
                let mut q = String::from('\'');
                quote_char_into(rune_of(*i), '\'', &mut q);
                q.push('\'');
                pad(&q, spec, out);
            }
            'U' => {
                let mut u = format!("U+{:04X}", *i);
                if spec.sharp {
                    u.push_str(&format!(" '{}'", rune_of(*i)));
                }
                pad(&u, spec, out);
            }
            _ => bad_verb(v, verb, out),
        },
        Value::Float(f) => match verb {
            'v' | 'g' | 'G' | 'e' | 'E' | 'f' | 'F' => fmt_float(*f, verb, spec, out),
            _ => bad_verb(v, verb, out),
        },
        Value::String(s) => match verb {
            'v' | 's' | 'q' | 'x' | 'X' => fmt_string(s, verb, spec, out),
            _ => bad_verb(v, verb, out),
        },
        Value::PathList(l) if matches!(verb, 'v' | 's' | 'q' | 'x' | 'X') => {
            fmt_string(&l.join(" "), verb, spec, out)
        }
        Value::PathList(l) => {
            let list = Value::strings(l.iter().cloned());
            fmt_value(&list, verb, spec, out)
        }
        Value::List(l) => {
            out.push('[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                fmt_value(e, verb, spec, out);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push_str("map[");
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                fmt_string(k, verb_for_key(verb), spec, out);
                out.push(':');
                fmt_value(e, verb, spec, out);
            }
            out.push(']');
        }
    }
}

fn verb_for_key(verb: char) -> char {
    match verb {
        's' | 'q' | 'x' | 'X' => verb,
        _ => 'v',
    }
}

fn rune_of(i: i64) -> char {
    u32::try_from(i)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{FFFD}')
}

/// `fmt.Sprintf`.
pub(crate) fn sprintf(format: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut argi = 0usize;
    let mut reordered = false;
    let mut chars = format.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = Spec::default();
        // Flags.
        while let Some(&(_, f)) = chars.peek() {
            match f {
                '#' => spec.sharp = true,
                '0' => spec.zero = true,
                '+' => spec.plus = true,
                '-' => spec.minus = true,
                ' ' => spec.space = true,
                _ => break,
            }
            chars.next();
        }
        let mut bad_index = false;
        let mut arg_index = |chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
                             argi: &mut usize,
                             reordered: &mut bool| {
            if chars.peek().map(|&(_, c)| c) != Some('[') {
                return;
            }
            chars.next();
            let mut n = String::new();
            let mut closed = false;
            for (_, c) in chars.by_ref() {
                if c == ']' {
                    closed = true;
                    break;
                }
                n.push(c);
            }
            *reordered = true;
            match n.parse::<usize>() {
                Ok(i) if closed && i >= 1 && i <= args.len() => *argi = i - 1,
                _ => bad_index = true,
            }
        };
        arg_index(&mut chars, &mut argi, &mut reordered);
        // Width.
        let mut bad_width = false;
        if chars.peek().map(|&(_, c)| c) == Some('*') {
            chars.next();
            match args.get(argi) {
                Some(Value::Int(w)) => {
                    let w = *w;
                    if w.unsigned_abs() as usize > MAX_WIDTH {
                        bad_width = true;
                    } else {
                        if w < 0 {
                            spec.minus = true;
                        }
                        spec.width = Some(w.unsigned_abs() as usize);
                    }
                }
                _ => bad_width = true,
            }
            argi += 1;
        } else {
            let mut w = String::new();
            while let Some(&(_, d)) = chars.peek() {
                if d.is_ascii_digit() {
                    w.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if !w.is_empty() {
                match w.parse::<usize>() {
                    Ok(n) if n <= MAX_WIDTH => spec.width = Some(n),
                    _ => bad_width = true,
                }
            }
        }
        // Precision.
        let mut bad_prec = false;
        if chars.peek().map(|&(_, c)| c) == Some('.') {
            chars.next();
            arg_index(&mut chars, &mut argi, &mut reordered);
            if chars.peek().map(|&(_, c)| c) == Some('*') {
                chars.next();
                match args.get(argi) {
                    Some(Value::Int(p)) if *p >= 0 && (*p as usize) <= MAX_WIDTH => {
                        spec.prec = Some(*p as usize)
                    }
                    _ => bad_prec = true,
                }
                argi += 1;
            } else {
                let mut p = String::new();
                while let Some(&(_, d)) = chars.peek() {
                    if d.is_ascii_digit() {
                        p.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match p.parse::<usize>() {
                    Ok(n) if n <= MAX_WIDTH => spec.prec = Some(n),
                    Err(_) if p.is_empty() => spec.prec = Some(0),
                    _ => bad_prec = true,
                }
            }
        }
        arg_index(&mut chars, &mut argi, &mut reordered);
        if bad_width {
            out.push_str("%!(BADWIDTH)");
        }
        if bad_prec {
            out.push_str("%!(BADPREC)");
        }
        let Some((_, verb)) = chars.next() else {
            out.push_str("%!(NOVERB)");
            break;
        };
        if verb == '%' {
            out.push('%');
            continue;
        }
        if bad_index {
            out.push_str("%!");
            out.push(verb);
            out.push_str("(BADINDEX)");
            continue;
        }
        let Some(arg) = args.get(argi) else {
            out.push_str("%!");
            out.push(verb);
            out.push_str("(MISSING)");
            continue;
        };
        argi += 1;
        if spec.minus {
            spec.zero = false;
        }
        fmt_value(arg, verb, &spec, &mut out);
    }
    if !reordered && argi < args.len() {
        out.push_str("%!(EXTRA ");
        for (i, a) in args[argi..].iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match a {
                Value::Nil => out.push_str("<nil>"),
                other => {
                    out.push_str(other.go_type());
                    out.push('=');
                    write_v(other, &mut out);
                }
            }
        }
        out.push(')');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_go() {
        for (f, want) in [
            (0.0, "0"),
            (1.0, "1"),
            (3.5, "3.5"),
            (123456.0, "123456"),
            (1234567.0, "1.234567e+06"),
            (1e6, "1e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-10, "-2.5e-10"),
            (1e21, "1e+21"),
            (f64::INFINITY, "+Inf"),
        ] {
            assert_eq!(format_g_shortest(f, false), want, "{f}");
        }
    }

    #[test]
    fn printf_verbs() {
        let s = |f: &str, a: Vec<Value>| sprintf(f, &a);
        assert_eq!(s("%d-%s", vec![Value::Int(3), "x".into()]), "3-x");
        assert_eq!(s("%5.2f|", vec![Value::Float(4.56789)]), " 4.57|");
        assert_eq!(s("%-4d|", vec![Value::Int(7)]), "7   |");
        assert_eq!(s("%04d", vec![Value::Int(-7)]), "-007");
        assert_eq!(
            s(
                "%x %X %#x",
                vec![Value::Int(255), Value::Int(255), Value::Int(255)]
            ),
            "ff FF 0xff"
        );
        assert_eq!(s("%q", vec!["a\"b".into()]), r#""a\"b""#);
        assert_eq!(s("%d", vec!["x".into()]), "%!d(string=x)");
        assert_eq!(s("%d %d", vec![Value::Int(1)]), "1 %!d(MISSING)");
        assert_eq!(
            s("%d", vec![Value::Int(1), "x".into()]),
            "1%!(EXTRA string=x)"
        );
        assert_eq!(s("%v", vec![Value::strings(["a", "b"])]), "[a b]");
        assert_eq!(s("%e", vec![Value::Float(1234.5678)]), "1.234568e+03");
        assert_eq!(s("%.3g", vec![Value::Float(1234.5678)]), "1.23e+03");
        assert_eq!(s("%g", vec![Value::Float(100000.0)]), "100000");
        assert_eq!(s("%[2]d %[1]d", vec![Value::Int(1), Value::Int(2)]), "2 1");
        assert_eq!(s("100%%", vec![]), "100%");
    }
}
