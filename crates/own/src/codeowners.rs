//! GitLab CODEOWNERS files: the parser and the compiled matcher.
//!
//! The syntax and matching rules are GitLab's ("Code Owners syntax" in the GitLab docs):
//!
//! - `pattern owner…` entries; owners are `@username`, `@group/subgroup`, `@@role` and email
//!   addresses. `#` starts a comment line, `\#` a pattern beginning with `#`, `\ ` is a space
//!   inside a pattern.
//! - Sections: `[Name]`, optional `^[Name]`, approval counts `[Name][2]`, and default owners
//!   after the header (`[Name] @owner`) for the section's entries that list none. Entries
//!   before the first header are in the default `codeowners` section. Sections with the same
//!   name (compared case-insensitively) are combined.
//! - Within a section the last matching entry wins; the owners of every section that matches
//!   are combined. `!pattern` excludes matching paths from the section.
//! - Patterns follow GitLab's normalization of `File.fnmatch` globs (`FNM_PATHNAME |
//!   FNM_DOTMATCH`): `*` alone matches everything, a pattern without a leading `/` matches at
//!   any depth (`/**/` is prepended), a trailing `/` matches everything below the directory
//!   (`**/*` is appended), `*` and `?` never cross `/`, and `**` is recursive only as a
//!   whole path component followed by `/`.
//!
//! [`CompiledCodeowners`] compiles each section into one `globset` matcher; compiled files are
//! cached by the CODEOWNERS blob OID ([`CodeownersCache`]), so a file is parsed once per
//! version however many paths are matched against it.

use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Where GitLab looks for a CODEOWNERS file, in its order: the first one found is used.
pub const GITLAB_CODEOWNERS_PATHS: [&str; 3] =
    ["CODEOWNERS", "docs/CODEOWNERS", ".gitlab/CODEOWNERS"];

/// Where GitHub looks for a CODEOWNERS file, in its order.
pub const GITHUB_CODEOWNERS_PATHS: [&str; 3] =
    [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

/// Where Gitea looks for a CODEOWNERS file, in its order.
pub const GITEA_CODEOWNERS_PATHS: [&str; 3] =
    ["CODEOWNERS", "docs/CODEOWNERS", ".gitea/CODEOWNERS"];

/// Where the code host of a repository looks for its CODEOWNERS file, in the host's order
/// (the first one found is used). `service_type` is the lower-case service type stored on
/// repositories (`gitlab`, `github`, `gitea`); any other value gets GitLab's locations.
pub fn codeowners_paths(service_type: &str) -> &'static [&'static str] {
    match service_type {
        "github" => &GITHUB_CODEOWNERS_PATHS,
        "gitea" => &GITEA_CODEOWNERS_PATHS,
        _ => &GITLAB_CODEOWNERS_PATHS,
    }
}

/// The name of the section holding the entries before the first section header.
pub const DEFAULT_SECTION: &str = "codeowners";

/// Larger CODEOWNERS files are ignored (GitLab's limit is 10 MB as well).
pub const MAX_CODEOWNERS_BYTES: usize = 10 * 1024 * 1024;

/// An owner reference as written in a CODEOWNERS file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OwnerRef {
    /// `@name` or `@group/subgroup`, without the `@`.
    Handle(String),
    /// An email address.
    Email(String),
    /// `@@developer`, `@@maintainer`, `@@owner`: a role, without the `@@`.
    Role(String),
}

impl OwnerRef {
    /// Parses one owner token; `None` for tokens that are not owners.
    pub fn parse(token: &str) -> Option<OwnerRef> {
        if let Some(role) = token.strip_prefix("@@") {
            return valid_name(role).then(|| OwnerRef::Role(role.to_owned()));
        }
        if let Some(handle) = token.strip_prefix('@') {
            return valid_handle(handle).then(|| OwnerRef::Handle(handle.to_owned()));
        }
        let (local, domain) = token.split_once('@')?;
        let ok = !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !token.contains(char::is_whitespace)
            && !domain.contains('@');
        ok.then(|| OwnerRef::Email(token.to_owned()))
    }

