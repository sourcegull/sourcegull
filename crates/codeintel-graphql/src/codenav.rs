//! Code navigation over GraphQL: `GitBlob.symbolInfo` (hover and definition) and
//! `usagesForSymbol`.
//!
//! Extensions: `SymbolInfo.range` (the identifier's range) and `SymbolInfo.provenance`, so a
//! client can highlight the hovered token and label the result.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use sourcegull_analytics::names::{CodeIntelAction, codeintel_event};
use sourcegull_codenav::{CodeNav, FileAt, NavError, RepoFilter};
use sourcegull_services::relay::marshal_string_id;

use crate::context::{codes, db_error, error, repo_access, services, viewer};
use crate::pagination::{PageInfo, decode_cursor, encode_cursor};
use sourcegull_graphql_core::git::git_error;
use sourcegull_graphql_core::repo::require_reader;
use sourcegull_services::git::GitReader;

/// Default and maximum occurrences per `CodeGraphData.occurrences` page.
pub const MAX_OCCURRENCES: i32 = 100_000;
/// Default and maximum usages per page.
pub const DEFAULT_USAGES: i32 = 100;
pub const MAX_USAGES: i32 = 1000;

pub(crate) fn nav_error(e: NavError) -> async_graphql::Error {
    match e {
        NavError::Git(e) => git_error(e),
        NavError::Search(m) | NavError::Internal(m) => {
            tracing::error!(error = %m, "code navigation failed");
            error(codes::INTERNAL, "Code navigation failed.")
        }
    }
}

pub(crate) fn code_nav<'a>(ctx: &Context<'a>) -> Result<CodeNav<'a>> {
    let state = services(ctx)?
        .app
        .as_ref()
        .ok_or_else(|| error(codes::INTERNAL, "Code navigation is not available."))?;
    Ok(CodeNav::new(state, viewer(ctx)))
}

fn int(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn uint(n: i32, what: &str) -> Result<u32> {
    u32::try_from(n).map_err(|_| {
        error(
            codes::INVALID_INPUT,
            format!("{what} must not be negative."),
        )
    })
}

/// A position in a file: 0-based line and UTF-16 character.
#[derive(SimpleObject, Debug, Clone, Copy)]
pub struct Position {
    pub line: i32,
    pub character: i32,
}

/// A range in a file, end exclusive.
#[derive(SimpleObject, Debug, Clone, Copy)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl From<sourcegull_codenav::Range> for Range {
    fn from(r: sourcegull_codenav::Range) -> Self {
        let p = |p: sourcegull_codenav::Position| Position {
            line: int(p.line),
            character: int(p.character),
        };
        Range {
            start: p(r.start),
            end: p(r.end),
        }
    }
}

/// Where code graph data comes from.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum CodeGraphDataProvenance {
    /// A compiler, type checker or similar source without false positives.
    Precise,
    /// Syntax trees without type information.
    Syntactic,
    /// Text search.
    SearchBased,
}

impl From<sourcegull_codenav::Provenance> for CodeGraphDataProvenance {
    fn from(p: sourcegull_codenav::Provenance) -> Self {
        match p {
            sourcegull_codenav::Provenance::Precise => Self::Precise,
            sourcegull_codenav::Provenance::Syntactic => Self::Syntactic,
            sourcegull_codenav::Provenance::SearchBased => Self::SearchBased,
        }
    }
}

/// How a usage relates to the symbol.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolUsageKind {
    Definition,
    Reference,
    Implementation,
    Super,
}

impl From<sourcegull_codenav::UsageKind> for SymbolUsageKind {
    fn from(k: sourcegull_codenav::UsageKind) -> Self {
        match k {
            sourcegull_codenav::UsageKind::Definition => Self::Definition,
            sourcegull_codenav::UsageKind::Reference => Self::Reference,
            sourcegull_codenav::UsageKind::Implementation => Self::Implementation,
            sourcegull_codenav::UsageKind::Super => Self::Super,
        }
    }
}

/// A span within one line.
#[derive(SimpleObject, Debug, Clone, Copy)]
pub struct LineRange {
    pub line: i32,
    pub character: i32,
    pub length: i32,
}

