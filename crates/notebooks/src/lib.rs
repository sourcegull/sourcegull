//! Notebooks: documents of Markdown, search query, file and symbol
//! blocks, owned by a user or an org, private or public, starred, and found through a
//! full-text search over their title and text.
//!
//! - [`access`]: who may see notebooks and who may manage them.
//! - [`blocks`]: the block kinds in their stored shape, validation, and
//!   [`merge_hidden`](blocks::merge_hidden), which keeps blocks a viewer could not see when
//!   they save.
//! - [`store`]: notebooks and stars in Postgres, with the visibility rule applied in SQL.
//! - [`hidden_blocks`]: which file and symbol blocks refer to repositories the viewer may not
//!   read; the API leaves them out so a notebook never reveals such a repository.
//!
//! The `.snb.md` export and import live in the web UI.

pub mod access;
pub mod blocks;
pub mod store;

use std::collections::{HashMap, HashSet};

use sourcegull_db::{Db, DbError};
use sourcegull_types::RepoAccess;

pub use blocks::{Block, BlockKind, FileInput, LineRange, SymbolInput, TextInput};
pub use store::{
    Access, ListNotebooks, Notebook, NotebookInput, NotebookOrder, NotebookStar, NotebookStore,
};

/// The IDs of the file and symbol blocks in `blocks` whose repository exists but `access`
/// may not read. Blocks naming a repository that does not exist stay visible: they reveal
/// nothing, and their author must be able to fix them.
pub async fn hidden_blocks(
    db: &Db,
    access: &RepoAccess,
    blocks: &[Block],
) -> Result<HashSet<String>, DbError> {
    let mut hidden = HashSet::new();
    if access.is_all() {
        return Ok(hidden);
    }
    let mut readable: HashMap<String, bool> = HashMap::new();
    for b in blocks {
        let Some(name) = b.repository_name() else {
            continue;
        };
        let key = name.to_lowercase();
        let ok = match readable.get(&key) {
            Some(ok) => *ok,
            None => {
                // The unfiltered lookup only tells an unreadable repository (hidden) from a
                // missing one (shown); nothing about the repository is returned.
                #[allow(clippy::disallowed_methods)]
                let repo = db.repos().get_by_name(name).await?;
                let ok = match repo {
                    Some(repo) => access.can_read(repo.id),
                    None => true,
                };
                readable.insert(key, ok);
                ok
            }
        };
        if !ok {
            hidden.insert(b.id.clone());
        }
    }
    Ok(hidden)
}
