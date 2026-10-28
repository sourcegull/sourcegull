//! Insight backfill, incremental against full evaluation: counts a series at `<points>`
//! times `<days>` apart ending at the head of `<git-dir>`, once with every point counted in
//! full (what one search per point costs) and once incrementally (the oldest point in full,
//! the others through tree diffs), checks both agree and prints the times.
//!
//! `cargo run --release -p sourcegull-search-exec --example series_bench -- <git-dir> [<regexp>] [<points>] [<days>] [capture]`

use std::sync::Arc;
use std::time::Instant;

use sourcegull_gitstore::commitlog::{CommitLog, log_path, update};
use sourcegull_search::PatternSpec;
use sourcegull_search_exec::series::{
    CountCache, Group, LeafKind, SeriesCounter, SeriesLeaf, SeriesRun, SeriesSpec, count_series,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let git_dir = args
        .get(1)
        .expect("usage: series_bench <git-dir> [regexp] [points] [days] [capture]");
    let regexp = args.get(2).map_or("TODO", String::as_str);
    let points: i64 = args.get(3).map_or(30, |p| p.parse().expect("points"));
    let days: i64 = args.get(4).map_or(30, |d| d.parse().expect("days"));
    let capture = args.get(5).is_some_and(|c| c == "capture");

    let repo = gix::open(git_dir).expect("open repo");
    let started = Instant::now();
    let current = CommitLog::open(&log_path(repo.git_dir())).expect("read log");
    update(&repo, current.as_ref()).expect("update commit log");
    let log = Arc::new(
        CommitLog::open(&log_path(repo.git_dir()))
            .expect("read log")
            .expect("log"),
    );
    println!(
        "commit log: {} commits ({:.2?})",
        log.len(),
        started.elapsed()
    );
    let head = repo.head_id().expect("head").detach();
    let head_time = repo
        .find_commit(head)
        .expect("head commit")
        .committer()
        .expect("committer")
        .time()
        .expect("time")
        .seconds;
    let times: Vec<i64> = (0..points)
        .rev()
        .map(|k| head_time - k * days * 86_400)
        .collect();
    let leaf = SeriesLeaf::new(
        LeafKind::Content,
        Some(PatternSpec::Atom {
            regex: regexp.into(),
            negated: false,
        }),
    );
    let spec = if capture {
        SeriesSpec {
            group: Group::Capture,
            capture: Some(regexp.into()),
            ..SeriesSpec::count(vec![leaf])
        }
    } else {
        SeriesSpec::count(vec![leaf])
    };
    let counter = SeriesCounter::compile(&spec, "bench").expect("compile");
    let mut results = Vec::new();
    for (name, full, cached) in [
        ("full", true, false),
        ("full+cache", true, true),
        ("incremental", false, true),
    ] {
        let cache = CountCache::new(256 << 20);
        let cache = cached.then_some(&cache);
        let run = SeriesRun {
            times: times.clone(),
            full,
            ..SeriesRun::default()
        };
        let started = Instant::now();
        let (pts, stats) =
            count_series(&repo, Some(&log), head, &counter, &run, cache).expect("count");
        let elapsed = started.elapsed();
        println!(
            "{name:>12}: {elapsed:>10.2?}  points full/delta/same {}/{}/{}  files counted {} (cache hits {})",
            stats.full_points,
            stats.delta_points,
            stats.same_points,
            stats
                .files_counted
                .load(std::sync::atomic::Ordering::Relaxed),
            stats.cache_hits.load(std::sync::atomic::Ordering::Relaxed),
        );
        results.push((pts, elapsed));
    }
    assert_eq!(results[0].0, results[2].0, "incremental differs from full");
    assert_eq!(results[1].0, results[2].0, "incremental differs from full");
    let totals: Vec<u64> = results[2].0.iter().map(|p| p.counts.total()).collect();
    println!("totals: {totals:?}");
    println!(
        "speedup: {:.1}x",
        results[0].1.as_secs_f64() / results[2].1.as_secs_f64()
    );
}
