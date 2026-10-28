//! The repository read rule of every resolver that loads a repository by ID.

use async_graphql::{Context, Result};
use sourcegull_types::{Repo, RepoId};

use crate::context::{codes, db_error, error, repo_access, services, viewer};

/// Requires a signed-in viewer, unless `auth.public` allows anonymous access.
pub fn require_reader(ctx: &Context<'_>) -> Result<()> {
    if viewer(ctx).is_some() || services(ctx)?.config.load().value.auth_public {
        Ok(())
    } else {
        Err(error(codes::UNAUTHORIZED, "You must be signed in."))
    }
}

/// Loads repository `id` if the viewer may read it; `None` when it is missing, unreadable or
/// out of range.
pub async fn load_readable(ctx: &Context<'_>, id: i64) -> Result<Option<Repo>> {
    require_reader(ctx)?;
    let Ok(id) = i32::try_from(id) else {
        return Ok(None);
    };
    let access = repo_access(ctx).await?;
    services(ctx)?
        .db
        .repos()
        .get_authorized(RepoId(id), &access)
        .await
        .map_err(db_error)
}
