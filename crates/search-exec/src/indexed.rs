//! Indexed search: query evaluation on shards and
//! [`ShardIndex`], the [`SearchIndex`] over a directory of shards.
//!
//! A repository has a base shard (its own, or its part of a compound shard
//! packing small repositories) and, after later commits, at most
//! one delta shard; its candidates merge with the base's (minus
//! the tombstoned docs) in path order, so results do not depend on how the
//! index was built. In a compound shard the trigram query is evaluated
//! once per search for all of its repositories.
//!
//! Per shard: the pattern's trigram query picks candidate docs from the
//! postings (rarest lists first), the branch, `file:` and `lang:` filters
//! run on the doc table and paths, and the candidates are verified with the
//! same [`Matcher`] and [`chunk_matches`] unindexed search uses, in parallel
//! batches, so both backends return the same matches.
//!
//! Symbol search uses the same candidates (a symbol's name is in
//! the content, so content trigrams are a sound prefilter) and matches the
//! names in the SYMBOLS section, with positions from the newline table;
//! contents are not read.
//!
//! Ranked results add each doc's static rank from the shard's RANKS section
//! to the file score as a tie-breaker.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use roaring::RoaringBitmap;
use sourcegull_gitstore::ObjectId;
use sourcegull_index_format::{
    BranchMeta, Doc, EXTENSION, FLAG_TOO_LARGE, FormatError, RepoMeta, Shard,
};
use sourcegull_indexer::{BuildOptions, IndexerError, KnownSymbols, ShardSpec, build_shard};
use sourcegull_search::Matcher;
use sourcegull_search::chunks::SymbolMatch;
use sourcegull_search::index::{
    Branch, BuildRequest, Cancel, DEFAULT_BRANCH, FLUSH_WALL_TIME, IndexError, IndexQuery,
    IndexSearchStats, IndexStats, IndexedFile, IndexedRepo, LanguageCount, MAX_BRANCHES,
    RepoIndexStatus, SearchIndex, ShardInfo,
};
use sourcegull_search::matcher::merge;
use sourcegull_types::{AlwaysIndex, RepoId};

use crate::pool::Lane;
use crate::ranking::{self, RANK_CAP, Scorer, Scoring};
use crate::trigrams::{TrigramQuery, extract};
use crate::unindexed::matching_symbols;
use crate::{ChunkMatch, CompileError, PathFilter, chunk_matches};

/// First and largest batch of docs verified in parallel; batches start
/// small so the first results come quickly.
const FIRST_BATCH: usize = 16;
const MAX_BATCH: usize = 512;
/// Files of a ranked search's flushed page chunked and sent before the rest.
const FLUSH_SLICE: usize = 64;

/// An [`IndexQuery`] compiled once for all shards.
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    matcher: Option<Matcher>,
    trigrams: TrigramQuery,
    paths: PathFilter,
    pub file_limit: usize,
    pub max_line_len: Option<usize>,
    pub threads: usize,
    /// Symbol search: the matcher applies to symbol names.
    symbols: bool,
    scorer: Scorer,
    cancel: Cancel,
}

impl CompiledQuery {
    pub fn compile(q: &IndexQuery) -> Result<CompiledQuery, CompileError> {
        let mut paths = PathFilter::compile(&q.files, q.case_sensitive)?;
        if let Some(p) = &q.path_pattern {
            paths = paths.with_pattern(Matcher::from_spec(p, q.case_sensitive)?);
        }
        let matcher = q
            .pattern
            .as_ref()
            .map(|p| Matcher::from_spec(p, q.case_sensitive))
            .transpose()?;
        let scorer = Scorer::new(matcher.as_ref(), q.case_sensitive, q.bm25);
        Ok(CompiledQuery {
            matcher,
            scorer,
            trigrams: q
                .pattern
                .as_ref()
                .map_or(TrigramQuery::All, |p| extract(p, q.case_sensitive)),
            paths,
            file_limit: if q.file_limit == 0 {
                usize::MAX
            } else {
                q.file_limit
            },
            max_line_len: q.max_line_len,
            threads: crate::pool::threads(),
            symbols: q.symbols,
            cancel: q.cancel.clone(),
        })
    }

    pub fn trigrams(&self) -> &TrigramQuery {
        &self.trigrams
    }

    /// Whether results are ranked: there is a pattern to score them by.
    pub fn ranked(&self) -> bool {
        self.matcher.is_some() || self.paths.has_pattern()
    }

    /// Whether the search was canceled; if so, marks `stats`.
    fn canceled(&self, stats: &mut IndexSearchStats) -> bool {
        if self.cancel.is_canceled() {
            stats.canceled = true;
            stats.limit_hit = true;
        }
        stats.canceled
    }
}

/// A match in a shard.
#[derive(Debug, Clone, PartialEq)]
pub struct ShardMatch {
    pub doc: Doc,
    pub path: String,
    pub chunks: Vec<ChunkMatch>,
    pub path_ranges: Vec<Range<usize>>,
    pub symbols: Vec<SymbolMatch>,
    /// The ranking score; 0 without a pattern.
    pub score: f64,
}

/// A doc that passed the filters, waiting for verification.
struct Candidate {
    doc: Doc,
    path: String,
    path_ranges: Vec<Range<usize>>,
    /// The caller's index of the repository searched (its scoring in
    /// [`verify`]).
    ctx: usize,
}

/// The docs of repository `repo` (an index into the shard's repos) on any
/// branch in `mask` that may match, in path order, leaving out `hidden`.
/// `picked` is the shard's docs selected by the trigram query (`None`: all).
/// `part` limits the docs looked at to a part of the repository's.
#[allow(clippy::too_many_arguments)]
fn candidates(
    shard: &Shard,
    repo: u16,
    mask: u64,
    picked: Option<&RoaringBitmap>,
    hidden: Option<&RoaringBitmap>,
    q: &CompiledQuery,
    stats: &mut IndexSearchStats,
    part: Option<Range<u32>>,
) -> Result<Vec<Candidate>, FormatError> {
    let range = part.unwrap_or_else(|| shard.repo_docs(repo));
    let shown = |id: u32| hidden.is_none_or(|h| !h.contains(id));
    let mut docs = Vec::new();
    let mut keep = |doc: Doc, path: &str, stats: &mut IndexSearchStats| {
        if doc.branch_mask & mask == 0 || !q.paths.matches(path) {
            return;
        }
        if q.symbols && !shard.has_symbols(&doc) {
            return;
        }
        let Some(path_ranges) = q.paths.path_ranges(path) else {
            return;
        };
        stats.files_considered += 1;
        if q.matcher.is_some() && !doc.has_content() {
            stats.files_skipped += 1;
            return;
        }
        docs.push(Candidate {
            doc,
            path: path.to_owned(),
            path_ranges,
            ctx: 0,
        });
    };
    match picked {
        // Few candidates: look their paths up one by one.
        Some(c) if c.range_cardinality(range.clone()) < (range.len() / 8) as u64 => {
            for id in c.range(range).filter(|&id| shown(id)) {
                let doc = shard.doc(id);
                keep(doc, &shard.path(&doc)?, stats);
            }
        }
        _ => shard.walk_docs(range.start, &mut |doc, path| {
            if doc.id >= range.end {
                return ControlFlow::Break(());
            }
            if picked.is_none_or(|c| c.contains(doc.id)) && shown(doc.id) {
                keep(doc, path, stats);
            }
            ControlFlow::Continue(())
        })?,
    }
    Ok(docs)
}

/// Maps `items` in order on the node's search pool ([`crate::pool`]), or
/// inline with one thread.
fn par_map<T: Send, R: Send>(
    items: Vec<T>,
    threads: usize,
    lane: Lane,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    if threads <= 1 {
        items.into_iter().map(f).collect()
    } else {
        crate::pool::map_ordered(items, lane, f)
    }
}

