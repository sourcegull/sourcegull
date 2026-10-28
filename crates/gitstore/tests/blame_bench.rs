//! Blame backend comparison on a real repository (ignored by default).
//!
//! ```sh
//! git clone --bare https://github.com/BurntSushi/ripgrep /tmp/rg.git
//! git -C /tmp/rg.git commit-graph write --reachable --changed-paths
//! SOURCEGULL_BLAME_BENCH_REPO=/tmp/rg.git SOURCEGULL_BLAME_BENCH_FILES=crates/core/main.rs,README.md \
//!   cargo test --release -p sourcegull-gitstore --test integration blame_bench:: -- --ignored --nocapture
//! ```
//!
//! Prints, per file, time to first hunk and total time for `git blame
//! --incremental` (streaming) and total time for gix blame.

use std::path::PathBuf;
use std::time::Instant;

use futures::StreamExt;
use sourcegull_gitstore::blame::{BlameOptions, blame_gix, blame_stream};
use sourcegull_gitstore::read::resolve_revision;
use sourcegull_gitstore::sync::open;

#[tokio::test]
#[ignore = "needs SOURCEGULL_BLAME_BENCH_REPO"]
async fn compare_blame_backends() {
    let Ok(repo_dir) = std::env::var("SOURCEGULL_BLAME_BENCH_REPO") else {
        eprintln!("SOURCEGULL_BLAME_BENCH_REPO not set; skipping");
        return;
    };
    let repo_dir = PathBuf::from(repo_dir);
    let files =
        std::env::var("SOURCEGULL_BLAME_BENCH_FILES").unwrap_or_else(|_| "README.md".into());
    let repo = open(&repo_dir).unwrap().to_thread_local();
    let head = resolve_revision(&repo, "HEAD").unwrap();
    println!(
        "{:<40} {:>12} {:>12} {:>12} {:>8}",
        "file", "cli first", "cli total", "gix total", "lines"
    );
    for file in files.split(',') {
        let start = Instant::now();
        let mut stream = blame_stream(
            "git".as_ref(),
            &repo_dir,
            head,
            file,
            &BlameOptions::default(),
        )
        .unwrap();
        let mut first = None;
        let mut cli_lines = 0;
        while let Some(h) = stream.next().await {
            first.get_or_insert_with(|| start.elapsed());
            cli_lines += h.unwrap().line_count;
        }
        let cli_total = start.elapsed();

        let start = Instant::now();
        let hunks = blame_gix(&repo, head, file, &BlameOptions::default()).unwrap();
        let gix_total = start.elapsed();
        let gix_lines: u32 = hunks.iter().map(|h| h.line_count).sum();
        assert_eq!(cli_lines, gix_lines, "{file}");
        println!(
            "{file:<40} {:>10.1}ms {:>10.1}ms {:>10.1}ms {cli_lines:>8}",
            first.unwrap_or_default().as_secs_f64() * 1e3,
            cli_total.as_secs_f64() * 1e3,
            gix_total.as_secs_f64() * 1e3,
        );
    }
}
