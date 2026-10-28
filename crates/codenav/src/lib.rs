//! Code navigation: hover, go to definition and find usages.
//!
//! Three tiers answer a request, best first: precise (SCIP uploads, [`precise`]), syntactic
//! (tree-sitter scopes) and search-based. The first tier with data for the position
//! answers; every result carries its [`Provenance`]. Precise usages of a global symbol are
//! complemented with search-based ones from repositories without precise data
//! ([`coverage`]).
//!
//! Positions are 0-based lines and UTF-16 columns. Searches run as the viewer through the
//! stream API's pipeline, so repository permissions apply to every result.

pub mod adjust;
pub mod coverage;
pub mod highlight;
pub mod precise;
pub mod search_based;
pub mod syntactic;
pub mod token;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::StreamExt;
use sourcegull_authn::Actor;
use sourcegull_highlight::Occurrence;
use sourcegull_search::stream::Match;
use sourcegull_services::AppState;
use sourcegull_services::git::{GitCallError, GitReader};
use sourcegull_types::{Repo, RepoId};

pub use coverage::{Coverage, CoverageReason};
pub use sourcegull_codeintel_syntactic::{
    Locals, Occurrence as LocalOccurrence, VERSION as SYNTACTIC_VERSION, scip_symbol,
};

/// Bytes of a file read for navigation; larger files are cut.
pub const FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Definitions a search-based lookup returns at most.
pub const MAX_DEFINITIONS: usize = 50;
/// Reference matches a search-based lookup collects per phase (this repository, others).
pub const MAX_REFERENCES: usize = 1000;
/// Files whose references are checked against the highlighting (comments and strings are
/// dropped); references in further files are kept unchecked.
pub const MAX_FILTERED_FILES: usize = 300;
/// How long a usage list is kept for paging.
pub const USAGES_TTL: Duration = Duration::from_secs(30);

/// A 0-based position; `character` counts UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A range, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    /// A range within one line.
    pub fn on_line(line: u32, start: u32, end: u32) -> Self {
        Range {
            start: Position {
                line,
                character: start,
            },
            end: Position {
                line,
                character: end,
            },
        }
    }
}

/// Where a result comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provenance {
    Precise,
    Syntactic,
    SearchBased,
}

/// How a usage relates to the symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsageKind {
    Definition,
    Reference,
    Implementation,
    Super,
}

/// A range in a file at a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub repo_id: i32,
    pub repo: String,
    pub commit: String,
    pub path: String,
    pub range: Range,
}

/// A precise result found through another version of the symbol's package: the symbol is
/// at version `requested` where it was looked up and at `found` in the upload that answered
/// (no upload holds it at `requested`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrossVersion {
    pub requested: String,
    pub found: String,
}

/// One usage of a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub location: Location,
    pub kind: UsageKind,
    pub provenance: Provenance,
    /// The symbol: a SCIP symbol for precise and syntactic results, the identifier for
    /// search-based ones.
    pub symbol: String,
    /// The text of the usage's line.
    pub line: String,
    /// Fine-grained source, for debugging.
    pub data_source: &'static str,
    /// Set when a precise usage was linked across package versions.
    pub cross_version: Option<CrossVersion>,
}

/// The symbol under a position: its range, best definition and hover text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub token: String,
    pub range: Range,
    pub provenance: Provenance,
    pub definition: Option<Location>,
    /// Markdown.
    pub hover: Option<String>,
    /// Set when the precise definition was linked across package versions.
    pub cross_version: Option<CrossVersion>,
}

/// A file at a resolved commit.
#[derive(Debug, Clone)]
pub struct FileAt {
    pub repo: Repo,
    /// The full commit oid.
    pub commit: String,
    pub path: String,
}

/// Limits which repositories usages come from (a filter on `repository.name`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RepoFilter {
    /// Only this repository.
    pub only: Option<String>,
    /// Not this repository.
    pub exclude: Option<String>,
}

impl RepoFilter {
    fn allows(&self, name: &str) -> bool {
        self.only.as_deref().is_none_or(|o| o == name) && self.exclude.as_deref() != Some(name)
    }
}

/// A page of usages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsagePage {
    pub usages: Vec<Usage>,
    /// The offset of the next page, if there is one.
    pub next: Option<usize>,
    /// For precise usages of a global symbol: which repositories answered precisely and
    /// which search-based.
    pub coverage: Option<Coverage>,
}

