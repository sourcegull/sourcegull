//! Precise tier: hover, definitions and usages from SCIP uploads.
//!
//! A file at a commit is served by the uploads nearest that commit ([`visibility`]): per root
//! and indexer, the completed upload at the commit or at the closest ancestor (up to
//! [`MAX_DISTANCE`] commits back), else at the closest descendant among recent uploads.
//! When the upload's commit differs from the viewed one, lines map through the file's diff
//! ([`crate::adjust`]); a position on a changed line has no precise data.
//!
//! Global symbols reach other uploads through Postgres: references through
//! `codeintel_symbol_postings` (in other repositories, only uploads visible from the default
//! branch head count), definitions through the package monikers an upload exports. Both
//! are limited to the repositories the viewer can read.
//!
//! A global symbol's package version is part of the symbol, so a library indexed at one
//! version and its users at others share no symbol. Navigation links them across versions:
//! when no upload defines the symbol at its own version, the definition comes from an upload
//! exporting the same package (scheme, manager, name) at another version, with the symbol's
//! version replaced (the upload serving its repository's default branch first, else the
//! newest); references add the uploads holding the symbol at other versions of the package.
//! Exact matches come first; at most [`MAX_OTHER_VERSIONS`] other versions are looked at.
//! Such results carry a [`CrossVersion`].

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use futures::StreamExt;
use sourcegull_codeintel_scip::CodeGraph;
use sourcegull_codeintel_scip::graph::{self, posting};
use sourcegull_codeintel_scip::proto::encoding;
use sourcegull_codeintel_scip::store::{CodeIntelStore, PackageMoniker, Upload};
use sourcegull_codeintel_scip::symbol::{is_local, package, symbol_hash, with_version};
use sourcegull_services::git::GitReader;
use sourcegull_types::{Repo, RepoAccess, RepoId};

use crate::adjust::LineMap;
use crate::{
    CodeNav, CrossVersion, FILE_MAX_BYTES, FileAt, Location, NavError, Position, Provenance, Range,
    RepoFilter, SymbolInfo, USAGES_TTL, Usage, UsageKind, UsagePage, search_based, token,
};

pub use sourcegull_codeintel_scip::visibility::{self, MAX_DISTANCE, Visible};

/// Uploads of other repositories looked at for references to a global symbol.
pub const MAX_REMOTE_UPLOADS: i64 = 200;
/// Other repositories whose uploads are searched for references.
pub const MAX_REMOTE_REPOS: usize = 25;
/// Usages a precise lookup collects at most.
pub const MAX_PRECISE_USAGES: usize = 10_000;
/// Other versions of a symbol's package looked at, the most recently uploaded first.
pub const MAX_OTHER_VERSIONS: i64 = 20;
/// Uploads per other version looked at for references.
pub const MAX_CROSS_VERSION_UPLOADS: i64 = 50;
/// Uploads per other version looked at for a definition.
const MAX_CROSS_VERSION_EXPORTERS: i64 = 3;

/// An upload other than the hit's to look for usages in: the symbol as that upload holds
/// it, and how its version differs.
type Remote = (Upload, Arc<str>, Option<Arc<CrossVersion>>);
/// A repository the viewer can read, with the uploads serving the commit it is shown at.
type Serving = Option<(Repo, HashSet<i64>)>;

type Cache<K, V> = moka::future::Cache<K, V>;
type Commits = Arc<Vec<String>>;

/// Commit → its first [`MAX_DISTANCE`] ancestors (itself first). History never changes, so
/// entries only age out.
static ANCESTORS: LazyLock<Cache<(i32, String), Commits>> =
    LazyLock::new(|| moka::future::Cache::builder().max_capacity(1024).build());

/// (repo, old commit, new commit, path)
type LineMapKey = (i32, String, String, String);

static LINE_MAPS: LazyLock<Cache<LineMapKey, Arc<LineMap>>> =
    LazyLock::new(|| moka::future::Cache::builder().max_capacity(16_384).build());

/// (viewer, repo, commit, upload, symbol, filter) → unresolved usages.
type RawKey = (Option<i64>, i32, String, i64, String, RepoFilter);

static RAW_USAGES: LazyLock<Cache<RawKey, Arc<Vec<Raw>>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(256)
        .time_to_live(USAGES_TTL)
        .build()
});

/// Search-based additions to precise usages, per [`RawKey`].
static COMPLEMENTS: LazyLock<Cache<RawKey, Arc<crate::coverage::Complement>>> =
    LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(256)
            .time_to_live(USAGES_TTL)
            .build()
    });

/// A repository's history as [`CodeNav::ancestors`] reads it.
struct NavHistory<'a, 'n> {
    nav: &'a CodeNav<'n>,
    repo: RepoId,
    name: &'a str,
}

#[async_trait::async_trait]
impl visibility::History for NavHistory<'_, '_> {
    type Error = NavError;

    async fn ancestors(&self, commit: &str) -> Result<Arc<Vec<String>>, NavError> {
        self.nav.ancestors(self.repo, self.name, commit).await
    }
}

