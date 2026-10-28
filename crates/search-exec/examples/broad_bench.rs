//! Broad queries on an existing index directory: per query, the warm
//! latency to the first result and to the end at the default 500-file
//! limit, the files verified, and the same with several queries running at
//! once. Shard-level numbers that show where the node's time goes; the
//! end-to-end measurement is `scripts/bench/search.sh`.
//!
//! `cargo run --release -p sourcegull-search-exec --example broad_bench -- <index-dir> [iterations] [concurrency] [query ...]`
//! (`FLUSH_MS` overrides the flush wall time; `RANK_K=20` also prints how
//! many of the first 20 files are among the best 20 when every match is held
//! until the end, the ranking quality the flush wall time costs).
//!
//! `<index-dir>` is a repo node's `data/index` (for example a benchmark run
//! kept with `KEEP_WORK=1`); every repository in it is searched on `HEAD`.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sourcegull_query::PatternType;
use sourcegull_search::index::{IndexQuery, RepoBranches, SearchIndex};
use sourcegull_search::{Job, Limits, PatternInfo, plan_query};
use sourcegull_search_exec::{PatternSpec, ShardIndex};

const QUERIES: &[&str] = &[
    "kmalloc_array",
    "spin_lock_irqsave",
    "static int",
    "error",
    "func main",
    "patterntype:regexp kmalloc_array\\(.*GFP_ATOMIC",
    "patterntype:regexp \\w+_lock\\(",
    "patterntype:regexp func\\s+main",
    "patterntype:regexp struct\\s+\\w+_ops\\s*\\{",
];

/// The query's content job, or its path job (`type:path`): the pattern, and
/// whether it matches paths.
fn content_job(q: &str) -> (PatternInfo, sourcegull_search::FileFilter, bool) {
    let job = plan_query(
        q,
        PatternType::Keyword,
        &Limits::default(),
        chrono::Utc::now(),
    )
    .expect("query plans");
    fn find(j: &Job) -> Option<(PatternInfo, sourcegull_search::FileFilter, bool)> {
        match j {
            Job::Content { pattern, files, .. } => Some((pattern.clone(), files.clone(), false)),
            Job::Path {
                pattern: Some(pattern),
                files,
                ..
            } => Some((pattern.clone(), files.clone(), true)),
            Job::Parallel(js) => js.iter().find_map(find),
            Job::Select { child, .. } | Job::Limit { child, .. } | Job::Timeout { child, .. } => {
                find(child)
            }
            _ => None,
        }
    }
    find(&job).expect("a content or path job")
}