#[derive(Debug, thiserror::Error)]
pub enum NavError {
    #[error(transparent)]
    Git(#[from] GitCallError),
    #[error("search failed: {0}")]
    Search(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<sourcegull_db::DbError> for NavError {
    fn from(e: sourcegull_db::DbError) -> Self {
        Self::Internal(e.to_string())
    }
}

/// A file's text with its highlighting.
struct Text {
    oid: String,
    text: Arc<str>,
    lang: Option<&'static str>,
    /// Sorted highlight occurrences (empty without a grammar).
    occ: Vec<Occurrence>,
    highlighted: bool,
}

impl Text {
    fn line(&self, n: u32) -> Option<&str> {
        token::line_of(&self.text, n)
    }

    fn occ(&self) -> Option<&[Occurrence]> {
        self.highlighted.then_some(self.occ.as_slice())
    }

    /// The identifier at `pos`, unless the highlighting says it is a keyword, literal or
    /// comment.
    fn symbol_at(&self, pos: Position) -> Option<token::Token> {
        let tok = token::token_at(self.line(pos.line)?, pos.character, self.lang)?;
        let inert = self.occ().is_some_and(|occ| {
            search_based::kind_at(occ, pos.line, tok.start).is_some_and(search_based::is_inert)
        });
        (!inert).then_some(tok)
    }

    async fn locals(&self) -> Option<Arc<sourcegull_codeintel_syntactic::Locals>> {
        syntactic::locals(&self.oid, self.lang, self.text.clone()).await
    }
}

/// (viewer, repo, commit, path, token, language, filter, phase)
type UsageKey = (
    Option<i64>,
    i32,
    String,
    String,
    String,
    Option<&'static str>,
    RepoFilter,
    bool,
);

/// (viewer, repo, commit, path, token, language)
type DefinitionsKey = (Option<i64>, i32, String, String, String, Option<String>);

static DEFINITIONS: LazyLock<moka::future::Cache<DefinitionsKey, Arc<Vec<Candidate>>>> =
    LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(4096)
            .time_to_live(USAGES_TTL)
            .build()
    });

static USAGES: LazyLock<moka::future::Cache<UsageKey, Arc<Vec<Usage>>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(512)
        .time_to_live(USAGES_TTL)
        .build()
});

/// Code navigation for one viewer.
pub struct CodeNav<'a> {
    state: &'a AppState,
    actor: Option<&'a Actor>,
}

/// A file's repository name, commit and usages.
type FileUsages = (String, String, Vec<Usage>);

/// A definition found by symbol search, before its line is read.
#[derive(Clone)]
struct Candidate {
    repo_id: i32,
    repo: String,
    commit: String,
    path: String,
    /// 0-based line and byte columns.
    line: u32,
    start_byte: u32,
    end_byte: u32,
}

impl<'a> CodeNav<'a> {
    pub fn new(state: &'a AppState, actor: Option<&'a Actor>) -> Self {
        Self { state, actor }
    }

    fn reader(&self, id: i32, name: &str) -> GitReader {
        GitReader::from_parts(self.state.repo_clients.clone(), RepoId(id), name)
    }

    async fn read(&self, id: i32, name: &str, commit: &str, path: &str) -> Result<Text, NavError> {
        let file = self
            .reader(id, name)
            .read_file(commit, path, FILE_MAX_BYTES)
            .await?;
        let file = Arc::new(file);
        let lang = highlight::language(path, &file.content);
        let h = highlight::highlight_cached(path, file.clone(), false)
            .await
            .map_err(|e| NavError::Internal(e.to_string()))?;
        Ok(Text {
            oid: file.oid.clone(),
            text: Arc::from(String::from_utf8_lossy(&file.content)),
            lang,
            highlighted: h.plain_reason.is_none(),
            occ: h.occurrences.clone(),
        })
    }

    /// Reads files (repo id, repo name, commit, path) a few at a time, in order.
    async fn read_all(
        &self,
        files: Vec<(i32, String, String, String)>,
    ) -> Vec<Result<Text, NavError>> {
        futures::stream::iter(files)
            .map(|(id, name, commit, path)| async move {
                self.read(id, &name, &commit, &path).await
            })
            .buffered(8)
            .collect()
            .await
    }

