//! Running compute commands over search results.
//!
//! The commands:
//!
//! - **match only** reports, per file, each match of the regexp within the
//!   search's matched ranges with its capture groups;
//! - **replace** rewrites the whole file (every match of the pattern);
//! - **output** expands the template once per match of the pattern within
//!   what each result offers: the matched ranges of a file, the path with
//!   `type:path`, the repository name of a repository result, the message of
//!   a commit or the changed lines of each file of a diff (one result per
//!   file). With `select:` the
//!   template is expanded once per result, without matching.
//!
//! Structural patterns match with [`sourcegull_structural`] in the file's
//! language and expand `:[hole]` references.

use regex::Regex;
use sourcegull_search::stream::{ChunkMatch, Match};

use crate::result::{
    ComputeMatch, ComputeResult, Data, Environment, Location, MatchContext, Origin, Range, Text,
};
use crate::template::{MetaEnv, Template};

/// Structural matches one chunk or file yields at most.
const STRUCTURAL_LIMIT: usize = 100_000;

/// What a command matches with.
#[derive(Debug, Clone)]
pub enum MatchPattern {
    Regexp {
        source: String,
        regex: Regex,
    },
    /// A structural template (`:[hole]` syntax).
    Structural(String),
}

impl PartialEq for MatchPattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MatchPattern::Regexp { source: a, .. }, MatchPattern::Regexp { source: b, .. }) => {
                a == b
            }
            (MatchPattern::Structural(a), MatchPattern::Structural(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Output,
    /// `output.extra`: results carry their repository.
    Extra,
}

#[derive(Debug, Clone)]
pub enum Command {
    MatchOnly {
        source: String,
        regex: Regex,
    },
    Replace {
        pattern: MatchPattern,
        template: Template,
    },
    Output {
        pattern: MatchPattern,
        template: Template,
        separator: String,
        /// The query's `type:`.
        type_value: Option<String>,
        /// The query's `select:`.
        selector: Option<String>,
        kind: OutputKind,
    },
}

/// One unit a result offers to a command: where it is from, the built-in
/// variables, and the texts to run over.
struct Unit {
    origin: Origin,
    meta: MetaEnv,
    /// Language id for structural matching.
    lang_id: Option<&'static str>,
    chunks: Vec<String>,
}

/// The language name of `path` (or `known`), and its `sourcegull-languages` id.
fn language(path: &str, known: Option<&str>) -> (String, Option<&'static str>) {
    let lang = match known {
        Some(name) => sourcegull_languages::by_name(name),
        None => sourcegull_languages::detect(path, None),
    };
    match lang {
        Some(l) => (l.name.to_owned(), Some(l.id)),
        None => (known.unwrap_or_default().to_owned(), None),
    }
}

/// The text of `range` within `chunk`, if the offsets are consistent.
fn chunk_text<'a>(
    chunk: &'a ChunkMatch,
    range: &sourcegull_search::stream::Range,
) -> Option<&'a str> {
    let base = chunk.content_start.offset;
    let start = range.start.offset.checked_sub(base)? as usize;
    let end = range.end.offset.checked_sub(base)? as usize;
    chunk.content.get(start..end)
}

/// Splits a diff match's content (`` ```diff `` fence, per file a header
/// line `old new` then hunks) into (path, changed lines) per file.
fn diff_files(content: &str) -> Vec<(String, String)> {
    let body = content
        .strip_prefix("```diff\n")
        .unwrap_or(content)
        .trim_end_matches("```");
    let mut files: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        if line.starts_with("@@") {
            continue;
        }
        if line.starts_with(['+', '-', ' ']) {
            if let Some((_, lines)) = files.last_mut() {
                lines.push_str(line);
                lines.push('\n');
            }
            continue;
        }
        // Header: `old new`, equal halves when the file kept its name.
        let half = line.len() / 2;
        let path = if line.len() % 2 == 1
            && line.as_bytes()[half] == b' '
            && line[..half] == line[half + 1..]
        {
            &line[half + 1..]
        } else {
            line.rsplit_once(' ').map_or(line, |(_, new)| new)
        };
        files.push((path.to_owned(), String::new()));
    }
    files
}

