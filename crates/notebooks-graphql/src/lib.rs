//! Notebooks: the `Notebook` type with its block union, `notebooks`, the create,
//! update, delete and star mutations.
//!
//! Visibility and the right to manage (edit, delete) are `sourcegull_notebooks::access`'s
//! rules; anonymous users read public notebooks when public access is on. File and symbol blocks
//! referring to repositories the viewer may not read are left out of `blocks`, and kept when
//! the viewer saves.

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use sourcegull_authn::Actor;
use sourcegull_db::Namespace;
use sourcegull_notebooks::{
    Access, Block, BlockKind, FileInput, LineRange, ListNotebooks, Notebook, NotebookInput,
    NotebookOrder, NotebookStar, NotebookStore, SymbolInput, TextInput,
};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

mod feature;

pub use feature::NotebooksGraphql;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use crate::context::{
    access_error, codes, db_error, error, page_size, repo_access, require_viewer, services, to_int,
    validation, viewer,
};
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::events::log_backend;
use sourcegull_graphql_core::ids::{parse_namespace_id, parse_user_id};
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::repo::require_reader;
use sourcegull_graphql_core::search::SearchPatternType;
use sourcegull_graphql_core::shared::{
    NamespaceRef, UserRef, load_namespace as load_namespace_value,
};

/// Node kind of notebook IDs.
pub const NOTEBOOK_KIND: &str = "Notebook";

fn store(ctx: &Context<'_>) -> Result<NotebookStore> {
    Ok(NotebookStore::new(services(ctx)?.db.pool().clone()))
}

fn access(ctx: &Context<'_>) -> Access {
    sourcegull_notebooks::access::access(viewer(ctx))
}

fn parse_notebook_id(id: &str) -> Result<i64> {
    unmarshal_int_id(NOTEBOOK_KIND, id)
        .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid notebook ID {id:?}.")))
}

async fn require_can_manage(ctx: &Context<'_>, actor: &Actor, ns: Namespace) -> Result<()> {
    sourcegull_notebooks::access::require_can_manage(&services(ctx)?.db, actor, ns)
        .await
        .map_err(access_error)
}

/// Loads a notebook the viewer may see, or `NOT_FOUND`.
async fn load_visible(ctx: &Context<'_>, id: &ID) -> Result<Notebook> {
    store(ctx)?
        .get(parse_notebook_id(id)?, access(ctx))
        .await
        .map_err(db_error)?
        .ok_or_else(|| error(codes::NOT_FOUND, "Notebook not found."))
}

/// Loads a notebook for `node(id:)`.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<NotebookNode>> {
    require_reader(ctx)?;
    Ok(store(ctx)?
        .get(id, access(ctx))
        .await
        .map_err(db_error)?
        .map(NotebookNode))
}

/// The `node(id:)` loader of notebooks.
pub fn node_loaders<N: From<NotebookNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<NotebookNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(NOTEBOOK_KIND, id) else {
                return Ok(None);
            };
            Ok(load_node(ctx, key).await?.map(N::from))
        })
    }
    [NodeLoader {
        kinds: &[NOTEBOOK_KIND],
        load: load::<N>,
    }]
}

async fn load_namespace(ctx: &Context<'_>, ns: Namespace) -> Result<Option<NamespaceRef>> {
    Ok(load_namespace_value(ctx, ns).await?.map(NamespaceRef))
}

/// The symbol kinds (LSP's, as `SymbolKind`).
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
#[allow(missing_docs)]
pub enum SymbolKind {
    Unknown,
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    #[graphql(name = "ENUMMEMBER")]
    EnumMember,
    Struct,
    Event,
    Operator,
    #[graphql(name = "TYPEPARAMETER")]
    TypeParameter,
}

