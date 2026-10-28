//! Builds the changes file of a bare repository from scratch and reports
//! its size and the time to build it, to load it, to build the contributor index of
//! `file:has.contributor()`, and to list the commits of the last 90 days before the newest
//! commit with their files (what the recent contributors indexer reads).
//!
//! `cargo run --release -p sourcegull-gitstore --example changes_bench -- <git-dir> [--gix]`
//! (`--gix` diffs trees in process instead of listing them with `git log --raw`).

use std::path::Path;
use std::time::Instant;

use sourcegull_gitstore::changes::{self, ChangeLog};
use sourcegull_gitstore::commitlog::{self, CommitLog};

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: changes_bench <git-dir>");
    let repo = gix::open(Path::new(&dir)).expect("open repo");
    let t = Instant::now();
    let s = commitlog::update(&repo, None).expect("commit log");
    println!(
        "commit log: {} commits ({} new) in {:?}",
        s.total,
        s.added,
        t.elapsed()
    );
    let log = CommitLog::open(&commitlog::log_path(repo.git_dir()))
        .unwrap()
        .unwrap();

    let path = changes::changes_path(repo.git_dir());
    let _ = std::fs::remove_file(&path);
    let git = std::env::args().nth(2).is_none_or(|a| a != "--gix");
    let t = Instant::now();
    changes::update(&repo, &log, None, git.then_some(Path::new("git"))).expect("changes");
    let built = t.elapsed();
    let bytes = std::fs::metadata(&path).unwrap().len();
    println!(
        "changes: built in {built:?} ({:.1} µs/commit), {bytes} bytes ({:.1} B/commit)",
        built.as_secs_f64() * 1e6 / log.len() as f64,
        bytes as f64 / log.len() as f64
    );
    let t = Instant::now();
    let c = ChangeLog::open(&path).unwrap().unwrap();
    println!("loaded in {:?}: {} paths", t.elapsed(), c.path_count());
    let t = Instant::now();
    let idx = c.contributors(&log);
    println!(
        "contributor index in {:?}: {} authors",
        t.elapsed(),
        idx.author_count()
    );

    let head = log.lookup(&repo.head_id().unwrap().detach()).unwrap();
    let newest = log.get(head).committer_time();
    let since = newest - 90 * 86_400;
    let t = Instant::now();
    let (mut commits, mut files) = (0, 0);
    for commit in log.walk(head) {
        if commit.committer_time() < since {
            break;
        }
        if commit.is_merge() || commit.author().time < since {
            continue;
        }
        commits += 1;
        files += c.changed_paths(commit.pos()).count();
    }
    println!(
        "90-day window before HEAD: {commits} commits, {files} changed files, read in {:?}",
        t.elapsed()
    );
}