/// A single-line range in a repository (`SymbolInfo.definition`).
pub struct SymbolLocation(sourcegull_codenav::Location);

#[Object]
impl SymbolLocation {
    async fn repo(&self) -> &str {
        &self.0.repo
    }

    async fn commit(&self) -> &str {
        &self.0.commit
    }

    async fn path(&self) -> &str {
        &self.0.path
    }

    async fn range(&self) -> LineRange {
        let r = self.0.range;
        LineRange {
            line: int(r.start.line),
            character: int(r.start.character),
            length: int(r.end.character.saturating_sub(r.start.character)),
        }
    }

    #[graphql(deprecation = "use range.line instead")]
    async fn line(&self) -> i32 {
        int(self.0.range.start.line)
    }

    #[graphql(deprecation = "use range.character instead")]
    async fn character(&self) -> i32 {
        int(self.0.range.start.character)
    }

    #[graphql(deprecation = "use range.length instead")]
    async fn length(&self) -> i32 {
        let r = self.0.range;
        int(r.end.character.saturating_sub(r.start.character))
    }
}

/// Hover and definition of the symbol at a position (`GitBlob.symbolInfo`).
pub struct SymbolInfo(sourcegull_codenav::SymbolInfo);

#[Object]
impl SymbolInfo {
    /// The definition of the symbol.
    async fn definition(&self) -> Option<SymbolLocation> {
        self.0.definition.clone().map(SymbolLocation)
    }

    /// The hover for the symbol, in Markdown.
    async fn hover(&self) -> Option<&str> {
        self.0.hover.as_deref()
    }

    /// The identifier's range.
    async fn range(&self) -> Range {
        self.0.range.into()
    }

    /// Where the definition and hover come from.
    async fn provenance(&self) -> CodeGraphDataProvenance {
        self.0.provenance.into()
    }

    /// Set when the precise definition was found at another version of the symbol's package.
    async fn cross_version(&self) -> Option<CrossVersion> {
        self.0.cross_version.clone().map(Into::into)
    }
}

/// A precise result linked across package versions: no upload holds the symbol at the
/// version it was looked up at (`requested`), one holds it at `found`.
#[derive(SimpleObject, Debug, Clone)]
pub struct CrossVersion {
    requested: String,
    found: String,
}

impl From<sourcegull_codenav::CrossVersion> for CrossVersion {
    fn from(c: sourcegull_codenav::CrossVersion) -> Self {
        Self {
            requested: c.requested,
            found: c.found,
        }
    }
}

/// `GitBlob.symbolInfo`: resolves the symbol at (`line`, `character`) of a file.
pub async fn symbol_info(
    ctx: &Context<'_>,
    file: FileAt,
    line: i32,
    character: i32,
) -> Result<Option<SymbolInfo>> {
    let pos = sourcegull_codenav::Position {
        line: uint(line, "line")?,
        character: uint(character, "character")?,
    };
    let info = code_nav(ctx)?
        .symbol_info(&file, pos)
        .await
        .map_err(nav_error)?;
    if let Some(info) = &info {
        let cross_repo = info
            .definition
            .as_ref()
            .is_some_and(|d| d.repo_id != file.repo.id.get());
        log_codeintel(
            ctx,
            CodeIntelAction::Hover,
            info.provenance == sourcegull_codenav::Provenance::Precise,
            cross_repo,
            &file,
        );
    }
    Ok(info.map(SymbolInfo))
}

/// Records a code navigation usage event (`codeintel.lsifHover`,
/// `codeintel.searchReferences.xrepo`, …) with the repository and language.
fn log_codeintel(
    ctx: &Context<'_>,
    action: CodeIntelAction,
    precise: bool,
    cross_repo: bool,
    file: &FileAt,
) {
    let language = sourcegull_languages::detect(&file.path, None).map_or("", |l| l.name);
    sourcegull_graphql_core::events::log_backend(
        ctx,
        codeintel_event(action, precise, cross_repo),
        serde_json::json!({ "repoName": file.repo.name, "languageId": language }),
    );
}

