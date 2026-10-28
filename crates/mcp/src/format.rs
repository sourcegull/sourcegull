//! Compact text output with a size budget.

use std::fmt::Write as _;

/// Characters a tool answer holds at most; past it the output is cut with a note, so one call
/// cannot flood an agent's context.
pub const MAX_OUTPUT_CHARS: usize = 60_000;
/// Characters of one source line shown at most.
pub const MAX_LINE_CHARS: usize = 400;

/// Text being built, cut at [`MAX_OUTPUT_CHARS`].
#[derive(Debug, Default)]
pub struct Out {
    text: String,
    /// Set once something was dropped for the budget.
    pub cut: bool,
}

impl Out {
    /// Appends a line; returns false (and drops it) once the budget is used up.
    pub fn line(&mut self, s: impl AsRef<str>) -> bool {
        let s = s.as_ref();
        if self.cut || self.text.len() + s.len() + 1 > MAX_OUTPUT_CHARS {
            self.cut = true;
            return false;
        }
        self.text.push_str(s);
        self.text.push('\n');
        true
    }

    /// Appends a line past the budget and clears the cut flag: for the note that says how to
    /// get the rest, which must not be dropped.
    pub fn note(&mut self, s: impl AsRef<str>) {
        self.cut = false;
        self.text.push_str(s.as_ref());
        self.text.push('\n');
    }

    /// The text, with `hint` appended when output was cut.
    pub fn finish(mut self, hint: &str) -> String {
        if self.cut {
            let _ = write!(
                self.text,
                "… output cut at {MAX_OUTPUT_CHARS} characters; {hint}"
            );
        }
        while self.text.ends_with('\n') {
            self.text.pop();
        }
        self.text
    }
}

/// `s` cut to [`MAX_LINE_CHARS`] characters (on a char boundary), without its line ending.
pub fn clip(s: &str) -> String {
    let s = s.trim_end_matches(['\n', '\r']);
    match s.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// The first `n` characters of an object id.
pub fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

/// `YYYY-MM-DD` of a git signature time.
pub fn date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// A path filter: a glob (`*`, `**`, `?`) matched against the whole path, or a
/// case-insensitive substring when it has no glob characters.
#[derive(Debug)]
pub enum PathFilter {
    Glob(regex::Regex),
    Substring(String),
}

impl PathFilter {
    pub fn new(pattern: &str) -> Result<Self, String> {
        if !pattern.contains(['*', '?']) {
            return Ok(Self::Substring(pattern.to_lowercase()));
        }
        let mut re = String::from("^");
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    // `**/` also matches no directory at all.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                c => re.push_str(&regex::escape(&c.to_string())),
            }
        }
        re.push('$');
        // A glob without a slash matches the base name anywhere (as .gitignore does).
        if !pattern.contains('/') {
            re = format!("(?:^|/){}", &re[1..]);
        }
        regex::Regex::new(&re)
            .map(Self::Glob)
            .map_err(|e| format!("invalid pattern: {e}"))
    }

    pub fn matches(&self, path: &str) -> bool {
        match self {
            Self::Glob(re) => re.is_match(path),
            Self::Substring(s) => path.to_lowercase().contains(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let f = PathFilter::new("*.go").unwrap();
        assert!(f.matches("a.go"));
        assert!(f.matches("src/x/a.go"));
        assert!(!f.matches("a.gox"));
        let f = PathFilter::new("src/**/*.rs").unwrap();
        assert!(f.matches("src/a.rs"));
        assert!(f.matches("src/x/y/a.rs"));
        assert!(!f.matches("lib/src/a.rs"));
        let f = PathFilter::new("src/*.rs").unwrap();
        assert!(!f.matches("src/x/a.rs"));
        let f = PathFilter::new("Readme").unwrap();
        assert!(f.matches("docs/README.md"));
    }

    #[test]
    fn clips_and_budgets() {
        let long = "é".repeat(MAX_LINE_CHARS + 5);
        assert_eq!(clip(&long).chars().count(), MAX_LINE_CHARS + 1);
        assert_eq!(clip("a\r\n"), "a");
        let mut out = Out::default();
        let line = "x".repeat(1000);
        while out.line(&line) {}
        assert!(out.cut);
        let text = out.finish("narrow it");
        assert!(text.len() < MAX_OUTPUT_CHARS + 200);
        assert!(text.ends_with("narrow it"));
    }
}
