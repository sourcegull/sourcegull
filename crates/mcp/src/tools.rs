//! The MCP tools: search, files, code navigation and history, run as the caller.

use std::fmt::Write as _;

use serde_json::{Value, json};
use sourcegull_authn::Actor;
use sourcegull_codenav::{
    CodeNav, FileAt, Position, Range, RepoFilter, UsageKind, UsagePage, token,
};
use sourcegull_db::stores::ListReposOptions;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search::stream::{Match, SkippedReason};
use sourcegull_services::AppState;
use sourcegull_services::git::{GitCallError, GitReader, TreeOptions, commit_subject};
use sourcegull_types::{Repo, RepoAccess};

use crate::format::{Out, PathFilter, clip, date, short};

/// Bytes of a file read for `read_file` and the navigation tools.
const FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Lines `read_file` returns per call at most.
const MAX_READ_LINES: usize = 2000;

/// A tool's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    /// The `tools/call` result.
    pub fn into_result(self) -> Value {
        json!({
            "content": [{ "type": "text", "text": self.text }],
            "isError": self.is_error,
        })
    }
}

type ToolResult = Result<String, String>;

/// The tool list (`tools/list`).
pub fn definitions() -> Value {
    let repo = json!({
        "type": "string",
        "description": "Repository name, e.g. `gitlab.example.com/group/project`; a unique suffix such as `group/project` works.",
    });
    let rev = json!({
        "type": "string",
        "description": "Branch, tag or commit; default: the default branch.",
    });
    let path = json!({ "type": "string", "description": "File path in the repository." });
    let position = |extra: Value| {
        let mut props = json!({
            "repo": repo,
            "path": path,
            "rev": rev,
            "line": { "type": "integer", "minimum": 1, "description": "1-based line of the identifier." },
            "symbol": { "type": "string", "description": "The identifier on that line (preferred over column)." },
            "occurrence": { "type": "integer", "minimum": 1, "description": "Which occurrence of `symbol` on the line (default 1)." },
            "column": { "type": "integer", "minimum": 1, "description": "1-based character column, if `symbol` is not given." },
        });
        if let (Some(p), Value::Object(e)) = (props.as_object_mut(), extra) {
            p.extend(e);
        }
        json!({ "type": "object", "properties": props, "required": ["repo", "path", "line"] })
    };
    let tools = [
        (
            "search",
            "Search code",
            "Search code across all repositories with Sourcegull's query language (fast, indexed). \
             Returns matching lines grouped by file, ripgrep-style (`line:text`). Terms are ANDed \
             per file; \"quoted phrase\" is exact; /regexp/ is a regex. Filters: repo:<regexp>, \
             file:<regexp>, -file:<regexp>, lang:<language>, case:yes, type:symbol|path|commit|diff|repo, \
             author:, after:/before: (commit and diff searches), rev:<branch>, select:repo|file, \
             and/or/not with parentheses. Examples: `lang:go func NewServer`, \
             `repo:acme/api file:\\.proto$ /rpc \\w+Stream/`, `type:diff after:\"1 week ago\" timeout`.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query." },
                    "repo": { "type": "string", "description": "Only this repository (added as an exact repo: filter)." },
                    "pattern_type": {
                        "type": "string",
                        "enum": ["keyword", "standard", "literal", "regexp", "structural"],
                        "description": "How patterns are read; default keyword (terms ANDed, quotes exact, /regexp/).",
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Results (files, commits, repos) to return; default 30." },
                    "max_lines_per_file": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Matching lines shown per file; default 10 (the rest is counted)." },
                },
                "required": ["query"],
            }),
        ),
        (
            "read_file",
            "Read a file",
            "Read a file (or a line range) from a repository at a revision, with line numbers. \
             Read only the range you need: search results and definitions give line numbers. \
             A directory path returns its listing.",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo, "path": path, "rev": rev,
                    "start_line": { "type": "integer", "minimum": 1, "description": "First line (1-based, default 1)." },
                    "end_line": { "type": "integer", "minimum": 1, "description": format!("Last line (inclusive); at most {MAX_READ_LINES} lines per call.") },
                },
                "required": ["repo", "path"],
            }),
        ),
        (
            "list_files",
            "List files",
            "List a directory of a repository, or with `recursive` all files below it. \
             `pattern` filters paths: a glob (`*.go`, `src/**/*.rs`) or a substring.",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo,
                    "path": { "type": "string", "description": "Directory; default the root." },
                    "rev": rev,
                    "recursive": { "type": "boolean", "description": "List the whole subtree (default false)." },
                    "pattern": { "type": "string", "description": "Glob or substring the path must match." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Entries to return; default 500." },
                },
                "required": ["repo"],
            }),
        ),
        (
            "find_symbols",
            "Find symbol definitions",
            "Find where functions, types, methods, constants … are defined, by name, across \
             repositories. Faster and more precise than text search for 'where is X defined'.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Symbol name, or a regexp unless `exact`." },
                    "exact": { "type": "boolean", "description": "Match the whole name exactly (default false: regexp, substring)." },
                    "repo": { "type": "string", "description": "Only this repository." },
                    "kind": { "type": "string", "description": "Only this kind: function, method, class, struct, interface, enum, constant, variable, field, module, type …" },
                    "language": { "type": "string", "description": "Only this language, e.g. go, rust, typescript." },
                    "filters": { "type": "string", "description": "Extra query filters, e.g. `file:^src/ -file:_test`." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Symbols to return; default 30." },
                },
                "required": ["name"],
            }),
        ),
        (
            "go_to_definition",
            "Go to definition",
            "Find the definition of the identifier at a line of a file (precise SCIP data when \
             uploaded, otherwise syntactic or search-based), with its hover documentation and \
             the definition's source lines.",
            position(json!({
                "context_lines": { "type": "integer", "minimum": 0, "maximum": 200, "description": "Definition lines to show; default 15." },
            })),
        ),
        (
            "find_references",
            "Find references",
            "Find the usages of the identifier at a line of a file, across repositories, \
             grouped by file with the line text. Page with `offset`.",
            position(json!({
                "scope": { "type": "string", "enum": ["all", "repo"], "description": "`repo`: only the file's repository; default all." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Usages per page; default 100." },
                "offset": { "type": "integer", "minimum": 0, "description": "Usages to skip (from a previous page)." },
            })),
        ),
        (
            "find_implementations",
            "Find implementations",
            "Find the implementations of the interface, trait or method at a line of a file, \
             across repositories, and the interfaces it implements; grouped by file with the \
             line text. Needs precise (SCIP) data. Page with `offset`.",
            position(json!({
                "scope": { "type": "string", "enum": ["all", "repo"], "description": "`repo`: only the file's repository; default all." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Locations per page; default 100." },
                "offset": { "type": "integer", "minimum": 0, "description": "Locations to skip (from a previous page)." },
            })),
        ),
        (
            "list_repos",
            "List repositories",
            "List the repositories you can search, optionally filtered by a name substring.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring of the name." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Default 100." },
                    "offset": { "type": "integer", "minimum": 0 },
                },
            }),
        ),
        (
            "list_branches",
            "List branches and tags",
            "List the branches and/or tags of a repository, most recently committed first.",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo,
                    "kind": { "type": "string", "enum": ["branches", "tags", "all"], "description": "Default branches." },
                    "prefix": { "type": "string", "description": "Only names starting with this." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Default 50." },
                },
                "required": ["repo"],
            }),
        ),
        (
            "commit_log",
            "Commit history",
            "The commit history of a revision, optionally of one path: one line per commit \
             (oid, date, author, subject).",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo, "rev": rev,
                    "path": { "type": "string", "description": "Only commits touching this file or directory." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 30." },
                    "offset": { "type": "integer", "minimum": 0 },
                },
                "required": ["repo"],
            }),
        ),
        (
            "get_commit",
            "Show a commit",
            "Show a commit: message, author, changed files and the unified diff (against its \
             first parent).",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo,
                    "rev": { "type": "string", "description": "Commit oid (or branch/tag)." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Only these paths." },
                    "diff": { "type": "boolean", "description": "Include the diff (default true); false gives the file list only." },
                },
                "required": ["repo", "rev"],
            }),
        ),
        (
            "compare_revisions",
            "Compare revisions",
            "Compare two revisions (`base...head`): the commits on head since the merge base, \
             the changed files, and optionally the diff.",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo,
                    "base": { "type": "string", "description": "Base revision." },
                    "head": { "type": "string", "description": "Head revision." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Only these paths in the diff." },
                    "diff": { "type": "boolean", "description": "Include the diff (default false)." },
                },
                "required": ["repo", "base", "head"],
            }),
        ),
        (
            "blame",
            "Blame",
            "Who last changed each line of a file: one line per hunk (lines, commit, date, \
             author, subject).",
            json!({
                "type": "object",
                "properties": {
                    "repo": repo, "path": path, "rev": rev,
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 },
                },
                "required": ["repo", "path"],
            }),
        ),
    ];
    Value::Array(
        tools
            .into_iter()
            .map(|(name, title, description, schema)| {
                json!({
                    "name": name,
                    "title": title,
                    "description": description,
                    "inputSchema": schema,
                    "annotations": {
                        "title": title,
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false,
                    },
                })
            })
            .collect(),
    )
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    opt_str(args, key).ok_or_else(|| format!("`{key}` is required"))
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