/// A position (input).
#[derive(InputObject, Debug, Clone, Copy)]
pub struct PositionInput {
    pub line: i32,
    pub character: i32,
}

/// A source range of a usage of the symbol to look up.
#[derive(InputObject, Debug, Clone)]
pub struct RangeInput {
    pub repository: String,
    /// Defaults to the default branch.
    pub revision: Option<String>,
    pub path: String,
    pub start: PositionInput,
    pub end: PositionInput,
}

#[derive(InputObject, Debug, Clone)]
pub struct StringComparator {
    pub equals: Option<String>,
}

#[derive(InputObject, Debug, Clone)]
pub struct RepositoryFilter {
    pub name: StringComparator,
}

/// Limits the usages returned.
#[derive(InputObject, Debug, Clone)]
pub struct UsagesFilter {
    pub not: Option<Box<UsagesFilter>>,
    pub repository: Option<RepositoryFilter>,
}

#[derive(InputObject, Debug, Clone)]
pub struct SymbolNameComparator {
    pub equals: Option<String>,
}

#[derive(InputObject, Debug, Clone)]
pub struct CodeGraphDataProvenanceComparator {
    pub equals: Option<CodeGraphDataProvenance>,
}

#[derive(InputObject, Debug, Clone)]
pub struct SymbolComparator {
    pub name: SymbolNameComparator,
    pub provenance: CodeGraphDataProvenanceComparator,
}

impl UsagesFilter {
    fn repo_filter(&self) -> Result<RepoFilter> {
        let name = |f: &UsagesFilter| f.repository.as_ref().and_then(|r| r.name.equals.clone());
        let mut out = RepoFilter {
            only: name(self),
            exclude: None,
        };
        if let Some(not) = &self.not {
            if not.not.is_some() {
                return Err(error(
                    codes::INVALID_INPUT,
                    "Nested `not` filters are not supported.",
                ));
            }
            out.exclude = name(not);
        }
        Ok(out)
    }
}

/// Information about a symbol.
#[derive(SimpleObject)]
pub struct SymbolInformation {
    /// The SCIP symbol, or the identifier for search-based usages.
    name: String,
    /// Hover documentation in Markdown; null when not looked up.
    documentation: Option<Vec<String>>,
}

/// Where a usage is.
#[derive(SimpleObject)]
pub struct UsageRange {
    repository: String,
    revision: String,
    path: String,
    range: Range,
}

/// A place where a symbol is used.
#[derive(SimpleObject)]
pub struct Usage {
    symbol: Option<SymbolInformation>,
    provenance: CodeGraphDataProvenance,
    data_source: Option<String>,
    usage_range: UsageRange,
    /// The line containing the usage.
    surrounding_content: String,
    usage_kind: SymbolUsageKind,
    /// Set when a precise usage was found at another version of the symbol's package.
    cross_version: Option<CrossVersion>,
}

#[derive(SimpleObject)]
pub struct UsageConnection {
    nodes: Vec<Usage>,
    page_info: PageInfo,
    /// For precise usages of a symbol other repositories can use: which repositories
    /// answered precisely and which search-based (precise usages come first, then the
    /// search-based ones from repositories without precise data).
    coverage: Option<UsageCoverage>,
}

/// Why a repository's usages are search-based in a precise answer.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsageCoverageReason {
    /// No upload serves the repository's default branch.
    NoIndex,
    /// Its newest upload or index job failed.
    IndexFailed,
    /// The time budget ran out before it was searched; none of its usages are listed.
    BudgetHit,
}

impl From<sourcegull_codenav::CoverageReason> for UsageCoverageReason {
    fn from(r: sourcegull_codenav::CoverageReason) -> Self {
        match r {
            sourcegull_codenav::CoverageReason::NoIndex => Self::NoIndex,
            sourcegull_codenav::CoverageReason::IndexFailed => Self::IndexFailed,
            sourcegull_codenav::CoverageReason::Budget => Self::BudgetHit,
        }
    }
}

