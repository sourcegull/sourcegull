//! Late-bound references to the shared object types.
//!
//! `User`, `Org`, `Repository` and the `Namespace` and `TeamMember` interfaces are defined in the schema
//! assembly (`sourcegull-graphql`), and their fields reach into many domains. A domain crate that
//! returns one of them returns a reference ([`UserRef`], [`OrgRef`], [`RepositoryRef`],
//! [`NamespaceRef`], [`TeamMemberRef`]) instead: an output type with the real type's GraphQL name that holds the
//! loaded row and, when resolved, hands it to the [`SharedTypes`] the assembly installs as
//! schema data. The types bulk changes return (`Person`, `Executor`, a branch `GitRef`,
//! `GitBlob`, `VirtualFile`, `File2`) are resolved the same way.
//!
//! A reference does not register a type: its `create_type_info` only returns the name. The
//! assembly registers the real types (`SchemaBuilder::register_output_type`), so the
//! registry holds exactly one definition per name and the schema is the same as if the
//! domain returned the real type.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_graphql::Context;
use async_graphql::parser::types::Field;
use async_graphql::registry::Registry;
use async_graphql::{ContextSelectionSet, OutputType, Positioned, ServerResult, Value};
use sourcegull_db::Namespace;
use sourcegull_types::{Org, Repo, User};

use crate::context::{db_error, services};

/// The future of resolving a shared object.
pub type ResolveFuture<'a> = Pin<Box<dyn Future<Output = ServerResult<Value>> + Send + 'a>>;

/// Resolves the shared object types; implemented by the schema assembly and installed as
/// schema data (`Arc<dyn SharedTypes>`).
pub trait SharedTypes: Send + Sync {
    /// Resolves `field`'s selection on the `User` `user`.
    fn user<'a, 'c>(
        &'a self,
        user: &'a User,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on the `Org` `org`.
    fn org<'a, 'c>(
        &'a self,
        org: &'a Org,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on the `Repository` `repo`.
    fn repository<'a, 'c>(
        &'a self,
        repo: &'a Repo,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on the `TeamMember` `user`.
    fn team_member<'a, 'c>(
        &'a self,
        user: &'a User,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on the `Namespace` `namespace`.
    fn namespace<'a, 'c>(
        &'a self,
        namespace: &'a NamespaceValue,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on a `Person`.
    fn person<'a, 'c>(
        &'a self,
        person: &'a PersonData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on an `Executor`.
    fn executor<'a, 'c>(
        &'a self,
        executor: &'a sourcegull_db::Executor,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on a `GitRef` naming a branch.
    fn branch<'a, 'c>(
        &'a self,
        branch: &'a BranchData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on a `GitBlob`.
    fn git_blob<'a, 'c>(
        &'a self,
        blob: &'a GitBlobData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on a `VirtualFile`.
    fn virtual_file<'a, 'c>(
        &'a self,
        file: &'a VirtualFileData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;

    /// Resolves `field`'s selection on a `File2`.
    fn file2<'a, 'c>(
        &'a self,
        file: &'a File2Data,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;
}

/// A loaded namespace: a user or an organization.
#[derive(Debug, Clone)]
pub enum NamespaceValue {
    /// A user.
    User(User),
    /// An organization.
    Org(Org),
}

/// Loads a live namespace (a user, or an organization that is not deleted), or `None`.
pub async fn load_namespace(
    ctx: &Context<'_>,
    ns: Namespace,
) -> async_graphql::Result<Option<NamespaceValue>> {
    let db = &services(ctx)?.db;
    Ok(match ns {
        Namespace::User(id) => db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(NamespaceValue::User),
        Namespace::Org(id) => db
            .orgs()
            .get(id)
            .await
            .map_err(db_error)?
            .filter(|o| o.deleted_at.is_none())
            .map(NamespaceValue::Org),
    })
}

macro_rules! shared_ref {
    ($(#[$meta:meta])* $name:ident($inner:ty) = $gql:literal, $method:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone)]
        pub struct $name(pub $inner);

        impl OutputType for $name {
            fn type_name() -> Cow<'static, str> {
                Cow::Borrowed($gql)
            }

            fn create_type_info(_registry: &mut Registry) -> String {
                Self::qualified_type_name()
            }

            fn resolve(
                &self,
                ctx: &ContextSelectionSet<'_>,
                field: &Positioned<Field>,
            ) -> impl Future<Output = ServerResult<Value>> + Send {
                async move {
                    let shared = ctx
                        .data::<Arc<dyn SharedTypes>>()
                        .map_err(|e| e.into_server_error(field.pos))?;
                    shared.$method(&self.0, ctx, field).await
                }
            }
        }
    };
}

shared_ref!(
    /// A `User`.
    UserRef(User) = "User",
    user
);
shared_ref!(
    /// An `Org`.
    OrgRef(Org) = "Org",
    org
);
shared_ref!(
    /// A `Repository` (its repo node state is loaded when a field needs it).
    RepositoryRef(Repo) = "Repository",
    repository
);
shared_ref!(
    /// A `Namespace` (the interface; resolved as its `User` or `Org`).
    NamespaceRef(NamespaceValue) = "Namespace",
    namespace
);
shared_ref!(
    /// A `TeamMember` (the interface; resolved as its `User`).
    TeamMemberRef(User) = "TeamMember",
    team_member
);

/// A file's content, computed when it is first asked for (the content of a `VirtualFile`).
pub type ContentLoader = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = async_graphql::Result<String>> + Send>> + Send + Sync,
>;

/// The fields of a `Person` (a commit or changeset author).
#[derive(Debug, Clone)]
pub struct PersonData {
    pub name: String,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    /// The user account of this person, when known.
    pub user: Option<User>,
}

/// A branch of a repository at a commit, without asking the repo node.
#[derive(Debug, Clone)]
pub struct BranchData {
    pub repo: Repo,
    /// `refs/heads/…` or a short name.
    pub name: String,
    pub commit_oid: String,
}

/// A file at a commit (not checked to exist).
#[derive(Debug, Clone)]
pub struct GitBlobData {
    pub repo: Repo,
    pub commit_oid: String,
    /// The revision the file was reached by, which its URL keeps; the commit when unset.
    pub rev: Option<String>,
    pub path: String,
}

/// A file that is not in a repository, whose content `loader` computes on first use.
#[derive(Clone)]
pub struct VirtualFileData {
    pub path: String,
    pub url: String,
    pub loader: ContentLoader,
}

impl std::fmt::Debug for VirtualFileData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualFileData")
            .field("path", &self.path)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// A `File2`: a file in a repository or a virtual file.
#[derive(Debug, Clone)]
pub enum File2Data {
    GitBlob(Box<GitBlobData>),
    VirtualFile(VirtualFileData),
}

shared_ref!(
    /// A `Person`.
    PersonRef(PersonData) = "Person",
    person
);
shared_ref!(
    /// An `Executor`.
    ExecutorRef(sourcegull_db::Executor) = "Executor",
    executor
);
shared_ref!(
    /// A `GitRef` naming a branch.
    BranchRef(BranchData) = "GitRef",
    branch
);
shared_ref!(
    /// A `GitBlob`.
    GitBlobRef(GitBlobData) = "GitBlob",
    git_blob
);
shared_ref!(
    /// A `VirtualFile`.
    VirtualFileRef(VirtualFileData) = "VirtualFile",
    virtual_file
);
shared_ref!(
    /// A `File2`.
    File2Ref(File2Data) = "File2",
    file2
);