/// An integer argument clamped to `1..=max`, `default` when absent.
fn limit(args: &Value, key: &str, default: usize, max: usize) -> Result<usize, String> {
    Ok(opt_u64(args, key)?
        .map_or(default, |n| usize::try_from(n).unwrap_or(max))
        .clamp(1, max))
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// A repository path without leading or trailing slashes.
fn clean_path(p: &str) -> String {
    p.trim().trim_matches('/').to_owned()
}

fn git_err(e: GitCallError) -> String {
    match e {
        GitCallError::Internal(m) => {
            tracing::error!(error = %m, "MCP: repo node call failed");
            "internal error".to_owned()
        }
        e => e.to_string(),
    }
}

/// A repository opened at a resolved commit.
struct Opened {
    repo: Repo,
    git: GitReader,
    commit: String,
}

/// Tool calls for one caller.
pub struct Ctx<'a> {
    state: &'a AppState,
    actor: Option<&'a Actor>,
}

impl<'a> Ctx<'a> {
    pub fn new(state: &'a AppState, actor: Option<&'a Actor>) -> Self {
        Self { state, actor }
    }

    /// Runs tool `name`; `None` for an unknown tool.
    pub async fn call(&self, name: &str, args: &Value) -> Option<ToolOutput> {
        let started = std::time::Instant::now();
        let result = match name {
            "search" => self.search(args).await,
            "read_file" => self.read_file(args).await,
            "list_files" => self.list_files(args).await,
            "find_symbols" => self.find_symbols(args).await,
            "go_to_definition" => self.go_to_definition(args).await,
            "find_references" => self.find_references(args).await,
            "find_implementations" => self.find_implementations(args).await,
            "list_repos" => self.list_repos(args).await,
            "list_branches" => self.list_branches(args).await,
            "commit_log" => self.commit_log(args).await,
            "get_commit" => self.get_commit(args).await,
            "compare_revisions" => self.compare_revisions(args).await,
            "blame" => self.blame(args).await,
            _ => return None,
        };
        tracing::debug!(
            tool = name,
            ok = result.is_ok(),
            ms = started.elapsed().as_millis() as u64,
            "MCP tool call"
        );
        Some(match result {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(text) => ToolOutput {
                text,
                is_error: true,
            },
        })
    }

    async fn access(&self) -> Result<RepoAccess, String> {
        self.state.authz.access(self.actor).await.map_err(|e| {
            tracing::error!(error = %e, "MCP: resolving repository permissions");
            "internal error".to_owned()
        })
    }

    /// The repository named `name` (or the only one whose name ends with `/name`) that the
    /// caller may read.
    async fn repo(&self, name: &str) -> Result<Repo, String> {
        let name = name
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_matches('/')
            .trim_end_matches(".git");
        let access = self.access().await?;
        let internal = |e: sourcegull_db::DbError| {
            tracing::error!(error = %e, "MCP: looking up a repository");
            "internal error".to_owned()
        };
        let repos = self.state.db.repos();
        let found = match repos
            .get_by_name_authorized(name, &access)
            .await
            .map_err(internal)?
        {
            Some(r) => r,
            None => {
                // `access` is the caller's.
                #[allow(clippy::disallowed_methods)]
                let candidates = repos
                    .list(&ListReposOptions {
                        query: Some(name.to_owned()),
                        access: Some(access),
                        limit: 50,
                        ..Default::default()
                    })
                    .await
                    .map_err(internal)?;
                let suffix = format!("/{}", name.to_lowercase());
                let mut by_suffix: Vec<&Repo> = candidates
                    .iter()
                    .filter(|r| r.name.to_lowercase().ends_with(&suffix))
                    .collect();
                if by_suffix.len() == 1 {
                    by_suffix.remove(0).clone()
                } else {
                    let shown: Vec<&str> = if by_suffix.is_empty() {
                        candidates
                            .iter()
                            .map(|r| r.name.as_str())
                            .take(10)
                            .collect()
                    } else {
                        by_suffix.iter().map(|r| r.name.as_str()).take(10).collect()
                    };
                    let mut msg = if by_suffix.len() > 1 {
                        format!("Repository name `{name}` is ambiguous.")
                    } else {
                        format!("Repository `{name}` not found.")
                    };
                    if !shown.is_empty() {
                        let _ = write!(msg, " Candidates: {}", shown.join(", "));
                    } else {
                        msg.push_str(" Use list_repos to see the repository names.");
                    }
                    return Err(msg);
                }
            }
        };
        if let Some(reason) = &found.blocked {
            return Err(format!("Repository {} is blocked: {reason}", found.name));
        }
        Ok(found)
    }

    /// The repository of `args.repo` at `rev` (default branch when empty).
    async fn open(&self, args: &Value, rev: Option<&str>) -> Result<Opened, String> {
        let repo = self.repo(req_str(args, "repo")?).await?;
        let git = GitReader::new(self.state.repo_clients.clone(), &repo);
        let commit = git
            .resolve_revision(rev.unwrap_or(""))
            .await
            .map_err(git_err)?;
        Ok(Opened { repo, git, commit })
    }

    /// `repo@<rev>` as given (or the short commit) for headers.
    fn at(o: &Opened, rev: Option<&str>) -> String {
        format!("{}@{}", o.repo.name, rev.unwrap_or(short(&o.commit)))
    }