    /// The text as written (`@alice`, `alice@example.com`, `@@maintainer`).
    pub fn text(&self) -> String {
        match self {
            OwnerRef::Handle(h) => format!("@{h}"),
            OwnerRef::Email(e) => e.clone(),
            OwnerRef::Role(r) => format!("@@{r}"),
        }
    }

    /// The comparison key ([`owner_key`] of [`Self::text`]).
    pub fn key(&self) -> String {
        owner_key(&self.text())
    }
}

/// The key owners are compared by: lower case, one leading `@` removed (`@Alice` and `alice`
/// are the same owner; emails keep their `@`; roles keep one `@`, so they never equal a name).
pub fn owner_key(text: &str) -> String {
    let t = text.trim();
    t.strip_prefix('@').unwrap_or(t).to_lowercase()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

fn valid_handle(s: &str) -> bool {
    !s.is_empty() && !s.ends_with('/') && s.split('/').all(valid_name)
}

/// One `pattern owners…` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The pattern as written, unescaped (`\ ` → space, leading `\#` → `#`), without a `!`.
    pub pattern: String,
    /// The entry's owners; empty means the section's default owners.
    pub owners: Vec<OwnerRef>,
    /// The 1-based line in the file.
    pub line: u32,
    /// `!pattern`: matching paths have no owners in this section.
    pub exclusion: bool,
}

/// A section of a CODEOWNERS file (several headers with one name are combined).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The name as first written; [`DEFAULT_SECTION`] for entries before any header.
    pub name: String,
    /// `^[Name]`: approval by these owners is optional.
    pub optional: bool,
    /// `[Name][N]`: approvals required.
    pub approvals: Option<u32>,
    /// Owners of entries in this section that list none.
    pub default_owners: Vec<OwnerRef>,
    pub rules: Vec<Rule>,
}

impl Section {
    fn new(name: &str) -> Self {
        Section {
            name: name.to_owned(),
            optional: false,
            approvals: None,
            default_owners: Vec::new(),
            rules: Vec::new(),
        }
    }

    /// Whether this is the default section (entries before any header).
    pub fn is_default(&self) -> bool {
        self.name.eq_ignore_ascii_case(DEFAULT_SECTION)
    }
}

/// A problem found while parsing or compiling; the line is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: u32,
    pub message: String,
}

/// A parsed CODEOWNERS file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Codeowners {
    /// The default section first (when it has entries), then the others in file order.
    pub sections: Vec<Section>,
    pub errors: Vec<ParseError>,
}

/// Splits a pattern token off `line` at the first unescaped whitespace; returns the
/// unescaped pattern and the rest.
fn split_pattern(line: &str) -> (String, &str) {
    let mut out = String::new();
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&(_, n)) if n.is_whitespace() => {
                    out.push(' ');
                    chars.next();
                    continue;
                }
                Some(&(_, '#')) if i == 0 => {
                    out.push('#');
                    chars.next();
                    continue;
                }
                _ => {}
            }
        } else if c.is_whitespace() {
            return (out, &line[i..]);
        }
        out.push(c);
    }
    (out, "")
}

/// Parses owner tokens until a `#` comment; returns the owners and the invalid tokens.
fn parse_owners(rest: &str) -> (Vec<OwnerRef>, Vec<String>) {
    let mut owners = Vec::new();
    let mut invalid = Vec::new();
    for token in rest.split_whitespace() {
        if token.starts_with('#') {
            break;
        }
        match OwnerRef::parse(token) {
            Some(o) if !owners.contains(&o) => owners.push(o),
            Some(_) => {}
            None => invalid.push(token.to_owned()),
        }
    }
    (owners, invalid)
}

/// A section header: `^?[name]([n])?( owners)?`, or `None` when the line is an entry.
fn parse_header(line: &str) -> Option<(bool, String, Option<u32>, &str)> {
    let (optional, rest) = match line.strip_prefix('^') {
        Some(r) => (true, r),
        None => (false, line),
    };
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    let name = rest[..close].trim();
    let mut rest = &rest[close + 1..];
    let mut approvals = None;
    if let Some(r) = rest.strip_prefix('[') {
        let close = r.find(']')?;
        approvals = Some(r[..close].trim().parse::<u32>().ok()?);
        rest = &r[close + 1..];
    }
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    if name.is_empty() {
        return None;
    }
    Some((optional, name.to_owned(), approvals, rest))
}