    /// The `repo:` scope for the file's own repository: without a revision when the file is
    /// at the default branch head, so the search index serves it.
    async fn local_scope(&self, file: &FileAt) -> String {
        let head = GitReader::new(self.state.repo_clients.clone(), &file.repo)
            .resolve_revision("HEAD")
            .await
            .ok();
        let rev = (head.as_deref() != Some(&file.commit)).then_some(file.commit.as_str());
        search_based::repo_filter(&file.repo.name, rev)
    }

    async fn search(&self, query: &str, max: usize) -> Result<Vec<Match>, NavError> {
        let out = sourcegull_search_web::collect(
            &self.state.search_backend(),
            self.actor,
            query,
            Some("regexp"),
            max,
        )
        .await;
        if let Some(e) = out.error {
            return Err(NavError::Search(e));
        }
        Ok(out.matches)
    }

    async fn find_definitions(
        &self,
        scope: &str,
        lang: Option<&str>,
        token: &str,
    ) -> Result<Vec<Candidate>, NavError> {
        let q = search_based::definitions_query(scope, lang, token, MAX_DEFINITIONS);
        let mut out = Vec::new();
        for m in self.search(&q, MAX_DEFINITIONS).await? {
            let Match::Symbol {
                path,
                repository_id,
                repository,
                commit,
                symbols,
                ..
            } = m
            else {
                continue;
            };
            for s in symbols.into_iter().filter(|s| s.name == token) {
                let r = s.range;
                out.push(Candidate {
                    repo_id: repository_id,
                    repo: repository.clone(),
                    commit: commit.clone(),
                    path: path.clone(),
                    line: r.start.line,
                    start_byte: r.start.column,
                    end_byte: if r.end.line == r.start.line {
                        r.end.column
                    } else {
                        r.start.column + u32::try_from(token.len()).unwrap_or(0)
                    },
                });
            }
        }
        Ok(out)
    }

    /// Search-based definitions of `token` used in `file`: in its repository, else in the
    /// other repositories the viewer can read; nearest paths first.
    async fn definitions(
        &self,
        file: &FileAt,
        token: &str,
        lang: Option<&str>,
    ) -> Result<Arc<Vec<Candidate>>, NavError> {
        let key: DefinitionsKey = (
            self.actor.map(|a| a.user_id.get()),
            file.repo.id.get(),
            file.commit.clone(),
            file.path.clone(),
            token.to_owned(),
            lang.map(str::to_owned),
        );
        DEFINITIONS
            .try_get_with(key, async {
                self.compute_definitions(file, token, lang)
                    .await
                    .map(Arc::new)
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => NavError::Internal(e.to_string()),
            })
    }

    async fn compute_definitions(
        &self,
        file: &FileAt,
        token: &str,
        lang: Option<&str>,
    ) -> Result<Vec<Candidate>, NavError> {
        let local = self.local_scope(file).await;
        let mut defs = self.find_definitions(&local, lang, token).await?;
        if defs.is_empty() {
            let others = search_based::exclude_repo_filter(&file.repo.name);
            defs = self.find_definitions(&others, lang, token).await?;
        }
        defs.sort_by(|a, b| {
            let same = |c: &Candidate| c.repo_id == file.repo.id.get();
            same(b)
                .cmp(&same(a))
                .then_with(|| {
                    let p = |c: &Candidate| {
                        if same(c) {
                            search_based::proximity(&file.path, &c.path)
                        } else {
                            0
                        }
                    };
                    p(b).cmp(&p(a))
                })
                .then_with(|| (&a.repo, &a.path, a.line).cmp(&(&b.repo, &b.path, b.line)))
        });
        Ok(defs)
    }