    async fn search(&self, args: &Value) -> ToolResult {
        let mut query = req_str(args, "query")?.to_owned();
        if let Some(name) = opt_str(args, "repo") {
            let repo = self.repo(name).await?;
            query = format!("repo:^{}$ {query}", regex::escape(&repo.name));
        }
        let max = limit(args, "limit", 30, 500)?;
        let pattern_type = opt_str(args, "pattern_type");
        let out = sourcegull_search_web::collect(
            &self.state.search_backend(),
            self.actor,
            &query,
            pattern_type,
            max,
        )
        .await;
        if let Some(e) = out.error {
            return Err(format!("Search failed: {e}"));
        }
        let mut text = Out::default();
        if out.progress.is_none() {
            // Rejected: the alerts say why.
            let mut msg = String::new();
            for a in &out.alerts {
                let _ = writeln!(msg, "{}: {}", a.title, a.description);
                for p in &a.proposed_queries {
                    let _ = writeln!(msg, "  try: {}  ({})", p.query, p.description);
                }
            }
            return Err(if msg.is_empty() {
                "The query was rejected.".to_owned()
            } else {
                msg.trim_end().to_owned()
            });
        }
        let lines: usize = out.matches.iter().map(match_lines).sum();
        let mut summary = format!(
            "{} result{} ({} matching line{})",
            out.matches.len(),
            if out.matches.len() == 1 { "" } else { "s" },
            lines,
            if lines == 1 { "" } else { "s" },
        );
        if let Some(p) = &out.progress {
            if let Some(n) = p.repositories_count {
                let _ = write!(summary, " in {n} searched repositories");
            }
            let _ = write!(summary, ", {} ms", p.duration_ms);
        }
        text.line(summary);
        if out.limit_hit() || out.matches.len() >= max {
            text.line(format!(
                "Result limit {max} reached: narrow the query (repo:, file:, lang:) or raise `limit`."
            ));
        }
        notes(&mut text, &out);
        if out.matches.is_empty() {
            text.line("No results.");
        }
        let per_file = limit(args, "max_lines_per_file", 10, 1000)?;
        for m in &out.matches {
            text.line("");
            if !format_match(&mut text, m, per_file) {
                break;
            }
        }
        Ok(text.finish("narrow the query or lower `limit`."))
    }

    async fn read_file(&self, args: &Value) -> ToolResult {
        let rev = opt_str(args, "rev");
        let o = self.open(args, rev).await?;
        let path = clean_path(req_str(args, "path")?);
        let entry = o.git.stat(&o.commit, &path).await.map_err(git_err)?;
        if entry.kind == pb::EntryKind::Directory as i32 {
            return self.list_dir(&o, rev, &path, false, None, 500).await;
        }
        if entry.kind == pb::EntryKind::Submodule as i32 {
            return Err(format!("{path} is a submodule."));
        }
        let file = o
            .git
            .read_file(&o.commit, &path, FILE_MAX_BYTES)
            .await
            .map_err(git_err)?;
        if file.binary {
            return Err(format!("{path} is a binary file ({} bytes).", file.size));
        }
        let content = String::from_utf8_lossy(&file.content);
        let all: Vec<&str> = content.lines().collect();
        let total = all.len();
        let start = opt_u64(args, "start_line")?.unwrap_or(1).max(1) as usize;
        if start > total.max(1) {
            return Err(format!(
                "start_line {start} is past the end: {path} has {total} lines."
            ));
        }
        let requested_end = opt_u64(args, "end_line")?.map(|n| n as usize);
        let end = requested_end
            .unwrap_or(total)
            .min(total)
            .min(start + MAX_READ_LINES - 1)
            .max(start);
        let mut text = Out::default();
        let mut header = format!("{} {path}", Self::at(&o, rev));
        let _ = write!(header, " (lines {start}-{end} of {total}");
        if file.truncated {
            let _ = write!(
                header,
                "; file is {} bytes, only the first {FILE_MAX_BYTES} read",
                file.size
            );
        }
        header.push(')');
        text.line(header);
        let width = end.to_string().len();
        let mut last = start.saturating_sub(1);
        for (i, line) in all.iter().enumerate().take(end).skip(start - 1) {
            if !text.line(format!("{:>width$}\t{}", i + 1, clip(line))) {
                break;
            }
            last = i + 1;
        }
        if last < total && (text.cut || requested_end.is_none_or(|e| e > last)) {
            text.note(format!(
                "… more lines follow: call again with start_line={}",
                last + 1
            ));
        }
        Ok(text.finish(""))
    }

    async fn list_files(&self, args: &Value) -> ToolResult {
        let rev = opt_str(args, "rev");
        let o = self.open(args, rev).await?;
        let path = opt_str(args, "path").map(clean_path).unwrap_or_default();
        let recursive = opt_bool(args, "recursive").unwrap_or(false);
        let filter = opt_str(args, "pattern").map(PathFilter::new).transpose()?;
        let max = limit(args, "limit", 500, 5000)?;
        self.list_dir(&o, rev, &path, recursive, filter, max).await
    }

    async fn list_dir(
        &self,
        o: &Opened,
        rev: Option<&str>,
        path: &str,
        recursive: bool,
        filter: Option<PathFilter>,
        max: usize,
    ) -> ToolResult {
        let fetch = if filter.is_some() { 200_000 } else { max + 1 };
        let opts = TreeOptions {
            recursive,
            include_sizes: false,
            limit: u32::try_from(fetch).unwrap_or(u32::MAX),
        };
        let (entries, node_cut) = o
            .git
            .list_tree(&o.commit, path, &opts)
            .await
            .map_err(git_err)?;
        let mut text = Out::default();
        text.line(format!(
            "{} {}/",
            Self::at(o, rev),
            if path.is_empty() { "" } else { path }
        ));
        let mut shown = 0;
        let mut more = node_cut;
        for e in &entries {
            let dir = e.kind == pb::EntryKind::Directory as i32;
            // A recursive listing names files; directories add nothing.
            if recursive && dir {
                continue;
            }
            if filter.as_ref().is_some_and(|f| !f.matches(&e.path)) {
                continue;
            }
            if shown == max {
                more = true;
                break;
            }
            let mut l = e.path.clone();
            if dir {
                l.push('/');
            } else if e.kind == pb::EntryKind::Submodule as i32 {
                l.push_str(" (submodule)");
            } else if e.kind == pb::EntryKind::Symlink as i32 {
                l.push_str(" (symlink)");
            }
            if !text.line(l) {
                break;
            }
            shown += 1;
        }
        if shown == 0 {
            text.line(if filter.is_some() {
                "No matching files."
            } else {
                "Empty directory."
            });
        }
        if more {
            text.line(format!(
                "… more entries: list a subdirectory, add `pattern`, or raise `limit` (shown {shown})."
            ));
        }
        Ok(text.finish("list a subdirectory or add `pattern`."))
    }