/// Docs looked at between checks of the cancel flag where each doc is cheap
/// (no content read).
const CANCEL_EVERY: usize = 1024;

/// Hits per atom counted when scoring a file whose chunks are built later:
/// enough for term frequencies to saturate.
const SCORE_HITS: usize = 64;

/// What [`verify`] reports to its caller.
// Passed by value straight to the caller; boxing would allocate per match.
#[allow(clippy::large_enum_variant)]
enum Step<'s> {
    /// A match; `chunked` says whether its chunks were built (see
    /// [`verify`]'s `chunks`).
    Match {
        shard: &'s Shard,
        m: ShardMatch,
        chunked: bool,
        /// The candidate's `ctx`.
        ctx: usize,
    },
    /// A batch of candidates was verified (with or without matches), so a
    /// caller holding results can check its clock.
    Batch,
}

/// Verifies candidates (each with its shard) in order, emitting matches
/// with their scores (`scorings[c.ctx]`). Stops after `limit` matches or
/// when `step` breaks. Batches grow from `batch_size`, which carries over
/// to the next call.
/// While `chunks` is false, content matches come without chunks (built
/// later for the files ranking keeps, [`fill_chunks`]) and are scored on
/// their first [`SCORE_HITS`] hits per atom; it is read before each batch,
/// so the caller can switch it on part way.
#[allow(clippy::too_many_arguments)]
fn verify<'s>(
    docs: Vec<(&'s Shard, Candidate)>,
    q: &CompiledQuery,
    scorings: &[Scoring<'_>],
    limit: usize,
    chunks: &Cell<bool>,
    batch_size: &mut usize,
    stats: &mut IndexSearchStats,
    step: &mut dyn FnMut(Step<'s>) -> ControlFlow<()>,
) -> Result<(), FormatError> {
    let mut found = 0usize;
    let ranked = q.ranked();
    let mut send = |shard: &'s Shard,
                    mut m: ShardMatch,
                    stats: &mut IndexSearchStats,
                    chunked: bool,
                    ctx: usize,
                    step: &mut dyn FnMut(Step<'s>) -> ControlFlow<()>| {
        if ranked {
            m.score += sourcegull_search::ranking::doc_rank_score(shard.doc_rank(m.doc.id));
        }
        found += 1;
        let flow = step(Step::Match {
            shard,
            m,
            chunked,
            ctx,
        });
        if flow.is_break() || found >= limit {
            stats.limit_hit = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    };
    let path_only = |c: Candidate| {
        let score = if c.path_ranges.is_empty() {
            0.0
        } else {
            ranking::path_score(&c.path, &c.path_ranges)
        };
        ShardMatch {
            doc: c.doc,
            path: c.path,
            chunks: Vec::new(),
            path_ranges: c.path_ranges,
            symbols: Vec::new(),
            score,
        }
    };
    if q.symbols {
        // Symbol names and positions come from the SYMBOLS and NEWLINES
        // sections; contents are not read.
        for (i, (shard, c)) in docs.into_iter().enumerate() {
            if i % CANCEL_EVERY == 0 && q.canceled(stats) {
                break;
            }
            stats.files_searched += 1;
            let symbols: Vec<_> = shard
                .symbols(&c.doc)?
                .into_iter()
                .filter(|s| {
                    q.matcher
                        .as_ref()
                        .is_none_or(|m| m.is_match(s.name.as_bytes()))
                })
                .map(|s| {
                    let r = s.range.start as usize..s.range.end as usize;
                    (s.name, s.container, s.kind, r)
                })
                .collect();
            if symbols.is_empty() {
                continue;
            }
            let score = match &q.matcher {
                Some(m) => ranking::symbols_score(
                    &c.path,
                    Some(m),
                    symbols.iter().map(|s| (s.0.as_str(), s.2)),
                ),
                None => 0.0,
            };
            let newlines = shard.newlines(&c.doc)?;
            let symbols = matching_symbols(symbols, None, &newlines);
            let ctx = c.ctx;
            let m = ShardMatch {
                symbols,
                score,
                ..path_only(c)
            };
            if send(shard, m, stats, chunks.get(), ctx, step).is_break() {
                break;
            }
        }
        return Ok(());
    }
    let Some(matcher) = &q.matcher else {
        for (i, (shard, c)) in docs.into_iter().enumerate() {
            if i % CANCEL_EVERY == 0 && q.canceled(stats) {
                break;
            }
            let ctx = c.ctx;
            if send(shard, path_only(c), stats, chunks.get(), ctx, step).is_break() {
                break;
            }
        }
        return Ok(());
    };
    let mut rest = docs.into_iter();
    loop {
        if q.canceled(stats) {
            return Ok(());
        }
        let batch: Vec<(&Shard, Candidate)> = rest.by_ref().take(*batch_size).collect();
        if batch.is_empty() {
            return Ok(());
        }
        *batch_size = (*batch_size * 2).min(MAX_BATCH);
        stats.files_searched += batch.len();
        let chunked = chunks.get();
        let verified = par_map(
            batch,
            q.threads,
            Lane::Normal,
            |(shard, c)| -> Result<_, FormatError> {
                let content = shard.content(&c.doc)?;
                let cap = if chunked { usize::MAX } else { SCORE_HITS };
                let lang = shard.lang(&c.doc);
                let Some(hits) = matcher.find_hits_in(&content, lang, cap) else {
                    return Ok((shard, c, None));
                };
                let chunks = if chunked {
                    chunk_matches(&content, &merge(&hits), q.max_line_len)
                } else {
                    Vec::new()
                };
                let defs = if shard.has_symbols(&c.doc) {
                    ranking::definitions(shard.symbol_ranges(&c.doc)?)
                } else {
                    Vec::new()
                };
                let score = scorings[c.ctx].content(&c.path, &content, &hits, &defs);
                Ok((shard, c, Some((chunks, score))))
            },
        );
        for v in verified {
            if let (shard, c, Some((chunks, score))) = v? {
                let ctx = c.ctx;
                let m = ShardMatch {
                    chunks,
                    score,
                    ..path_only(c)
                };
                if send(shard, m, stats, chunked, ctx, step).is_break() {
                    return Ok(());
                }
            }
        }
        if step(Step::Batch).is_break() {
            return Ok(());
        }
    }
}

/// Searches the docs of repository `repo` (an index into the shard's repos)
/// on any branch in `mask`, emitting matches in path order. Stops at
/// `q.file_limit` matches or when `emit` breaks.
pub fn search_shard(
    shard: &Shard,
    repo: u16,
    mask: u64,
    q: &CompiledQuery,
    emit: &mut dyn FnMut(ShardMatch) -> ControlFlow<()>,
) -> Result<IndexSearchStats, FormatError> {
    let mut stats = IndexSearchStats::default();
    let picked = eval(&q.trigrams, shard)?;
    let docs = candidates(
        shard,
        repo,
        mask,
        picked.as_ref(),
        None,
        q,
        &mut stats,
        None,
    )?;
    let docs = docs.into_iter().map(|c| (shard, c)).collect();
    let scoring = q.scorer.for_shard(shard)?;
    let mut batch = FIRST_BATCH;
    verify(
        docs,
        q,
        &[scoring],
        q.file_limit,
        &Cell::new(true),
        &mut batch,
        &mut stats,
        &mut |s| match s {
            Step::Match { m, .. } => emit(m),
            Step::Batch => ControlFlow::Continue(()),
        },
    )?;
    Ok(stats)
}

/// Adds the file counts of `stats` to `total`.
fn add_counts(total: &mut IndexSearchStats, stats: &IndexSearchStats) {
    total.files_considered += stats.files_considered;
    total.files_searched += stats.files_searched;
    total.files_skipped += stats.files_skipped;
    total.canceled |= stats.canceled;
    total.limit_hit |= stats.canceled;
}

/// Candidates pooled across repositories before they are verified.
const POOL: usize = 2 * MAX_BATCH;

/// Repositories whose candidates are made at once, at most; the first
/// round makes one per worker, and rounds double up to this.
const MAX_REPO_ROUND: usize = 128;

/// The trigram query's docs per base shard (keyed by [`shard_key`]),
/// evaluated once per search for all the repositories of a compound shard.
type Picked = HashMap<usize, Option<RoaringBitmap>>;

fn shard_key(shard: &Arc<Shard>) -> usize {
    Arc::as_ptr(shard) as usize
}

/// Evaluates the trigram query on the base shards of `repos` that `picked`
/// does not hold yet, in parallel.
fn pick_bases<'a>(
    repos: impl IntoIterator<Item = &'a RepoShards>,
    q: &CompiledQuery,
    picked: &mut Picked,
) -> Result<(), FormatError> {
    let mut seen = HashSet::new();
    let todo: Vec<&Arc<Shard>> = repos
        .into_iter()
        .map(|rs| &rs.base)
        .filter(|s| !picked.contains_key(&shard_key(s)) && seen.insert(shard_key(s)))
        .collect();
    for (shard, p) in par_map(todo, q.threads, Lane::Normal, |s| {
        (shard_key(s), eval(&q.trigrams, s))
    }) {
        picked.insert(shard, p?);
    }
    Ok(())
}

/// The candidates of a repository's base shard (minus what its delta hides)
/// and delta, in path order; `picked` holds the base's trigram query docs
/// ([`pick_bases`]).
fn repo_candidates<'s>(
    rs: &'s RepoShards,
    mask: u64,
    q: &CompiledQuery,
    picked: &Picked,
    stats: &mut IndexSearchStats,
) -> Result<Vec<(&'s Shard, Candidate)>, FormatError> {
    let base = base_candidates(rs, mask, q, picked, stats, None)?;
    with_delta(rs, base, mask, q, stats)
}

/// The candidates of (a part of) a repository's base shard, minus what its
/// delta hides.
fn base_candidates(
    rs: &RepoShards,
    mask: u64,
    q: &CompiledQuery,
    picked: &Picked,
    stats: &mut IndexSearchStats,
    part: Option<Range<u32>>,
) -> Result<Vec<Candidate>, FormatError> {
    candidates(
        &rs.base,
        rs.repo,
        mask,
        picked
            .get(&shard_key(&rs.base))
            .expect("the base shard was picked")
            .as_ref(),
        rs.hidden(),
        q,
        stats,
        part,
    )
}

/// A repository's base candidates merged with its delta's, in path order.
fn with_delta<'s>(
    rs: &'s RepoShards,
    base: Vec<Candidate>,
    mask: u64,
    q: &CompiledQuery,
    stats: &mut IndexSearchStats,
) -> Result<Vec<(&'s Shard, Candidate)>, FormatError> {
    Ok(match &rs.delta {
        Some(d) => {
            let p = eval(&q.trigrams, d)?;
            let delta = candidates(d, 0, mask, p.as_ref(), None, q, stats, None)?;
            merge_candidates(base, &rs.base, delta, d)
        }
        None => base.into_iter().map(|c| (&*rs.base, c)).collect(),
    })
}

