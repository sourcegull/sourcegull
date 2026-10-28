//! The shared object types behind the late-bound references of `sourcegull-graphql-core`
//! (`UserRef`, `OrgRef`, `RepositoryRef`, `NamespaceRef`, `TeamMemberRef`): [`register`] registers
//! `User`, `Org`, `Repository`, `Namespace` and `TeamMember` (and `Person`, `Executor`,
//! `GitRef`, `GitBlob`, `VirtualFile` and `File2`, which bulk changes return) and installs the
//! resolver that turns a reference into its full type.

use std::sync::Arc;

use async_graphql::parser::types::Field;
use async_graphql::{ContextSelectionSet, OutputType, Positioned};
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::shared::{
    BranchData, File2Data, GitBlobData, NamespaceValue, PersonData, ResolveFuture, SharedTypes,
    VirtualFileData,
};
use sourcegull_types::{Org, Repo, User};

use crate::Builder;
use crate::file2::{File2, VirtualFileNode};
use crate::git::{GitBlobNode, GitRefNode, Person, RepoHandle};
use crate::namespace::NamespaceNode;
use crate::orgs::OrgNode;
use crate::repos::RepositoryNode;
use crate::users::{TeamMember, UserNode};
use sourcegull_executors_graphql::executors::ExecutorNode;

/// Registers the shared object types and installs their resolver as schema data.
pub(crate) fn register<F: GraphqlFeature>(builder: Builder<F>) -> Builder<F> {
    let shared: Arc<dyn SharedTypes> = Arc::new(Shared);
    builder
        .register_output_type::<UserNode>()
        .register_output_type::<OrgNode>()
        .register_output_type::<RepositoryNode>()
        .register_output_type::<NamespaceNode>()
        .register_output_type::<TeamMember>()
        .register_output_type::<Person>()
        .register_output_type::<ExecutorNode>()
        .register_output_type::<GitRefNode>()
        .register_output_type::<GitBlobNode>()
        .register_output_type::<VirtualFileNode>()
        .register_output_type::<File2>()
        .data(shared)
}

struct Shared;

impl SharedTypes for Shared {
    fn user<'a, 'c>(
        &'a self,
        user: &'a User,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = UserNode(user.clone());
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn org<'a, 'c>(
        &'a self,
        org: &'a Org,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = OrgNode(org.clone());
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn repository<'a, 'c>(
        &'a self,
        repo: &'a Repo,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = RepositoryNode {
            repo: repo.clone(),
            gitserver: None,
        };
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn team_member<'a, 'c>(
        &'a self,
        user: &'a User,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = TeamMember::User(UserNode(user.clone()));
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn namespace<'a, 'c>(
        &'a self,
        namespace: &'a NamespaceValue,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = NamespaceNode::from(namespace.clone());
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn person<'a, 'c>(
        &'a self,
        person: &'a PersonData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = Person {
            name: person.name.clone(),
            email: person.email.clone(),
            display_name: person.display_name.clone(),
            avatar_url: person.avatar_url.clone(),
            user: person.user.clone().map(UserNode),
        };
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn executor<'a, 'c>(
        &'a self,
        executor: &'a sourcegull_db::Executor,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = ExecutorNode(executor.clone());
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn branch<'a, 'c>(
        &'a self,
        branch: &'a BranchData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        Box::pin(async move {
            let h = repo_handle(ctx, &branch.repo).map_err(|e| e.into_server_error(field.pos))?;
            let node = GitRefNode::branch(h, &branch.name, &branch.commit_oid);
            OutputType::resolve(&node, ctx, field).await
        })
    }

    fn git_blob<'a, 'c>(
        &'a self,
        blob: &'a GitBlobData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        Box::pin(async move {
            let node = git_blob(ctx, blob).map_err(|e| e.into_server_error(field.pos))?;
            OutputType::resolve(&node, ctx, field).await
        })
    }

    fn virtual_file<'a, 'c>(
        &'a self,
        file: &'a VirtualFileData,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        let node = virtual_file(file);
        Box::pin(async move { OutputType::resolve(&node, ctx, field).await })
    }

    fn file2<'a, 'c>(
        &'a self,
        file: &'a File2Data,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        Box::pin(async move {
            let node = match file {
                File2Data::GitBlob(b) => {
                    File2::GitBlob(git_blob(ctx, b).map_err(|e| e.into_server_error(field.pos))?)
                }
                File2Data::VirtualFile(f) => File2::VirtualFile(virtual_file(f)),
            };
            OutputType::resolve(&node, ctx, field).await
        })
    }
}

/// A handle for `repo` from the schema's repo node clients.
fn repo_handle(ctx: &ContextSelectionSet<'_>, repo: &Repo) -> async_graphql::Result<RepoHandle> {
    let clients = ctx
        .data::<sourcegull_graphql_core::Services>()?
        .repo_clients
        .clone();
    Ok(RepoHandle {
        git: sourcegull_services::git::GitReader::new(clients, repo),
        repo: Arc::new(repo.clone()),
    })
}

fn git_blob(
    ctx: &ContextSelectionSet<'_>,
    blob: &GitBlobData,
) -> async_graphql::Result<GitBlobNode> {
    Ok(GitBlobNode::at(
        repo_handle(ctx, &blob.repo)?,
        &blob.commit_oid,
        blob.rev.as_deref(),
        &blob.path,
    ))
}

fn virtual_file(file: &VirtualFileData) -> VirtualFileNode {
    VirtualFileNode::lazy(file.path.clone(), file.url.clone(), file.loader.clone())
}