impl SymbolKind {
    /// The kind named `s` (as stored); unknown names are `UNKNOWN`.
    fn from_name(s: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.name() == s)
            .unwrap_or(SymbolKind::Unknown)
    }

    const ALL: [SymbolKind; 27] = [
        Self::Unknown,
        Self::File,
        Self::Module,
        Self::Namespace,
        Self::Package,
        Self::Class,
        Self::Method,
        Self::Property,
        Self::Field,
        Self::Constructor,
        Self::Enum,
        Self::Interface,
        Self::Function,
        Self::Variable,
        Self::Constant,
        Self::String,
        Self::Number,
        Self::Boolean,
        Self::Array,
        Self::Object,
        Self::Key,
        Self::Null,
        Self::EnumMember,
        Self::Struct,
        Self::Event,
        Self::Operator,
        Self::TypeParameter,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::File => "FILE",
            Self::Module => "MODULE",
            Self::Namespace => "NAMESPACE",
            Self::Package => "PACKAGE",
            Self::Class => "CLASS",
            Self::Method => "METHOD",
            Self::Property => "PROPERTY",
            Self::Field => "FIELD",
            Self::Constructor => "CONSTRUCTOR",
            Self::Enum => "ENUM",
            Self::Interface => "INTERFACE",
            Self::Function => "FUNCTION",
            Self::Variable => "VARIABLE",
            Self::Constant => "CONSTANT",
            Self::String => "STRING",
            Self::Number => "NUMBER",
            Self::Boolean => "BOOLEAN",
            Self::Array => "ARRAY",
            Self::Object => "OBJECT",
            Self::Key => "KEY",
            Self::Null => "NULL",
            Self::EnumMember => "ENUMMEMBER",
            Self::Struct => "STRUCT",
            Self::Event => "EVENT",
            Self::Operator => "OPERATOR",
            Self::TypeParameter => "TYPEPARAMETER",
        }
    }
}

/// Markdown block renders the Markdown formatted input string into HTML.
#[derive(SimpleObject)]
pub struct MarkdownBlock {
    /// ID of the block.
    id: String,
    /// Markdown formatted input string.
    markdown_input: String,
}

/// Query block allows performing inline search queries within a notebook.
#[derive(SimpleObject)]
pub struct QueryBlock {
    /// ID of the block.
    id: String,
    /// A search query string.
    query_input: String,
}

/// A line range inside a file.
#[derive(SimpleObject)]
pub struct FileBlockLineRange {
    /// The first line to fetch (0-indexed, inclusive).
    start_line: i32,
    /// The last line to fetch (0-indexed, exclusive).
    end_line: i32,
}

/// FileBlockInput contains the information necessary to fetch the file.
#[derive(SimpleObject)]
#[graphql(name = "FileBlockInput")]
pub struct FileBlockInputNode {
    /// Name of the repository, e.g. "gitlab.com/group/project".
    repository_name: String,
    /// Path within the repository, e.g. "client/web/file.tsx".
    file_path: String,
    /// An optional revision; if omitted, the default branch.
    revision: Option<String>,
    /// An optional line range; if omitted, the entire file.
    line_range: Option<FileBlockLineRange>,
}

/// FileBlock specifies a file (or part of a file) to display within the block.
#[derive(SimpleObject)]
pub struct FileBlock {
    /// ID of the block.
    id: String,
    /// File block input.
    file_input: FileBlockInputNode,
}

/// SymbolBlockInput contains the information necessary to find the symbol.
#[derive(SimpleObject)]
#[graphql(name = "SymbolBlockInput")]
pub struct SymbolBlockInputNode {
    /// Name of the repository, e.g. "gitlab.com/group/project".
    repository_name: String,
    /// Path within the repository, e.g. "client/web/file.tsx".
    file_path: String,
    /// An optional revision; if omitted, the default branch.
    revision: Option<String>,
    /// Number of lines to show before and after the matched symbol line.
    line_context: i32,
    /// The symbol name.
    symbol_name: String,
    /// Name of the symbol container.
    symbol_container_name: String,
    /// The symbol kind.
    symbol_kind: SymbolKind,
}

/// SymbolBlock specifies a symbol to display within the block.
#[derive(SimpleObject)]
pub struct SymbolBlock {
    /// ID of the block.
    id: String,
    /// Symbol block input.
    symbol_input: SymbolBlockInputNode,
}

/// Notebook blocks: Markdown, query, file and symbol blocks.
#[derive(Union)]
pub enum NotebookBlock {
    Markdown(MarkdownBlock),
    Query(QueryBlock),
    File(FileBlock),
    Symbol(SymbolBlock),
}