/// Docs of a repository's base shard whose candidates one worker makes:
/// a large repository is split so its walk (every path in a path search, or
/// a weak trigram query) runs on all workers instead of keeping one busy
/// while the others wait (Chromium's 449,000 paths took about 55 ms alone).
const CANDIDATE_PART: u32 = 16_384;

/// The parts of `docs` of at most `size` docs, in order.
fn parts(docs: Range<u32>, size: u32) -> impl Iterator<Item = Range<u32>> {
    docs.clone()
        .step_by(size as usize)
        .map(move |start| start..start.saturating_add(size).min(docs.end))
}

/// Merges two candidate lists in (path, oid) order.
fn merge_candidates<'s>(
    a: Vec<Candidate>,
    sa: &'s Shard,
    b: Vec<Candidate>,
    sb: &'s Shard,
) -> Vec<(&'s Shard, Candidate)> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => (&x.path, x.doc.oid) <= (&y.path, y.doc.oid),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return out,
        };
        if take_a {
            out.extend(a.next().map(|c| (sa, c)));
        } else {
            out.extend(b.next().map(|c| (sb, c)));
        }
    }
}

/// The docs that may match `q`; `None` means all docs.
fn eval(q: &TrigramQuery, shard: &Shard) -> Result<Option<RoaringBitmap>, FormatError> {
    Ok(match q {
        TrigramQuery::All => None,
        TrigramQuery::Never => Some(RoaringBitmap::new()),
        TrigramQuery::Trigram(t) => Some(shard.postings(*t)?),
        TrigramQuery::Or(qs) => {
            let mut acc = RoaringBitmap::new();
            for q in qs {
                match eval(q, shard)? {
                    None => return Ok(None),
                    Some(b) => acc |= b,
                }
            }
            Some(acc)
        }
        TrigramQuery::And(qs) => {
            let mut by_cost = Vec::with_capacity(qs.len());
            for q in qs {
                by_cost.push((estimate(q, shard)?, q));
            }
            by_cost.sort_by_key(|(n, _)| *n);
            let mut acc: Option<RoaringBitmap> = None;
            for (n, q) in by_cost {
                if let Some(a) = &acc {
                    // Verifying a few docs costs less than decoding a long list.
                    if a.is_empty() || (a.len() <= 16 && n > 64 * a.len()) {
                        break;
                    }
                }
                if let Some(b) = eval(q, shard)? {
                    acc = Some(match acc {
                        None => b,
                        Some(a) => a & b,
                    });
                }
            }
            acc
        }
    })
}

/// An upper bound on the docs `q` selects, from posting lengths alone.
pub(crate) fn estimate(q: &TrigramQuery, shard: &Shard) -> Result<u64, FormatError> {
    Ok(match q {
        TrigramQuery::All => u64::MAX,
        TrigramQuery::Never => 0,
        TrigramQuery::Trigram(t) => shard.trigram_count(*t)?,
        TrigramQuery::And(qs) => {
            let mut min = u64::MAX;
            for q in qs {
                min = min.min(estimate(q, shard)?);
            }
            min
        }
        TrigramQuery::Or(qs) => {
            let mut sum = 0u64;
            for q in qs {
                sum = sum.saturating_add(estimate(q, shard)?);
            }
            sum
        }
    })
}

/// A ranked match waiting for its chunks, with the doc they come from.
type Found<'s> = (IndexedFile, &'s Shard, Doc);

/// Keeps the `k` highest-scoring files, best first (ties in path order).
fn keep_best(files: &mut Vec<Found<'_>>, k: usize) {
    files.sort_by(|(a, ..), (b, ..)| {
        ranking::by_score(a.score, b.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.repo.0.cmp(&b.repo.0))
    });
    files.truncate(k);
}

/// Builds the chunks of content matches that ranking kept.
fn fill_chunks(found: Vec<Found<'_>>, q: &CompiledQuery) -> Result<Vec<IndexedFile>, FormatError> {
    let Some(matcher) = q.matcher.as_ref().filter(|_| !q.symbols) else {
        return Ok(found.into_iter().map(|(f, ..)| f).collect());
    };
    par_map(
        found,
        q.threads,
        // The first page of a ranked search waits on these.
        Lane::Urgent,
        |(mut f, shard, doc)| {
            let content = shard.content(&doc)?;
            let ranges = matcher
                .find_in(&content, shard.lang(&doc))
                .unwrap_or_default();
            f.chunks = chunk_matches(&content, &ranges, q.max_line_len);
            Ok(f)
        },
    )
    .into_iter()
    .collect()
}