/// A repository answered search-based.
#[derive(SimpleObject)]
pub struct SearchBasedRepository {
    repository: String,
    reason: UsageCoverageReason,
}

/// A repository whose precise usages were found at another version of the symbol's package.
#[derive(SimpleObject)]
pub struct CrossVersionRepository {
    repository: String,
    /// The package version its upload has.
    version: String,
}

/// The repositories a precise usages answer covers, and how.
#[derive(SimpleObject)]
pub struct UsageCoverage {
    /// Repositories with precise usages.
    precise_repositories: Vec<String>,
    /// Repositories among `preciseRepositories` linked across package versions.
    cross_version_repositories: Vec<CrossVersionRepository>,
    /// Repositories that may use the symbol but have no precise data, searched instead.
    search_based_repositories: Vec<SearchBasedRepository>,
    /// More repositories mention the symbol's package than were checked.
    truncated: bool,
    /// One line, e.g. `precise: 5 repositories · other versions: lib (v1.1.0) ·
    /// search-based: push-service (no index)`.
    summary: String,
}

impl From<sourcegull_codenav::Coverage> for UsageCoverage {
    fn from(c: sourcegull_codenav::Coverage) -> Self {
        Self {
            summary: c.summary(),
            precise_repositories: c.precise,
            cross_version_repositories: c
                .cross_version
                .into_iter()
                .map(|(repository, version)| CrossVersionRepository {
                    repository,
                    version,
                })
                .collect(),
            search_based_repositories: c
                .search_based
                .into_iter()
                .map(|(repository, why)| SearchBasedRepository {
                    repository,
                    reason: why.into(),
                })
                .collect(),
            truncated: c.truncated,
        }
    }
}

#[derive(Default)]
pub struct CodeNavQuery;

