//! Language statistics of a repository's head (Trends pie charts): read from the
//! search index's per-doc metadata, against counting the tree with a cold and a warm
//! blob-OID line cache. Checks all three agree and prints the times.
//!
//! `cargo run --release -p sourcegull-search-exec --example langstats_bench -- <git-dir>`

use std::time::Instant;

use sourcegull_search::index::{BuildRequest, SearchIndex};
use sourcegull_search_exec::langstats::{LocCache, tree_language_stats};
use sourcegull_search_exec::{ShardIndex, branches_to_index, update_index};
use sourcegull_types::RepoId;

fn main() {
    let git_dir = std::env::args()
        .nth(1)
        .expect("usage: langstats_bench <git-dir>");
    let repo = gix::open(&git_dir).expect("open repo");
    let head = repo.head_id().expect("head").detach();
    let dir = tempfile::tempdir().expect("temp dir");
    let (index, _) = ShardIndex::open(dir.path(), Default::default()).expect("open index");
    let id = RepoId(1);
    let req = BuildRequest {
        repo: id,
        name: "bench".into(),
        git_dir: repo.git_dir().to_path_buf(),
        branches: branches_to_index(&repo, &[]).expect("branches"),
        ranks: None,
        always_index: Default::default(),
    };
    let started = Instant::now();
    update_index(&index, &req).expect("index");
    println!("index build: {:.2?}", started.elapsed());

    let branch = &req.branches[0].name;
    let mut best = std::time::Duration::MAX;
    let mut from_index = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        from_index = index.language_stats(id, branch).expect("indexed");
        best = best.min(t.elapsed());
    }
    println!("from the index: {best:.2?} (best of 10)");

    let cache = LocCache::new(1 << 20);
    let t = Instant::now();
    let (cold, read) =
        tree_language_stats(&repo, head, &cache, &Default::default()).expect("count");
    println!("tree, cold cache: {:.2?} ({read} blobs read)", t.elapsed());
    let t = Instant::now();
    let (warm, read) =
        tree_language_stats(&repo, head, &cache, &Default::default()).expect("count");
    println!("tree, warm cache: {:.2?} ({read} blobs read)", t.elapsed());
    assert_eq!(cold, warm);
    assert_eq!(from_index, cold, "index and tree counts differ");
    let lines: u64 = cold.iter().map(|c| c.lines).sum();
    println!("{} languages, {lines} lines", cold.len());
    for c in cold.iter().take(5) {
        println!(
            "  {:<12} {:>9} lines {:>11} bytes",
            c.lang, c.lines, c.bytes
        );
    }
}