    async fn find_symbols(&self, args: &Value) -> ToolResult {
        let name = req_str(args, "name")?;
        let max = limit(args, "limit", 30, 500)?;
        let mut query = format!("type:symbol count:{}", max.max(50));
        if let Some(r) = opt_str(args, "repo") {
            let repo = self.repo(r).await?;
            let _ = write!(query, " repo:^{}$", regex::escape(&repo.name));
        }
        if let Some(lang) = opt_str(args, "language") {
            let _ = write!(query, " lang:{lang}");
        }
        if let Some(kind) = opt_str(args, "kind") {
            let _ = write!(query, " select:symbol.{}", kind.to_lowercase());
        }
        if let Some(f) = opt_str(args, "filters") {
            let _ = write!(query, " {f}");
        }
        let pattern = if opt_bool(args, "exact").unwrap_or(false) {
            format!("^{}$", regex::escape(name))
        } else {
            name.to_owned()
        };
        let _ = write!(query, " {pattern}");
        let out = sourcegull_search_web::collect(
            &self.state.search_backend(),
            self.actor,
            &query,
            Some("regexp"),
            max,
        )
        .await;
        if let Some(e) = out.error {
            return Err(format!("Symbol search failed: {e}"));
        }
        if out.progress.is_none() {
            let msg: Vec<String> = out
                .alerts
                .iter()
                .map(|a| format!("{}: {}", a.title, a.description))
                .collect();
            return Err(msg.join("\n"));
        }
        let mut text = Out::default();
        let mut n = 0;
        'outer: for m in &out.matches {
            let Match::Symbol {
                path,
                repository,
                symbols,
                ..
            } = m
            else {
                continue;
            };
            for s in symbols {
                if n == max {
                    break 'outer;
                }
                let mut l = format!("{} {}", s.kind.to_lowercase(), s.name);
                if !s.container_name.is_empty() {
                    let _ = write!(l, " (in {})", s.container_name);
                }
                let _ = write!(l, "  {repository} {path}:{}", s.line);
                if !text.line(l) {
                    break 'outer;
                }
                n += 1;
            }
        }
        notes(&mut text, &out);
        if n == 0 {
            text.line("No symbols found. Try a shorter name, drop `exact`, or use search.");
        } else if n == max || out.limit_hit() {
            text.line(format!(
                "… limit {max} reached: add repo, kind or language, or set `exact`."
            ));
        }
        Ok(text.finish("narrow with repo, kind or language."))
    }

    /// The file and 0-based position of a navigation request, with the file's text.
    async fn position(&self, args: &Value) -> Result<(FileAt, Position, Range, String), String> {
        let o = self.open(args, opt_str(args, "rev")).await?;
        let path = clean_path(req_str(args, "path")?);
        let line_no = opt_u64(args, "line")?.ok_or("`line` is required")? as usize;
        if line_no == 0 {
            return Err("`line` is 1-based".to_owned());
        }
        let file = o
            .git
            .read_file(&o.commit, &path, FILE_MAX_BYTES)
            .await
            .map_err(git_err)?;
        let content = String::from_utf8_lossy(&file.content).into_owned();
        let Some(line) = content.lines().nth(line_no - 1) else {
            return Err(format!(
                "line {line_no} is past the end: {path} has {} lines.",
                content.lines().count()
            ));
        };
        let (start, end) = if let Some(sym) = opt_str(args, "symbol") {
            let nth = opt_u64(args, "occurrence")?.unwrap_or(1).max(1) as usize;
            let ident = |c: Option<char>| c.is_some_and(|c| token::is_ident_char(c, None));
            let found = line
                .match_indices(sym)
                .filter(|(i, _)| {
                    !ident(line[..*i].chars().next_back())
                        && !ident(line[i + sym.len()..].chars().next())
                })
                .map(|(i, _)| i)
                .nth(nth - 1)
                .or_else(|| line.match_indices(sym).map(|(i, _)| i).nth(nth - 1));
            let Some(b) = found else {
                return Err(format!(
                    "`{sym}` is not on line {line_no} of {path}: {}",
                    clip(line)
                ));
            };
            (b, b + sym.len())
        } else if let Some(col) = opt_u64(args, "column")? {
            let col = col.max(1) as usize - 1;
            let Some((b, _)) = line.char_indices().nth(col) else {
                return Err(format!(
                    "column {} is past the end of line {line_no}",
                    col + 1
                ));
            };
            (b, b)
        } else {
            return Err("pass `symbol` (the identifier on the line) or `column`".to_owned());
        };
        let line0 = u32::try_from(line_no - 1).unwrap_or(u32::MAX);
        let pos = Position {
            line: line0,
            character: token::byte_to_utf16(line, start),
        };
        let range = Range::on_line(
            line0,
            pos.character,
            token::byte_to_utf16(line, end).max(pos.character),
        );
        Ok((
            FileAt {
                repo: o.repo,
                commit: o.commit,
                path,
            },
            pos,
            range,
            content,
        ))
    }

    async fn go_to_definition(&self, args: &Value) -> ToolResult {
        let (file, pos, _, content) = self.position(args).await?;
        let context = opt_u64(args, "context_lines")?.unwrap_or(15).min(200) as usize;
        let nav = CodeNav::new(self.state, self.actor);
        let info = nav
            .symbol_info(&file, pos)
            .await
            .map_err(|e| format!("Code navigation failed: {e}"))?;
        let Some(info) = info else {
            return Err(format!(
                "No identifier at line {} of {} (keywords, literals and comments have no definition).",
                pos.line + 1,
                file.path
            ));
        };
        let mut text = Out::default();
        let Some(def) = &info.definition else {
            text.line(format!(
                "No definition found for `{}` ({}). Try find_symbols or search.",
                info.token,
                provenance(info.provenance)
            ));
            return Ok(text.finish(""));
        };
        let def_line = def.range.start.line as usize + 1;
        let lang = sourcegull_codenav::highlight::language(&file.path, content.as_bytes());
        // `pkg` of `pkg.Name` / `pkg::Name` / `pkg->Name` before the identifier.
        let qualifier = content
            .lines()
            .nth(pos.line as usize)
            .and_then(|l| Some(&l[..token::utf16_to_byte(l, info.range.start.character)?]))
            .and_then(|before| {
                let before = before
                    .strip_suffix('.')
                    .or_else(|| before.strip_suffix("::"))
                    .or_else(|| before.strip_suffix("->"))?;
                let start = before
                    .rfind(|c: char| !token::is_ident_char(c, None))
                    .map_or(0, |i| {
                        i + before[i..].chars().next().map_or(1, char::len_utf8)
                    });
                Some(before[start..].to_owned()).filter(|q| !q.is_empty())
            });
        text.line(format!(
            "Definition of `{}` ({}): {} {}:{}",
            info.token,
            provenance(info.provenance),
            def.repo,
            def.path,
            def_line
        ));
        if let Some(c) = &info.cross_version {
            text.line(format!(
                "Linked across versions: the symbol is used at {} and the package is indexed at {} only.",
                c.requested, c.found
            ));
        }
        if context > 0 {
            let same = def.repo_id == file.repo.id.get()
                && def.commit == file.commit
                && def.path == file.path;
            let def_content = if same {
                Some(content)
            } else {
                GitReader::from_parts(
                    self.state.repo_clients.clone(),
                    sourcegull_types::RepoId(def.repo_id),
                    &def.repo,
                )
                .read_file(&def.commit, &def.path, FILE_MAX_BYTES)
                .await
                .ok()
                .map(|f| String::from_utf8_lossy(&f.content).into_owned())
            };
            if let Some(c) = def_content {
                let last = def_line + context - 1;
                let width = last.to_string().len();
                text.line("");
                for (i, l) in c.lines().enumerate().skip(def_line - 1).take(context) {
                    text.line(format!("{:>width$}\t{}", i + 1, clip(l)));
                }
            }
        }
        if let Some(hover) = info.hover.as_deref().filter(|h| !h.trim().is_empty()) {
            text.line("");
            text.line("Hover:");
            text.line(hover.trim());
        }
        if info.provenance == sourcegull_codenav::Provenance::SearchBased {
            let others = self
                .other_definitions(&file, lang, &info.token, qualifier.as_deref(), def)
                .await;
            if !others.is_empty() {
                text.line("");
                text.line(
                    "Search-based guess (no precise index); other definitions with this name:",
                );
                for o in others {
                    text.line(o);
                }
            }
        }
        Ok(text.finish("lower `context_lines`."))
    }

    /// Up to 5 other symbols named `token` (in language `lang`), for a search-based
    /// definition that may be the wrong one of several: the file's repository is searched
    /// first, then all, and candidates in a directory named like the reference's qualifier
    /// (`psql` of `psql.New`) come first.
    async fn other_definitions(
        &self,
        file: &FileAt,
        lang: Option<&str>,
        token: &str,
        qualifier: Option<&str>,
        chosen: &sourcegull_codenav::Location,
    ) -> Vec<String> {
        let local = sourcegull_codenav::search_based::repo_filter(&file.repo.name, None);
        let chosen_line = chosen.range.start.line + 1;
        // (qualifier matches, other repository, line)
        let mut found: Vec<(bool, bool, String)> = Vec::new();
        for scope in [local.as_str(), ""] {
            let query = sourcegull_codenav::search_based::definitions_query(scope, lang, token, 50);
            let out = sourcegull_search_web::collect(
                &self.state.search_backend(),
                self.actor,
                &query,
                Some("regexp"),
                50,
            )
            .await;
            for m in &out.matches {
                let Match::Symbol {
                    path,
                    repository,
                    symbols,
                    ..
                } = m
                else {
                    continue;
                };
                for sym in symbols {
                    if *repository == chosen.repo && *path == chosen.path && sym.line == chosen_line
                    {
                        continue;
                    }
                    let mut l = format!("  {} {}", sym.kind.to_lowercase(), sym.name);
                    if !sym.container_name.is_empty() {
                        let _ = write!(l, " (in {})", sym.container_name);
                    }
                    let _ = write!(l, "  {repository} {path}:{}", sym.line);
                    if found.iter().any(|f| f.2 == l) {
                        continue;
                    }
                    let qualified = qualifier.is_some_and(|q| {
                        path.rsplit('/')
                            .skip(1)
                            .any(|dir| dir.eq_ignore_ascii_case(q))
                            || sym.container_name.eq_ignore_ascii_case(q)
                    });
                    found.push((qualified, *repository != file.repo.name, l));
                }
            }
        }
        // Stable: search order within each group.
        found.sort_by_key(|f| (!f.0, f.1));
        let chosen_qualified = qualifier.is_some_and(|q| {
            chosen
                .path
                .rsplit('/')
                .skip(1)
                .any(|dir| dir.eq_ignore_ascii_case(q))
        });
        let mut lines: Vec<String> = Vec::new();
        if let Some(q) = qualifier
            && !chosen_qualified
            && !found.iter().any(|f| f.0)
        {
            lines.push(format!(
                "  (none is in a `{q}` directory: `{q}.{token}` may come from a dependency \
                 that is not mirrored here)"
            ));
        }
        lines.extend(found.into_iter().take(5).map(|(q, _, l)| {
            if q && !chosen_qualified {
                format!("{l}  ← matches `{}.`", qualifier.unwrap_or_default())
            } else {
                l
            }
        }));
        lines
    }

    async fn find_references(&self, args: &Value) -> ToolResult {
        let (file, _, range, content) = self.position(args).await?;
        let name = content
            .lines()
            .nth(range.start.line as usize)
            .and_then(|l| {
                let start = token::utf16_to_byte(l, range.start.character)?;
                let end = token::utf16_to_byte(l, range.end.character)?;
                l.get(start..end)
            })
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let max = limit(args, "limit", 100, 1000)?;
        let offset = opt_u64(args, "offset")?.unwrap_or(0) as usize;
        let filter = match opt_str(args, "scope") {
            Some("repo") => RepoFilter {
                only: Some(file.repo.name.clone()),
                exclude: None,
            },
            _ => RepoFilter::default(),
        };
        let nav = CodeNav::new(self.state, self.actor);
        let page = nav
            .usages(&file, range, &filter, offset, max)
            .await
            .map_err(|e| format!("Code navigation failed: {e}"))?;
        Ok(references_text(&page, name.as_deref(), offset))
    }

    async fn find_implementations(&self, args: &Value) -> ToolResult {
        let (file, _, range, content) = self.position(args).await?;
        let name = content
            .lines()
            .nth(range.start.line as usize)
            .and_then(|l| {
                let start = token::utf16_to_byte(l, range.start.character)?;
                let end = token::utf16_to_byte(l, range.end.character)?;
                l.get(start..end)
            })
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let max = limit(args, "limit", 100, 1000)?;
        let offset = opt_u64(args, "offset")?.unwrap_or(0) as usize;
        let filter = match opt_str(args, "scope") {
            Some("repo") => RepoFilter {
                only: Some(file.repo.name.clone()),
                exclude: None,
            },
            _ => RepoFilter::default(),
        };
        let nav = CodeNav::new(self.state, self.actor);
        let page = nav
            .implementations(&file, range, &filter, offset, max)
            .await
            .map_err(|e| format!("Code navigation failed: {e}"))?;
        let mut text = Out::default();
        let Some(page) = page else {
            text.line(format!(
                "No precise code intelligence data covers line {} of {} in {} at {}, so \
                 implementations are unknown: they come only from SCIP uploads. \
                 find_references or search may find them by name.",
                range.start.line + 1,
                file.path,
                file.repo.name,
                &file.commit[..file.commit.len().min(12)],
            ));
            return Ok(text.finish(""));
        };
        let what = name
            .as_deref()
            .map(|n| format!("`{n}`"))
            .unwrap_or_else(|| "the symbol".to_owned());
        if page.usages.is_empty() {
            text.line(if offset > 0 {
                "No more implementations.".to_owned()
            } else {
                format!(
                    "No implementations of {what} and nothing it implements in precise data \
                     (only repositories with precise data are covered)."
                )
            });
            return Ok(text.finish(""));
        }
        let implementations = page
            .usages
            .iter()
            .filter(|u| u.kind == UsageKind::Implementation)
            .count();
        let supers = page.usages.len() - implementations;
        let mut kinds: Vec<&str> = page
            .usages
            .iter()
            .map(|u| provenance(u.provenance))
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        text.line(format!(
            "Implementations {}–{} of {what}: {implementations} implementation{}, {supers} \
             implemented symbol{} ({}){}",
            offset + 1,
            offset + page.usages.len(),
            if implementations == 1 { "" } else { "s" },
            if supers == 1 { "" } else { "s" },
            kinds.join(", "),
            page.next
                .map(|n| format!("; more with offset={n}"))
                .unwrap_or_default()
        ));
        let mut files: Vec<((&str, &str), Vec<String>)> = Vec::new();
        for u in &page.usages {
            let key = (u.location.repo.as_str(), u.location.path.as_str());
            let line = format!(
                "{}:{}{}",
                u.location.range.start.line + 1,
                clip(u.line.trim_end()),
                if u.kind == UsageKind::Super {
                    "  [implements]"
                } else {
                    ""
                }
            );
            match files.iter_mut().find(|(k, _)| *k == key) {
                Some((_, lines)) => lines.push(line),
                None => files.push((key, vec![line])),
            }
        }
        'files: for ((repo, path), lines) in files {
            text.line("");
            if !text.line(format!("{repo} {path}")) {
                break;
            }
            for l in lines {
                if !text.line(l) {
                    break 'files;
                }
            }
        }
        if let Some(n) = page.next
            && text.cut
        {
            text.note(format!(
                "… more implementations: call again with offset={n}"
            ));
        }
        Ok(text.finish("lower `limit`."))
    }

    async fn list_repos(&self, args: &Value) -> ToolResult {
        let access = self.access().await?;
        let max = limit(args, "limit", 100, 1000)?;
        let offset = opt_u64(args, "offset")?.unwrap_or(0);
        // `access` is the caller's.
        #[allow(clippy::disallowed_methods)]
        let repos = self
            .state
            .db
            .repos()
            .list(&ListReposOptions {
                query: opt_str(args, "query").map(str::to_owned),
                access: Some(access),
                limit: i64::try_from(max + 1).unwrap_or(i64::MAX),
                offset: i64::try_from(offset).unwrap_or(0),
                ..Default::default()
            })
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "MCP: listing repositories");
                "internal error".to_owned()
            })?;
        let mut text = Out::default();
        if repos.is_empty() {
            text.line("No repositories.");
        }
        for r in repos.iter().take(max) {
            let mut l = r.name.clone();
            if let Some(b) = &r.default_branch {
                let _ = write!(l, " ({b})");
            }
            for (flag, label) in [(r.archived, "archived"), (r.fork, "fork")] {
                if flag {
                    let _ = write!(l, " [{label}]");
                }
            }
            let desc = r.description.trim();
            if !desc.is_empty() {
                let _ = write!(l, " — {}", clip(desc.lines().next().unwrap_or("")));
            }
            if !text.line(l) {
                break;
            }
        }
        if repos.len() > max {
            text.line(format!(
                "… more: call again with offset={}",
                offset as usize + max
            ));
        }
        Ok(text.finish("lower `limit` or add `query`."))
    }

    async fn list_branches(&self, args: &Value) -> ToolResult {
        let o = self.open(args, None).await?;
        let (branches, tags) = match opt_str(args, "kind") {
            Some("tags") => (false, true),
            Some("all") => (true, true),
            _ => (true, false),
        };
        let max = limit(args, "limit", 50, 1000)?;
        let resp = o
            .git
            .list_refs(branches, tags, opt_str(args, "prefix").unwrap_or(""))
            .await
            .map_err(git_err)?;
        let mut refs = resp.refs;
        refs.sort_by_key(|r| std::cmp::Reverse(r.commit_time));
        let mut text = Out::default();
        text.line(format!("{} ({} refs)", o.repo.name, refs.len()));
        for r in refs.iter().take(max) {
            let kind = if r.name.starts_with("refs/tags/") {
                "tag"
            } else {
                "branch"
            };
            let default = if r.name == resp.head_ref {
                " (default)"
            } else {
                ""
            };
            if !text.line(format!(
                "{kind} {}{default}  {} {}",
                r.short_name,
                short(&r.commit_oid),
                date(r.commit_time)
            )) {
                break;
            }
        }
        if refs.len() > max {
            text.line(format!(
                "… {} more: use `prefix` or raise `limit`.",
                refs.len() - max
            ));
        }
        Ok(text.finish("use `prefix`."))
    }

    async fn commit_log(&self, args: &Value) -> ToolResult {
        let rev = opt_str(args, "rev");
        let o = self.open(args, rev).await?;
        let path = opt_str(args, "path").map(clean_path).unwrap_or_default();
        let max = limit(args, "limit", 30, 500)?;
        let offset = opt_u64(args, "offset")?.unwrap_or(0);
        let (commits, more) = o
            .git
            .commit_log(
                &o.commit,
                &path,
                u32::try_from(offset).unwrap_or(u32::MAX),
                u32::try_from(max).unwrap_or(u32::MAX),
            )
            .await
            .map_err(git_err)?;
        let mut text = Out::default();
        let mut header = Self::at(&o, rev);
        if !path.is_empty() {
            let _ = write!(header, " {path}");
        }
        text.line(header);
        for c in &commits {
            if !text.line(commit_line(c)) {
                break;
            }
        }
        if commits.is_empty() {
            text.line("No commits.");
        }
        if more {
            text.line(format!(
                "… more: call again with offset={}",
                offset as usize + commits.len()
            ));
        }
        Ok(text.finish("lower `limit`."))
    }

    async fn get_commit(&self, args: &Value) -> ToolResult {
        let rev = req_str(args, "rev")?;
        let o = self.open(args, Some(rev)).await?;
        let c = o.git.get_commit(&o.commit).await.map_err(git_err)?;
        let with_diff = opt_bool(args, "diff").unwrap_or(true);
        let files = o
            .git
            .diff("", &o.commit, str_list(args, "paths"), !with_diff)
            .await
            .map_err(git_err)?;
        let mut text = Out::default();
        text.line(format!("commit {} ({})", c.oid, o.repo.name));
        if !c.parents.is_empty() {
            text.line(format!(
                "parents {}",
                c.parents
                    .iter()
                    .map(|p| short(p))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if let Some(a) = &c.author {
            text.line(format!("author {} <{}> {}", a.name, a.email, date(a.time)));
        }
        if let Some(cm) = &c.committer
            && c.author.as_ref().is_none_or(|a| a.name != cm.name)
        {
            text.line(format!(
                "committer {} <{}> {}",
                cm.name,
                cm.email,
                date(cm.time)
            ));
        }
        text.line("");
        for l in c.message.trim_end().lines() {
            text.line(format!("    {l}"));
        }
        text.line("");
        format_files(&mut text, &files, with_diff);
        Ok(text.finish("pass `paths` or set diff=false."))
    }

    async fn compare_revisions(&self, args: &Value) -> ToolResult {
        let base_rev = req_str(args, "base")?;
        let head_rev = req_str(args, "head")?;
        let o = self.open(args, Some(head_rev)).await?;
        let base = o.git.resolve_revision(base_rev).await.map_err(git_err)?;
        let cmp = o
            .git
            .compare(&base, &o.commit, 250)
            .await
            .map_err(git_err)?;
        let mut text = Out::default();
        text.line(format!(
            "{} {base_rev}...{head_rev}: merge base {}, {} commit{}{}",
            o.repo.name,
            short(&cmp.merge_base_oid),
            cmp.commits.len(),
            if cmp.commits.len() == 1 { "" } else { "s" },
            if cmp.commits_truncated {
                " (first 250 shown)"
            } else {
                ""
            }
        ));
        for c in &cmp.commits {
            if !text.line(commit_line(c)) {
                break;
            }
        }
        text.line("");
        let paths = str_list(args, "paths");
        if opt_bool(args, "diff").unwrap_or(false) && !cmp.merge_base_oid.is_empty() {
            let files = o
                .git
                .diff(&cmp.merge_base_oid, &o.commit, paths, false)
                .await
                .map_err(git_err)?;
            format_files(&mut text, &files, true);
        } else {
            let files: Vec<pb::FileDiff> = cmp
                .files
                .into_iter()
                .filter(|f| {
                    paths.is_empty()
                        || paths.iter().any(|p| {
                            f.new_path.starts_with(p.as_str()) || f.old_path.starts_with(p.as_str())
                        })
                })
                .collect();
            format_files(&mut text, &files, false);
        }
        Ok(text.finish("pass `paths` or set diff=false."))
    }

    async fn blame(&self, args: &Value) -> ToolResult {
        let rev = opt_str(args, "rev");
        let o = self.open(args, rev).await?;
        let path = clean_path(req_str(args, "path")?);
        let start = opt_u64(args, "start_line")?.unwrap_or(0);
        let end = opt_u64(args, "end_line")?.unwrap_or(0);
        let hunks = o
            .git
            .blame_all(
                &o.commit,
                &path,
                u32::try_from(start).unwrap_or(u32::MAX),
                u32::try_from(end).unwrap_or(u32::MAX),
            )
            .await
            .map_err(git_err)?;
        let mut text = Out::default();
        text.line(format!("{} {path}", Self::at(&o, rev)));
        for h in &hunks {
            let last = h.start_line + h.line_count.saturating_sub(1);
            let lines = if h.line_count > 1 {
                format!("{}-{last}", h.start_line)
            } else {
                h.start_line.to_string()
            };
            let (who, when) = h
                .author
                .as_ref()
                .map(|a| (a.name.as_str(), date(a.time)))
                .unwrap_or_default();
            if !text.line(format!(
                "L{lines}  {} {when} {who}  {}",
                short(&h.commit_oid),
                clip(&h.summary)
            )) {
                break;
            }
        }
        Ok(text.finish("pass start_line and end_line."))
    }
}

/// Why a search may be incomplete (skipped repositories, timeouts) and its alerts.
fn notes(text: &mut Out, out: &sourcegull_search_web::SearchOutcome) {
    if let Some(p) = &out.progress {
        for s in &p.skipped {
            if s.reason == SkippedReason::DisplayLimit {
                continue;
            }
            let mut l = format!("Note: {}", s.title);
            if !s.message.is_empty() {
                let _ = write!(l, ": {}", s.message);
            }
            text.line(l);
        }
    }
    for a in &out.alerts {
        text.line(format!("Alert: {}: {}", a.title, a.description));
    }
}

fn provenance(p: sourcegull_codenav::Provenance) -> &'static str {
    match p {
        sourcegull_codenav::Provenance::Precise => "precise",
        sourcegull_codenav::Provenance::Syntactic => "syntactic",
        sourcegull_codenav::Provenance::SearchBased => "search-based",
    }
}

fn commit_line(c: &pb::Commit) -> String {
    let (who, when) = c
        .author
        .as_ref()
        .map(|a| (a.name.as_str(), date(a.time)))
        .unwrap_or_default();
    format!(
        "{} {when} {who}  {}",
        short(&c.oid),
        clip(commit_subject(&c.message))
    )
}

/// The file list (`M path +3 -1`) and, with `diff`, the hunks.
fn format_files(text: &mut Out, files: &[pb::FileDiff], diff: bool) {
    text.line(format!(
        "{} changed file{}",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    ));
    for f in files {
        let status = match pb::ChangeStatus::try_from(f.status) {
            Ok(pb::ChangeStatus::Added) => "A",
            Ok(pb::ChangeStatus::Deleted) => "D",
            Ok(pb::ChangeStatus::Renamed) => "R",
            Ok(pb::ChangeStatus::Copied) => "C",
            Ok(pb::ChangeStatus::TypeChanged) => "T",
            _ => "M",
        };
        let name = if f.old_path != f.new_path && !f.old_path.is_empty() && !f.new_path.is_empty() {
            format!("{} → {}", f.old_path, f.new_path)
        } else if f.new_path.is_empty() {
            f.old_path.clone()
        } else {
            f.new_path.clone()
        };
        if !text.line(format!(
            " {status} {name} +{} -{}",
            f.additions, f.deletions
        )) {
            return;
        }
    }
    if !diff {
        return;
    }
    for f in files {
        text.line("");
        let old = if f.old_path.is_empty() {
            "/dev/null".to_owned()
        } else {
            format!("a/{}", f.old_path)
        };
        let new = if f.new_path.is_empty() {
            "/dev/null".to_owned()
        } else {
            format!("b/{}", f.new_path)
        };
        text.line(format!("--- {old}"));
        text.line(format!("+++ {new}"));
        if f.binary {
            text.line("Binary file");
            continue;
        }
        if f.too_large {
            text.line("Diff too large to show");
            continue;
        }
        for h in &f.hunks {
            let mut header = format!(
                "@@ -{},{} +{},{} @@",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            );
            if !h.section.is_empty() {
                let _ = write!(header, " {}", h.section.trim());
            }
            if !text.line(header) {
                return;
            }
            for l in h.body.lines() {
                if !text.line(clip(l)) {
                    return;
                }
            }
        }
    }
}

/// Matching lines of a result, for the summary.
fn match_lines(m: &Match) -> usize {
    match m {
        Match::Content { chunk_matches, .. } => chunk_matches
            .iter()
            .map(|c| {
                let mut lines: Vec<u32> = c
                    .ranges
                    .iter()
                    .flat_map(|r| r.start.line..=r.end.line)
                    .collect();
                lines.dedup();
                lines.len().max(1)
            })
            .sum(),
        Match::Symbol { symbols, .. } => symbols.len(),
        _ => 1,
    }
}

/// `repo path` with the branch when the result is not on the default branch.
fn file_header(repo: &str, path: &str, branches: &[String]) -> String {
    match branches.first().filter(|b| !b.is_empty() && *b != "HEAD") {
        Some(b) => format!("{repo}@{b} {path}"),
        None => format!("{repo} {path}"),
    }
}

/// Writes one search result; false once the output budget is used up.
fn format_match(text: &mut Out, m: &Match, per_file: usize) -> bool {
    match m {
        Match::Content {
            path,
            repository,
            branches,
            chunk_matches,
            ..
        } => {
            if !text.line(file_header(repository, path, branches)) {
                return false;
            }
            let mut shown = 0;
            let mut hidden = 0;
            for c in chunk_matches {
                let first = c.content_start.line as usize + 1;
                for (i, l) in c.content.lines().enumerate() {
                    if shown == per_file {
                        hidden += 1;
                        continue;
                    }
                    if !text.line(format!("{}:{}", first + i, clip(l))) {
                        return false;
                    }
                    shown += 1;
                }
            }
            if hidden > 0 {
                text.line(format!(
                    "… {hidden} more line{} in this file (read_file, or raise max_lines_per_file)",
                    if hidden == 1 { "" } else { "s" }
                ));
            }
            if chunk_matches.is_empty() {
                text.line("(path match)");
            }
            true
        }
        Match::Path {
            path,
            repository,
            branches,
            ..
        } => text.line(format!(
            "{} (path match)",
            file_header(repository, path, branches)
        )),
        Match::Symbol {
            path,
            repository,
            branches,
            symbols,
            ..
        } => {
            if !text.line(file_header(repository, path, branches)) {
                return false;
            }
            symbols.iter().all(|s| {
                let mut l = format!("{}: {} {}", s.line, s.kind.to_lowercase(), s.name);
                if !s.container_name.is_empty() {
                    let _ = write!(l, " (in {})", s.container_name);
                }
                text.line(l)
            })
        }
        Match::Commit {
            repository,
            oid,
            author_name,
            author_date,
            message,
            content,
            ..
        } => {
            let day = author_date.get(..10).unwrap_or(author_date);
            if !text.line(format!(
                "{repository} commit {} {day} {author_name}  {}",
                short(oid),
                clip(commit_subject(message))
            )) {
                return false;
            }
            // Diff results carry the matching hunks in a fenced block.
            if let Some(body) = content.strip_prefix("```diff\n") {
                for l in body.trim_end_matches("```").trim_end().lines() {
                    if !text.line(clip(l)) {
                        return false;
                    }
                }
            }
            true
        }
        Match::Repo {
            repository,
            description,
            ..
        } => {
            let mut l = format!("{repository} (repository)");
            if let Some(d) = description.as_deref().filter(|d| !d.trim().is_empty()) {
                let _ = write!(l, " — {}", clip(d.lines().next().unwrap_or("")));
            }
            text.line(l)
        }
        Match::Person { handle, email, .. } => text.line(format!("owner {handle} {email}")),
        Match::Team { handle, name, .. } => text.line(format!("team {handle} {name}")),
    }
}

/// The `find_references` output for a page of usages from `offset`: a header, the usages
/// grouped by file (each group tagged with its provenance when the page mixes them), and for
/// a precise answer a closing coverage line.
fn references_text(page: &UsagePage, name: Option<&str>, offset: usize) -> String {
    let mut text = Out::default();
    let coverage = page
        .coverage
        .as_ref()
        .map(|c| format!("Coverage: {}", c.summary()));
    if page.usages.is_empty() {
        text.line(if offset > 0 {
            "No more usages."
        } else {
            "No usages found (is `symbol` an identifier on that line?)."
        });
        if let Some(c) = coverage {
            text.line(c);
        }
        return text.finish("");
    }
    let mut kinds: Vec<&str> = page
        .usages
        .iter()
        .map(|u| provenance(u.provenance))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    let tagged = kinds.len() > 1 || page.coverage.is_some();
    let symbol =
        name.unwrap_or_else(|| page.usages[0].symbol.rsplit(' ').next().unwrap_or_default());
    text.line(format!(
        "Usages {}–{} of `{symbol}` ({}){}",
        offset + 1,
        offset + page.usages.len(),
        kinds.join(", "),
        page.next
            .map(|n| format!("; more with offset={n}"))
            .unwrap_or_default()
    ));
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut order: Vec<(&str, &str, &str, Option<&str>)> = Vec::new();
    for u in &page.usages {
        let p = provenance(u.provenance);
        let version = u.cross_version.as_ref().map(|c| c.found.as_str());
        let key = (
            u.location.repo.as_str(),
            u.location.path.as_str(),
            p,
            version,
        );
        let idx = match order.iter().position(|k| *k == key) {
            Some(i) => i,
            None => {
                order.push(key);
                let header = match (tagged, version) {
                    (_, Some(v)) => format!("{} {}  ({p}, at version {v})", key.0, key.1),
                    (true, None) => format!("{} {}  ({p})", key.0, key.1),
                    (false, None) => format!("{} {}", key.0, key.1),
                };
                groups.push((header, Vec::new()));
                order.len() - 1
            }
        };
        let tag = match u.kind {
            UsageKind::Definition => "  [definition]",
            UsageKind::Implementation => "  [implementation]",
            UsageKind::Super => "  [super]",
            UsageKind::Reference => "",
        };
        groups[idx].1.push(format!(
            "{}:{}{tag}",
            u.location.range.start.line + 1,
            clip(u.line.trim_end())
        ));
    }
    'out: for (header, lines) in groups {
        text.line("");
        if !text.line(header) {
            break;
        }
        for l in lines {
            if !text.line(l) {
                break 'out;
            }
        }
    }
    let cut = text.cut && page.next.is_none();
    if let Some(n) = page.next
        && text.cut
    {
        text.note(format!("… more usages: call again with offset={n}"));
    }
    if let Some(c) = coverage {
        text.note("");
        text.note(c);
    }
    text.cut = cut;
    text.finish("lower `limit`.")
}