impl Command {
    /// Whether running on `m` needs the whole file ([`Command::run`]'s `file`).
    pub fn needs_file(&self, m: &Match) -> bool {
        matches!(self, Command::Replace { .. }) && matches!(m, Match::Content { .. })
    }

    fn only_path(&self) -> bool {
        matches!(self, Command::Output { type_value: Some(t), .. } if t == "path")
    }

    fn structural(&self) -> bool {
        matches!(
            self,
            Command::Output {
                pattern: MatchPattern::Structural(_),
                ..
            }
        )
    }

    /// What `m` offers to an output command.
    fn units(&self, m: &Match) -> Vec<Unit> {
        match m {
            Match::Repo {
                repository_id,
                repository,
                ..
            } => vec![Unit {
                origin: Origin {
                    repository_id: *repository_id,
                    repository: repository.clone(),
                    ..Origin::default()
                },
                meta: MetaEnv {
                    repo: repository.clone(),
                    ..MetaEnv::default()
                },
                lang_id: None,
                chunks: vec![repository.clone()],
            }],
            Match::Content {
                path,
                repository_id,
                repository,
                commit,
                chunk_matches,
                language: lang,
                ..
            } => {
                let (lang, lang_id) = language(path, lang.as_deref());
                let mut chunks: Vec<String> = if self.only_path() {
                    vec![path.clone()]
                } else {
                    chunk_matches
                        .iter()
                        .flat_map(|c| c.ranges.iter().filter_map(|r| chunk_text(c, r)))
                        .map(str::to_owned)
                        .collect()
                };
                if self.structural() && chunks.len() > 1 {
                    // One structural run over all of them.
                    chunks = vec![chunks.concat()];
                }
                vec![Unit {
                    origin: Origin {
                        repository_id: *repository_id,
                        repository: repository.clone(),
                        commit: commit.clone(),
                        path: path.clone(),
                    },
                    meta: MetaEnv {
                        repo: repository.clone(),
                        path: path.clone(),
                        commit: commit.clone(),
                        lang,
                        ..MetaEnv::default()
                    },
                    lang_id,
                    chunks,
                }]
            }
            Match::Path {
                path,
                repository_id,
                repository,
                commit,
                language: lang,
                ..
            } => {
                let (lang, lang_id) = language(path, lang.as_deref());
                vec![Unit {
                    origin: Origin {
                        repository_id: *repository_id,
                        repository: repository.clone(),
                        commit: commit.clone(),
                        path: path.clone(),
                    },
                    meta: MetaEnv {
                        repo: repository.clone(),
                        path: path.clone(),
                        commit: commit.clone(),
                        lang,
                        ..MetaEnv::default()
                    },
                    lang_id,
                    chunks: if self.only_path() {
                        vec![path.clone()]
                    } else {
                        Vec::new()
                    },
                }]
            }
            Match::Commit {
                repository_id,
                repository,
                oid,
                message,
                author_name,
                committer_date,
                content,
                ..
            } => {
                let origin = Origin {
                    repository_id: *repository_id,
                    repository: repository.clone(),
                    commit: oid.clone(),
                    path: String::new(),
                };
                let meta = MetaEnv {
                    repo: repository.clone(),
                    commit: oid.clone(),
                    author: author_name.clone(),
                    date: chrono::DateTime::parse_from_rfc3339(committer_date)
                        .ok()
                        .map(|d| d.to_utc()),
                    ..MetaEnv::default()
                };
                if !content.starts_with("```diff") {
                    return vec![Unit {
                        origin,
                        meta,
                        lang_id: None,
                        chunks: vec![message.clone()],
                    }];
                }
                diff_files(content)
                    .into_iter()
                    .map(|(path, lines)| {
                        let (lang, lang_id) = language(&path, None);
                        Unit {
                            origin: Origin {
                                path: path.clone(),
                                ..origin.clone()
                            },
                            meta: MetaEnv {
                                path,
                                lang,
                                ..meta.clone()
                            },
                            lang_id,
                            chunks: vec![lines],
                        }
                    })
                    .collect()
            }
            // Symbol and owner results are not computed over.
            Match::Symbol { .. } | Match::Person { .. } | Match::Team { .. } => Vec::new(),
        }
    }