#[Object]
impl CodeNavQuery {
    /// Usages of the symbol at `range`: grouped by provenance (precise, syntactic,
    /// search-based), then by repository (the range's own first) and file, deduplicated.
    /// Precise usages of a symbol other repositories can use are followed by search-based
    /// ones from repositories without precise data; `coverage` lists both.
    async fn usages_for_symbol(
        &self,
        ctx: &Context<'_>,
        symbol: Option<SymbolComparator>,
        range: RangeInput,
        filter: Option<UsagesFilter>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<UsageConnection> {
        require_reader(ctx)?;
        let first = first.unwrap_or(DEFAULT_USAGES);
        if !(0..=MAX_USAGES).contains(&first) {
            return Err(error(
                codes::INVALID_INPUT,
                format!("first must be between 0 and {MAX_USAGES}."),
            ));
        }
        let offset = decode_cursor(after.as_deref())?;
        let empty = UsageConnection {
            nodes: Vec::new(),
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
            coverage: None,
        };
        let want = symbol.as_ref().and_then(|s| s.provenance.equals);
        let repo_filter = filter
            .as_ref()
            .map(UsagesFilter::repo_filter)
            .transpose()?
            .unwrap_or_default();
        let access = repo_access(ctx).await?;
        let Some(repo) = services(ctx)?
            .db
            .repos()
            .get_by_name_authorized(&range.repository, &access)
            .await
            .map_err(db_error)?
        else {
            return Err(error(
                codes::NOT_FOUND,
                format!("Repository {:?} not found.", range.repository),
            ));
        };
        let git = GitReader::new(services(ctx)?.repo_clients.clone(), &repo);
        let rev = range.revision.as_deref().unwrap_or("HEAD");
        let commit = git.resolve_revision(rev).await.map_err(git_error)?;
        let file = FileAt {
            repo,
            commit,
            path: range.path.clone(),
        };
        let r = sourcegull_codenav::Range {
            start: sourcegull_codenav::Position {
                line: uint(range.start.line, "line")?,
                character: uint(range.start.character, "character")?,
            },
            end: sourcegull_codenav::Position {
                line: uint(range.end.line, "line")?,
                character: uint(range.end.character, "character")?,
            },
        };
        let mut page = code_nav(ctx)?
            .usages(
                &file,
                r,
                &repo_filter,
                usize::try_from(offset).unwrap_or(usize::MAX),
                usize::try_from(first).unwrap_or(0),
            )
            .await
            .map_err(nav_error)?;
        // The first page of a lookup is one "find references" request.
        if offset == 0 {
            let precise = page
                .usages
                .first()
                .is_some_and(|u| u.provenance == sourcegull_codenav::Provenance::Precise);
            let cross_repo = page
                .usages
                .iter()
                .any(|u| u.location.repo_id != file.repo.id.get());
            log_codeintel(ctx, CodeIntelAction::References, precise, cross_repo, &file);
        }
        // One tier answers a request: the best one with data at the range. A precise answer
        // may end with search-based usages from repositories without precise data; asking
        // for precise usages leaves those out.
        if let Some(p) = want {
            let tier = if page.coverage.is_some() {
                Some(CodeGraphDataProvenance::Precise)
            } else {
                page.usages.first().map(|u| u.provenance.into())
            };
            if tier.is_some_and(|t| t != p) {
                return Ok(empty);
            }
            page.usages.retain(|u| p == u.provenance.into());
        }
        let end_cursor = page
            .next
            .map(|n| encode_cursor(i64::try_from(n).unwrap_or(i64::MAX)));
        Ok(UsageConnection {
            nodes: page
                .usages
                .into_iter()
                .map(|u| Usage {
                    symbol: Some(SymbolInformation {
                        name: u.symbol,
                        documentation: None,
                    }),
                    provenance: u.provenance.into(),
                    data_source: Some(u.data_source.to_owned()),
                    usage_range: UsageRange {
                        repository: u.location.repo,
                        revision: u.location.commit,
                        path: u.location.path,
                        range: u.location.range.into(),
                    },
                    surrounding_content: u.line,
                    usage_kind: u.kind.into(),
                    cross_version: u.cross_version.map(Into::into),
                })
                .collect(),
            page_info: PageInfo {
                has_next_page: end_cursor.is_some(),
                end_cursor,
            },
            coverage: page.coverage.map(Into::into),
        })
    }
}

/// Filters `GitBlob.codeGraphData`.
#[derive(InputObject, Debug, Clone)]
pub struct CodeGraphDataFilter {
    /// Without it, the first provenance with data in the order precise, syntactic,
    /// search-based.
    pub provenance: Option<CodeGraphDataProvenanceComparator>,
}

/// The tool that made code graph data.
#[derive(SimpleObject)]
pub struct CodeGraphToolInfo {
    name: Option<String>,
    version: Option<String>,
}

/// The role of a symbol at an occurrence.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolRole {
    Definition,
    Reference,
    ForwardDefinition,
}

/// A symbol at a range.
#[derive(SimpleObject)]
#[graphql(name = "SCIPOccurrence")]
pub struct ScipOccurrence {
    symbol: Option<String>,
    range: Range,
    roles: Option<Vec<SymbolRole>>,
}

#[derive(SimpleObject)]
#[graphql(name = "SCIPOccurrenceConnection")]
pub struct ScipOccurrenceConnection {
    nodes: Vec<ScipOccurrence>,
    page_info: PageInfo,
}

/// Occurrences of code graph data.
enum Occurrences {
    Syntactic(Arc<sourcegull_codenav::Locals>),
    Precise(Vec<sourcegull_codenav::precise::PreciseOccurrence>),
}

impl Occurrences {
    fn len(&self) -> usize {
        match self {
            Self::Syntactic(l) => l.occurrences.len(),
            Self::Precise(o) => o.len(),
        }
    }