#[cfg(test)]
mod references_tests {
    use sourcegull_codenav::{Coverage, CoverageReason, Location, Provenance, Usage};

    use super::*;

    fn usage(repo: &str, path: &str, line: u32, p: Provenance) -> Usage {
        Usage {
            location: Location {
                repo_id: 1,
                repo: repo.into(),
                commit: "c".into(),
                path: path.into(),
                range: Range::on_line(line, 0, 4),
            },
            kind: UsageKind::Reference,
            provenance: p,
            symbol: "scip-go gomod m v1 `m`/Send().".into(),
            line: format!("Send() // {line}"),
            data_source: "",
            cross_version: None,
        }
    }

    #[test]
    fn precise_with_search_based_additions() {
        let page = UsagePage {
            usages: vec![
                usage("lib", "a.go", 1, Provenance::Precise),
                usage("lib", "a.go", 4, Provenance::Precise),
                usage("push-service", "main.go", 9, Provenance::SearchBased),
            ],
            next: Some(3),
            coverage: Some(Coverage {
                precise: vec!["lib".into(), "api".into()],
                cross_version: vec![],
                search_based: vec![("push-service".into(), CoverageReason::NoIndex)],
                truncated: false,
            }),
        };
        assert_eq!(
            references_text(&page, Some("Send"), 0),
            "Usages 1–3 of `Send` (precise, search-based); more with offset=3\n\
             \n\
             lib a.go  (precise)\n\
             2:Send() // 1\n\
             5:Send() // 4\n\
             \n\
             push-service main.go  (search-based)\n\
             10:Send() // 9\n\
             \n\
             Coverage: precise: 2 repositories · search-based: push-service (no index)"
        );
    }