/// Parses a CODEOWNERS file. Malformed lines are skipped and listed in
/// [`Codeowners::errors`].
pub fn parse(text: &str) -> Codeowners {
    let mut sections = vec![Section::new(DEFAULT_SECTION)];
    let mut current = 0usize;
    let mut errors = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = u32::try_from(n + 1).unwrap_or(u32::MAX);
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((optional, name, approvals, rest)) = parse_header(line) {
            let (owners, invalid) = parse_owners(rest);
            for t in invalid {
                errors.push(ParseError {
                    line: line_no,
                    message: format!("invalid owner {t:?}"),
                });
            }
            current = match sections
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(&name))
            {
                Some(i) => i,
                None => {
                    sections.push(Section::new(&name));
                    sections.len() - 1
                }
            };
            let s = &mut sections[current];
            s.optional |= optional;
            if approvals.is_some() {
                s.approvals = approvals;
            }
            for o in owners {
                if !s.default_owners.contains(&o) {
                    s.default_owners.push(o);
                }
            }
            continue;
        }
        let (exclusion, line) = match line.strip_prefix('!') {
            Some(l) => (true, l),
            None => (false, line),
        };
        let (pattern, rest) = split_pattern(line);
        if pattern.is_empty() {
            errors.push(ParseError {
                line: line_no,
                message: "missing pattern".to_owned(),
            });
            continue;
        }
        let (owners, invalid) = parse_owners(rest);
        for t in invalid {
            errors.push(ParseError {
                line: line_no,
                message: format!("invalid owner {t:?}"),
            });
        }
        sections[current].rules.push(Rule {
            pattern,
            owners,
            line: line_no,
            exclusion,
        });
    }
    // An empty default section carries nothing.
    if sections[0].rules.is_empty() && sections[0].default_owners.is_empty() {
        sections.remove(0);
    }
    Codeowners { sections, errors }
}

/// The glob GitLab matches a pattern as (see the module docs), for paths with a leading `/`.
pub fn glob_for(pattern: &str) -> String {
    if pattern == "*" {
        return "/**/*".to_owned();
    }
    let mut p = pattern.to_owned();
    if !p.starts_with('/') {
        p = format!("/**/{p}");
    }
    if p.ends_with('/') {
        p.push_str("**/*");
    }
    // `**` is recursive only as a whole component followed by `/`; elsewhere it is `*`.
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            out.push_str(&p[i..i + 2]);
            i += 2;
            continue;
        }
        if b[i] == b'*' && b.get(i + 1) == Some(&b'*') {
            let mut j = i;
            while j < b.len() && b[j] == b'*' {
                j += 1;
            }
            let whole = (i == 0 || b[i - 1] == b'/') && b.get(j) == Some(&b'/');
            out.push_str(if whole { "**" } else { "*" });
            i = j;
            continue;
        }
        let c = p[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A matcher for one section: one glob set over its entries and one over its exclusions.
#[derive(Debug, Clone)]
struct CompiledSection {
    set: GlobSet,
    /// Glob index → rule index.
    rules: Vec<usize>,
    exclusions: GlobSet,
}

/// A CODEOWNERS file compiled for matching paths.
#[derive(Debug, Clone)]
pub struct CompiledCodeowners {
    file: Codeowners,
    sections: Vec<CompiledSection>,
}

/// An entry that gives a path owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleMatch<'a> {
    pub section: &'a Section,
    pub rule: &'a Rule,
    /// The entry's owners, or the section's default owners when it lists none.
    pub owners: &'a [OwnerRef],
}

impl CompiledCodeowners {
    /// Parses and compiles `text`. Patterns that are not valid globs are skipped and listed
    /// in [`Codeowners::errors`].
    pub fn compile(text: &str) -> Self {
        Self::from_parsed(parse(text))
    }