/// Where [`ShardIndex::search`] sends its matches: straight out in an
/// unranked search; in a ranked one, held (the best `file_limit`) until the
/// search ends or the deadline passes, then flushed best first, and
/// streamed after that.
struct Collector<'a, 's> {
    q: &'a CompiledQuery,
    emit: &'a mut dyn FnMut(IndexedFile) -> ControlFlow<()>,
    /// Matches verified before the search stops.
    budget: usize,
    deadline: Instant,
    /// Whether matches are built with chunks: from the start in an
    /// unranked search, after the flush in a ranked one.
    chunks: &'a Cell<bool>,
    held: Vec<Found<'s>>,
    /// Matching files found.
    matched: usize,
    emitted: usize,
    limit_hit: bool,
    error: Option<FormatError>,
}

impl<'s> Collector<'_, 's> {
    fn streaming(&self) -> bool {
        self.chunks.get()
    }

    /// Takes a match; `chunked` says whether its chunks were built.
    fn add(&mut self, found: Found<'s>, chunked: bool) -> ControlFlow<()> {
        self.tick()?;
        self.matched += 1;
        if self.streaming() {
            let f = if chunked {
                found.0
            } else {
                match fill_chunks(vec![found], self.q) {
                    Ok(mut f) => f.pop().expect("one file in, one out"),
                    Err(e) => {
                        self.error = Some(e);
                        return ControlFlow::Break(());
                    }
                }
            };
            self.send(f)?;
        } else {
            self.held.push(found);
            let limit = self.q.file_limit;
            if self.held.len() >= limit.max(64).saturating_mul(2) {
                self.keep_best();
            }
        }
        if self.matched >= self.budget {
            self.limit_hit = true;
            self.flush()?;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    fn send(&mut self, f: IndexedFile) -> ControlFlow<()> {
        self.emitted += 1;
        let flow = (self.emit)(f);
        if flow.is_break() || self.emitted >= self.q.file_limit {
            self.limit_hit = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    fn keep_best(&mut self) {
        if self.held.len() > self.q.file_limit {
            self.limit_hit = true;
        }
        keep_best(&mut self.held, self.q.file_limit);
    }

    /// Flushes once the deadline has passed.
    fn tick(&mut self) -> ControlFlow<()> {
        if !self.streaming() && Instant::now() >= self.deadline {
            return self.flush();
        }
        ControlFlow::Continue(())
    }

    /// Sends what is held, best first, and streams from then on.
    fn flush(&mut self) -> ControlFlow<()> {
        if self.streaming() {
            return ControlFlow::Continue(());
        }
        self.chunks.set(true);
        self.keep_best();
        let mut held = std::mem::take(&mut self.held);
        // The best files go out before the chunks of the rest are built:
        // a page of 500 broad matches takes milliseconds to chunk, and the
        // first event needs only its first files.
        let mut slice = FLUSH_SLICE;
        while !held.is_empty() {
            let rest = held.split_off(held.len().min(slice));
            slice = usize::MAX;
            match fill_chunks(std::mem::replace(&mut held, rest), self.q) {
                Ok(files) => files.into_iter().try_for_each(|f| self.send(f))?,
                Err(e) => {
                    self.error = Some(e);
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    }
}

pub(crate) fn format_error(e: FormatError) -> IndexError {
    match e {
        FormatError::Io(e) => IndexError::Io(e.to_string()),
        e => IndexError::Corrupt(e.to_string()),
    }
}

/// A repository's shards: a full build (its own shard, or its part of a
/// compound shard) and, after later commits, a delta holding the changed
/// paths and hiding their old versions in the base.
#[derive(Debug, Clone)]
pub struct RepoShards {
    pub base: Arc<Shard>,
    /// The repository's index in `base.repos()`.
    pub repo: u16,
    /// The compound shard's sequence number when `base` is one.
    pub compound: Option<u64>,
    pub delta: Option<Arc<Shard>>,
}

impl RepoShards {
    fn single(base: Arc<Shard>) -> RepoShards {
        RepoShards {
            base,
            repo: 0,
            compound: None,
            delta: None,
        }
    }

    /// The branches and commits searched: the delta's when there is one.
    pub fn meta(&self) -> &RepoMeta {
        match &self.delta {
            Some(d) => &d.repos()[0],
            None => &self.base.repos()[usize::from(self.repo)],
        }
    }

    pub(crate) fn hidden(&self) -> Option<&RoaringBitmap> {
        self.delta
            .as_ref()
            .and_then(|d| d.tombstones())
            .map(|(_, h)| h)
    }

    /// The repository's docs in the base that are not hidden.
    pub(crate) fn base_docs(&self) -> u64 {
        let hidden = self.hidden().map_or(0, RoaringBitmap::len);
        (self.base.repo_docs(self.repo).len() as u64).saturating_sub(hidden)
    }

    /// Docs searched.
    pub fn docs(&self) -> u64 {
        self.base_docs() + self.delta.as_ref().map_or(0, |d| u64::from(d.doc_count()))
    }

    /// Text bytes searchable.
    pub fn content_bytes(&self) -> u64 {
        let hidden = self.hidden();
        let base = self
            .base
            .repo_docs(self.repo)
            .map(|id| self.base.doc(id))
            .filter(|d| d.has_content() && hidden.is_none_or(|h| !h.contains(d.id)))
            .map(|d| u64::from(d.size))
            .sum::<u64>();
        let delta = self.delta.as_ref().map_or(0, |d| {
            d.docs()
                .filter(Doc::has_content)
                .map(|d| u64::from(d.size))
                .sum()
        });
        base + delta
    }

    /// Whether each live doc above `max_file_size` has content exactly when
    /// `rule` allows it.
    pub fn follows_always_index(&self, max_file_size: u64, rule: &AlwaysIndex) -> bool {
        let holds = |shard: &Shard, doc: &Doc| {
            let size = u64::from(doc.size);
            if size <= max_file_size {
                return true;
            }
            // A path that does not decode cannot match the rule.
            let wanted = shard
                .path(doc)
                .is_ok_and(|path| size <= rule.max_file_size(&path, max_file_size));
            wanted == (doc.flags & FLAG_TOO_LARGE == 0)
        };
        let hidden = self.hidden();
        let base = self
            .base
            .repo_docs(self.repo)
            .filter(|id| hidden.is_none_or(|h| !h.contains(*id)))
            .all(|id| holds(&self.base, &self.base.doc(id)));
        base && self
            .delta
            .as_ref()
            .is_none_or(|d| d.docs().all(|doc| holds(d, &doc)))
    }

    /// Lines and bytes per language of the text docs on any branch in
    /// `mask` (bits of [`RepoShards::meta`], as searches select them), most
    /// lines first. Docs without a language are left out.
    pub fn language_stats(&self, mask: u64) -> Vec<LanguageCount> {
        let mut by_lang: HashMap<String, (u64, u64)> = HashMap::new();
        let mut add = |lang: Option<&str>, doc: &Doc| {
            let Some(lang) = lang else { return };
            if doc.branch_mask & mask == 0 || !doc.has_content() {
                return;
            }
            let (lines, bytes) = (u64::from(doc.line_count), u64::from(doc.size));
            match by_lang.get_mut(lang) {
                Some(e) => {
                    e.0 += lines;
                    e.1 += bytes;
                }
                None => {
                    by_lang.insert(lang.to_owned(), (lines, bytes));
                }
            }
        };
        let hidden = self.hidden();
        for id in self.base.repo_docs(self.repo) {
            if hidden.is_none_or(|h| !h.contains(id)) {
                let doc = self.base.doc(id);
                add(self.base.lang(&doc), &doc);
            }
        }
        if let Some(d) = &self.delta {
            for doc in d.docs() {
                add(d.lang(&doc), &doc);
            }
        }
        let mut out: Vec<LanguageCount> = by_lang
            .into_iter()
            .map(|(lang, (lines, bytes))| LanguageCount { lang, lines, bytes })
            .collect();
        out.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.lang.cmp(&b.lang)));
        out
    }

    /// The symbols already extracted for the repository's docs, for a
    /// rebuild to reuse.
    pub fn known_symbols(&self) -> KnownSymbols<'_> {
        let mut known = KnownSymbols::default();
        known.add(&self.base, self.base.repo_docs(self.repo));
        if let Some(d) = &self.delta {
            known.add(d, 0..d.doc_count());
        }
        known
    }

    /// Lines of the searchable text docs, and of those on the default
    /// branch (bit 0 of the branch mask).
    pub fn line_counts(&self) -> (u64, u64) {
        let add = |doc: &Doc, totals: &mut (u64, u64)| {
            if doc.has_content() {
                totals.0 += u64::from(doc.line_count);
                if doc.branch_mask & 1 != 0 {
                    totals.1 += u64::from(doc.line_count);
                }
            }
        };
        let mut totals = (0, 0);
        let hidden = self.hidden();
        for id in self.base.repo_docs(self.repo) {
            if hidden.is_none_or(|h| !h.contains(id)) {
                add(&self.base.doc(id), &mut totals);
            }
        }
        if let Some(d) = &self.delta {
            for doc in d.docs() {
                add(&doc, &mut totals);
            }
        }
        totals
    }

    /// Bytes on disk; in a compound shard, the repository's share of it by
    /// doc count.
    pub fn size_bytes(&self) -> u64 {
        let base = match self.compound {
            None => self.base.size_bytes(),
            Some(_) => {
                let docs = self.base.repo_docs(self.repo).len() as u64;
                self.base.size_bytes() * docs / u64::from(self.base.doc_count()).max(1)
            }
        };
        base + self.delta.as_ref().map_or(0, |d| d.size_bytes())
    }
}

/// Rebuild fully instead of writing a delta when the delta would hold more
/// than this fraction of the repository's docs in the base.
const COMPACT_RATIO: f64 = 0.2;

/// When small repositories are packed into compound shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompoundOptions {
    /// Repositories whose shards are smaller than this are packed.
    pub max_repo_bytes: u64,
    /// Compound shards are filled up to about this size.
    pub max_shard_bytes: u64,
}

impl Default for CompoundOptions {
    fn default() -> Self {
        CompoundOptions {
            max_repo_bytes: 2 << 20,
            max_shard_bytes: 100 << 20,
        }
    }
}

/// A compound shard and the repositories in it that are no longer served
/// from it (rebuilt on their own or removed), persisted next to it.
#[derive(Debug)]
pub(crate) struct Compound {
    pub(crate) shard: Arc<Shard>,
    pub(crate) dead: BTreeSet<i32>,
}

/// The shards of the repositories in a directory, mapped and swapped
/// atomically when a build finishes. Searches keep the shards they started
/// with until they end. Files:
///
/// - `<repo id>.gullshard`: a repository's own base shard;
/// - `compound-<seq>.gullshard`: a compound shard packing small
///   repositories ([`ShardIndex::merge_compounds`]), with
///   `compound-<seq>.dead` listing the repository ids it no longer serves;
/// - `<repo id>.delta.gullshard`: a delta over the repository's base,
///   whichever file that is.
///
/// A repository's own shard takes precedence over compound shards, and a
/// newer compound (higher `seq`) over older ones, so every step of a
/// rebuild or merge leaves files that open to a correct index; copies that
/// lost are recorded as dead when the index opens.
pub struct ShardIndex {
    pub(crate) dir: PathBuf,
    pub(crate) opts: BuildOptions,
    pub(crate) compound: CompoundOptions,
    pub(crate) shards: ArcSwap<BTreeMap<i32, Arc<RepoShards>>>,
    /// Held by every change to the files: builds, removals and merges.
    pub(crate) write: Mutex<BTreeMap<u64, Compound>>,
    /// How long ranked searches hold their matches ([`FLUSH_WALL_TIME`]).
    pub(crate) flush_wall_time: Duration,
    /// Docs per part of a repository whose candidates one worker makes
    /// ([`CANDIDATE_PART`]).
    candidate_part: u32,
}

impl ShardIndex {
    /// Opens every shard in `dir` (created if missing). Shards that fail to
    /// open are left out and returned with their errors, to be deleted and
    /// rebuilt; a delta whose base is missing or different is returned the
    /// same way.
    pub fn open(
        dir: impl Into<PathBuf>,
        opts: BuildOptions,
    ) -> Result<(ShardIndex, Vec<(PathBuf, IndexError)>), IndexError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io_error)?;
        let mut singles: BTreeMap<i32, Arc<Shard>> = BTreeMap::new();
        let mut compounds: BTreeMap<u64, Compound> = BTreeMap::new();
        let mut deltas = Vec::new();
        let mut failed = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            if path.extension().is_none_or(|x| x != EXTENSION) {
                continue;
            }
            let shard = match Shard::open(&path) {
                Ok(s) => s,
                Err(e) => {
                    failed.push((path, format_error(e)));
                    continue;
                }
            };
            if shard.tombstones().is_some() {
                deltas.push((path, shard));
            } else if let Some(seq) = compound_seq(&path) {
                match read_dead(&dead_path(&dir, seq)) {
                    Ok(dead) => {
                        let shard = Arc::new(shard);
                        compounds.insert(seq, Compound { shard, dead });
                    }
                    Err(e) => failed.push((path, e)),
                }
            } else {
                let shard = Arc::new(shard);
                for r in shard.repos() {
                    singles.insert(r.id, shard.clone());
                }
            }
        }
        let mut shards: BTreeMap<i32, Arc<RepoShards>> = BTreeMap::new();
        for (id, base) in singles {
            let repo = base
                .repos()
                .iter()
                .position(|r| r.id == id)
                .expect("listed") as u16;
            let rs = RepoShards {
                repo,
                ..RepoShards::single(base)
            };
            shards.insert(id, Arc::new(rs));
        }
        for (&seq, c) in compounds.iter_mut().rev() {
            let mut lost = false;
            for (i, r) in c.shard.repos().iter().enumerate() {
                if c.dead.contains(&r.id) {
                    continue;
                }
                if shards.contains_key(&r.id) {
                    lost |= c.dead.insert(r.id);
                    continue;
                }
                let rs = RepoShards {
                    base: c.shard.clone(),
                    repo: i as u16,
                    compound: Some(seq),
                    delta: None,
                };
                shards.insert(r.id, Arc::new(rs));
            }
            if lost {
                write_dead(&dir, seq, &c.dead)?;
            }
        }
        for (path, d) in deltas {
            let (base_id, _) = d.tombstones().expect("deltas have tombstones");
            let slot = d.repos().first().and_then(|r| shards.get_mut(&r.id));
            match slot {
                Some(rs) if rs.base.id() == base_id => {
                    Arc::make_mut(rs).delta = Some(Arc::new(d));
                }
                _ => failed.push((path, IndexError::Corrupt("delta without its base".into()))),
            }
        }
        let index = ShardIndex {
            dir,
            opts,
            compound: CompoundOptions::default(),
            shards: ArcSwap::from_pointee(shards),
            write: Mutex::new(compounds),
            flush_wall_time: FLUSH_WALL_TIME,
            candidate_part: CANDIDATE_PART,
        };
        Ok((index, failed))
    }

    /// Sets when small repositories are packed into compound shards.
    pub fn with_compound_options(mut self, compound: CompoundOptions) -> Self {
        self.compound = compound;
        self
    }

    /// Sets how long ranked searches hold their matches to send the best
    /// first ([`FLUSH_WALL_TIME`] by default).
    pub fn with_flush_wall_time(mut self, flush: Duration) -> Self {
        self.flush_wall_time = flush;
        self
    }

    /// Sets the docs per part of a repository whose candidates are made on
    /// one worker ([`CANDIDATE_PART`] by default; for tests).
    pub fn with_candidate_part(mut self, docs: u32) -> Self {
        self.candidate_part = docs.max(1);
        self
    }

    /// The repositories indexed.
    pub fn repos(&self) -> Vec<RepoId> {
        self.shards.load().keys().map(|&id| RepoId(id)).collect()
    }

    /// The repository's shards.
    pub fn shards(&self, repo: RepoId) -> Option<Arc<RepoShards>> {
        self.shards.load().get(&repo.0).cloned()
    }

    pub(crate) fn base_path(&self, repo: i32) -> PathBuf {
        self.dir.join(format!("{repo}.{EXTENSION}"))
    }

    pub(crate) fn delta_path(&self, repo: i32) -> PathBuf {
        self.dir.join(format!("{repo}.delta.{EXTENSION}"))
    }

    pub(crate) fn compound_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("compound-{seq}.{EXTENSION}"))
    }

    fn insert(&self, repo: RepoId, shards: RepoShards) {
        let shards = Arc::new(shards);
        self.shards.rcu(|m| {
            let mut m = BTreeMap::clone(m);
            m.insert(repo.0, shards.clone());
            m
        });
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Compound>> {
        self.write.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that the compound shard `seq` no longer serves `repo`.
    fn mark_dead(
        &self,
        compounds: &mut BTreeMap<u64, Compound>,
        seq: u64,
        repo: i32,
    ) -> Result<(), IndexError> {
        if let Some(c) = compounds.get_mut(&seq)
            && c.dead.insert(repo)
        {
            write_dead(&self.dir, seq, &c.dead)?;
        }
        Ok(())
    }
}

/// The sequence number of a compound shard file, `compound-<seq>.gullshard`.
fn compound_seq(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("compound-")?
        .strip_suffix(EXTENSION)?
        .strip_suffix('.')?
        .parse()
        .ok()
}

pub(crate) fn dead_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("compound-{seq}.dead"))
}