impl From<Block> for NotebookBlock {
    fn from(b: Block) -> Self {
        let id = b.id;
        match b.kind {
            BlockKind::Markdown { markdown_input } => NotebookBlock::Markdown(MarkdownBlock {
                id,
                markdown_input: markdown_input.text,
            }),
            BlockKind::Query { query_input } => NotebookBlock::Query(QueryBlock {
                id,
                query_input: query_input.text,
            }),
            BlockKind::File { file_input: f } => NotebookBlock::File(FileBlock {
                id,
                file_input: FileBlockInputNode {
                    repository_name: f.repository_name,
                    file_path: f.file_path,
                    revision: f.revision,
                    line_range: f.line_range.map(|r| FileBlockLineRange {
                        start_line: r.start_line,
                        end_line: r.end_line,
                    }),
                },
            }),
            BlockKind::Symbol { symbol_input: s } => NotebookBlock::Symbol(SymbolBlock {
                id,
                symbol_input: SymbolBlockInputNode {
                    repository_name: s.repository_name,
                    file_path: s.file_path,
                    revision: s.revision,
                    line_context: s.line_context,
                    symbol_name: s.symbol_name,
                    symbol_container_name: s.symbol_container_name,
                    symbol_kind: SymbolKind::from_name(&s.symbol_kind),
                },
            }),
        }
    }
}

/// A notebook with an array of blocks.
#[derive(Debug, Clone)]
pub struct NotebookNode(pub Notebook);

#[Object(name = "Notebook")]
impl NotebookNode {
    /// The unique id of the notebook.
    pub async fn id(&self) -> ID {
        ID(marshal_id(NOTEBOOK_KIND, self.0.id))
    }

    /// The title of the notebook.
    async fn title(&self) -> &str {
        &self.0.title
    }

    /// Array of notebook blocks. File and symbol blocks in repositories the viewer may not
    /// read are left out.
    async fn blocks(&self, ctx: &Context<'_>) -> Result<Vec<NotebookBlock>> {
        let access = repo_access(ctx).await?;
        let hidden =
            sourcegull_notebooks::hidden_blocks(&services(ctx)?.db, &access, &self.0.blocks)
                .await
                .map_err(db_error)?;
        Ok(self
            .0
            .blocks
            .iter()
            .filter(|b| !hidden.contains(&b.id))
            .cloned()
            .map(NotebookBlock::from)
            .collect())
    }