    /// Compiles a parsed file.
    pub fn from_parsed(mut file: Codeowners) -> Self {
        let mut sections = Vec::with_capacity(file.sections.len());
        let mut errors = Vec::new();
        for s in &file.sections {
            let mut set = GlobSetBuilder::new();
            let mut excl = GlobSetBuilder::new();
            let mut rules = Vec::new();
            for (i, r) in s.rules.iter().enumerate() {
                let glob = GlobBuilder::new(&glob_for(&r.pattern))
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build();
                match glob {
                    Ok(g) if r.exclusion => {
                        excl.add(g);
                    }
                    Ok(g) => {
                        set.add(g);
                        rules.push(i);
                    }
                    Err(e) => errors.push(ParseError {
                        line: r.line,
                        message: format!("invalid pattern {:?}: {e}", r.pattern),
                    }),
                }
            }
            sections.push(CompiledSection {
                set: set.build().unwrap_or_else(|_| GlobSet::empty()),
                rules,
                exclusions: excl.build().unwrap_or_else(|_| GlobSet::empty()),
            });
        }
        file.errors.extend(errors);
        CompiledCodeowners { file, sections }
    }

    /// The parsed file.
    pub fn file(&self) -> &Codeowners {
        &self.file
    }

    /// The winning entry of each section that matches `path` (a repository-relative file
    /// path without a leading `/`), in section order. Entries without owners in a section
    /// without default owners give no owners and are left out.
    pub fn matches(&self, path: &str) -> Vec<RuleMatch<'_>> {
        let rooted = format!("/{}", path.trim_start_matches('/'));
        self.matches_rooted(&[rooted.as_str()])
    }

    /// The entries that make the directory `dir` (`""` for the root) owned: those matching
    /// the directory itself or any file directly in it. A directory pattern (`/docs/`)
    /// therefore owns `docs`, while `/docs/*.md` does not.
    pub fn matches_dir(&self, dir: &str) -> Vec<RuleMatch<'_>> {
        let dir = dir.trim_matches('/');
        // A file name no pattern names literally: matched only by wildcards.
        const ANY_FILE: &str = "\u{1}";
        if dir.is_empty() {
            return self.matches_rooted(&[&format!("/{ANY_FILE}")]);
        }
        self.matches_rooted(&[&format!("/{dir}"), &format!("/{dir}/{ANY_FILE}")])
    }

    fn matches_rooted(&self, candidates: &[&str]) -> Vec<RuleMatch<'_>> {
        let mut out = Vec::new();
        let mut hits = Vec::new();
        for (s, c) in self.file.sections.iter().zip(&self.sections) {
            let mut best: Option<usize> = None;
            for cand in candidates {
                if c.exclusions.is_match(cand) {
                    best = None;
                    break;
                }
                c.set.matches_into(cand, &mut hits);
                if let Some(&last) = hits.iter().max() {
                    best = Some(best.map_or(last, |b| b.max(last)));
                }
            }
            let Some(glob) = best else { continue };
            let rule = &s.rules[c.rules[glob]];
            let owners = if rule.owners.is_empty() {
                &s.default_owners
            } else {
                &rule.owners
            };
            if owners.is_empty() {
                continue;
            }
            out.push(RuleMatch {
                section: s,
                rule,
                owners,
            });
        }
        out
    }

    /// The owners of `path` from every matching section, deduplicated by key.
    pub fn owners(&self, path: &str) -> Vec<&OwnerRef> {
        let mut out: Vec<&OwnerRef> = Vec::new();
        for m in self.matches(path) {
            for o in m.owners {
                if !out.iter().any(|x| x.key() == o.key()) {
                    out.push(o);
                }
            }
        }
        out
    }
}

/// Compiled CODEOWNERS files by key (the blob OID, or a digest of an ingested file's text),
/// bounded by the number of files.
#[derive(Clone)]
pub struct CodeownersCache {
    inner: moka::sync::Cache<String, Arc<CompiledCodeowners>>,
}