/// Reads a compound shard's dead list: one repository id per line; a
/// missing file is an empty list.
fn read_dead(path: &Path) -> Result<BTreeSet<i32>, IndexError> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|l| {
                l.parse()
                    .map_err(|_| IndexError::Corrupt(format!("{}: bad line {l:?}", path.display())))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(io_error(e)),
    }
}

/// Replaces a compound shard's dead list atomically.
fn write_dead(dir: &Path, seq: u64, dead: &BTreeSet<i32>) -> Result<(), IndexError> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io_error)?;
    for id in dead {
        writeln!(tmp, "{id}").map_err(io_error)?;
    }
    tmp.as_file().sync_all().map_err(io_error)?;
    tmp.persist(dead_path(dir, seq))
        .map_err(|e| io_error(e.error))?;
    Ok(())
}

pub(crate) fn io_error(e: std::io::Error) -> IndexError {
    IndexError::Io(e.to_string())
}

/// Deletes `path`; returns whether it existed.
pub(crate) fn remove_file(path: &Path) -> Result<bool, IndexError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(e)),
    }
}

/// The branches to index in `git`: the default branch as
/// [`DEFAULT_BRANCH`], then each of `extra` that resolves to a commit (as
/// given: a branch, tag or commit), without duplicates, at most
/// [`MAX_BRANCHES`]. Empty when HEAD is unborn.
pub fn branches_to_index(
    git: &gix::Repository,
    extra: &[String],
) -> Result<Vec<Branch>, IndexError> {
    let gix_err = |e: &dyn std::fmt::Display| IndexError::Git(e.to_string());
    let head = git.head().map_err(|e| gix_err(&e))?;
    let Some(head) = head.id() else {
        return Ok(Vec::new());
    };
    let mut out = vec![Branch {
        name: DEFAULT_BRANCH.to_owned(),
        commit: head.to_string(),
    }];
    for name in extra {
        if out.len() == MAX_BRANCHES {
            break;
        }
        if out.iter().any(|b| &b.name == name) {
            continue;
        }
        let Ok(id) = git.rev_parse_single(name.as_str()) else {
            continue;
        };
        let Some(commit) = id.object().ok().and_then(|o| o.peel_to_commit().ok()) else {
            continue;
        };
        out.push(Branch {
            name: name.clone(),
            commit: commit.id.to_string(),
        });
    }
    Ok(out)
}

