//! Glob matching for the `matches` function, without separators: `*` and `**` match any run of
//! characters, `?` one character, `[abc]`, `[a-z]`, `[!…]` character classes, `{a,b}` alternatives
//! (nestable) and `\` escapes.

#[derive(Debug, Clone)]
enum Item {
    Lit(char),
    Any,
    Single,
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
    },
}

#[derive(Debug, Clone)]
enum Part {
    Item(Item),
    Alt(Vec<Vec<Part>>),
}

/// Alternatives are expanded into flat patterns; more than this is rejected.
const MAX_EXPANSIONS: usize = 1024;

pub(crate) struct Glob {
    alternatives: Vec<Vec<Item>>,
}

impl Glob {
    pub(crate) fn compile(pattern: &str) -> Result<Glob, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        let parts = parse_seq(&chars, &mut i, 0)?;
        let alternatives = expand(&parts)?;
        Ok(Glob { alternatives })
    }

    pub(crate) fn matches(&self, input: &str) -> bool {
        let s: Vec<char> = input.chars().collect();
        self.alternatives.iter().any(|p| match_flat(p, &s))
    }
}

fn parse_seq(chars: &[char], i: &mut usize, depth: usize) -> Result<Vec<Part>, String> {
    if depth > 32 {
        return Err("glob alternatives nested too deeply".to_owned());
    }
    let mut parts = Vec::new();
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            ',' | '}' if depth > 0 => return Ok(parts),
            '*' => {
                *i += 1;
                while *i < chars.len() && chars[*i] == '*' {
                    *i += 1;
                }
                parts.push(Part::Item(Item::Any));
            }
            '?' => {
                *i += 1;
                parts.push(Part::Item(Item::Single));
            }
            '\\' => {
                *i += 1;
                let Some(&e) = chars.get(*i) else {
                    return Err("unclosed escape sequence".to_owned());
                };
                *i += 1;
                parts.push(Part::Item(Item::Lit(e)));
            }
            '[' => {
                *i += 1;
                parts.push(Part::Item(parse_class(chars, i)?));
            }
            '{' => {
                *i += 1;
                let mut alts = Vec::new();
                loop {
                    alts.push(parse_seq(chars, i, depth + 1)?);
                    match chars.get(*i) {
                        Some(',') => *i += 1,
                        Some('}') => {
                            *i += 1;
                            break;
                        }
                        _ => return Err("unexpected end of input: unclosed {".to_owned()),
                    }
                }
                parts.push(Part::Alt(alts));
            }
            c => {
                *i += 1;
                parts.push(Part::Item(Item::Lit(c)));
            }
        }
    }
    Ok(parts)
}

fn parse_class(chars: &[char], i: &mut usize) -> Result<Item, String> {
    let unclosed = || "unexpected end of input: unclosed [".to_owned();
    let mut neg = false;
    if chars.get(*i) == Some(&'!') {
        neg = true;
        *i += 1;
    }
    let mut ranges = Vec::new();
    loop {
        let Some(&c) = chars.get(*i) else {
            return Err(unclosed());
        };
        *i += 1;
        match c {
            ']' => break,
            '\\' => {
                let Some(&e) = chars.get(*i) else {
                    return Err(unclosed());
                };
                *i += 1;
                ranges.push((e, e));
            }
            c if chars.get(*i) == Some(&'-') && chars.get(*i + 1).is_some_and(|&n| n != ']') => {
                let hi = chars[*i + 1];
                *i += 2;
                if hi < c {
                    return Err(format!("invalid range {c}-{hi}"));
                }
                ranges.push((c, hi));
            }
            c => ranges.push((c, c)),
        }
    }
    if ranges.is_empty() {
        return Err("empty character class".to_owned());
    }
    Ok(Item::Class { neg, ranges })
}

fn expand(parts: &[Part]) -> Result<Vec<Vec<Item>>, String> {
    let mut out: Vec<Vec<Item>> = vec![Vec::new()];
    for p in parts {
        match p {
            Part::Item(it) => {
                for o in &mut out {
                    o.push(it.clone());
                }
            }
            Part::Alt(alts) => {
                let mut options = Vec::new();
                for a in alts {
                    options.extend(expand(a)?);
                }
                if out.len().saturating_mul(options.len()) > MAX_EXPANSIONS {
                    return Err("glob pattern has too many alternatives".to_owned());
                }
                let mut next = Vec::with_capacity(out.len() * options.len());
                for o in &out {
                    for opt in &options {
                        let mut v = o.clone();
                        v.extend(opt.iter().cloned());
                        next.push(v);
                    }
                }
                out = next;
            }
        }
    }
    Ok(out)
}

fn item_matches(it: &Item, c: char) -> bool {
    match it {
        Item::Lit(l) => *l == c,
        Item::Single | Item::Any => true,
        Item::Class { neg, ranges } => ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *neg,
    }
}

/// Wildcard matching with single-star backtracking (O(pattern × input)).
fn match_flat(p: &[Item], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            if let Item::Any = p[pi] {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            if item_matches(&p[pi], s[si]) {
                pi += 1;
                si += 1;
                continue;
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|it| matches!(it, Item::Any))
}

#[cfg(test)]
mod tests {
    use super::Glob;

    fn m(p: &str, s: &str) -> bool {
        Glob::compile(p).unwrap().matches(s)
    }

    #[test]
    fn globs() {
        assert!(m(
            "gitlab.example.com/acme/*",
            "gitlab.example.com/acme/tools-cli"
        ));
        assert!(m(
            "gitlab.example.com*",
            "gitlab.example.com/acme/tools-cli"
        ));
        assert!(!m("github.com*", "gitlab.example.com/acme/tools-cli"));
        assert!(m("*/tools-?li", "gitlab.example.com/acme/tools-cli"));
        assert!(m("a[bc]d", "acd"));
        assert!(!m("a[!bc]d", "acd"));
        assert!(m("a[a-z]d", "axd"));
        assert!(m("{foo,ba{r,z}}.go", "baz.go"));
        assert!(!m("{foo,bar}.go", "baz.go"));
        assert!(m("\\*x", "*x"));
        assert!(m("", ""));
        assert!(m("**", "a/b/c"));
        assert!(Glob::compile("[abc").is_err());
        assert!(Glob::compile("{a,b").is_err());
    }
}