    fn page(&self, skip: usize, take: usize) -> Vec<ScipOccurrence> {
        match self {
            Self::Syntactic(l) => l
                .occurrences
                .iter()
                .skip(skip)
                .take(take)
                .map(|o| ScipOccurrence {
                    symbol: Some(sourcegull_codenav::scip_symbol(o.symbol)),
                    range: sourcegull_codenav::Range::on_line(o.line, o.start, o.end).into(),
                    roles: Some(vec![if o.definition {
                        SymbolRole::Definition
                    } else {
                        SymbolRole::Reference
                    }]),
                })
                .collect(),
            Self::Precise(occ) => occ
                .iter()
                .skip(skip)
                .take(take)
                .map(|o| {
                    let mut roles = Vec::new();
                    if o.roles & 0x1 != 0 {
                        roles.push(SymbolRole::Definition);
                    } else if o.roles & 0x40 != 0 {
                        roles.push(SymbolRole::ForwardDefinition);
                    } else {
                        roles.push(SymbolRole::Reference);
                    }
                    ScipOccurrence {
                        symbol: Some(o.symbol.clone()),
                        range: o.range.into(),
                        roles: Some(roles),
                    }
                })
                .collect(),
        }
    }
}

/// Code graph data of a file from one source.
pub struct CodeGraphData {
    id: String,
    provenance: CodeGraphDataProvenance,
    commit: String,
    tool: (String, String),
    occurrences: Occurrences,
}

#[Object]
impl CodeGraphData {
    async fn id(&self) -> ID {
        ID(marshal_string_id("CodeGraphData", &self.id))
    }

    async fn provenance(&self) -> CodeGraphDataProvenance {
        self.provenance
    }

    /// The commit the data was computed at.
    async fn commit(&self) -> &str {
        &self.commit
    }

    async fn tool_info(&self) -> CodeGraphToolInfo {
        CodeGraphToolInfo {
            name: Some(self.tool.0.clone()),
            version: Some(self.tool.1.clone()),
        }
    }

    /// Occurrences sorted by range.
    async fn occurrences(
        &self,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<ScipOccurrenceConnection> {
        let first = first.unwrap_or(MAX_OCCURRENCES);
        if !(0..=MAX_OCCURRENCES).contains(&first) {
            return Err(error(
                codes::INVALID_INPUT,
                format!("first must be between 0 and {MAX_OCCURRENCES}."),
            ));
        }
        let offset = decode_cursor(after.as_deref())?;
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(first).unwrap_or(0);
        let total = self.occurrences.len();
        Ok(ScipOccurrenceConnection {
            nodes: self.occurrences.page(skip, take),
            page_info: PageInfo::new(offset, i64::from(first), total.saturating_sub(skip)),
        })
    }
}

/// `GitBlob.codeGraphData`.
pub async fn code_graph_data(
    ctx: &Context<'_>,
    file: FileAt,
    filter: Option<CodeGraphDataFilter>,
) -> Result<Option<Vec<CodeGraphData>>> {
    let want = filter.and_then(|f| f.provenance).and_then(|p| p.equals);
    let nav = code_nav(ctx)?;
    if matches!(want, None | Some(CodeGraphDataProvenance::Precise)) {
        let docs = nav.precise_documents(&file).await.map_err(nav_error)?;
        if !docs.is_empty() || want.is_some() {
            return Ok(Some(
                docs.into_iter()
                    .map(|d| CodeGraphData {
                        id: format!("precise:{}:{}:{}", d.upload.id, file.commit, file.path),
                        provenance: CodeGraphDataProvenance::Precise,
                        commit: d.upload.commit,
                        tool: (d.upload.indexer, d.upload.indexer_version),
                        occurrences: Occurrences::Precise(d.occurrences),
                    })
                    .collect(),
            ));
        }
    }
    if !matches!(want, None | Some(CodeGraphDataProvenance::Syntactic)) {
        return Ok(Some(Vec::new()));
    }
    let Some(locals) = nav.locals(&file).await.map_err(nav_error)? else {
        return Ok(Some(Vec::new()));
    };
    Ok(Some(vec![CodeGraphData {
        id: format!(
            "syntactic:{}:{}:{}",
            file.repo.id.get(),
            file.commit,
            file.path
        ),
        provenance: CodeGraphDataProvenance::Syntactic,
        commit: file.commit,
        tool: (
            "sourcegull-syntactic".to_owned(),
            sourcegull_codenav::SYNTACTIC_VERSION.to_string(),
        ),
        occurrences: Occurrences::Syntactic(locals),
    }]))
}