    #[test]
    fn single_tier_is_untagged() {
        let page = UsagePage {
            usages: vec![usage("r", "x.go", 0, Provenance::SearchBased)],
            next: None,
            coverage: None,
        };
        assert_eq!(
            references_text(&page, Some("Send"), 0),
            "Usages 1–1 of `Send` (search-based)\n\nr x.go\n1:Send() // 0"
        );
    }
    #[test]
    fn cross_version_groups_name_the_version() {
        let mut other = usage("lib", "b.go", 2, Provenance::Precise);
        other.cross_version = Some(sourcegull_codenav::CrossVersion {
            requested: "v1.0.54".into(),
            found: "v1.1.0".into(),
        });
        let page = UsagePage {
            usages: vec![usage("svc", "a.go", 0, Provenance::Precise), other],
            next: None,
            coverage: Some(Coverage {
                precise: vec!["svc".into(), "lib".into()],
                cross_version: vec![("lib".into(), "v1.1.0".into())],
                search_based: vec![],
                truncated: false,
            }),
        };
        assert_eq!(
            references_text(&page, Some("Send"), 0),
            "Usages 1–2 of `Send` (precise)\n\
             \n\
             svc a.go  (precise)\n\
             1:Send() // 0\n\
             \n\
             lib b.go  (precise, at version v1.1.0)\n\
             3:Send() // 2\n\
             \n\
             Coverage: precise: 2 repositories · other versions: lib (v1.1.0) · search-based: none"
        );
    }
}