    /// Runs the command on one search result. `file` is the whole file when
    /// [`Command::needs_file`] says so (otherwise ignored); without it a
    /// replace gives nothing.
    pub fn run(&self, m: &Match, file: Option<&str>) -> Vec<ComputeResult> {
        match self {
            Command::MatchOnly { regex, .. } => match_only(m, regex).into_iter().collect(),
            Command::Replace { pattern, template } => {
                let (
                    Match::Content {
                        path,
                        repository_id,
                        repository,
                        commit,
                        language: lang,
                        ..
                    },
                    Some(content),
                ) = (m, file)
                else {
                    return Vec::new();
                };
                let (lang, lang_id) = language(path, lang.as_deref());
                let meta = MetaEnv {
                    repo: repository.clone(),
                    path: path.clone(),
                    commit: commit.clone(),
                    lang,
                    content: content.to_owned(),
                    ..MetaEnv::default()
                };
                vec![ComputeResult::Text(Text {
                    value: replace(pattern, template, &meta, lang_id, content),
                    kind: "replace-in-place".to_owned(),
                    origin: Origin {
                        repository_id: *repository_id,
                        repository: repository.clone(),
                        commit: commit.clone(),
                        path: path.clone(),
                    },
                    extra: false,
                })]
            }
            Command::Output {
                pattern,
                template,
                separator,
                selector,
                kind,
                ..
            } => self
                .units(m)
                .into_iter()
                .map(|unit| {
                    let mut value = String::new();
                    for chunk in &unit.chunks {
                        let meta = MetaEnv {
                            content: chunk.clone(),
                            ..unit.meta.clone()
                        };
                        if selector.is_some() {
                            // An explicit `select:` value: the template as is.
                            value.push_str(&template.render(&meta, &|_| None));
                            value.push_str(separator);
                            continue;
                        }
                        for v in outputs(pattern, template, &meta, unit.lang_id, chunk) {
                            value.push_str(&v);
                            value.push_str(separator);
                        }
                    }
                    ComputeResult::Text(Text {
                        value,
                        kind: "output".to_owned(),
                        origin: unit.origin,
                        extra: *kind == OutputKind::Extra,
                    })
                })
                .collect(),
        }
    }

    /// The values the command outputs for a whole file, one per match, for
    /// counting. Match-only commands yield the first capture group
    /// (or the match); replace commands yield nothing.
    pub fn file_values(&self, meta: &MetaEnv, content: &str) -> Vec<String> {
        let mut meta = meta.clone();
        meta.content = content.to_owned();
        let lang = sourcegull_languages::by_name(&meta.lang)
            .or_else(|| sourcegull_languages::detect(&meta.path, Some(content.as_bytes())));
        if meta.lang.is_empty()
            && let Some(l) = lang
        {
            meta.lang = l.name.to_owned();
        }
        let lang_id = lang.map(|l| l.id);
        match self {
            Command::MatchOnly { regex, .. } => regex
                .captures_iter(content)
                .map(|c| {
                    c.iter()
                        .skip(1)
                        .flatten()
                        .next()
                        .or_else(|| c.get(0))
                        .map_or_else(String::new, |m| m.as_str().to_owned())
                })
                .collect(),
            Command::Replace { .. } => Vec::new(),
            Command::Output {
                pattern,
                template,
                selector,
                ..
            } => {
                if selector.is_some() {
                    return vec![template.render(&meta, &|_| None)];
                }
                outputs(pattern, template, &meta, lang_id, content)
            }
        }
    }
}

/// The template expanded for each match of `pattern` in `text`.
fn outputs(
    pattern: &MatchPattern,
    template: &Template,
    meta: &MetaEnv,
    lang_id: Option<&str>,
    text: &str,
) -> Vec<String> {
    match pattern {
        MatchPattern::Regexp { regex, .. } => regex
            .captures_iter(text)
            .map(|caps| template.render(meta, &|name| capture(regex, &caps, name)))
            .collect(),
        MatchPattern::Structural(t) => {
            let Ok(p) = sourcegull_structural::Pattern::new(t, None) else {
                return Vec::new();
            };
            p.find_iter(text.as_bytes(), lang_id, STRUCTURAL_LIMIT)
                .into_iter()
                .map(|m| template.render(meta, &|name| hole(text, &m.env, name)))
                .collect()
        }
    }
}