/// What [`update_index`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexUpdate {
    /// The index already holds these branches at these commits.
    Unchanged,
    Built(IndexedRepo),
    /// Nothing to index (HEAD unborn); the repository was dropped.
    Removed,
}

/// Brings `index` up to date with the heads of `req.branches`, the
/// generation of `req.ranks` and `req.always_index`: builds when they differ
/// from what the index holds (fully when the ranks or the rule changed),
/// drops the repository when there is nothing to index.
pub fn update_index(
    index: &dyn SearchIndex,
    req: &BuildRequest,
) -> Result<IndexUpdate, IndexError> {
    if req.branches.is_empty() {
        index.remove(req.repo)?;
        return Ok(IndexUpdate::Removed);
    }
    let same_contents = index.ranks_generation(req.repo) == Some(req.ranks_generation())
        && index.follows_always_index(req.repo, &req.always_index);
    match index.branches(req.repo) {
        Some(held) if held == req.branches && same_contents => Ok(IndexUpdate::Unchanged),
        Some(_) if same_contents => Ok(IndexUpdate::Built(index.apply_delta(req)?)),
        _ => Ok(IndexUpdate::Built(index.build(req)?)),
    }
}

fn branch(b: &BranchMeta) -> Branch {
    Branch {
        name: b.name.clone(),
        commit: ObjectId::from(b.commit).to_string(),
    }
}

fn parse_commit(b: &Branch) -> Result<ObjectId, IndexError> {
    ObjectId::from_hex(b.commit.as_bytes())
        .map_err(|_| IndexError::Invalid(format!("branch {}: bad commit {:?}", b.name, b.commit)))
}

pub(crate) fn indexer_error(e: IndexerError) -> IndexError {
    match e {
        IndexerError::Git(e) => IndexError::Git(e.to_string()),
        IndexerError::Format(e) => format_error(e),
        IndexerError::Invalid(m) => IndexError::Invalid(m),
    }
}

fn spec(req: &BuildRequest) -> Result<ShardSpec, IndexError> {
    Ok(ShardSpec {
        repo_id: req.repo.0,
        name: req.name.clone(),
        rank: 0,
        branches: req
            .branches
            .iter()
            .map(|b| Ok((b.name.clone(), parse_commit(b)?)))
            .collect::<Result<_, IndexError>>()?,
        ranks: req.ranks.clone(),
        always_index: req.always_index.clone(),
    })
}

fn indexed_repo(req: &BuildRequest, shards: &RepoShards) -> IndexedRepo {
    IndexedRepo {
        repo: req.repo,
        branches: req.branches.clone(),
        docs: shards.docs() as usize,
        content_bytes: shards.content_bytes(),
        index_bytes: shards.size_bytes(),
    }
}

