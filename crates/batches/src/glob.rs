//! Repository name globs without separators, used by `changesetTemplate.published` rules and
//! `workspaces[].in`.
//!
//! - `*` (and `**`) match any sequence of characters, `/` included; `?` matches one character.
//! - `[abc]`, `[a-z]` match one character of the class; `[!abc]` one character outside it.
//! - `{a,b*,c}` matches any of the comma-separated patterns.
//! - `\` makes the next character literal.

use regex::Regex;

/// A compiled glob.
#[derive(Debug, Clone)]
pub struct Glob {
    pattern: String,
    regex: Regex,
}

impl PartialEq for Glob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Glob {
    /// Compiles `pattern`. Unclosed `[`/`{`, empty classes and a trailing `\` are errors.
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut out = String::from("(?s)^");
        let mut i = 0;
        let mut braces = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '*' => {
                    while i + 1 < chars.len() && chars[i + 1] == '*' {
                        i += 1;
                    }
                    out.push_str(".*");
                }
                '?' => out.push('.'),
                '\\' => {
                    i += 1;
                    let Some(&next) = chars.get(i) else {
                        return Err(format!("invalid glob {pattern:?}: trailing escape"));
                    };
                    push_literal(&mut out, next);
                }
                '[' => {
                    i += 1;
                    let negated = chars.get(i) == Some(&'!');
                    if negated {
                        i += 1;
                    }
                    let mut class = String::new();
                    let mut closed = false;
                    while i < chars.len() {
                        match chars[i] {
                            ']' => {
                                closed = true;
                                break;
                            }
                            '\\' if i + 1 < chars.len() => {
                                i += 1;
                                push_class_char(&mut class, chars[i]);
                            }
                            '-' if !class.is_empty()
                                && i + 1 < chars.len()
                                && chars[i + 1] != ']' =>
                            {
                                class.push('-');
                            }
                            ch => push_class_char(&mut class, ch),
                        }
                        i += 1;
                    }
                    if !closed {
                        return Err(format!("invalid glob {pattern:?}: unclosed '['"));
                    }
                    if class.is_empty() {
                        return Err(format!("invalid glob {pattern:?}: empty character class"));
                    }
                    out.push('[');
                    if negated {
                        out.push('^');
                    }
                    out.push_str(&class);
                    out.push(']');
                }
                '{' => {
                    braces += 1;
                    out.push_str("(?:");
                }
                ',' if braces > 0 => out.push('|'),
                '}' if braces > 0 => {
                    braces -= 1;
                    out.push(')');
                }
                other => push_literal(&mut out, other),
            }
            i += 1;
        }
        if braces > 0 {
            return Err(format!("invalid glob {pattern:?}: unclosed '{{'"));
        }
        out.push('$');
        let regex = Regex::new(&out).map_err(|e| format!("invalid glob {pattern:?}: {e}"))?;
        Ok(Glob {
            pattern: pattern.to_string(),
            regex,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn is_match(&self, name: &str) -> bool {
        self.regex.is_match(name)
    }
}

fn push_literal(out: &mut String, c: char) {
    let mut buf = [0u8; 4];
    out.push_str(&regex::escape(c.encode_utf8(&mut buf)));
}

fn push_class_char(class: &mut String, c: char) {
    if matches!(c, '[' | ']' | '\\' | '^' | '-' | '&' | '~') {
        class.push('\\');
    }
    class.push(c);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Glob::new(p).unwrap().is_match(s)
    }

    #[test]
    fn matching() {
        assert!(m("*", "gitlab.com/a/b"));
        assert!(m("gitlab.com/a/*", "gitlab.com/a/b/c"));
        assert!(!m("gitlab.com/a/*", "gitlab.com/b/c"));
        assert!(m("gitlab.com/**", "gitlab.com/x"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("[ab]x", "bx"));
        assert!(m("[a-c]x", "cx"));
        assert!(!m("[!a-c]x", "cx"));
        assert!(m("[!a-c]x", "dx"));
        assert!(m("gitlab.com/{foo,bar*}", "gitlab.com/barbaz"));
        assert!(!m("gitlab.com/{foo,bar}", "gitlab.com/baz"));
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
        assert!(m("a.b", "a.b"));
        assert!(!m("a.b", "axb"));
        assert!(m("a,b}", "a,b}"));
    }

    #[test]
    fn errors() {
        for p in ["[abc", "{a,b", "a\\", "[]"] {
            assert!(Glob::new(p).is_err(), "{p}");
        }
    }
}