fn capture<'t>(regex: &Regex, caps: &regex::Captures<'t>, name: &str) -> Option<&'t str> {
    let m = match name.parse::<usize>() {
        Ok(i) => caps.get(i),
        Err(_) => regex
            .capture_names()
            .position(|n| n == Some(name))
            .and_then(|i| caps.get(i)),
    };
    m.map(|m| m.as_str())
}

fn hole<'t>(text: &'t str, env: &sourcegull_structural::Env, name: &str) -> Option<&'t str> {
    env.iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, r)| text.get(r.clone()))
}

/// `content` with every match of `pattern` replaced by the template.
fn replace(
    pattern: &MatchPattern,
    template: &Template,
    meta: &MetaEnv,
    lang_id: Option<&str>,
    content: &str,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut last = 0;
    match pattern {
        MatchPattern::Regexp { regex, .. } => {
            for caps in regex.captures_iter(content) {
                let m = caps.get(0).expect("group 0");
                out.push_str(&content[last..m.start()]);
                out.push_str(&template.render(meta, &|name| capture(regex, &caps, name)));
                last = m.end();
            }
        }
        MatchPattern::Structural(t) => {
            let Ok(p) = sourcegull_structural::Pattern::new(t, None) else {
                return content.to_owned();
            };
            for m in p.find_iter(content.as_bytes(), lang_id, STRUCTURAL_LIMIT) {
                let (Some(before), Some(_)) = (
                    content.get(last..m.range.start),
                    content.get(m.range.clone()),
                ) else {
                    continue;
                };
                out.push_str(before);
                out.push_str(&template.render(meta, &|name| hole(content, &m.env, name)));
                last = m.range.end;
            }
        }
    }
    out.push_str(&content[last..]);
    out
}

/// The location of byte `pos` of `chunk`'s content.
fn locate(chunk: &ChunkMatch, pos: usize) -> Location {
    let before = &chunk.content.as_bytes()[..pos.min(chunk.content.len())];
    let newlines = memchr_count(before);
    let column = match before.iter().rposition(|&b| b == b'\n') {
        Some(nl) => pos - nl - 1,
        None => pos + chunk.content_start.column as usize,
    };
    Location {
        offset: i64::from(chunk.content_start.offset) + pos as i64,
        line: i64::from(chunk.content_start.line) + newlines as i64,
        column: column as i64,
    }
}

fn memchr_count(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}