/// The index query for `q` over `repos`.
fn index_query(q: &str, repos: &[RepoBranches]) -> IndexQuery {
    let (pattern, files, path) = content_job(q);
    let spec = Some(PatternSpec::from_pattern(&pattern).expect("regexp pattern"));
    let (pattern_spec, path_pattern) = if path { (None, spec) } else { (spec, None) };
    IndexQuery {
        repos: repos.to_vec(),
        case_sensitive: pattern.case_sensitive,
        bm25: pattern.pattern_type == PatternType::Keyword,
        pattern: pattern_spec,
        path_pattern,
        files,
        file_limit: Limits::default().default_count,
        max_line_len: None,
        symbols: false,
        repos_only: false,
        flush_wall_time: None,
        cancel: Default::default(),
    }
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

/// One search's numbers.
struct Run {
    first: Duration,
    done: Duration,
    files: usize,
    verified: usize,
    /// Match ranges and chunk text bytes in the results.
    ranges: usize,
    bytes: usize,
}

fn run(index: &ShardIndex, q: &IndexQuery) -> Run {
    let t = Instant::now();
    let mut first = None;
    let (mut files, mut ranges, mut bytes) = (0, 0, 0);
    let stats = index
        .search(q, &mut |f| {
            first.get_or_insert_with(|| t.elapsed());
            files += 1;
            ranges += f.chunks.iter().map(|c| c.ranges.len()).sum::<usize>();
            bytes += f.chunks.iter().map(|c| c.content.len()).sum::<usize>();
            ControlFlow::Continue(())
        })
        .expect("search");
    let done = t.elapsed();
    Run {
        first: first.unwrap_or(done),
        done,
        files,
        verified: stats.files_searched,
        ranges,
        bytes,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index_dir = &args[1];
    let iterations: usize = args.get(2).map_or(20, |n| n.parse().expect("iterations"));
    let concurrency: usize = args.get(3).map_or(8, |n| n.parse().expect("concurrency"));
    let queries: Vec<String> = if args.len() > 4 {
        args[4..].to_vec()
    } else {
        QUERIES.iter().map(|q| (*q).to_owned()).collect()
    };
    let (index, failed) = ShardIndex::open(index_dir, Default::default()).expect("open index");
    assert!(failed.is_empty(), "shards failed to open: {failed:?}");
    // FLUSH_MS sets the flush wall time (a large value holds every match until the end).
    let flush = std::env::var("FLUSH_MS").map_or(sourcegull_search::index::FLUSH_WALL_TIME, |ms| {
        Duration::from_millis(ms.parse().expect("FLUSH_MS"))
    });
    let index = Arc::new(index.with_flush_wall_time(flush));
    println!("flush wall time {flush:?}");
    let repos: Vec<RepoBranches> = index
        .repos()
        .into_iter()
        .map(|repo| RepoBranches {
            repo,
            branches: vec!["HEAD".into()],
        })
        .collect();
    println!("{} repositories", repos.len());
    println!(
        "{:<48} {:>5} {:>7} {:>7} {:>6} {:>8} {:>8} {:>8} {:>8} | c={concurrency}: {:>8} {:>8} {:>8}",
        "query",
        "files",
        "verif",
        "ranges",
        "KB",
        "first50",
        "first99",
        "done50",
        "done99",
        "first50",
        "first99",
        "done99"
    );
    for q in &queries {
        let iq = Arc::new(index_query(q, &repos));
        for _ in 0..2 {
            run(&index, &iq);
        }
        let mut first = Vec::new();
        let mut done = Vec::new();
        let mut last = None;
        for _ in 0..iterations {
            let r = run(&index, &iq);
            first.push(r.first);
            done.push(r.done);
            last = Some(r);
        }
        let last = last.expect("at least one iteration");
        first.sort();
        done.sort();
        // The same query from `concurrency` threads at once.
        let par: Vec<(Duration, Duration)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..concurrency)
                .map(|_| {
                    let (index, iq) = (index.clone(), iq.clone());
                    s.spawn(move || {
                        (0..iterations.div_ceil(2))
                            .map(|_| {
                                let r = run(&index, &iq);
                                (r.first, r.done)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("thread"))
                .collect()
        });
        let mut par_first: Vec<Duration> = par.iter().map(|p| p.0).collect();
        let mut par_done: Vec<Duration> = par.iter().map(|p| p.1).collect();
        par_first.sort();
        par_done.sort();
        println!(
            "{:<48} {:>5} {:>7} {:>7} {:>6} {:>8.1} {:>8.1} {:>8.1} {:>8.1} |      {:>8.1} {:>8.1} {:>8.1}",
            q,
            last.files,
            last.verified,
            last.ranges,
            last.bytes / 1000,
            pct(&first, 0.5),
            pct(&first, 0.99),
            pct(&done, 0.5),
            pct(&done, 0.99),
            pct(&par_first, 0.5),
            pct(&par_first, 0.99),
            pct(&par_done, 0.99),
        );
    }
    println!("(ms; first/done = to the first result / to the end at the default 500-file limit)");
    if let Some(k) = std::env::var("RANK_K")
        .ok()
        .map(|k| k.parse::<usize>().expect("RANK_K"))
    {
        rank_overlap(&index, &repos, &queries, k, iterations);
    }
}

/// The first `k` files of each query (paths with repositories), and how many
/// of them are among the first `k` when nothing is flushed before the end
/// (averaged over `iterations` runs).
fn rank_overlap(
    index: &ShardIndex,
    repos: &[RepoBranches],
    queries: &[String],
    k: usize,
    iterations: usize,
) {
    let first = |q: &IndexQuery| {
        let mut out = Vec::new();
        index
            .search(q, &mut |f| {
                out.push((f.repo, f.path));
                if out.len() >= k {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .expect("search");
        out
    };
    println!("top {k} overlap with holding every match until the end:");
    for q in queries {
        let mut iq = index_query(q, repos);
        iq.flush_wall_time = Some(Duration::from_secs(3600));
        let best: std::collections::HashSet<_> = first(&iq).into_iter().collect();
        iq.flush_wall_time = None;
        let mut shared = 0;
        for _ in 0..iterations {
            shared += first(&iq).iter().filter(|f| best.contains(*f)).count();
        }
        println!(
            "{:<48} {:>5.1} / {}",
            q,
            shared as f64 / iterations as f64,
            best.len()
        );
    }
}