impl ShardIndex {
    /// Builds the repository's own shard; a copy in a compound shard is
    /// marked dead and any delta dropped. Called with the write lock held.
    fn full_build(
        &self,
        compounds: &mut BTreeMap<u64, Compound>,
        git: &gix::Repository,
        req: &BuildRequest,
    ) -> Result<IndexedRepo, IndexError> {
        let out = self.base_path(req.repo.0);
        let old = self.shards(req.repo);
        let known = old
            .as_deref()
            .map(RepoShards::known_symbols)
            .unwrap_or_default();
        build_shard(git, &spec(req)?, &self.opts, &known, &out).map_err(indexer_error)?;
        drop(known);
        let shards = RepoShards::single(Arc::new(Shard::open(&out).map_err(format_error)?));
        let repo = indexed_repo(req, &shards);
        self.insert(req.repo, shards);
        if let Some(seq) = old.and_then(|o| o.compound) {
            self.mark_dead(compounds, seq, req.repo.0)?;
        }
        // The new base makes any delta stale.
        remove_file(&self.delta_path(req.repo.0))?;
        Ok(repo)
    }
}

fn open_git(req: &BuildRequest) -> Result<gix::Repository, IndexError> {
    gix::open(&req.git_dir).map_err(|e| IndexError::Git(e.to_string()))
}