    /// User that created the notebook or null if the user was removed.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.creator).await
    }

    /// User that last updated the notebook or null if that user was removed.
    async fn updater(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.updater).await
    }

    /// Notebook namespace or null if the namespace (user or org) was removed.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<Option<NamespaceRef>> {
        load_namespace(ctx, self.0.namespace).await
    }

    /// Public notebooks are available to any user on the instance; private ones only to
    /// their user, or to the members of their organization.
    async fn public(&self) -> bool {
        self.0.public
    }

    /// Date and time the notebook was last updated.
    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }

    /// Date and time the notebook was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// If current viewer can manage (edit, delete) the notebook.
    async fn viewer_can_manage(&self, ctx: &Context<'_>) -> Result<bool> {
        sourcegull_notebooks::access::viewer_can_manage(
            &services(ctx)?.db,
            viewer(ctx),
            self.0.namespace,
        )
        .await
        .map_err(db_error)
    }

    /// If current viewer has starred the notebook.
    async fn viewer_has_starred(&self, ctx: &Context<'_>) -> Result<bool> {
        let Some(actor) = viewer(ctx) else {
            return Ok(false);
        };
        Ok(store(ctx)?
            .get_star(self.0.id, actor.user_id)
            .await
            .map_err(db_error)?
            .is_some())
    }

    /// Notebook stars.
    async fn stars(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<NotebookStarConnection> {
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let (stars, total) = store(ctx)?
            .list_stars(self.0.id, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, stars.len());
        let mut nodes: Vec<_> = stars.into_iter().map(NotebookStarNode).collect();
        nodes.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(NotebookStarConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }

    /// The default pattern type that is used to interpret queries that do not contain a
    /// patternType: filter.
    async fn pattern_type(&self) -> SearchPatternType {
        match self.0.pattern_type.as_str() {
            "literal" => SearchPatternType::Literal,
            "regexp" => SearchPatternType::Regexp,
            "structural" => SearchPatternType::Structural,
            "keyword" => SearchPatternType::Keyword,
            _ => SearchPatternType::Standard,
        }
    }
}

async fn user(ctx: &Context<'_>, id: Option<sourcegull_types::UserId>) -> Result<Option<UserRef>> {
    let Some(id) = id else { return Ok(None) };
    Ok(services(ctx)?
        .db
        .users()
        .get_by_id(id)
        .await
        .map_err(db_error)?
        .map(UserRef))
}

/// A notebook star.
pub struct NotebookStarNode(NotebookStar);

#[Object(name = "NotebookStar")]
impl NotebookStarNode {
    /// User that starred the notebook.
    async fn user(&self, ctx: &Context<'_>) -> Result<UserRef> {
        user(ctx, Some(self.0.user_id))
            .await?
            .ok_or_else(|| error(codes::NOT_FOUND, "User not found."))
    }

    /// Date and time the notebook star was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }
}

/// A paginated list of notebook stars.
#[derive(SimpleObject)]
pub struct NotebookStarConnection {
    /// A list of notebook stars.
    nodes: Vec<NotebookStarNode>,
    /// The total number of notebook stars in the connection.
    total_count: i32,
    /// Pagination information.
    page_info: PageInfo,
}

/// A paginated list of notebooks.
#[derive(SimpleObject)]
pub struct NotebookConnection {
    /// A list of notebooks.
    nodes: Vec<NotebookNode>,
    /// The total number of notebooks in the connection.
    total_count: i32,
    /// Pagination information.
    page_info: PageInfo,
}

/// NotebooksOrderBy enumerates the ways notebooks can be ordered.
// The variant names are the GraphQL values.
#[allow(clippy::enum_variant_names)]
#[derive(Enum, Copy, Clone, Eq, PartialEq, Default)]
pub enum NotebooksOrderBy {
    #[default]
    NotebookUpdatedAt,
    NotebookCreatedAt,
    NotebookStarCount,
}

/// Input to create a line range for a file block.
#[derive(InputObject)]
pub struct CreateFileBlockLineRangeInput {
    /// The first line to fetch (0-indexed, inclusive).
    start_line: i32,
    /// The last line to fetch (0-indexed, exclusive).
    end_line: i32,
}

/// CreateFileBlockInput contains the information necessary to create a file block.
#[derive(InputObject)]
pub struct CreateFileBlockInput {
    /// Name of the repository, e.g. "gitlab.com/group/project".
    repository_name: String,
    /// Path within the repository.
    file_path: String,
    /// An optional revision; if omitted, the default branch.
    revision: Option<String>,
    /// An optional line range; if omitted, the entire file.
    line_range: Option<CreateFileBlockLineRangeInput>,
}

/// CreateSymbolBlockInput contains the information necessary to create a symbol block.
#[derive(InputObject)]
pub struct CreateSymbolBlockInput {
    /// Name of the repository, e.g. "gitlab.com/group/project".
    repository_name: String,
    /// Path within the repository.
    file_path: String,
    /// An optional revision; if omitted, the default branch.
    revision: Option<String>,
    /// Number of lines to show before and after the matched symbol line.
    line_context: i32,
    /// The symbol name.
    symbol_name: String,
    /// Name of the symbol container.
    symbol_container_name: String,
    /// The symbol kind.
    symbol_kind: SymbolKind,
}

/// Enum of possible block types.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum NotebookBlockType {
    Markdown,
    Query,
    File,
    Symbol,
}

/// GraphQL does not accept union types as inputs, so all block inputs are optional and
/// `type` selects the one used.
#[derive(InputObject)]
pub struct CreateNotebookBlockInput {
    /// ID of the block.
    id: String,
    /// Block type.
    #[graphql(name = "type")]
    ty: NotebookBlockType,
    /// Markdown input.
    markdown_input: Option<String>,
    /// Query input.
    query_input: Option<String>,
    /// File input.
    file_input: Option<CreateFileBlockInput>,
    /// Symbol input.
    symbol_input: Option<CreateSymbolBlockInput>,
}

/// Input for a new notebook.
#[derive(InputObject)]
#[graphql(name = "NotebookInput")]
pub struct NotebookInputObject {
    /// The title of the notebook.
    title: String,
    /// Array of notebook blocks.
    blocks: Vec<CreateNotebookBlockInput>,
    /// Notebook namespace (user or org). Controls the visibility of the notebook and who can
    /// edit the notebook.
    namespace: ID,
    /// Whether every user of the instance may see the notebook.
    public: bool,
}