    /// The symbol at `pos` in `file` with its best definition and hover, or `None` when there
    /// is no identifier there (or it is a keyword, literal or comment).
    pub async fn symbol_info(
        &self,
        file: &FileAt,
        pos: Position,
    ) -> Result<Option<SymbolInfo>, NavError> {
        let text = self
            .read(
                file.repo.id.get(),
                &file.repo.name,
                &file.commit,
                &file.path,
            )
            .await?;
        let Some(tok) = text.symbol_at(pos) else {
            return Ok(None);
        };
        let range = Range::on_line(pos.line, tok.start, tok.end);
        if let Some(info) = self
            .precise_symbol_info(file, text.line(pos.line).unwrap_or(""), pos, &tok.text)
            .await?
        {
            return Ok(Some(info));
        }
        if let Some(locals) = text.locals().await
            && let Some(o) = locals.at(pos.line, pos.character)
        {
            let def = locals.of(o.symbol).find(|o| o.definition);
            let hover = def.map(|d| {
                let line = text.line(d.line).unwrap_or("");
                let docs = search_based::doc_comment(&text.text, d.line, text.lang, text.occ());
                search_based::hover_markdown(text.lang, line, &docs)
            });
            return Ok(Some(SymbolInfo {
                token: tok.text.clone(),
                range: Range::on_line(o.line, o.start, o.end),
                provenance: Provenance::Syntactic,
                definition: def.map(|d| Location {
                    repo_id: file.repo.id.get(),
                    repo: file.repo.name.clone(),
                    commit: file.commit.clone(),
                    path: file.path.clone(),
                    range: Range::on_line(d.line, d.start, d.end),
                }),
                hover,
                cross_version: None,
            }));
        }
        let defs = self.definitions(file, &tok.text, text.lang).await?;
        let mut info = SymbolInfo {
            token: tok.text.clone(),
            range,
            provenance: Provenance::SearchBased,
            definition: None,
            hover: None,
            cross_version: None,
        };
        let Some(def) = defs.first().cloned() else {
            return Ok(Some(info));
        };
        let same_file =
            def.repo_id == file.repo.id.get() && def.commit == file.commit && def.path == file.path;
        let def_text = if same_file {
            text
        } else {
            self.read(def.repo_id, &def.repo, &def.commit, &def.path)
                .await?
        };
        let line = def_text.line(def.line).unwrap_or("");
        let docs =
            search_based::doc_comment(&def_text.text, def.line, def_text.lang, def_text.occ());
        info.hover = Some(search_based::hover_markdown(def_text.lang, line, &docs));
        info.definition = Some(Location {
            range: Range::on_line(
                def.line,
                token::byte_to_utf16(line, def.start_byte as usize),
                token::byte_to_utf16(line, def.end_byte as usize),
            ),
            repo_id: def.repo_id,
            repo: def.repo,
            commit: def.commit,
            path: def.path,
        });
        Ok(Some(info))
    }

    /// The syntactic occurrences (locals) of `file`; `None` when its language has no scope
    /// query.
    pub async fn locals(
        &self,
        file: &FileAt,
    ) -> Result<Option<Arc<sourcegull_codeintel_syntactic::Locals>>, NavError> {
        let text = self
            .read(
                file.repo.id.get(),
                &file.repo.name,
                &file.commit,
                &file.path,
            )
            .await?;
        Ok(text.locals().await)
    }

    /// Usages of the symbol at `range` in `file`, `limit` from `offset`. Results come grouped
    /// by repository (the file's own first) and by file (the file itself first, then the
    /// nearest paths), each file's usages in order.
    pub async fn usages(
        &self,
        file: &FileAt,
        range: Range,
        filter: &RepoFilter,
        offset: usize,
        limit: usize,
    ) -> Result<UsagePage, NavError> {
        let text = self
            .read(
                file.repo.id.get(),
                &file.repo.name,
                &file.commit,
                &file.path,
            )
            .await?;
        let Some(tok) = text.symbol_at(range.start) else {
            return Ok(UsagePage {
                usages: Vec::new(),
                next: None,
                coverage: None,
            });
        };
        if let Some(hit) = self
            .precise_hit(file, text.line(range.start.line).unwrap_or(""), range.start)
            .await?
        {
            return self
                .precise_usages(file, &hit, filter, &tok.text, text.lang, offset, limit)
                .await;
        }
        if let Some(locals) = text.locals().await
            && let Some(o) = locals.at(range.start.line, range.start.character)
        {
            if !filter.allows(&file.repo.name) {
                return Ok(UsagePage {
                    usages: Vec::new(),
                    next: None,
                    coverage: None,
                });
            }
            let symbol = sourcegull_codeintel_syntactic::scip_symbol(o.symbol);
            let mut usages: Vec<Usage> = locals
                .of(o.symbol)
                .skip(offset)
                .take(limit + 1)
                .map(|u| Usage {
                    location: Location {
                        repo_id: file.repo.id.get(),
                        repo: file.repo.name.clone(),
                        commit: file.commit.clone(),
                        path: file.path.clone(),
                        range: Range::on_line(u.line, u.start, u.end),
                    },
                    kind: if u.definition {
                        UsageKind::Definition
                    } else {
                        UsageKind::Reference
                    },
                    provenance: Provenance::Syntactic,
                    symbol: symbol.clone(),
                    line: text.line(u.line).unwrap_or("").to_owned(),
                    data_source: "syntactic",
                    cross_version: None,
                })
                .collect();
            let next = (usages.len() > limit).then_some(offset + limit);
            usages.truncate(limit);
            return Ok(UsagePage {
                usages,
                next,
                coverage: None,
            });
        }
        let local = self
            .phase(file, &tok.text, text.lang, filter, false)
            .await?;
        let mut usages: Vec<Usage> = local.iter().skip(offset).take(limit + 1).cloned().collect();
        if usages.len() <= limit {
            let remote = self.phase(file, &tok.text, text.lang, filter, true).await?;
            let skip = offset.saturating_sub(local.len());
            usages.extend(
                remote
                    .iter()
                    .skip(skip)
                    .take(limit + 1 - usages.len())
                    .cloned(),
            );
        }
        let next = (usages.len() > limit).then_some(offset + limit);
        usages.truncate(limit);
        Ok(UsagePage {
            usages,
            next,
            coverage: None,
        })
    }

