//! Ranking from the code graph.
//!
//! A leader-elected job in the `worker` role ([`job`]) exports the reference graph of the SCIP
//! uploads serving each repository's default branch head ([`export`]): file → file edges
//! weighted by reference count, within an upload and across uploads and repositories through
//! global symbols. It runs sparse PageRank over the graph ([`pagerank`]: power iteration,
//! `f32`, CSR adjacency, parallel with rayon, damping 0.85) and stores each repository's file
//! ranks in `ranking_path_ranks`, normalized to `[0, 1]` on a log scale.
//!
//! Repo nodes write the ranks into the shards' RANKS section at the next full build (delta
//! shards keep the base's ranks), and indexed search adds them to the file score as a
//! tie-breaker (`sourcegull_search::ranking::doc_rank_score`).
//!
//! PageRank is computed in memory rather than as a plain reference count, so a reference from
//! a well-referenced file counts more than one from an unused file.

pub mod export;
mod feature;
pub mod job;
pub mod pagerank;
pub mod store;

pub use export::{ReferenceGraph, UploadExport, assemble, export_upload};
pub use feature::RankingFeature;
pub use job::{PassOutcome, Ranker, RankingError, RankingReport, compute_ranks, run_ranker};
pub use pagerank::{Graph, GraphBuilder, Options, Ranks, normalize, pagerank};
