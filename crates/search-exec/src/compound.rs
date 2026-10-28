//! Packing small repositories into compound shards.
//!
//! [`ShardIndex::merge_compounds`] runs as maintenance after indexing. It
//! takes the repositories with their own shard under `max_repo_bytes`, plus
//! every repository of compound shards that are small (under half of
//! `max_shard_bytes`) or dirty (more than a third of their docs no longer
//! served), and repacks them by repository id into new compound shards of
//! up to `max_shard_bytes` ([`CompoundOptions`](crate::CompoundOptions)).
//! Docs are copied from the shards, deltas folded in, so no git access is
//! needed. Nothing is written unless at least two files are merged or a
//! dirty compound is cleaned, so a settled index stays as it is.
//!
//! Each new compound is swapped in before any file it replaces is deleted,
//! and it has a higher sequence number than the compounds it replaces, so a
//! crash at any step leaves files that open to the same index.

use std::collections::BTreeMap;
use std::sync::Arc;

use sourcegull_index_format::Shard;
use sourcegull_indexer::{CompoundPart, build_compound};
use sourcegull_search::index::IndexError;

use crate::indexed::{
    Compound, RepoShards, ShardIndex, dead_path, format_error, indexer_error, remove_file,
};

/// What [`ShardIndex::merge_compounds`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Compound shards written.
    pub shards: usize,
    /// Repositories packed into them.
    pub repos: usize,
    /// Bytes written.
    pub bytes: u64,
    /// Shard files deleted (merged, or compounds no repository uses).
    pub removed: usize,
}

impl ShardIndex {
    /// Packs small repositories into compound shards; see the module docs.
    pub fn merge_compounds(&self) -> Result<MergeReport, IndexError> {
        let mut compounds = self.lock();
        let shards = self.shards.load_full();
        let mut report = MergeReport::default();
        let mut live: BTreeMap<u64, Vec<&Arc<RepoShards>>> =
            compounds.keys().map(|&seq| (seq, Vec::new())).collect();
        let mut singles = Vec::new();
        for rs in shards.values() {
            match rs.compound {
                Some(seq) => live.entry(seq).or_default().push(rs),
                None if rs.size_bytes() < self.compound.max_repo_bytes => singles.push(rs),
                None => {}
            }
        }
        // Compounds no repository is served from any more.
        for (seq, repos) in &live {
            if repos.is_empty() {
                self.remove_compound(&mut compounds, *seq)?;
                report.removed += 1;
            }
        }
        live.retain(|_, repos| !repos.is_empty());
        let mut dirty = false;
        let mut merged: Vec<u64> = Vec::new();
        for (&seq, repos) in &live {
            let base = &repos[0].base;
            let served: u64 = repos.iter().map(|rs| rs.base_docs()).sum();
            let is_dirty = 3 * (u64::from(base.doc_count()) - served) > u64::from(base.doc_count());
            if is_dirty || base.size_bytes() < self.compound.max_shard_bytes / 2 {
                dirty |= is_dirty;
                merged.push(seq);
            }
        }
        if singles.len() + merged.len() < 2 && !dirty {
            return Ok(report);
        }

        let mut repos: Vec<&Arc<RepoShards>> = singles.clone();
        for seq in &merged {
            repos.extend(live[seq].iter().copied());
        }
        repos.sort_by_key(|rs| rs.meta().id);
        let mut groups: Vec<Vec<&Arc<RepoShards>>> = Vec::new();
        let mut size = 0u64;
        for rs in repos {
            let bytes = rs.size_bytes();
            match groups.last_mut() {
                Some(g) if size + bytes <= self.compound.max_shard_bytes => g.push(rs),
                _ => {
                    groups.push(vec![rs]);
                    size = 0;
                }
            }
            size += bytes;
        }

        let first = compounds.keys().last().map_or(0, |s| s + 1);
        for (seq, group) in (first..).zip(groups) {
            // A dead list left by a compound that once had this number.
            remove_file(&dead_path(&self.dir, seq))?;
            let parts: Vec<CompoundPart<'_>> = group
                .iter()
                .map(|rs| CompoundPart {
                    base: &rs.base,
                    repo: rs.repo,
                    delta: rs.delta.as_deref(),
                })
                .collect();
            let out = self.compound_path(seq);
            let stats = build_compound(&parts, &self.opts, &out).map_err(indexer_error)?;
            let shard = Arc::new(Shard::open(&out).map_err(format_error)?);
            let ids: Vec<i32> = group.iter().map(|rs| rs.meta().id).collect();
            self.shards.rcu(|m| {
                let mut m = BTreeMap::clone(m);
                for (i, id) in ids.iter().enumerate() {
                    let rs = RepoShards {
                        base: shard.clone(),
                        repo: i as u16,
                        compound: Some(seq),
                        delta: None,
                    };
                    m.insert(*id, Arc::new(rs));
                }
                m
            });
            let dead = Default::default();
            compounds.insert(seq, Compound { shard, dead });
            report.shards += 1;
            report.repos += group.len();
            report.bytes += stats.shard_bytes;
        }

        // Own shards before their deltas: a delta left alone fails to open
        // (its base is gone) and is dropped, while a base left without its
        // delta would lose the newer docs.
        for rs in &singles {
            let id = rs.meta().id;
            report.removed += usize::from(remove_file(&self.base_path(id))?);
            remove_file(&self.delta_path(id))?;
        }
        for seq in merged {
            for rs in &live[&seq] {
                remove_file(&self.delta_path(rs.meta().id))?;
            }
            self.remove_compound(&mut compounds, seq)?;
            report.removed += 1;
        }
        Ok(report)
    }

    fn remove_compound(
        &self,
        compounds: &mut BTreeMap<u64, Compound>,
        seq: u64,
    ) -> Result<(), IndexError> {
        remove_file(&self.compound_path(seq))?;
        remove_file(&dead_path(&self.dir, seq))?;
        compounds.remove(&seq);
        Ok(())
    }
}