fn convert_block(b: CreateNotebookBlockInput) -> Result<Block> {
    let missing = |what: &str| {
        validation(vec![format!(
            "{what} block with id {} is missing input.",
            b.id
        )])
    };
    let kind = match b.ty {
        NotebookBlockType::Markdown => BlockKind::Markdown {
            markdown_input: TextInput {
                text: b
                    .markdown_input
                    .clone()
                    .ok_or_else(|| missing("Markdown"))?,
            },
        },
        NotebookBlockType::Query => BlockKind::Query {
            query_input: TextInput {
                text: b.query_input.clone().ok_or_else(|| missing("Query"))?,
            },
        },
        NotebookBlockType::File => {
            let Some(f) = b.file_input else {
                return Err(missing("File"));
            };
            BlockKind::File {
                file_input: FileInput {
                    repository_name: f.repository_name.trim().to_owned(),
                    file_path: f.file_path.trim().trim_start_matches('/').to_owned(),
                    revision: f.revision.filter(|r| !r.trim().is_empty()),
                    line_range: f.line_range.map(|r| LineRange {
                        start_line: r.start_line,
                        end_line: r.end_line,
                    }),
                },
            }
        }
        NotebookBlockType::Symbol => {
            let Some(s) = b.symbol_input else {
                return Err(missing("Symbol"));
            };
            BlockKind::Symbol {
                symbol_input: SymbolInput {
                    repository_name: s.repository_name.trim().to_owned(),
                    file_path: s.file_path.trim().trim_start_matches('/').to_owned(),
                    revision: s.revision.filter(|r| !r.trim().is_empty()),
                    line_context: s.line_context,
                    symbol_name: s.symbol_name,
                    symbol_container_name: s.symbol_container_name,
                    symbol_kind: s.symbol_kind.name().to_owned(),
                },
            }
        }
    };
    Ok(Block { id: b.id, kind })
}

/// Converts and checks a notebook input; the namespace must exist.
async fn convert_input(ctx: &Context<'_>, input: NotebookInputObject) -> Result<NotebookInput> {
    let namespace = parse_namespace_id(&input.namespace)?;
    if load_namespace(ctx, namespace).await?.is_none() {
        return Err(error(codes::NOT_FOUND, "Namespace not found."));
    }
    let blocks = input
        .blocks
        .into_iter()
        .map(convert_block)
        .collect::<Result<Vec<_>>>()?;
    Ok(NotebookInput {
        title: input.title.trim().to_owned(),
        blocks,
        public: input.public,
        namespace,
    })
}

fn check(input: &NotebookInput) -> Result<()> {
    let problems = sourcegull_notebooks::blocks::validate(&input.title, &input.blocks);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(validation(problems))
    }
}

/// Notebook queries.
#[derive(Default)]
pub struct NotebooksQuery;

#[Object]
impl NotebooksQuery {
    /// All notebooks the viewer may see.
    #[allow(clippy::too_many_arguments)]
    async fn notebooks(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50, desc = "Returns the first n notebooks from the list.")]
        first: Option<i32>,
        #[graphql(desc = "Opaque pagination cursor.")] after: Option<String>,
        #[graphql(desc = "Query to filter notebooks by title and blocks content.")] query: Option<
            String,
        >,
        #[graphql(
            name = "creatorUserID",
            desc = "Filter to notebooks made by a single creator."
        )]
        creator_user_id: Option<ID>,
        #[graphql(
            name = "starredByUserID",
            desc = "Filter to notebooks that were starred by the user."
        )]
        starred_by_user_id: Option<ID>,
        #[graphql(
            desc = "Filter to notebooks associated with a specific namespace (user or org)."
        )]
        namespace: Option<ID>,
        #[graphql(
            default_with = "Some(NotebooksOrderBy::NotebookUpdatedAt)",
            desc = "Sort field."
        )]
        order_by: Option<NotebooksOrderBy>,
        #[graphql(default_with = "Some(false)", desc = "Sort direction.")] descending: Option<bool>,
    ) -> Result<NotebookConnection> {
        require_reader(ctx)?;
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let opts = ListNotebooks {
            access: access(ctx),
            query: query.filter(|q| !q.trim().is_empty()),
            creator: creator_user_id.map(|id| parse_user_id(&id)).transpose()?,
            starred_by: starred_by_user_id
                .map(|id| parse_user_id(&id))
                .transpose()?,
            namespace: namespace.map(|id| parse_namespace_id(&id)).transpose()?,
            order: match order_by.unwrap_or_default() {
                NotebooksOrderBy::NotebookUpdatedAt => NotebookOrder::UpdatedAt,
                NotebooksOrderBy::NotebookCreatedAt => NotebookOrder::CreatedAt,
                NotebooksOrderBy::NotebookStarCount => NotebookOrder::StarCount,
            },
            descending: descending.unwrap_or(false),
            limit: limit + 1,
            offset,
        };
        let (rows, total) = store(ctx)?.list(&opts).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, rows.len());
        let mut nodes: Vec<_> = rows.into_iter().map(NotebookNode).collect();
        nodes.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(NotebookConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }

    /// The notebook with this ID, if the viewer may see it (also available as `node`, which
    /// needs a signed-in viewer; this one serves anonymous readers of public notebooks).
    async fn notebook(&self, ctx: &Context<'_>, id: ID) -> Result<Option<NotebookNode>> {
        require_reader(ctx)?;
        let Ok(id) = parse_notebook_id(&id) else {
            return Ok(None);
        };
        load_node(ctx, id).await
    }
}

