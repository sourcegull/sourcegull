//! Output and replacement templates.
//!
//! A template is text with variables: `$name` or `${name}`. Built-in
//! variables describe the result ([`MetaEnv`]: `$repo`, `$path`, `$commit`,
//! `$author`, `$email`, `$date` with `.year`, `.month`, `.month.name` and
//! `.day`, `$lang`, `$content`); any other name is a regexp capture group
//! (`$1`, `$name`) or a structural hole. Structural holes can also be
//! written as `:[name]` (and `:[[name]]`, `:[name.]`, `:[name\n]`).
//! Unknown variables expand to nothing. `\n`, `\r`, `\t` are control
//! characters; `\\`, `\$`, `\ ` and `\.` are the character itself.
//!
//! Variables are expanded in one pass, so text a variable expands to is
//! never expanded again.

use chrono::{DateTime, Datelike, Utc};

/// One part of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Atom {
    /// Literal text (escapes already processed). It may hold `:[hole]`
    /// references, expanded for structural matches.
    Constant(String),
    /// A variable, without the `$`, e.g. `1`, `repo`, `date.year`.
    Variable(String),
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Template {
    pub atoms: Vec<Atom>,
}

fn var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Attributes `$date` may carry, longest first.
const DATE_ATTRS: &[&str] = &[".month.name", ".month", ".year", ".day"];

impl Template {
    /// Scans `text`. Scanning never fails: anything that is not a variable is
    /// literal text.
    pub fn parse(text: &str) -> Template {
        let mut atoms = Vec::new();
        let mut constant = String::new();
        let mut chars = text.char_indices().peekable();
        let flush = |constant: &mut String, atoms: &mut Vec<Atom>| {
            if !constant.is_empty() {
                atoms.push(Atom::Constant(std::mem::take(constant)));
            }
        };
        while let Some((i, c)) = chars.next() {
            match c {
                '$' => {
                    let rest = &text[i + 1..];
                    // `${name}`.
                    if let Some(body) = rest.strip_prefix('{')
                        && let Some(end) = body.find('}')
                        && end > 0
                        && body[..end].chars().all(|c| var_char(c) || c == '.')
                    {
                        flush(&mut constant, &mut atoms);
                        atoms.push(Atom::Variable(body[..end].to_owned()));
                        let skip = 1 + end + 1;
                        while chars.peek().is_some_and(|&(j, _)| j <= i + skip) {
                            chars.next();
                        }
                        continue;
                    }
                    let len = rest.find(|c: char| !var_char(c)).unwrap_or(rest.len());
                    if len == 0 {
                        // A lone or trailing `$` is literal.
                        constant.push('$');
                        continue;
                    }
                    let mut name = rest[..len].to_owned();
                    if name == "date"
                        && let Some(attr) = DATE_ATTRS.iter().find(|a| {
                            rest[len..].starts_with(*a)
                                && !rest[len + a.len()..].starts_with(var_char)
                        })
                    {
                        name.push_str(attr);
                    }
                    flush(&mut constant, &mut atoms);
                    let end = i + 1 + name.len();
                    atoms.push(Atom::Variable(name));
                    while chars.peek().is_some_and(|&(j, _)| j < end) {
                        chars.next();
                    }
                }
                '\\' => match chars.next().map(|(_, e)| e) {
                    Some('n') => constant.push('\n'),
                    Some('r') => constant.push('\r'),
                    Some('t') => constant.push('\t'),
                    Some(e @ ('\\' | '$' | ' ' | '.')) => constant.push(e),
                    // Any other escape is kept as written.
                    Some(e) => {
                        constant.push('\\');
                        constant.push(e);
                    }
                    None => constant.push('\\'),
                },
                c => constant.push(c),
            }
        }
        flush(&mut constant, &mut atoms);
        Template { atoms }
    }

    /// The variables the template uses.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.atoms.iter().filter_map(|a| match a {
            Atom::Variable(v) => Some(v.as_str()),
            Atom::Constant(_) => None,
        })
    }

    /// Expands the template: built-in variables from `meta`, other variables
    /// and `:[hole]` references from `lookup` (capture groups or holes).
    pub fn render<'a>(&self, meta: &MetaEnv, lookup: &dyn Fn(&str) -> Option<&'a str>) -> String {
        let mut out = String::new();
        for atom in &self.atoms {
            match atom {
                Atom::Constant(c) => substitute_holes(c, lookup, &mut out),
                Atom::Variable(name) => match meta.get(name) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(lookup(name).unwrap_or("")),
                },
            }
        }
        out
    }
}

/// Appends `text` to `out`, replacing structural hole references
/// (`:[x]`, `:[[x]]`, `:[x.]`, `:[x\n]`, `:[x~re]`) found by `lookup`.
/// References to unknown holes stay as written.
fn substitute_holes<'a>(text: &str, lookup: &dyn Fn(&str) -> Option<&'a str>, out: &mut String) {
    let mut rest = text;
    while let Some(at) = rest.find(":[") {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        match hole_ref(after) {
            Some((name, len)) if lookup(name).is_some() => {
                out.push_str(lookup(name).unwrap_or_default());
                rest = &after[len..];
            }
            _ => {
                out.push_str(":[");
                rest = &after[2..];
            }
        }
    }
    out.push_str(rest);
}

