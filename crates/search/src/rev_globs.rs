//! Ref globs in revision specifiers: `*refs/heads/*` selects refs by name and
//! `*!refs/heads/old*` leaves refs out.

/// Whether `rev` is a ref glob, which selects refs by name.
pub fn is_rev_glob(rev: &str) -> bool {
    rev.starts_with('*')
}

/// Parsed revision specifiers. Parsing compiles the globs, so a search over many repositories
/// parses once and matches each repository's refs against the result.
pub struct RevSpec {
    plain: Vec<String>,
    include: Vec<regex::Regex>,
    exclude: Vec<regex::Regex>,
}

impl RevSpec {
    /// Parses specifiers separated by `:`. Exclude globs alone apply to all branches.
    pub fn parse(spec: &str) -> Self {
        let mut out = Self {
            plain: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
        };
        for s in spec.split(':').filter(|s| !s.is_empty()) {
            match s.strip_prefix('*') {
                Some(glob) => match glob.strip_prefix('!') {
                    Some(glob) => out.exclude.push(glob_regex(glob)),
                    None => out.include.push(glob_regex(glob)),
                },
                None => {
                    if !out.plain.iter().any(|r| r == s) {
                        out.plain.push(s.to_owned());
                    }
                }
            }
        }
        if out.include.is_empty() && !out.exclude.is_empty() {
            out.include.push(glob_regex("refs/heads/*"));
        }
        out
    }

    /// Whether the globs select the full ref name `name`.
    pub fn selects(&self, name: &str) -> bool {
        self.include.iter().any(|re| re.is_match(name))
            && !self.exclude.iter().any(|re| re.is_match(name))
    }

    /// The revisions, in order and without duplicates: plain revisions as they are, then the
    /// refs (full names, from `refs`) the globs select.
    pub fn expand(&self, refs: &[String]) -> Vec<String> {
        let mut out = self.plain.clone();
        for r in refs {
            if self.selects(r) && !out.contains(r) {
                out.push(r.clone());
            }
        }
        out
    }
}

/// The revisions of `spec`: [`RevSpec::parse`] then [`RevSpec::expand`].
pub fn expand(spec: &str, refs: &[String]) -> Vec<String> {
    RevSpec::parse(spec).expand(refs)
}

/// A ref glob as an anchored regex: `*` matches any run of characters.
fn glob_regex(glob: &str) -> regex::Regex {
    let pattern = glob
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    regex::Regex::new(&format!("^{pattern}$")).expect("escaped glob is a valid regex")
}