    /// The usages in the file's repository (`remote` false) or in the others, cached for
    /// [`USAGES_TTL`] so pages come from one list.
    async fn phase(
        &self,
        file: &FileAt,
        token: &str,
        lang: Option<&'static str>,
        filter: &RepoFilter,
        remote: bool,
    ) -> Result<Arc<Vec<Usage>>, NavError> {
        let key: UsageKey = (
            self.actor.map(|a| a.user_id.get()),
            file.repo.id.get(),
            file.commit.clone(),
            file.path.clone(),
            token.to_owned(),
            lang,
            filter.clone(),
            remote,
        );
        USAGES
            .try_get_with(key, async {
                self.compute_phase(file, token, lang, filter, remote)
                    .await
                    .map(Arc::new)
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => NavError::Internal(e.to_string()),
            })
    }

    async fn compute_phase(
        &self,
        file: &FileAt,
        token: &str,
        lang: Option<&'static str>,
        filter: &RepoFilter,
        remote: bool,
    ) -> Result<Vec<Usage>, NavError> {
        let name = file.repo.name.as_str();
        let scope = if !remote {
            if !filter.allows(name) {
                return Ok(Vec::new());
            }
            self.local_scope(file).await
        } else {
            match (&filter.only, &filter.exclude) {
                (Some(o), _) if o == name => return Ok(Vec::new()),
                (Some(o), Some(x)) if o == x => return Ok(Vec::new()),
                (Some(o), _) => search_based::repo_filter(o, None),
                (None, Some(x)) if x != name => format!(
                    "{} {}",
                    search_based::exclude_repo_filter(name),
                    search_based::exclude_repo_filter(x)
                ),
                (None, _) => search_based::exclude_repo_filter(name),
            }
        };
        self.search_usages(file, &scope, token, lang).await
    }