impl SearchIndex for ShardIndex {
    fn search(
        &self,
        query: &IndexQuery,
        emit: &mut dyn FnMut(IndexedFile) -> ControlFlow<()>,
    ) -> Result<IndexSearchStats, IndexError> {
        let q = CompiledQuery::compile(query)?;
        let mut total = IndexSearchStats::default();
        let shards = self.shards.load();
        // Resolve every repo and branch first so `missing` is complete even
        // when the limit stops the search early.
        let mut plan = Vec::new();
        for rb in &query.repos {
            let Some(rs) = shards.get(&rb.repo.0) else {
                total
                    .missing
                    .extend(rb.branches.iter().map(|b| (rb.repo, b.clone())));
                continue;
            };
            let meta = rs.meta();
            let mut mask = 0u64;
            for b in &rb.branches {
                match meta.branches.iter().position(|m| &m.name == b) {
                    Some(bit) => mask |= 1 << bit,
                    None => total.missing.push((rb.repo, b.clone())),
                }
            }
            if mask != 0 {
                plan.push((rb.repo, rs, mask));
            }
        }
        let mut picked = Picked::new();
        let file = |repo: RepoId, meta: &RepoMeta, mask: u64, m: ShardMatch| IndexedFile {
            repo,
            branches: meta
                .branches
                .iter()
                .enumerate()
                .filter(|(bit, _)| m.doc.branch_mask & mask & (1 << bit) != 0)
                .map(|(_, b)| branch(b))
                .collect(),
            path: m.path,
            oid: m.doc.oid,
            chunks: m.chunks,
            path_ranges: m.path_ranges,
            symbols: m.symbols,
            score: m.score,
        };
        if query.repos_only {
            // The first match per branch; a repository is done once every requested
            // branch has one. The limit does not apply.
            for (repo, rs, mask) in plan {
                let mut stats = IndexSearchStats::default();
                pick_bases([&**rs], &q, &mut picked).map_err(format_error)?;
                let docs =
                    repo_candidates(rs, mask, &q, &picked, &mut stats).map_err(format_error)?;
                let scoring = q.scorer.for_shard(&rs.base).map_err(format_error)?;
                let mut covered = 0u64;
                let mut stopped = false;
                let mut batch = FIRST_BATCH;
                verify(
                    docs,
                    &q,
                    &[scoring],
                    usize::MAX,
                    &Cell::new(false),
                    &mut batch,
                    &mut stats,
                    &mut |s| {
                        let Step::Match { m, .. } = s else {
                            return ControlFlow::Continue(());
                        };
                        let new = m.doc.branch_mask & mask & !covered;
                        if new == 0 {
                            return ControlFlow::Continue(());
                        }
                        covered |= new;
                        if emit(file(repo, rs.meta(), mask, m)).is_break() {
                            stopped = true;
                            return ControlFlow::Break(());
                        }
                        if covered == mask {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                )
                .map_err(format_error)?;
                add_counts(&mut total, &stats);
                if stopped {
                    total.limit_hit = true;
                    break;
                }
                if q.canceled(&mut total) {
                    break;
                }
            }
            return Ok(total);
        }
        let ranked = q.ranked();
        // Ranked: verify up to RANK_CAP matches in all, holding the best
        // `file_limit` until the search ends or the flush wall time passes;
        // then they go out best first and later matches stream in order.
        let budget = if ranked {
            q.file_limit.max(RANK_CAP)
        } else {
            q.file_limit
        };
        let chunks = Cell::new(!ranked);
        let mut out = Collector {
            q: &q,
            emit,
            budget,
            deadline: Instant::now() + query.flush_wall_time.unwrap_or(self.flush_wall_time),
            chunks: &chunks,
            held: Vec::new(),
            matched: 0,
            emitted: 0,
            limit_hit: false,
            error: None,
        };
        // Candidates are pooled across repositories, so many small ones are
        // verified in full parallel batches; per repository, what its
        // matches need (`Candidate::ctx` indexes these). Candidates are made
        // for rounds of repositories in parallel.
        let mut repos: Vec<(RepoId, &RepoMeta, u64)> = Vec::new();
        let mut scorings = Vec::new();
        let mut pool: Vec<(&Shard, Candidate)> = Vec::new();
        let mut batch = FIRST_BATCH;
        let mut plan = plan.into_iter();
        let mut more = true;
        let mut stopped = false;
        let mut round = q.threads.clamp(1, MAX_REPO_ROUND);
        loop {
            while more && pool.len() < POOL {
                let next: Vec<_> = plan.by_ref().take(round).collect();
                if next.is_empty() {
                    more = false;
                    break;
                }
                round = (round * 2).min(MAX_REPO_ROUND);
                pick_bases(next.iter().map(|p| Arc::as_ref(p.1)), &q, &mut picked)
                    .map_err(format_error)?;
                // Each repository's base docs in parts, made in parallel; the
                // first part also makes the repository's scoring, the last
                // its delta's candidates.
                let pieces: Vec<(usize, Range<u32>, bool, bool)> = next
                    .iter()
                    .enumerate()
                    .flat_map(|(i, (_, rs, _))| {
                        let docs = rs.base.repo_docs(rs.repo);
                        let mut ps: Vec<_> = parts(docs.clone(), self.candidate_part).collect();
                        if ps.is_empty() {
                            ps.push(docs);
                        }
                        let n = ps.len();
                        ps.into_iter()
                            .enumerate()
                            .map(move |(j, r)| (i, r, j == 0, j + 1 == n))
                    })
                    .collect();
                let made = par_map(pieces, q.threads, Lane::Normal, |(i, part, first, last)| {
                    let (_, rs, mask) = next[i];
                    let mut stats = IndexSearchStats::default();
                    // A canceled search makes no more candidates.
                    let docs = if q.cancel.is_canceled() {
                        Vec::new()
                    } else {
                        base_candidates(rs, mask, &q, &picked, &mut stats, Some(part))?
                    };
                    let scoring = first.then(|| q.scorer.for_shard(&rs.base)).transpose()?;
                    let delta = match (&rs.delta, last) {
                        (Some(d), true) => {
                            let p = eval(&q.trigrams, d)?;
                            Some(candidates(
                                d,
                                0,
                                mask,
                                p.as_ref(),
                                None,
                                &q,
                                &mut stats,
                                None,
                            )?)
                        }
                        _ => None,
                    };
                    Ok::<_, FormatError>((i, docs, scoring, delta, last, stats))
                });
                let mut base: Vec<Candidate> = Vec::new();
                let mut scoring = None;
                for made in made {
                    let (i, docs, first, delta, last, stats) = made.map_err(format_error)?;
                    add_counts(&mut total, &stats);
                    scoring = first.or(scoring);
                    base.extend(docs);
                    if !last {
                        continue;
                    }
                    let (repo, rs, mask) = next[i];
                    let docs = std::mem::take(&mut base);
                    let docs = match (&rs.delta, delta) {
                        (Some(d), Some(delta)) => merge_candidates(docs, &rs.base, delta, d),
                        _ => docs.into_iter().map(|c| (&*rs.base, c)).collect(),
                    };
                    let ctx = repos.len();
                    repos.push((repo, rs.meta(), mask));
                    scorings.push(scoring.take().expect("the first part makes the scoring"));
                    pool.extend(docs.into_iter().map(|(shard, mut c)| {
                        c.ctx = ctx;
                        (shard, c)
                    }));
                }
                // Making candidates over many repositories takes a while:
                // flush on time between rounds too.
                stopped = out.tick().is_break() || q.canceled(&mut total);
                if stopped {
                    break;
                }
            }
            if let Some(e) = out.error.take() {
                return Err(format_error(e));
            }
            if stopped || pool.is_empty() {
                break;
            }
            let mut stats = IndexSearchStats::default();
            let mut flow = ControlFlow::Continue(());
            verify(
                std::mem::take(&mut pool),
                &q,
                &scorings,
                usize::MAX,
                &chunks,
                &mut batch,
                &mut stats,
                &mut |s| {
                    flow = match s {
                        Step::Match {
                            shard,
                            m,
                            chunked,
                            ctx,
                        } => {
                            let doc = m.doc;
                            let (repo, meta, mask) = repos[ctx];
                            out.add((file(repo, meta, mask, m), shard, doc), chunked)
                        }
                        Step::Batch => out.tick(),
                    };
                    flow
                },
            )
            .map_err(format_error)?;
            add_counts(&mut total, &stats);
            if total.canceled {
                break;
            }
            if flow.is_continue() {
                flow = out.tick();
            }
            if let Some(e) = out.error.take() {
                return Err(format_error(e));
            }
            if flow.is_break() {
                break;
            }
        }
        let _ = out.flush();
        if let Some(e) = out.error.take() {
            return Err(format_error(e));
        }
        total.limit_hit |= out.limit_hit;
        Ok(total)
    }

    fn build(&self, req: &BuildRequest) -> Result<IndexedRepo, IndexError> {
        let mut compounds = self.lock();
        self.full_build(&mut compounds, &open_git(req)?, req)
    }

    /// Writes a delta over the current base, or rebuilds fully when the
    /// branch list, the ranks generation or the always-index rule changed or
    /// the delta would exceed [`COMPACT_RATIO`] of the base. A delta keeps
    /// the base's ranks.
    fn apply_delta(&self, req: &BuildRequest) -> Result<IndexedRepo, IndexError> {
        let mut compounds = self.lock();
        let git = open_git(req)?;
        let Some(rs) = self.shards(req.repo).filter(|rs| {
            rs.base.ranks_generation(rs.repo) == req.ranks_generation()
                && rs.follows_always_index(self.opts.max_file_size, &req.always_index)
        }) else {
            return self.full_build(&mut compounds, &git, req);
        };
        let spec = spec(req)?;
        let plan = sourcegull_indexer::plan_delta(&git, &spec, &rs.base).map_err(indexer_error)?;
        let Some(plan) = plan.filter(|p| p.docs() as f64 <= COMPACT_RATIO * f64::from(p.base_docs))
        else {
            return self.full_build(&mut compounds, &git, req);
        };
        let out = self.delta_path(req.repo.0);
        let known = rs.known_symbols();
        sourcegull_indexer::build_delta(&git, &spec, plan, &self.opts, &known, &out)
            .map_err(indexer_error)?;
        drop(known);
        let shards = RepoShards {
            delta: Some(Arc::new(Shard::open(&out).map_err(format_error)?)),
            ..RepoShards::clone(&rs)
        };
        let repo = indexed_repo(req, &shards);
        self.insert(req.repo, shards);
        Ok(repo)
    }

    fn branches(&self, repo: RepoId) -> Option<Vec<Branch>> {
        let rs = self.shards(repo)?;
        Some(rs.meta().branches.iter().map(branch).collect())
    }

    fn ranks_generation(&self, repo: RepoId) -> Option<u64> {
        let rs = self.shards(repo)?;
        Some(rs.base.ranks_generation(rs.repo))
    }

    fn follows_always_index(&self, repo: RepoId, rule: &AlwaysIndex) -> bool {
        self.shards(repo)
            .is_none_or(|rs| rs.follows_always_index(self.opts.max_file_size, rule))
    }

    fn repo_status(&self, repo: RepoId) -> Option<RepoIndexStatus> {
        let rs = self.shards(repo)?;
        let (base_kind, base_path) = match rs.compound {
            Some(seq) => ("compound", self.compound_path(seq)),
            None => ("base", self.base_path(repo.0)),
        };
        let mut files = vec![(
            base_kind,
            base_path,
            rs.base.size_bytes(),
            rs.base_docs(),
            rs.base.repos().len() as u32,
        )];
        if let Some(d) = &rs.delta {
            files.push((
                "delta",
                self.delta_path(repo.0),
                d.size_bytes(),
                u64::from(d.doc_count()),
                1,
            ));
        }
        let updated_at = files
            .iter()
            .filter_map(|f| std::fs::metadata(&f.1).and_then(|m| m.modified()).ok())
            .max();
        let (lines, default_branch_lines) = rs.line_counts();
        Some(RepoIndexStatus {
            branches: rs.meta().branches.iter().map(branch).collect(),
            shards: files
                .into_iter()
                .map(|(kind, path, size_bytes, docs, repos)| ShardInfo {
                    kind: kind.to_owned(),
                    file: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    size_bytes,
                    docs,
                    repos,
                })
                .collect(),
            docs: rs.docs(),
            content_bytes: rs.content_bytes(),
            index_bytes: rs.size_bytes(),
            lines,
            default_branch_lines,
            updated_at,
            ranks_generation: rs.base.ranks_generation(rs.repo),
        })
    }

    fn language_stats(&self, repo: RepoId, branch: &str) -> Option<Vec<LanguageCount>> {
        let rs = self.shards(repo)?;
        let bit = rs.meta().branches.iter().position(|b| b.name == branch)?;
        Some(rs.language_stats(1 << bit))
    }

    fn remove(&self, repo: RepoId) -> Result<bool, IndexError> {
        let mut compounds = self.lock();
        let mut old = None;
        self.shards.rcu(|m| {
            let mut m = BTreeMap::clone(m);
            old = m.remove(&repo.0);
            m
        });
        let mut removed = old.is_some();
        if let Some(seq) = old.and_then(|o| o.compound) {
            self.mark_dead(&mut compounds, seq, repo.0)?;
        }
        for path in [self.delta_path(repo.0), self.base_path(repo.0)] {
            removed |= remove_file(&path)?;
        }
        Ok(removed)
    }

    fn indexed_repos(&self) -> Vec<RepoId> {
        self.repos()
    }

    fn stats(&self) -> IndexStats {
        let shards = self.shards.load();
        let mut s = IndexStats::default();
        let mut bases = BTreeSet::new();
        for rs in shards.values() {
            s.repos += 1;
            s.docs += rs.docs() as usize;
            s.content_bytes += rs.content_bytes();
            if bases.insert(Arc::as_ptr(&rs.base)) {
                s.index_bytes += rs.base.size_bytes();
            }
            s.index_bytes += rs.delta.as_ref().map_or(0, |d| d.size_bytes());
        }
        s
    }
}
