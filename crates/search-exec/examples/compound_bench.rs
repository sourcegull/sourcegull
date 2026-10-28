//! Compound shards against one shard per repository: indexes the default
//! branch of `<git-dir>` as `<repos>` separate repositories, each in its own
//! shard, searches all of them, then packs them into compound shards
//! (merging until nothing changes) and searches again. Both layouts must
//! return the same files.
//!
//! `cargo run --release -p sourcegull-search-exec --example compound_bench -- <git-dir> <scratch-dir> <repos>`

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use sourcegull_search::PatternSpec;
use sourcegull_search::index::{Branch, BuildRequest, IndexQuery, RepoBranches, SearchIndex};
use sourcegull_search_exec::{CompoundOptions, ShardIndex};
use sourcegull_types::RepoId;

const QUERIES: &[&str] = &["fn main", "SearchIndex", "TODO", "nosuchstringanywhere"];

fn open(dir: &str) -> (ShardIndex, Duration) {
    let t = Instant::now();
    let (index, failed) = ShardIndex::open(dir, Default::default()).expect("open index");
    assert!(failed.is_empty());
    let index = index.with_compound_options(CompoundOptions {
        max_repo_bytes: u64::MAX,
        ..CompoundOptions::default()
    });
    (index, t.elapsed())
}

/// Files found, time to the first one, and the total time (best of 5).
fn search(index: &ShardIndex, repos: i32, pattern: &str) -> (Vec<(RepoId, String)>, f64, f64) {
    let q = IndexQuery {
        repos: (1..=repos)
            .map(|id| RepoBranches {
                repo: RepoId(id),
                branches: vec!["HEAD".into()],
            })
            .collect(),
        pattern: Some(PatternSpec::Atom {
            regex: regex_syntax::escape(pattern),
            negated: false,
        }),
        ..IndexQuery::default()
    };
    let mut best = (Vec::new(), f64::MAX, f64::MAX);
    for _ in 0..5 {
        let mut out = Vec::new();
        let mut first = None;
        let t = Instant::now();
        index
            .search(&q, &mut |f| {
                first.get_or_insert_with(|| t.elapsed());
                out.push((f.repo, f.path));
                ControlFlow::Continue(())
            })
            .expect("search");
        let total = t.elapsed().as_secs_f64() * 1e3;
        let first = first.map_or(total, |f| f.as_secs_f64() * 1e3);
        if total < best.2 {
            best = (out, first, total);
        }
    }
    best
}

fn report(
    label: &str,
    index: &ShardIndex,
    dir: &str,
    opened: Duration,
    repos: i32,
) -> Vec<Vec<(RepoId, String)>> {
    let files = std::fs::read_dir(dir).expect("list").count();
    let stats = index.stats();
    println!(
        "{label}: {files} files, {:.1} MB, opened in {:.1} ms",
        stats.index_bytes as f64 / 1e6,
        opened.as_secs_f64() * 1e3
    );
    QUERIES
        .iter()
        .map(|p| {
            let (found, first, total) = search(index, repos, p);
            println!(
                "  {p:24} {:6} files  first {first:7.2} ms  all {total:7.2} ms",
                found.len()
            );
            found
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (git_dir, scratch) = (&args[1], &args[2]);
    let repos: i32 = args[3].parse().expect("repo count");
    let _ = std::fs::remove_dir_all(scratch);
    let git = gix::open(git_dir).expect("open repo");
    let head = git.head_id().expect("HEAD").to_string();

    let (index, _) = open(scratch);
    let t = Instant::now();
    for id in 1..=repos {
        index
            .build(&BuildRequest {
                repo: RepoId(id),
                name: format!("bench/r{id}"),
                git_dir: git_dir.into(),
                branches: vec![Branch {
                    name: "HEAD".into(),
                    commit: head.clone(),
                }],
                ranks: None,
                always_index: Default::default(),
            })
            .expect("build");
    }
    println!(
        "built {repos} repos in {:.1} s ({} docs each)",
        t.elapsed().as_secs_f64(),
        index.stats().docs / repos as usize
    );
    drop(index);
    let (index, opened) = open(scratch);
    let single = report("own shards", &index, scratch, opened, repos);

    // Merge until settled: a pass sizes repos by their own shards, which
    // overstates their share of a compound, so the next pass may repack.
    loop {
        let t = Instant::now();
        let merged = index.merge_compounds().expect("merge");
        if merged == Default::default() {
            break;
        }
        println!(
            "merged {} repos into {} compound shards ({:.1} MB) in {:.1} s",
            merged.repos,
            merged.shards,
            merged.bytes as f64 / 1e6,
            t.elapsed().as_secs_f64()
        );
    }
    drop(index);
    let (index, opened) = open(scratch);
    let compound = report("compound", &index, scratch, opened, repos);
    assert_eq!(single, compound, "both layouts return the same files");
    println!("both layouts agree");
}