/// Notebook mutations.
#[derive(Default)]
pub struct NotebooksMutation;

#[Object]
impl NotebooksMutation {
    /// Create a notebook.
    async fn create_notebook(
        &self,
        ctx: &Context<'_>,
        notebook: NotebookInputObject,
    ) -> Result<NotebookNode> {
        let actor = require_viewer(ctx)?;
        let input = convert_input(ctx, notebook).await?;
        require_can_manage(ctx, actor, input.namespace).await?;
        check(&input)?;
        let notebook = store(ctx)?
            .create(&input, actor.user_id)
            .await
            .map_err(db_error)?;
        log_backend(
            ctx,
            sourcegull_analytics::names::NOTEBOOK_CREATED,
            serde_json::json!({}),
        );
        Ok(NotebookNode(notebook))
    }

    /// Update a notebook. Only users who can manage it can, and moving it to another
    /// namespace needs the right to manage notebooks there too. Blocks the viewer was not
    /// shown (in repositories they may not read) are kept.
    async fn update_notebook(
        &self,
        ctx: &Context<'_>,
        id: ID,
        notebook: NotebookInputObject,
    ) -> Result<NotebookNode> {
        let actor = require_viewer(ctx)?;
        let old = load_visible(ctx, &id).await?;
        require_can_manage(ctx, actor, old.namespace).await?;
        let mut input = convert_input(ctx, notebook).await?;
        require_can_manage(ctx, actor, input.namespace).await?;
        let access = repo_access(ctx).await?;
        let hidden = sourcegull_notebooks::hidden_blocks(&services(ctx)?.db, &access, &old.blocks)
            .await
            .map_err(db_error)?;
        input.blocks =
            sourcegull_notebooks::blocks::merge_hidden(&old.blocks, input.blocks, &hidden);
        check(&input)?;
        Ok(NotebookNode(
            store(ctx)?
                .update(old.id, &input, actor.user_id)
                .await
                .map_err(db_error)?,
        ))
    }

    /// Delete a notebook. Only users who can manage it can.
    async fn delete_notebook(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let old = load_visible(ctx, &id).await?;
        require_can_manage(ctx, actor, old.namespace).await?;
        store(ctx)?.delete(old.id).await.map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Star a notebook for the current user (starring twice keeps one star).
    async fn create_notebook_star(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "notebookID")] notebook_id: ID,
    ) -> Result<NotebookStarNode> {
        let actor = require_viewer(ctx)?;
        let nb = load_visible(ctx, &notebook_id).await?;
        Ok(NotebookStarNode(
            store(ctx)?
                .create_star(nb.id, actor.user_id)
                .await
                .map_err(db_error)?,
        ))
    }

    /// Delete the notebook star for the current user, if it exists.
    async fn delete_notebook_star(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "notebookID")] notebook_id: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let nb = load_visible(ctx, &notebook_id).await?;
        store(ctx)?
            .delete_star(nb.id, actor.user_id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }
}