/// The hole name and length of the reference at the start of `s` (`:[`…).
fn hole_ref(s: &str) -> Option<(&str, usize)> {
    if let Some(body) = s.strip_prefix(":[[") {
        let end = body.find("]]")?;
        let name = &body[..end];
        return name.chars().all(var_char).then_some((name, 3 + end + 2));
    }
    let body = s.strip_prefix(":[")?;
    let end = body.find(']')?;
    let inner = &body[..end];
    let name = inner.split('~').next().unwrap_or_default();
    let name = name.trim_start_matches(' ');
    let name = name
        .strip_suffix('.')
        .or_else(|| name.strip_suffix("\\n"))
        .or_else(|| name.strip_suffix('\n'))
        .unwrap_or(name);
    (!name.is_empty() && name.chars().all(var_char)).then_some((name, 2 + end + 1))
}

/// What the built-in variables expand to for one result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaEnv {
    pub repo: String,
    pub path: String,
    pub commit: String,
    pub author: String,
    pub email: String,
    /// Committer date of a commit result.
    pub date: Option<DateTime<Utc>>,
    /// Language name, e.g. `Go`.
    pub lang: String,
    /// The text the command ran over (a matched chunk, a file, a diff).
    pub content: String,
}

/// The built-in variable names.
pub const BUILTINS: &[&str] = &[
    "repo",
    "path",
    "content",
    "commit",
    "author",
    "email",
    "date",
    "date.year",
    "date.month",
    "date.month.name",
    "date.day",
    "lang",
];

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl MetaEnv {
    /// The value of built-in variable `name`; `None` for other names.
    pub fn get(&self, name: &str) -> Option<String> {
        let date =
            |f: &dyn Fn(&DateTime<Utc>) -> String| self.date.as_ref().map(f).unwrap_or_default();
        Some(match name {
            "repo" => self.repo.clone(),
            "path" => self.path.clone(),
            "content" => self.content.clone(),
            "commit" => self.commit.clone(),
            "author" => self.author.clone(),
            "email" => self.email.clone(),
            "lang" => self.lang.clone(),
            "date" => date(&|d| d.format("%Y-%m-%d").to_string()),
            "date.year" => date(&|d| d.year().to_string()),
            "date.month" => date(&|d| format!("{:02}", d.month())),
            "date.month.name" => date(&|d| MONTHS[d.month0() as usize].to_owned()),
            "date.day" => date(&|d| d.day().to_string()),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Atom {
        Atom::Constant(s.to_owned())
    }
    fn v(s: &str) -> Atom {
        Atom::Variable(s.to_owned())
    }

    #[test]
    fn scans_templates() {
        let t = |s: &str| Template::parse(s).atoms;
        // Other escapes.
        assert_eq!(t("$foo\\n"), [v("foo"), c("\n")]);
        assert_eq!(t("a\\qb\\"), [c("a\\qb\\")]);
        assert_eq!(t("$1.$2"), [v("1"), c("."), v("2")]);
    }

    #[test]
    fn backslash_escapes() {
        let t = |s: &str| Template::parse(s).atoms;
        assert_eq!(t(r"tab\there"), [c("tab\there")]);
        assert_eq!(t(r"$x\.$y"), [v("x"), c("."), v("y")]);
        assert_eq!(t(r"cost: \$5"), [c("cost: $5")]);
        assert_eq!(t(r"a\\b"), [c(r"a\b")]);
        assert_eq!(t(r"line\r\n"), [c("line\r\n")]);
        assert_eq!(t(r"path\/x"), [c(r"path\/x")]);
        assert_eq!(t(r"end\"), [c(r"end\")]);
        assert_eq!(t(r"$name\ !"), [v("name"), c(" !")]);
        assert_eq!(t(r"\$$v"), [c("$"), v("v")]);
        assert_eq!(t(r"$date\.year"), [v("date"), c(".year")]);
    }

    #[test]
    fn dollar_signs() {
        let t = |s: &str| Template::parse(s).atoms;
        assert_eq!(t("$$a b$"), [c("$"), v("a"), c(" b$")]);
        assert_eq!(t("($id)"), [c("("), v("id"), c(")")]);
        assert_eq!(t("$p:$q"), [v("p"), c(":"), v("q")]);
        assert_eq!(t("$k$"), [v("k"), c("$")]);
        assert_eq!(t("$"), [c("$")]);
        assert_eq!(t("${}x"), [c("${}x")]);
    }

    #[test]
    fn braces_and_date_attributes() {
        assert_eq!(Template::parse("${1}x").atoms, [v("1"), c("x")]);
        assert_eq!(
            Template::parse("$date.year-$date.month.name.").atoms,
            [v("date.year"), c("-"), v("date.month.name"), c(".")]
        );
        // `$date.yearly` is `$date` then text.
        assert_eq!(
            Template::parse("$date.yearly").atoms,
            [v("date"), c(".yearly")]
        );
    }

    #[test]
    fn renders_builtins_captures_and_holes() {
        let meta = MetaEnv {
            repo: "r".into(),
            path: "a/b.go".into(),
            lang: "Go".into(),
            date: Some("2024-03-05T10:00:00Z".parse().unwrap()),
            ..MetaEnv::default()
        };
        let lookup = |n: &str| match n {
            "1" => Some("one"),
            "x" => Some("hole"),
            _ => None,
        };
        let t = Template::parse(
            "$repo:$path ($lang) $1 $9 :[x] :[[x]] :[y] $date $date.month.name/$date.day",
        );
        assert_eq!(
            t.render(&meta, &lookup),
            "r:a/b.go (Go) one  hole hole :[y] 2024-03-05 March/5"
        );
    }
}