    /// Search-based usages of `token` in the repositories `scope` selects, grouped by
    /// repository (the file's own first) and by file.
    async fn search_usages(
        &self,
        file: &FileAt,
        scope: &str,
        token: &str,
        lang: Option<&'static str>,
    ) -> Result<Vec<Usage>, NavError> {
        let defs = self.find_definitions(scope, lang, token).await?;
        let def_at: HashSet<(i32, String, u32, u32)> = defs
            .iter()
            .map(|d| (d.repo_id, d.path.clone(), d.line, d.start_byte))
            .collect();
        let q = search_based::references_query(scope, lang, token, MAX_REFERENCES);
        let refs = self.search(&q, MAX_REFERENCES).await?;

        // (repo, path) → (commit, usages)
        let mut files: BTreeMap<(i32, String), FileUsages> = BTreeMap::new();
        let usage = |repo_id: i32,
                     repo: &str,
                     commit: &str,
                     path: &str,
                     line: &str,
                     n: u32,
                     s: u32,
                     e: u32,
                     kind| Usage {
            location: Location {
                repo_id,
                repo: repo.to_owned(),
                commit: commit.to_owned(),
                path: path.to_owned(),
                range: Range::on_line(
                    n,
                    token::byte_to_utf16(line, s as usize),
                    token::byte_to_utf16(line, e as usize),
                ),
            },
            kind,
            provenance: Provenance::SearchBased,
            symbol: token.to_owned(),
            line: line.to_owned(),
            data_source: "search-based",
            cross_version: None,
        };
        for m in refs {
            let Match::Content {
                path,
                repository_id,
                repository,
                commit,
                chunk_matches,
                ..
            } = m
            else {
                continue;
            };
            let entry = files
                .entry((repository_id, path.clone()))
                .or_insert_with(|| (repository.clone(), commit.clone(), Vec::new()));
            for c in chunk_matches {
                let lines: Vec<&str> = c.content.split('\n').collect();
                for r in c.ranges {
                    if r.start.line != r.end.line {
                        continue;
                    }
                    let i = (r.start.line - c.content_start.line) as usize;
                    let line = lines
                        .get(i)
                        .map_or("", |l| l.strip_suffix('\r').unwrap_or(l));
                    let kind = if def_at.contains(&(
                        repository_id,
                        path.clone(),
                        r.start.line,
                        r.start.column,
                    )) {
                        UsageKind::Definition
                    } else {
                        UsageKind::Reference
                    };
                    entry.2.push(usage(
                        repository_id,
                        &repository,
                        &commit,
                        &path,
                        line,
                        r.start.line,
                        r.start.column,
                        r.end.column,
                        kind,
                    ));
                }
            }
        }

        // Definitions the text search did not return (it stops at its limit).
        let missing: Vec<&Candidate> = defs
            .iter()
            .filter(|d| {
                !files.get(&(d.repo_id, d.path.clone())).is_some_and(|f| {
                    f.2.iter().any(|u| {
                        u.kind == UsageKind::Definition && u.location.range.start.line == d.line
                    })
                })
            })
            .collect();
        let texts = self
            .read_all(
                missing
                    .iter()
                    .map(|d| (d.repo_id, d.repo.clone(), d.commit.clone(), d.path.clone()))
                    .collect(),
            )
            .await;
        let reads = missing.into_iter().zip(texts);
        for (d, t) in reads {
            let Ok(t) = t else { continue };
            let line = t.line(d.line).unwrap_or("");
            files
                .entry((d.repo_id, d.path.clone()))
                .or_insert_with(|| (d.repo.clone(), d.commit.clone(), Vec::new()))
                .2
                .push(usage(
                    d.repo_id,
                    &d.repo,
                    &d.commit,
                    &d.path,
                    line,
                    d.line,
                    d.start_byte,
                    d.end_byte,
                    UsageKind::Definition,
                ));
        }

        // Drop references inside comments and strings, by each file's highlighting.
        let keys: Vec<(i32, String, String, String)> = files
            .iter()
            .take(MAX_FILTERED_FILES)
            .map(|((id, path), (repo, commit, _))| {
                (*id, repo.clone(), commit.clone(), path.clone())
            })
            .collect();
        let texts = self.read_all(keys.clone()).await;
        let checks = keys
            .into_iter()
            .map(|(id, _, _, path)| (id, path))
            .zip(texts);
        for (key, t) in checks {
            let Ok(t) = t else { continue };
            let Some(occ) = t.occ() else { continue };
            if let Some(f) = files.get_mut(&key) {
                f.2.retain(|u| {
                    let r = u.location.range;
                    u.kind == UsageKind::Definition
                        || !search_based::in_comment_or_string(
                            occ,
                            r.start.line,
                            r.start.character,
                            r.end.character,
                        )
                });
            }
        }

        let mut groups: Vec<((i32, String), FileUsages)> =
            files.into_iter().filter(|(_, f)| !f.2.is_empty()).collect();
        let own = file.repo.id.get();
        groups.sort_by(|((ra, pa), (na, _, _)), ((rb, pb), (nb, _, _))| {
            (*rb == own)
                .cmp(&(*ra == own))
                .then_with(|| na.cmp(nb))
                .then_with(|| {
                    let p = |r: i32, p: &str| {
                        if r == own {
                            search_based::proximity(&file.path, p)
                        } else {
                            0
                        }
                    };
                    p(*rb, pb).cmp(&p(*ra, pa))
                })
                .then_with(|| pa.cmp(pb))
        });
        let mut out = Vec::new();
        for (_, (_, _, mut usages)) in groups {
            usages.sort_by_key(|u| (u.location.range, u.kind));
            usages.dedup_by(|a, b| a.location.range == b.location.range);
            out.extend(usages);
        }
        Ok(out)
    }
}