impl std::fmt::Debug for CodeownersCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeownersCache")
            .field("entries", &self.inner.entry_count())
            .finish()
    }
}

impl Default for CodeownersCache {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl CodeownersCache {
    /// A cache holding up to `capacity` compiled files.
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: moka::sync::Cache::new(capacity),
        }
    }

    /// The compiled file for `key`, if cached.
    pub fn get(&self, key: &str) -> Option<Arc<CompiledCodeowners>> {
        self.inner.get(key)
    }

    /// The compiled file for `key`, compiling `text()` on a miss.
    pub fn get_or_compile(
        &self,
        key: &str,
        text: impl FnOnce() -> String,
    ) -> Arc<CompiledCodeowners> {
        if let Some(c) = self.inner.get(key) {
            return c;
        }
        let c = Arc::new(CompiledCodeowners::compile(&text()));
        self.inner.insert(key.to_owned(), c.clone());
        c
    }

    /// The compiled file for the text of an ingested CODEOWNERS file (keyed by its digest).
    pub fn get_text(&self, text: &str) -> Arc<CompiledCodeowners> {
        self.get_or_compile(&text_key(text), || text.to_owned())
    }
}

/// A cache key for CODEOWNERS text that is not a git blob: FNV-1a 64 over the text plus its
/// length, so it never collides with a 40-character blob OID.
pub fn text_key(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("text:{h:016x}:{}", text.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn codeowners_paths_follow_the_code_host() {
        assert_eq!(super::codeowners_paths("gitlab")[2], ".gitlab/CODEOWNERS");
        assert_eq!(super::codeowners_paths("github")[0], ".github/CODEOWNERS");
        assert_eq!(super::codeowners_paths("gitea")[2], ".gitea/CODEOWNERS");
        assert_eq!(super::codeowners_paths(""), super::GITLAB_CODEOWNERS_PATHS);
    }

    use super::*;

    fn owners(c: &CompiledCodeowners, path: &str) -> Vec<String> {
        c.owners(path).iter().map(|o| o.text()).collect()
    }

    #[test]
    fn parses_entries_owners_and_comments() {
        let f = parse(
            "# comment\n\n*.rb @ruby-owner\n/docs/ @docs alice@example.com @@maintainer\n\
             \\#file_with_pound.rb @owner-file-with-pound\n\
             /path\\ with\\ spaces/ @space-owner\nREADME.md @x not-an-owner # trailing comment @y\n",
        );
        assert_eq!(f.sections.len(), 1);
        let s = &f.sections[0];
        assert!(s.is_default());
        assert_eq!(s.rules.len(), 5);
        assert_eq!(s.rules[0].pattern, "*.rb");
        assert_eq!(s.rules[0].line, 3);
        assert_eq!(
            s.rules[1].owners,
            vec![
                OwnerRef::Handle("docs".into()),
                OwnerRef::Email("alice@example.com".into()),
                OwnerRef::Role("maintainer".into()),
            ]
        );
        assert_eq!(s.rules[2].pattern, "#file_with_pound.rb");
        assert_eq!(s.rules[3].pattern, "/path with spaces/");
        assert_eq!(s.rules[4].owners, vec![OwnerRef::Handle("x".into())]);
        assert_eq!(f.errors.len(), 1);
        assert_eq!(f.errors[0].line, 7);
    }

    #[test]
    fn sections_defaults_approvals_and_merging() {
        let f = parse(
            "* @everyone\n[Documentation][2] @docs-team\ndocs/\nREADME.md @readme\n\
             ^[Optional Section] @opt\n*.go\n[documentation]\n*.txt @txt\n[] @bad\n",
        );
        let names: Vec<_> = f.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["codeowners", "Documentation", "Optional Section"]
        );
        let docs = &f.sections[1];
        assert_eq!(docs.approvals, Some(2));
        assert_eq!(
            docs.default_owners,
            vec![OwnerRef::Handle("docs-team".into())]
        );
        // `[documentation]` merges into `[Documentation]`; `[] @bad` is not a header but an
        // entry with the pattern `[]`.
        let patterns: Vec<_> = docs.rules.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, vec!["docs/", "README.md", "*.txt", "[]"]);
        assert!(f.sections[2].optional);
    }

    #[test]
    fn header_like_patterns_are_entries() {
        let f = parse("[abc]*.rb @x\n");
        assert_eq!(f.sections.len(), 1);
        assert_eq!(f.sections[0].rules[0].pattern, "[abc]*.rb");
        let c = CompiledCodeowners::from_parsed(f);
        assert_eq!(owners(&c, "lib/a.rb"), vec!["@x"]);
        assert!(owners(&c, "lib/d.rb").is_empty());
    }

    #[test]
    fn glob_normalization() {
        assert_eq!(glob_for("*"), "/**/*");
        assert_eq!(glob_for("README.md"), "/**/README.md");
        assert_eq!(glob_for("/docs/"), "/docs/**/*");
        assert_eq!(glob_for("docs/"), "/**/docs/**/*");
        assert_eq!(glob_for("/docs/**/index.md"), "/docs/**/index.md");
        assert_eq!(glob_for("/docs/**"), "/docs/*");
        assert_eq!(glob_for("/a**b"), "/a*b");
    }

    /// The examples of GitLab's "Code Owners syntax" documentation.
    #[test]
    fn gitlab_documented_examples() {
        let c = CompiledCodeowners::compile(
            "\
# Default owners for everything in the repo
* @default-codeowner
# Rules defined later in the file take precedence
*.rb @ruby-owner
# Files with a `#` can still be accessed by escaping the pound sign
\\#file_with_pound.rb @owner-file-with-pound
# Multiple codeowners can be specified, separated by spaces or tabs
CODEOWNERS @multiple @code @owners
# Both usernames or email addresses can be used to match users
LICENSE @legal this_does_not_match janedoe@gitlab.com
# Group names can be used to match groups and nested groups
README @group @group/with-nested/subgroup
# End a path in a `/` for the rule to apply to every file nested in the directory
/docs/ @all-docs
# End a path in `/*` to apply the rule to every file directly in the directory
/docs/* @root-docs
# Include `/**` to apply the rule to every file in the directory and its subdirectories
/docs/**/*.md @markdown-docs
# A path without a leading `/` matches the directory anywhere in the repository
lib/ @lib-owner
# Paths in the root directory only
/config/ @config-owner
# Spaces in paths are escaped
path\\ with\\ spaces/ @space-owner
# Globstar paths
/foo/**/bar.rb @foobar-owner
",
        );
        // Only the token GitLab's example says does not match.
        assert_eq!(c.file().errors.len(), 1, "{:?}", c.file().errors);
        assert!(c.file().errors[0].message.contains("this_does_not_match"));
        assert_eq!(owners(&c, "src/main.go"), vec!["@default-codeowner"]);
        assert_eq!(owners(&c, "app/models/user.rb"), vec!["@ruby-owner"]);
        assert_eq!(
            owners(&c, "#file_with_pound.rb"),
            vec!["@owner-file-with-pound"]
        );
        assert_eq!(
            owners(&c, "CODEOWNERS"),
            vec!["@multiple", "@code", "@owners"]
        );
        assert_eq!(owners(&c, "LICENSE"), vec!["@legal", "janedoe@gitlab.com"]);
        assert_eq!(
            owners(&c, "README"),
            vec!["@group", "@group/with-nested/subgroup"]
        );
        // Last match wins: `/docs/*` over `/docs/` for direct children…
        assert_eq!(owners(&c, "docs/index.html"), vec!["@root-docs"]);
        // …and `/docs/` for nested files.
        assert_eq!(owners(&c, "docs/api/index.html"), vec!["@all-docs"]);
        assert_eq!(owners(&c, "docs/api/graphql.md"), vec!["@markdown-docs"]);
        assert_eq!(owners(&c, "docs/intro.md"), vec!["@markdown-docs"]);
        assert_eq!(owners(&c, "lib/a.go"), vec!["@lib-owner"]);
        assert_eq!(owners(&c, "app/lib/deep/a.go"), vec!["@lib-owner"]);
        assert_eq!(owners(&c, "config/app.yml"), vec!["@config-owner"]);
        assert_eq!(owners(&c, "app/config/app.yml"), vec!["@default-codeowner"]);
        assert_eq!(owners(&c, "x/path with spaces/f.txt"), vec!["@space-owner"]);
        assert_eq!(owners(&c, "foo/bar.rb"), vec!["@foobar-owner"]);
        assert_eq!(owners(&c, "foo/a/b/bar.rb"), vec!["@foobar-owner"]);
        // `*` does not cross `/`; dotfiles match.
        assert_eq!(owners(&c, ".gitlab-ci.yml"), vec!["@default-codeowner"]);
    }

    #[test]
    fn sections_combine_and_defaults_apply() {
        let c = CompiledCodeowners::compile(
            "\
* @admin
[Documentation] @docs-team
docs/
README.md @readme-owner
[Database] @database-team
model/db/
config/db/database-setup.md @docs-team
^[Optional]
*.md @optional-reviewer
",
        );
        assert_eq!(owners(&c, "src/a.rs"), vec!["@admin"]);
        assert_eq!(owners(&c, "docs/x.txt"), vec!["@admin", "@docs-team"]);
        assert_eq!(
            owners(&c, "README.md"),
            vec!["@admin", "@readme-owner", "@optional-reviewer"]
        );
        assert_eq!(
            owners(&c, "model/db/a.sql"),
            vec!["@admin", "@database-team"]
        );
        // The same owner from two sections appears once.
        assert_eq!(
            owners(&c, "config/db/database-setup.md"),
            vec!["@admin", "@docs-team", "@optional-reviewer"]
        );
        let m = c.matches("docs/x.txt");
        assert_eq!(m[1].section.name, "Documentation");
        assert_eq!(m[1].rule.line, 3);
    }

    #[test]
    fn exclusions_and_ownerless_entries() {
        let c = CompiledCodeowners::compile(
            "* @all\n!*.lock\n/vendor/\n[Frontend] @fe\n*.js\n!/vendor/\n",
        );
        assert!(owners(&c, "Cargo.lock").is_empty());
        // The default section's last match has no owners: the path has none there.
        assert_eq!(owners(&c, "app.js"), vec!["@all", "@fe"]);
        assert!(owners(&c, "vendor/lib.js").is_empty());
    }

    #[test]
    fn directory_owners() {
        let c = CompiledCodeowners::compile(
            "* @root\n/docs/ @docs\n/src/*.md @md\n/src/ @src\n/api @api-file\n",
        );
        let dir = |d: &str| -> Vec<String> {
            c.matches_dir(d)
                .iter()
                .flat_map(|m| m.owners.iter().map(OwnerRef::text))
                .collect()
        };
        assert_eq!(dir(""), vec!["@root"]);
        assert_eq!(dir("docs"), vec!["@docs"]);
        assert_eq!(dir("docs/deep"), vec!["@docs"]);
        assert_eq!(dir("src"), vec!["@src"]);
        // `/api` names the path itself, file or directory.
        assert_eq!(dir("api"), vec!["@api-file"]);
    }

    #[test]
    fn keys_and_owner_tokens() {
        assert_eq!(owner_key("@Alice"), "alice");
        assert_eq!(owner_key("Alice@Example.com"), "alice@example.com");
        assert_eq!(owner_key("@@maintainer"), "@maintainer");
        assert_eq!(OwnerRef::parse("@"), None);
        assert_eq!(OwnerRef::parse("@group/"), None);
        assert_eq!(OwnerRef::parse("plain"), None);
        assert_eq!(OwnerRef::parse("a@b"), None);
        assert!(text_key("x").starts_with("text:"));
    }

    #[test]
    fn cache_compiles_once() {
        let cache = CodeownersCache::new(10);
        let mut n = 0;
        for _ in 0..3 {
            cache.get_or_compile("oid", || {
                n += 1;
                "* @a".to_owned()
            });
        }
        assert_eq!(n, 1);
    }
}