/// The match-only result for a file match: each match of `regex` within the
/// search's matched ranges.
fn match_only(m: &Match, regex: &Regex) -> Option<ComputeResult> {
    let Match::Content {
        path,
        repository_id,
        repository,
        commit,
        chunk_matches,
        ..
    } = m
    else {
        return None;
    };
    let mut matches = Vec::new();
    for chunk in chunk_matches {
        for range in &chunk.ranges {
            let Some(text) = chunk_text(chunk, range) else {
                continue;
            };
            let base = (range.start.offset - chunk.content_start.offset) as usize;
            let span = |s: usize, e: usize| Range {
                start: locate(chunk, base + s),
                end: locate(chunk, base + e),
            };
            for caps in regex.captures_iter(text) {
                let whole = caps.get(0).expect("group 0");
                let mut env = Vec::new();
                for (i, name) in regex.capture_names().enumerate().skip(1) {
                    let Some(g) = caps.get(i) else { continue };
                    env.push((
                        name.map_or_else(|| i.to_string(), str::to_owned),
                        Data {
                            value: g.as_str().to_owned(),
                            range: span(g.start(), g.end()),
                        },
                    ));
                }
                matches.push(ComputeMatch {
                    value: whole.as_str().to_owned(),
                    range: span(whole.start(), whole.end()),
                    environment: Environment(env),
                });
            }
        }
    }
    Some(ComputeResult::MatchContext(MatchContext {
        matches,
        origin: Origin {
            repository_id: *repository_id,
            repository: repository.clone(),
            commit: commit.clone(),
            path: path.clone(),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ComputeQuery;
    use sourcegull_search::stream::Location as SLoc;

    fn sloc(offset: u32, line: u32, column: u32) -> SLoc {
        SLoc {
            offset,
            line,
            column,
        }
    }

    /// A content match on `file` whose ranges are the matches of `re`.
    fn content_match(path: &str, file: &str, re: &str) -> Match {
        let re = Regex::new(re).unwrap();
        let mut chunks = Vec::new();
        for m in re.find_iter(file) {
            let line_start = file[..m.start()].rfind('\n').map_or(0, |i| i + 1);
            let line_end = file[m.end()..]
                .find('\n')
                .map_or(file.len(), |i| m.end() + i);
            let line = file[..line_start].matches('\n').count() as u32;
            let loc = |o: usize| {
                let l = file[..o].matches('\n').count() as u32;
                let c = o - file[..o].rfind('\n').map_or(0, |i| i + 1);
                sloc(o as u32, l, c as u32)
            };
            chunks.push(ChunkMatch {
                content: file[line_start..line_end].to_owned(),
                content_start: sloc(line_start as u32, line, 0),
                ranges: vec![sourcegull_search::stream::Range {
                    start: loc(m.start()),
                    end: loc(m.end()),
                }],
            });
        }
        Match::Content {
            path: path.into(),
            repository_id: 1,
            repository: "github.com/a/b".into(),
            commit: "abc".into(),
            branches: Vec::new(),
            chunk_matches: chunks,
            language: None,
        }
    }

    fn run(q: &str, m: &Match, file: Option<&str>) -> Vec<ComputeResult> {
        ComputeQuery::parse(q).unwrap().command.run(m, file)
    }

    fn texts(results: &[ComputeResult]) -> Vec<String> {
        results
            .iter()
            .map(|r| match r {
                ComputeResult::Text(t) => t.value.clone(),
                ComputeResult::MatchContext(_) => panic!("text expected"),
            })
            .collect()
    }

    const FILE: &str = "package main\n\nfunc a() { foo(1) }\nfunc b() { foo(22) }\n";

    #[test]
    fn match_only_reports_captures_with_positions() {
        let m = content_match("main.go", FILE, r"foo\(\d+\)");
        let r = run(r"foo\((\d+)\)", &m, None);
        let ComputeResult::MatchContext(mc) = &r[0] else {
            panic!()
        };
        assert_eq!(mc.matches.len(), 2);
        let second = &mc.matches[1];
        assert_eq!(second.value, "foo(22)");
        let g = second.environment.get("1").unwrap();
        assert_eq!(g.value, "22");
        assert_eq!(g.range.start.line, 3);
        assert_eq!(g.range.start.column, 15);
        assert_eq!(g.range.start.offset, FILE.find("22").unwrap() as i64);
        // Named groups keep their names.
        let r = run(r"foo\((?P<n>\d+)\)", &m, None);
        let ComputeResult::MatchContext(mc) = &r[0] else {
            panic!()
        };
        assert_eq!(mc.matches[0].environment.get("n").unwrap().value, "1");
    }

    #[test]
    fn output_expands_captures_and_metavariables() {
        let m = content_match("main.go", FILE, r"foo\(\d+\)");
        let r = run(r"content:output(foo\((\d+)\) -> $1@$path:$lang)", &m, None);
        assert_eq!(texts(&r), ["1@main.go:Go\n22@main.go:Go\n"]);
        let r = run(
            r"content:output.extra(foo\((\d+)\) -> $repo) separator:,",
            &m,
            None,
        );
        assert_eq!(texts(&r), ["github.com/a/b,github.com/a/b,"]);
        let ComputeResult::Text(t) = &r[0] else {
            panic!()
        };
        assert!(t.extra);
        // `type:path` runs over the path.
        let r = run(r"content:output(\.(\w+)$ -> $1) type:path", &m, None);
        assert_eq!(texts(&r), ["go\n"]);
        // `select:` expands the template once, without matching.
        let r = run(r"content:output(foo -> $repo) select:repo", &m, None);
        assert_eq!(texts(&r), ["github.com/a/b\ngithub.com/a/b\n"]);
    }

    #[test]
    fn replace_rewrites_the_file() {
        let m = content_match("main.go", FILE, r"foo");
        assert!(
            ComputeQuery::parse("content:replace(foo -> bar)")
                .unwrap()
                .command
                .needs_file(&m)
        );
        let r = run(
            r"content:replace(foo\((\d+)\) -> bar($1, 0))",
            &m,
            Some(FILE),
        );
        assert_eq!(
            texts(&r),
            ["package main\n\nfunc a() { bar(1, 0) }\nfunc b() { bar(22, 0) }\n"]
        );
        let ComputeResult::Text(t) = &r[0] else {
            panic!()
        };
        assert_eq!(t.kind, "replace-in-place");
        assert_eq!(t.origin.path, "main.go");
        // Without the file: nothing.
        assert!(run("content:replace(foo -> bar)", &m, None).is_empty());
    }

    #[test]
    fn structural_replace_and_output() {
        let file = "fmt.Println(a(b), \"x)\")\nfmt.Println(c)\n";
        let m = content_match("x.go", file, r"fmt\.Println\(.*\)");
        let r = run(
            "content:replace.structural(fmt.Println(:[args]) -> log.Print(:[args]))",
            &m,
            Some(file),
        );
        assert_eq!(texts(&r), ["log.Print(a(b), \"x)\")\nlog.Print(c)\n"]);
        let r = run(
            "content:output.structural(fmt.Println(:[args]) -> :[args]) separator:|",
            &m,
            None,
        );
        assert_eq!(texts(&r), ["a(b), \"x)\"|c|"]);
    }

    #[test]
    fn commits_and_diffs() {
        let commit = Match::Commit {
            label: String::new(),
            url: String::new(),
            detail: String::new(),
            repository_id: 3,
            repository: "r".into(),
            oid: "deadbeef".into(),
            message: "fix: thing (#12)".into(),
            author_name: "Ann".into(),
            author_date: "2024-01-02T03:04:05Z".into(),
            committer_name: "Bob".into(),
            committer_date: "2024-01-02T03:04:05Z".into(),
            content: "```COMMIT_EDITMSG\nfix: thing (#12)\n```".into(),
            ranges: Vec::new(),
        };
        let r = run(
            r"content:output(#(\d+) -> $1 $author $date.year) type:commit",
            &commit,
            None,
        );
        assert_eq!(texts(&r), ["12 Ann 2024\n"]);

        let mut diff = commit;
        if let Match::Commit { content, .. } = &mut diff {
            *content = "```diff\nsrc/a.rs src/a.rs\n@@ -1,1 +1,1 @@ \n-old\n+new\nb.go b.go\n@@ -1 +1 @@ \n+added\n```"
                .into();
        }
        let r = run(
            r"content:output(^\+(\w+) -> $path:$1:$lang) type:diff",
            &diff,
            None,
        );
        assert_eq!(texts(&r), ["", "b.go:added:Go\n"]);
        // Multi-line: `(?m)` anchors per line.
        let r = run(
            r"content:output((?m)^\+(\w+) -> $path:$1:$lang) type:diff",
            &diff,
            None,
        );
        assert_eq!(texts(&r), ["src/a.rs:new:Rust\n", "b.go:added:Go\n"]);
        assert_eq!(r[1].origin().path, "b.go");
    }

    #[test]
    fn repo_results_and_file_values() {
        let repo = Match::Repo {
            repository_id: 1,
            repository: "gitlab.com/x/y".into(),
            description: None,
            fork: false,
            archived: false,
            private: false,
        };
        let r = run(r"content:output(/(\w+)$ -> $1) type:repo", &repo, None);
        assert_eq!(texts(&r), ["y\n"]);

        let q = ComputeQuery::parse(r"content:output(foo\((\d+)\) -> $1)").unwrap();
        let meta = MetaEnv {
            path: "main.go".into(),
            ..MetaEnv::default()
        };
        assert_eq!(q.command.file_values(&meta, FILE), ["1", "22"]);
        let q = ComputeQuery::parse(r"foo\((\d+)\)").unwrap();
        assert_eq!(q.command.file_values(&meta, FILE), ["1", "22"]);
    }
}
