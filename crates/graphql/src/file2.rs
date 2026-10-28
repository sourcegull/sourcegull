//! `File2`: a file that is a git blob (`GitBlob`) or a file that only exists in the database
//! (`VirtualFile`), as CODEOWNERS reasons and bulk change previews return it. `File2` has only
//! the fields shared by `GitBlob` and `VirtualFile` (path, name, isDirectory, url,
//! canonicalURL), plus `VirtualFile.content`.

use std::sync::Arc;

use async_graphql::{Interface, Object, Result};
use sourcegull_graphql_core::shared::ContentLoader;

use crate::git::GitBlobNode;

/// A file that is a git blob or a file that only exists in the database.
// The interface fields repeat `ty = "String"`, which clippy mistakes for duplicate attributes.
#[allow(clippy::duplicated_attributes, clippy::large_enum_variant)]
#[derive(Interface, Clone)]
#[graphql(
    name = "File2",
    field(name = "path", ty = "String", desc = "Path from the repository root."),
    field(name = "name", ty = "String", desc = "The last path component."),
    field(name = "is_directory", ty = "bool", desc = "Always false."),
    field(name = "url", ty = "String", desc = "The file's page."),
    field(
        name = "canonicalURL",
        method = "canonical_url",
        ty = "String",
        desc = "The file's page at a fixed commit."
    )
)]
pub enum File2 {
    GitBlob(GitBlobNode),
    VirtualFile(VirtualFileNode),
}

/// A file that is not in a repository (an ingested CODEOWNERS file, or the new version of a
/// file in a changeset spec's diff).
#[derive(Clone)]
pub struct VirtualFileNode {
    path: String,
    url: String,
    content: Arc<tokio::sync::OnceCell<String>>,
    loader: ContentLoader,
}

impl VirtualFileNode {
    /// A file whose content `loader` computes on first use (once).
    pub fn lazy(path: impl Into<String>, url: impl Into<String>, loader: ContentLoader) -> Self {
        Self {
            path: path.into(),
            url: url.into(),
            content: Arc::new(tokio::sync::OnceCell::new()),
            loader,
        }
    }

    async fn text(&self) -> Result<&String> {
        self.content.get_or_try_init(|| (self.loader)()).await
    }
}

#[Object(name = "VirtualFile")]
impl VirtualFileNode {
    async fn path(&self) -> String {
        self.path.clone()
    }

    async fn name(&self) -> String {
        self.path.rsplit('/').next().unwrap_or("").to_owned()
    }

    async fn is_directory(&self) -> bool {
        false
    }

    /// Where the file is shown: the admin page of an ingested CODEOWNERS file; empty for the
    /// new version of a file in a changeset spec's diff, which has no page.
    async fn url(&self) -> &str {
        &self.url
    }

    #[graphql(name = "canonicalURL")]
    async fn canonical_url(&self) -> &str {
        &self.url
    }

    /// The file's text.
    async fn content(&self) -> Result<&str> {
        Ok(self.text().await?.as_str())
    }

    /// The size in bytes.
    async fn byte_size(&self) -> Result<i32> {
        Ok(i32::try_from(self.text().await?.len()).unwrap_or(i32::MAX))
    }
}