/// The occurrence under a position in an upload.
#[derive(Clone)]
pub(crate) struct Hit {
    pub upload: Arc<Upload>,
    pub graph: Arc<CodeGraph>,
    pub symbol: String,
    /// The occurrence's range in the viewed file.
    pub range: Range,
    pub language: String,
    pub override_docs: Vec<String>,
}

/// A usage in an upload, before its position is adjusted and its line read.
#[derive(Debug, Clone)]
pub(crate) struct Raw {
    pub(crate) upload: Arc<Upload>,
    pub(crate) repo: Arc<str>,
    path: String,
    range: graph::Range,
    encoding: u32,
    kind: UsageKind,
    symbol: Arc<str>,
    /// Set when the upload holds the symbol at another package version.
    pub(crate) cross_version: Option<Arc<CrossVersion>>,
}

/// Marks usages as found through another package version.
fn mark(raw: &mut [Raw], cross: &Arc<CrossVersion>) {
    for r in raw {
        r.cross_version = Some(cross.clone());
    }
}

/// An occurrence of a precise `codeGraphData` document, in the viewed file.
#[derive(Debug, Clone)]
pub struct PreciseOccurrence {
    pub symbol: String,
    pub range: Range,
    /// `SymbolRole` bits.
    pub roles: u32,
}

/// A file's precise occurrences from one upload.
#[derive(Debug, Clone)]
pub struct PreciseDocument {
    pub upload: Upload,
    pub occurrences: Vec<PreciseOccurrence>,
}

/// A column in the document's encoding from a UTF-16 one.
fn to_doc_col(line: &str, col: u32, enc: u32) -> u32 {
    let byte = || token::utf16_to_byte(line, col).unwrap_or(line.len());
    match i32::try_from(enc).unwrap_or(0) {
        encoding::UTF8 => u32::try_from(byte()).unwrap_or(u32::MAX),
        encoding::UTF32 => u32::try_from(line[..byte()].chars().count()).unwrap_or(u32::MAX),
        _ => col,
    }
}

/// A UTF-16 column from one in the document's encoding.
fn from_doc_col(line: &str, col: u32, enc: u32) -> u32 {
    match i32::try_from(enc).unwrap_or(0) {
        encoding::UTF8 => token::byte_to_utf16(line, col as usize),
        encoding::UTF32 => {
            let byte = line
                .char_indices()
                .nth(col as usize)
                .map_or(line.len(), |(i, _)| i);
            token::byte_to_utf16(line, byte)
        }
        _ => col,
    }
}

/// Hover Markdown from SCIP documentation: the signature in a code block, then the docs.
fn hover(language: &str, signature: Option<&str>, docs: &[String]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(sig) = signature.filter(|s| !s.trim().is_empty()) {
        let fence = if sig.contains("```") { "````" } else { "```" };
        parts.push(format!(
            "{fence}{}\n{}\n{fence}",
            language.to_ascii_lowercase(),
            sig.trim_end()
        ));
    }
    parts.extend(docs.iter().filter(|d| !d.trim().is_empty()).cloned());
    (!parts.is_empty()).then(|| parts.join("\n\n---\n\n"))
}

