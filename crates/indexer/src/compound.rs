//! Compound shards: many small repositories packed
//! into one shard, merged from the shards that already index them, so no
//! git access is needed.

use std::path::Path;
use std::time::Instant;

use roaring::RoaringBitmap;
use sourcegull_gitstore::parallel::map_ordered_with;
use sourcegull_index_format::{Doc, RepoMeta, Shard, ShardWriter, prepare_copy};

use crate::{BATCH, BuildOptions, BuildStats, Result};

/// One repository's current docs: its docs in `base` (a single-repo or
/// compound shard) except those `delta` hides, plus the delta's docs.
#[derive(Debug, Clone, Copy)]
pub struct CompoundPart<'a> {
    pub base: &'a Shard,
    /// The repository's index in `base.repos()`.
    pub repo: u16,
    /// A delta shard over `base` for this repository.
    pub delta: Option<&'a Shard>,
}

impl CompoundPart<'_> {
    /// The repository as searched: the delta's metadata when there is one.
    pub fn meta(&self) -> &RepoMeta {
        match self.delta {
            Some(d) => &d.repos()[0],
            None => &self.base.repos()[usize::from(self.repo)],
        }
    }

    /// The repository's current docs with their shards, in (path, oid)
    /// order.
    fn docs(&self) -> Result<Vec<(&Shard, Doc)>> {
        let hidden = self.delta.and_then(|d| d.tombstones()).map(|(_, h)| h);
        let range = self.base.repo_docs(self.repo);
        let base = keyed(self.base, range.start, range.end, hidden)?;
        let delta = match self.delta {
            Some(d) => keyed(d, 0, d.doc_count(), None)?,
            None => Vec::new(),
        };
        // Merge by (path, oid); a path is either all in the base or all in
        // the delta, since the delta holds every version of a changed path.
        let mut out = Vec::with_capacity(base.len() + delta.len());
        let mut a = base.into_iter().peekable();
        let mut b = delta.into_iter().peekable();
        loop {
            let take_a = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => (&x.0, x.2.oid) <= (&y.0, y.2.oid),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => return Ok(out),
            };
            let (_, shard, doc) = if take_a { a.next() } else { b.next() }.expect("peeked");
            out.push((shard, doc));
        }
    }
}

/// The docs `start..end` of `shard` not in `hidden`, with their paths.
fn keyed<'s>(
    shard: &'s Shard,
    start: u32,
    end: u32,
    hidden: Option<&RoaringBitmap>,
) -> Result<Vec<(String, &'s Shard, Doc)>> {
    let mut out = Vec::new();
    shard.walk_docs(start, &mut |d, p| {
        if d.id >= end {
            return std::ops::ControlFlow::Break(());
        }
        if hidden.is_none_or(|h| !h.contains(d.id)) {
            out.push((p.to_owned(), shard, d));
        }
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// Writes one shard holding every part's repository, in the order given,
/// atomically to `out`. Doc contents are copied, not read from git.
pub fn build_compound(
    parts: &[CompoundPart<'_>],
    opts: &BuildOptions,
    out: &Path,
) -> Result<BuildStats> {
    let start = Instant::now();
    let mut writer = ShardWriter::new(opts.codec).with_threads(opts.threads);
    // The compound's symbols are as old as its oldest part's.
    let version = parts
        .iter()
        .flat_map(|p| std::iter::once(p.base).chain(p.delta))
        .map(Shard::symbols_version)
        .min()
        .unwrap_or(0);
    writer.set_symbols_version(version);
    let mut stats = BuildStats::default();
    for part in parts {
        let repo = writer.add_repo(part.meta().clone())?;
        writer.set_ranks_generation(repo, part.base.ranks_generation(part.repo))?;
        let mut rest = part.docs()?;
        while !rest.is_empty() {
            let tail = rest.split_off(rest.len().min(BATCH));
            let batch = std::mem::replace(&mut rest, tail);
            let docs = map_ordered_with(
                batch,
                opts.threads,
                || (),
                |_, (shard, doc)| prepare_copy(shard, &doc, repo, opts.codec),
            )
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
            stats.count(&docs);
            writer.push_batch(docs)?;
        }
    }
    stats.shard_bytes = writer.finish(out)?;
    stats.elapsed = start.elapsed();
    Ok(stats)
}