impl CodeNav<'_> {
    fn graph_cache(&self) -> Option<&sourcegull_codeintel_scip::GraphCache> {
        self.state.graphs.as_deref()
    }

    pub(crate) async fn access(&self) -> Result<RepoAccess, NavError> {
        self.state
            .authz
            .access(self.actor)
            .await
            .map_err(|e| NavError::Internal(e.to_string()))
    }

    /// The first [`MAX_DISTANCE`] commits of `commit`'s history, itself first.
    async fn ancestors(
        &self,
        repo: RepoId,
        name: &str,
        commit: &str,
    ) -> Result<Arc<Vec<String>>, NavError> {
        let git = self.reader(repo.get(), name);
        ANCESTORS
            .try_get_with((repo.get(), commit.to_owned()), async move {
                let (commits, _) = git.commit_log(commit, "", 0, MAX_DISTANCE).await?;
                Ok::<_, NavError>(Arc::new(commits.into_iter().map(|c| c.oid).collect()))
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => NavError::Internal(e.to_string()),
            })
    }

    /// The uploads serving `commit` of `repo` ([`visibility::nearest_uploads`]).
    pub async fn nearest_uploads(
        &self,
        repo: &Repo,
        commit: &str,
    ) -> Result<Vec<Visible>, NavError> {
        let history = NavHistory {
            nav: self,
            repo: repo.id,
            name: &repo.name,
        };
        visibility::nearest_uploads(
            &CodeIntelStore::new(&self.state.db),
            repo.id,
            commit,
            &history,
        )
        .await
    }

    /// Whether the upload serves its repository's default branch head (`isLatestForRepo`).
    pub async fn is_latest_for_repo(&self, repo: &Repo, upload: i64) -> Result<bool, NavError> {
        let head = GitReader::new(self.state.repo_clients.clone(), repo)
            .resolve_revision("HEAD")
            .await?;
        Ok(self
            .nearest_uploads(repo, &head)
            .await?
            .iter()
            .any(|v| v.upload.id == upload))
    }

    /// Maps lines of `path` between `old` and `new`.
    async fn line_map(
        &self,
        repo: i32,
        name: &str,
        old: &str,
        new: &str,
        path: &str,
    ) -> Result<Arc<LineMap>, NavError> {
        if old == new {
            return Ok(Arc::new(LineMap::identity()));
        }
        let git = self.reader(repo, name);
        let key = (repo, old.to_owned(), new.to_owned(), path.to_owned());
        LINE_MAPS
            .try_get_with(key, async move {
                let files = git.diff(old, new, vec![path.to_owned()], false).await?;
                let Some(f) = files
                    .iter()
                    .find(|f| f.new_path == path || f.old_path == path)
                else {
                    return Ok::<_, NavError>(Arc::new(LineMap::identity()));
                };
                if f.binary || f.too_large || f.old_path != f.new_path {
                    return Ok(Arc::new(LineMap::absent()));
                }
                Ok(Arc::new(LineMap::from_hunks(f.hunks.iter().map(|h| {
                    (
                        h.old_start,
                        h.old_lines,
                        h.new_start,
                        h.new_lines,
                        h.body.as_str(),
                    )
                }))))
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => NavError::Internal(e.to_string()),
            })
    }

    /// The upload's code graph; `None` (logged) when it cannot be read.
    async fn graph(&self, upload: &Upload) -> Option<Arc<CodeGraph>> {
        let cache = self.graph_cache()?;
        match cache.get(upload).await {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(upload = upload.id, error = %e, "reading a code graph");
                None
            }
        }
    }

    /// The precise occurrence at `pos` (UTF-16) of `file`, whose line `pos.line` reads
    /// `line`: from the nearest upload whose document has one.
    pub(crate) async fn precise_hit(
        &self,
        file: &FileAt,
        line: &str,
        pos: Position,
    ) -> Result<Option<Hit>, NavError> {
        if self.graph_cache().is_none() {
            return Ok(None);
        }
        for v in self.nearest_uploads(&file.repo, &file.commit).await? {
            let u = v.upload;
            if !file.path.starts_with(&u.root) {
                continue;
            }
            let map = self
                .line_map(
                    file.repo.id.get(),
                    &file.repo.name,
                    &u.commit,
                    &file.commit,
                    &file.path,
                )
                .await?;
            let Some(old_line) = map.new_to_old(pos.line) else {
                continue;
            };
            let Some(graph) = self.graph(&u).await else {
                continue;
            };
            let Some(doc) = graph
                .document(&file.path)
                .map_err(|e| NavError::Internal(e.to_string()))?
            else {
                continue;
            };
            let enc = doc.position_encoding;
            let col = to_doc_col(line, pos.character, enc);
            let mut hits: Vec<&graph::Occurrence> = doc
                .occurrences
                .iter()
                .take_while(|o| o.range.start_line <= old_line)
                .filter(|o| o.symbol.is_some() && o.range.contains(old_line, col))
                .collect();
            // Innermost first.
            hits.sort_by_key(|o| {
                let r = o.range;
                (
                    std::cmp::Reverse((r.start_line, r.start_character)),
                    (r.end_line, r.end_character),
                )
            });
            let Some(o) = hits.first() else { continue };
            let Some(sym) = o.symbol else { continue };
            let symbol = graph
                .symbol_name(sym)
                .map_err(|e| NavError::Internal(e.to_string()))?
                .to_owned();
            let r = o.range;
            let (start, end) = if r.start_line == r.end_line {
                (
                    from_doc_col(line, r.start_character, enc),
                    from_doc_col(line, r.end_character, enc),
                )
            } else {
                // Report a multi-line occurrence as its part on the hovered line.
                let start = if r.start_line == old_line {
                    from_doc_col(line, r.start_character, enc)
                } else {
                    0
                };
                (start, token::byte_to_utf16(line, line.len()))
            };
            return Ok(Some(Hit {
                upload: Arc::new(u),
                graph,
                symbol,
                range: Range::on_line(pos.line, start, end),
                language: doc.language.clone(),
                override_docs: o.override_documentation.clone(),
            }));
        }
        Ok(None)
    }

    /// The occurrences of `symbol` in `graph` (an upload of repository `repo`): definitions,
    /// and with `all` references, implementations and the symbols it implements.
    fn occurrences_in(
        graph: &CodeGraph,
        upload: &Arc<Upload>,
        repo: &Arc<str>,
        symbol: &str,
        all: bool,
        out: &mut Vec<Raw>,
    ) -> Result<(), NavError> {
        let err = |e: graph::GraphError| NavError::Internal(e.to_string());
        let Some(sym) = graph.symbol(symbol).map_err(err)? else {
            return Ok(());
        };
        let sym_name: Arc<str> = Arc::from(symbol);
        let mut wanted: Vec<(u32, UsageKind, bool)> = vec![(sym.id, UsageKind::Reference, true)];
        if all {
            wanted.extend(
                sym.implementations
                    .iter()
                    .map(|&s| (s, UsageKind::Implementation, false)),
            );
            if let Some(info) = &sym.info {
                wanted.extend(
                    info.relationships
                        .iter()
                        .filter(|r| r.flags & graph::relation::IMPLEMENTATION != 0)
                        .map(|r| (r.symbol, UsageKind::Super, false)),
                );
            }
        }
        for (id, kind, own) in wanted {
            let s = if id == sym.id {
                sym.clone()
            } else {
                graph.symbol_by_id(id).map_err(err)?
            };
            for p in &s.postings {
                let defs_only = !all || !own;
                if defs_only && p.flags & posting::DEFINITION == 0 {
                    continue;
                }
                let doc = graph.document_by_id(p.document).map_err(err)?;
                for o in doc.occurrences.iter().filter(|o| o.symbol == Some(id)) {
                    let def = o.is_definition();
                    if defs_only && !def {
                        continue;
                    }
                    out.push(Raw {
                        upload: upload.clone(),
                        repo: repo.clone(),
                        path: doc.path.clone(),
                        range: o.range,
                        encoding: doc.position_encoding,
                        kind: if !own {
                            kind
                        } else if def {
                            UsageKind::Definition
                        } else {
                            UsageKind::Reference
                        },
                        symbol: sym_name.clone(),
                        cross_version: None,
                    });
                    if out.len() >= MAX_PRECISE_USAGES {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Definitions of the hit's symbol: in its upload, else in an upload exporting the
    /// symbol's package (or any upload defining it), else in one exporting another version of
    /// the package.
    async fn precise_definitions(&self, file: &FileAt, hit: &Hit) -> Result<Vec<Raw>, NavError> {
        let repo: Arc<str> = Arc::from(file.repo.name.as_str());
        let mut out = Vec::new();
        Self::occurrences_in(&hit.graph, &hit.upload, &repo, &hit.symbol, false, &mut out)?;
        if !out.is_empty() || is_local(&hit.symbol) {
            return Ok(out);
        }
        let access = self.access().await?;
        let hash = symbol_hash(&hit.symbol);
        let store = CodeIntelStore::new(&self.state.db);
        let db = |e: sourcegull_db::DbError| NavError::Internal(e.to_string());
        let mut uploads = match package(&hit.symbol) {
            Some(p) => store
                .uploads_exporting(
                    &PackageMoniker {
                        kind: "export",
                        scheme: p.scheme,
                        manager: p.manager,
                        name: p.name,
                        version: p.version,
                    },
                    hash,
                    &access,
                    10,
                )
                .await
                .map_err(db)?,
            None => Vec::new(),
        };
        if uploads.is_empty() {
            uploads = store
                .uploads_with_symbol(hash, i16::from(posting::DEFINITION), &access, 10)
                .await
                .map_err(db)?;
        }
        for u in uploads {
            let Some(r) = self
                .state
                .db
                .repos()
                .get_authorized(u.repo_id, &access)
                .await
                .map_err(db)?
            else {
                continue;
            };
            let Some(graph) = self.graph(&u).await else {
                continue;
            };
            let name: Arc<str> = Arc::from(r.name.as_str());
            Self::occurrences_in(&graph, &Arc::new(u), &name, &hit.symbol, false, &mut out)?;
            if !out.is_empty() {
                break;
            }
        }
        if out.is_empty() {
            out = self
                .cross_version_definitions(file, &hit.symbol, &access)
                .await?;
        }
        Ok(out)
    }

    /// Definitions of `symbol` in an upload exporting its package at another version, with
    /// the symbol's version replaced: uploads serving their repository's default branch head
    /// first, then the newest.
    async fn cross_version_definitions(
        &self,
        file: &FileAt,
        symbol: &str,
        access: &RepoAccess,
    ) -> Result<Vec<Raw>, NavError> {
        let mut out = Vec::new();
        let Some(p) = package(symbol) else {
            return Ok(out);
        };
        let store = CodeIntelStore::new(&self.state.db);
        let versions = store
            .package_versions(
                &p.scheme,
                &p.manager,
                &p.name,
                &p.version,
                true,
                access,
                MAX_OTHER_VERSIONS,
            )
            .await?;
        let mut candidates: Vec<(Upload, Arc<str>, Arc<CrossVersion>)> = Vec::new();
        for v in versions {
            let Some(s) = with_version(symbol, &v) else {
                continue;
            };
            let moniker = PackageMoniker {
                kind: "export",
                scheme: p.scheme.clone(),
                manager: p.manager.clone(),
                name: p.name.clone(),
                version: v.clone(),
            };
            let uploads = store
                .uploads_exporting(
                    &moniker,
                    symbol_hash(&s),
                    access,
                    MAX_CROSS_VERSION_EXPORTERS,
                )
                .await?;
            let s: Arc<str> = Arc::from(s);
            let cross = Arc::new(CrossVersion {
                requested: p.version.clone(),
                found: v,
            });
            for u in uploads {
                if !candidates.iter().any(|c| c.0.id == u.id) {
                    candidates.push((u, s.clone(), cross.clone()));
                }
            }
        }
        let mut servings: HashMap<i32, Serving> = HashMap::new();
        let mut ranked = Vec::with_capacity(candidates.len());
        for (u, s, cross) in candidates {
            let repo_id = u.repo_id.get();
            if let Entry::Vacant(e) = servings.entry(repo_id) {
                e.insert(
                    self.serving(file, repo_id, access, &RepoFilter::default())
                        .await?,
                );
            }
            let Some(Some((repo, visible))) = servings.get(&repo_id) else {
                continue;
            };
            let name: Arc<str> = Arc::from(repo.name.as_str());
            ranked.push((visible.contains(&u.id), name, u, s, cross));
        }
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.2.uploaded_at.cmp(&a.2.uploaded_at))
                .then_with(|| b.2.id.cmp(&a.2.id))
        });
        for (_, name, u, s, cross) in ranked {
            let Some(graph) = self.graph(&u).await else {
                continue;
            };
            Self::occurrences_in(&graph, &Arc::new(u), &name, &s, false, &mut out)?;
            if !out.is_empty() {
                mark(&mut out, &cross);
                break;
            }
        }
        Ok(out)
    }

    /// Repository `repo_id` if the viewer can read it and `filter` allows it, with the uploads
    /// serving the viewed commit (`file`'s repository) or its default branch head (others).
    /// `None` also when those cannot be found (logged).
    async fn serving(
        &self,
        file: &FileAt,
        repo_id: i32,
        access: &RepoAccess,
        filter: &RepoFilter,
    ) -> Result<Serving, NavError> {
        let (repo, commit) = if repo_id == file.repo.id.get() {
            if !filter.allows(&file.repo.name) {
                return Ok(None);
            }
            (file.repo.clone(), file.commit.clone())
        } else {
            let Some(r) = self
                .state
                .db
                .repos()
                .get_authorized(RepoId(repo_id), access)
                .await?
            else {
                return Ok(None);
            };
            if !filter.allows(&r.name) {
                return Ok(None);
            }
            let head = GitReader::new(self.state.repo_clients.clone(), &r)
                .resolve_revision("HEAD")
                .await;
            match head {
                Ok(h) => (r, h),
                Err(e) => {
                    tracing::debug!(repo = r.name, error = %e, "resolving HEAD");
                    return Ok(None);
                }
            }
        };
        match self.nearest_uploads(&repo, &commit).await {
            Ok(v) => Ok(Some((repo, v.into_iter().map(|v| v.upload.id).collect()))),
            Err(e) => {
                tracing::debug!(repo = repo.name, error = %e, "finding visible uploads");
                Ok(None)
            }
        }
    }

    /// Positions `raw` usages in the files they are shown at: uploads of the viewed
    /// repository at the viewed commit (dropping usages on changed lines), others at their
    /// own commit. Reads each file once for the usage lines.
    async fn resolve(&self, file: &FileAt, raw: &[Raw]) -> Result<Vec<Usage>, NavError> {
        let own = file.repo.id.get();
        // (upload, path) → indexes into `raw`, in order.
        let mut groups: Vec<((i64, String), Vec<usize>)> = Vec::new();
        let mut index: HashMap<(i64, String), usize> = HashMap::new();
        for (i, r) in raw.iter().enumerate() {
            let key = (r.upload.id, r.path.clone());
            let g = *index.entry(key.clone()).or_insert_with(|| {
                groups.push((key, Vec::new()));
                groups.len() - 1
            });
            groups[g].1.push(i);
        }
        let jobs = groups.into_iter().map(|(_, idx)| {
            let first = &raw[idx[0]];
            let repo_id = first.upload.repo_id.get();
            let shown = if repo_id == own {
                file.commit.clone()
            } else {
                first.upload.commit.clone()
            };
            async move {
                let map = self
                    .line_map(
                        repo_id,
                        &first.repo,
                        &first.upload.commit,
                        &shown,
                        &first.path,
                    )
                    .await?;
                let text = self
                    .reader(repo_id, &first.repo)
                    .read_file(&shown, &first.path, FILE_MAX_BYTES)
                    .await
                    .map(|f| String::from_utf8_lossy(&f.content).into_owned())
                    .unwrap_or_default();
                let mut out = Vec::with_capacity(idx.len());
                for i in idx {
                    let r = &raw[i];
                    let Some(start_line) = map.old_to_new(r.range.start_line) else {
                        continue;
                    };
                    let end_line = map
                        .old_to_new(r.range.end_line)
                        .filter(|&l| l >= start_line)
                        .unwrap_or(start_line);
                    let line = token::line_of(&text, start_line).unwrap_or("");
                    let end_text = token::line_of(&text, end_line).unwrap_or("");
                    let start = from_doc_col(line, r.range.start_character, r.encoding);
                    let end = if end_line == start_line && r.range.end_line != r.range.start_line {
                        token::byte_to_utf16(line, line.len())
                    } else {
                        from_doc_col(end_text, r.range.end_character, r.encoding)
                    };
                    out.push(Usage {
                        location: Location {
                            repo_id,
                            repo: r.repo.to_string(),
                            commit: shown.clone(),
                            path: r.path.clone(),
                            range: Range {
                                start: Position {
                                    line: start_line,
                                    character: start,
                                },
                                end: Position {
                                    line: end_line,
                                    character: end,
                                },
                            },
                        },
                        kind: r.kind,
                        provenance: Provenance::Precise,
                        symbol: r.symbol.to_string(),
                        line: line.to_owned(),
                        data_source: "scip",
                        cross_version: r.cross_version.as_deref().cloned(),
                    });
                }
                Ok::<_, NavError>(out)
            }
        });
        let mut out = Vec::new();
        let results: Vec<Result<Vec<Usage>, NavError>> =
            futures::stream::iter(jobs).buffered(8).collect().await;
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }

    /// Hover and definition of the precise symbol at `pos`.
    pub(crate) async fn precise_symbol_info(
        &self,
        file: &FileAt,
        line: &str,
        pos: Position,
        token: &str,
    ) -> Result<Option<SymbolInfo>, NavError> {
        let Some(hit) = self.precise_hit(file, line, pos).await? else {
            return Ok(None);
        };
        let defs = self.precise_definitions(file, &hit).await?;
        // The hover: the occurrence's override, else the symbol's documentation in its upload
        // or in the defining one.
        let mut docs = hit.override_docs.clone();
        let mut signature = None;
        if docs.is_empty() {
            let mut graphs = vec![(hit.upload.id, hit.graph.clone(), hit.symbol.as_str())];
            for d in &defs {
                if !graphs.iter().any(|(id, ..)| *id == d.upload.id)
                    && let Some(g) = self.graph(&d.upload).await
                {
                    graphs.push((d.upload.id, g, &d.symbol));
                }
            }
            for (_, g, symbol) in graphs {
                let info = g
                    .symbol(symbol)
                    .map_err(|e| NavError::Internal(e.to_string()))?
                    .and_then(|s| s.info);
                if let Some(info) = info
                    && (!info.documentation.is_empty() || info.signature.is_some())
                {
                    docs = info.documentation;
                    signature = info.signature;
                    break;
                }
            }
        }
        let definition = match defs.first() {
            Some(d) => self
                .resolve(file, std::slice::from_ref(d))
                .await?
                .into_iter()
                .next()
                .map(|u| u.location),
            None => None,
        };
        Ok(Some(SymbolInfo {
            token: token.to_owned(),
            range: hit.range,
            provenance: Provenance::Precise,
            definition,
            hover: hover(&hit.language, signature.as_deref(), &docs),
            cross_version: defs
                .first()
                .and_then(|d| d.cross_version.as_deref().cloned()),
        }))
    }

    /// All usages of the hit's symbol, unresolved, in display order: the viewed repository
    /// first (the viewed file, then nearest paths), then other repositories by name.
    async fn precise_raw(
        &self,
        file: &FileAt,
        hit: &Hit,
        filter: &RepoFilter,
    ) -> Result<Vec<Raw>, NavError> {
        let own_name: Arc<str> = Arc::from(file.repo.name.as_str());
        let mut raw = Vec::new();
        if filter.allows(&file.repo.name) {
            Self::occurrences_in(
                &hit.graph,
                &hit.upload,
                &own_name,
                &hit.symbol,
                true,
                &mut raw,
            )?;
        }
        let local = is_local(&hit.symbol);
        if local {
            raw.retain(|r| r.path == file.path);
        } else {
            raw.extend(self.precise_remote(file, hit, filter).await?);
            // Definitions found only through monikers.
            if !raw.iter().any(|r| r.kind == UsageKind::Definition) {
                raw.extend(
                    self.precise_definitions(file, hit)
                        .await?
                        .into_iter()
                        .filter(|d| filter.allows(&d.repo)),
                );
            }
        }
        let own = file.repo.id.get();
        let mut seen = HashSet::new();
        raw.retain(|r| {
            seen.insert((
                r.upload.repo_id,
                r.upload.commit.clone(),
                r.path.clone(),
                r.range,
            ))
        });
        raw.sort_by(|a, b| {
            let is_own = |r: &Raw| r.upload.repo_id.get() == own;
            let prox = |r: &Raw| {
                if !is_own(r) {
                    0
                } else if r.path == file.path {
                    usize::MAX
                } else {
                    search_based::proximity(&file.path, &r.path)
                }
            };
            is_own(b)
                .cmp(&is_own(a))
                .then_with(|| a.repo.cmp(&b.repo))
                .then_with(|| prox(b).cmp(&prox(a)))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.range.cmp(&b.range))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        raw.truncate(MAX_PRECISE_USAGES);
        Ok(raw)
    }

    /// Usages of a global symbol in other uploads: other uploads serving the viewed commit,
    /// and in other repositories the uploads serving their default branch head. Then the
    /// same at other versions of the symbol's package.
    async fn precise_remote(
        &self,
        file: &FileAt,
        hit: &Hit,
        filter: &RepoFilter,
    ) -> Result<Vec<Raw>, NavError> {
        let access = self.access().await?;
        let store = CodeIntelStore::new(&self.state.db);
        let flags = i16::from(posting::DEFINITION | posting::REFERENCE | posting::IMPLEMENTATION);
        let exact = store
            .uploads_with_symbol(symbol_hash(&hit.symbol), flags, &access, MAX_REMOTE_UPLOADS)
            .await?;
        let mut seen: HashSet<i64> = exact.iter().map(|u| u.id).collect();
        seen.insert(hit.upload.id);
        let symbol: Arc<str> = Arc::from(hit.symbol.as_str());
        let mut by_repo: BTreeMap<i32, Vec<Remote>> = BTreeMap::new();
        for u in exact.into_iter().filter(|u| u.id != hit.upload.id) {
            by_repo
                .entry(u.repo_id.get())
                .or_default()
                .push((u, symbol.clone(), None));
        }
        let mut servings: HashMap<i32, Serving> = HashMap::new();
        let mut out = Vec::new();
        self.remote_usages(file, filter, &access, by_repo, &mut servings, &mut out)
            .await?;

        let Some(p) = package(&hit.symbol) else {
            return Ok(out);
        };
        if out.len() >= MAX_PRECISE_USAGES {
            return Ok(out);
        }
        let versions = store
            .package_versions(
                &p.scheme,
                &p.manager,
                &p.name,
                &p.version,
                false,
                &access,
                MAX_OTHER_VERSIONS,
            )
            .await?;
        let lookups = versions.into_iter().filter_map(|v| {
            let s = with_version(&hit.symbol, &v)?;
            let (store, access) = (&store, &access);
            Some(async move {
                let uploads = store
                    .uploads_with_symbol(symbol_hash(&s), flags, access, MAX_CROSS_VERSION_UPLOADS)
                    .await;
                (v, s, uploads)
            })
        });
        let found: Vec<_> = futures::stream::iter(lookups).buffered(4).collect().await;
        let mut by_repo: BTreeMap<i32, Vec<Remote>> = BTreeMap::new();
        for (v, s, uploads) in found {
            let s: Arc<str> = Arc::from(s);
            let cross = Arc::new(CrossVersion {
                requested: p.version.clone(),
                found: v,
            });
            for u in uploads? {
                if seen.insert(u.id) {
                    by_repo.entry(u.repo_id.get()).or_default().push((
                        u,
                        s.clone(),
                        Some(cross.clone()),
                    ));
                }
            }
        }
        self.remote_usages(file, filter, &access, by_repo, &mut servings, &mut out)
            .await?;
        Ok(out)
    }

    /// Usages in the uploads of `by_repo` (at most [`MAX_REMOTE_REPOS`] repositories) that
    /// serve the commit their repository is shown at ([`Self::serving`], kept in `servings`).
    async fn remote_usages(
        &self,
        file: &FileAt,
        filter: &RepoFilter,
        access: &RepoAccess,
        by_repo: BTreeMap<i32, Vec<Remote>>,
        servings: &mut HashMap<i32, Serving>,
        out: &mut Vec<Raw>,
    ) -> Result<(), NavError> {
        for (n, (repo_id, uploads)) in by_repo.into_iter().enumerate() {
            if n >= MAX_REMOTE_REPOS || out.len() >= MAX_PRECISE_USAGES {
                break;
            }
            if let Entry::Vacant(e) = servings.entry(repo_id) {
                e.insert(self.serving(file, repo_id, access, filter).await?);
            }
            let Some(Some((repo, visible))) = servings.get(&repo_id) else {
                continue;
            };
            let name: Arc<str> = Arc::from(repo.name.as_str());
            for (u, symbol, cross) in uploads.into_iter().filter(|u| visible.contains(&u.0.id)) {
                let Some(graph) = self.graph(&u).await else {
                    continue;
                };
                let start = out.len();
                Self::occurrences_in(&graph, &Arc::new(u), &name, &symbol, true, out)?;
                if let Some(c) = &cross {
                    mark(&mut out[start..], c);
                }
            }
        }
        Ok(())
    }

    /// A page of precise usages of the hit's symbol (`token` in a file in `lang`), then, for a
    /// global symbol, the search-based usages in repositories without precise data
    /// ([`crate::coverage`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn precise_usages(
        &self,
        file: &FileAt,
        hit: &Hit,
        filter: &RepoFilter,
        token: &str,
        lang: Option<&'static str>,
        offset: usize,
        limit: usize,
    ) -> Result<UsagePage, NavError> {
        let raw = self.raw_usages(file, hit, filter).await?;
        let extra = if is_local(&hit.symbol) {
            None
        } else {
            Some(
                COMPLEMENTS
                    .get_with(self.raw_key(file, hit, filter), async {
                        Arc::new(self.complement(file, hit, filter, &raw, token, lang).await)
                    })
                    .await,
            )
        };
        let added = extra.as_ref().map_or(&[][..], |c| c.usages.as_slice());
        let total = raw.len() + added.len();
        let end = offset.saturating_add(limit).min(total);
        let precise_end = end.min(raw.len());
        let page = raw
            .get(offset.min(precise_end)..precise_end)
            .unwrap_or_default();
        let mut usages = self.resolve(file, page).await?;
        if end > raw.len() {
            let from = offset.max(raw.len()) - raw.len();
            usages.extend(added[from..end - raw.len()].iter().cloned());
        }
        Ok(UsagePage {
            usages,
            next: (end < total).then_some(end),
            coverage: extra.map(|c| c.coverage.clone()),
        })
    }

    /// The implementations of the symbol at `range` in `file` (kind
    /// [`UsageKind::Implementation`]) and the symbols it implements ([`UsageKind::Super`]),
    /// `limit` from `offset`, in the order of [`Self::usages`]. `None` when no precise data
    /// covers the position: only precise data records implementations.
    pub async fn implementations(
        &self,
        file: &FileAt,
        range: Range,
        filter: &RepoFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Option<UsagePage>, NavError> {
        let text = self
            .read(
                file.repo.id.get(),
                &file.repo.name,
                &file.commit,
                &file.path,
            )
            .await?;
        let line = text.line(range.start.line).unwrap_or("");
        let Some(hit) = self.precise_hit(file, line, range.start).await? else {
            return Ok(None);
        };
        let raw = self.raw_usages(file, &hit, filter).await?;
        let mut matching = raw
            .iter()
            .filter(|r| matches!(r.kind, UsageKind::Implementation | UsageKind::Super))
            .skip(offset);
        let page: Vec<Raw> = matching.by_ref().take(limit).cloned().collect();
        let next = matching.next().is_some().then_some(offset + page.len());
        let usages = self.resolve(file, &page).await?;
        Ok(Some(UsagePage {
            usages,
            next,
            coverage: None,
        }))
    }

    /// The cache key of the hit's usages as `file`'s viewer sees them.
    fn raw_key(&self, file: &FileAt, hit: &Hit, filter: &RepoFilter) -> RawKey {
        (
            self.actor.map(|a| a.user_id.get()),
            file.repo.id.get(),
            file.commit.clone(),
            hit.upload.id,
            hit.symbol.clone(),
            filter.clone(),
        )
    }

    /// All precise usages of the hit's symbol, unresolved, cached for [`USAGES_TTL`] so pages
    /// come from one list.
    async fn raw_usages(
        &self,
        file: &FileAt,
        hit: &Hit,
        filter: &RepoFilter,
    ) -> Result<Arc<Vec<Raw>>, NavError> {
        RAW_USAGES
            .try_get_with(self.raw_key(file, hit, filter), async {
                self.precise_raw(file, hit, filter).await.map(Arc::new)
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => NavError::Internal(e.to_string()),
            })
    }

    /// The precise occurrences of `file`, per upload serving it, at the viewed commit
    /// (occurrences on changed lines are dropped).
    pub async fn precise_documents(&self, file: &FileAt) -> Result<Vec<PreciseDocument>, NavError> {
        if self.graph_cache().is_none() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut text: Option<String> = None;
        for v in self.nearest_uploads(&file.repo, &file.commit).await? {
            let u = v.upload;
            if !file.path.starts_with(&u.root) {
                continue;
            }
            let Some(graph) = self.graph(&u).await else {
                continue;
            };
            let Some(doc) = graph
                .document(&file.path)
                .map_err(|e| NavError::Internal(e.to_string()))?
            else {
                continue;
            };
            let map = self
                .line_map(
                    file.repo.id.get(),
                    &file.repo.name,
                    &u.commit,
                    &file.commit,
                    &file.path,
                )
                .await?;
            if text.is_none() {
                let f = self
                    .reader(file.repo.id.get(), &file.repo.name)
                    .read_file(&file.commit, &file.path, FILE_MAX_BYTES)
                    .await?;
                text = Some(String::from_utf8_lossy(&f.content).into_owned());
            }
            let text = text.as_deref().unwrap_or("");
            let enc = doc.position_encoding;
            let mut occurrences = Vec::with_capacity(doc.occurrences.len());
            for o in &doc.occurrences {
                let Some(sym) = o.symbol else { continue };
                let (Some(sl), Some(el)) = (
                    map.old_to_new(o.range.start_line),
                    map.old_to_new(o.range.end_line),
                ) else {
                    continue;
                };
                let sline = token::line_of(text, sl).unwrap_or("");
                let eline = token::line_of(text, el).unwrap_or("");
                occurrences.push(PreciseOccurrence {
                    symbol: graph
                        .symbol_name(sym)
                        .map_err(|e| NavError::Internal(e.to_string()))?
                        .to_owned(),
                    range: Range {
                        start: Position {
                            line: sl,
                            character: from_doc_col(sline, o.range.start_character, enc),
                        },
                        end: Position {
                            line: el,
                            character: from_doc_col(eline, o.range.end_character, enc),
                        },
                    },
                    roles: o.roles,
                });
            }
            out.push(PreciseDocument {
                upload: u,
                occurrences,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        let line = "let naïve = 𝔸;";
        // "naïve" starts at byte/UTF-16/UTF-32 column 4; "𝔸" at UTF-16 12, byte 13, char 12.
        assert_eq!(to_doc_col(line, 12, encoding::UTF8 as u32), 13);
        assert_eq!(to_doc_col(line, 14, encoding::UTF32 as u32), 13);
        assert_eq!(to_doc_col(line, 12, encoding::UTF16 as u32), 12);
        assert_eq!(from_doc_col(line, 13, encoding::UTF8 as u32), 12);
        assert_eq!(from_doc_col(line, 13, encoding::UTF32 as u32), 14);
        assert_eq!(from_doc_col(line, 7, 0), 7);
    }

    #[test]
    fn hovers() {
        assert_eq!(hover("go", None, &[]), None);
        assert_eq!(
            hover("Go", Some("func Foo()"), &["Foo does it.".into()]).as_deref(),
            Some("```go\nfunc Foo()\n```\n\n---\n\nFoo does it.")
        );
        assert_eq!(
            hover("", None, &["```go\nfunc Foo()\n```".into(), "Docs.".into()]).as_deref(),
            Some("```go\nfunc Foo()\n```\n\n---\n\nDocs.")
        );
    }
}
