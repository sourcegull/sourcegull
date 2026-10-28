//! Commit search over the commit metadata log against the git walk
//! (`type:commit message:` over 1M commits). Builds the log of `<git-dir>`
//! from scratch, then runs a few queries from HEAD both ways and checks they
//! return the same commits.
//!
//! A synthetic history: `scripts/bench/synthetic-history.py 1000000 | git -C <git-dir> fast-import`.
//!
//! `cargo run --release -p sourcegull-search-exec --example commit_bench -- <git-dir> [runs]`

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sourcegull_gitstore::commitlog::{self, CommitLog};
use sourcegull_search::Matcher;
use sourcegull_search_client::CommitFilterSpec;
use sourcegull_search_exec::{CommitFilters, CommitSearchOptions, PatternSpec, search_commits};

struct Run {
    first: Option<Duration>,
    done: Duration,
    oids: Vec<gix::ObjectId>,
}

fn run(
    repo: &gix::Repository,
    head: gix::ObjectId,
    log: Option<Arc<CommitLog>>,
    filters: &CommitFilters,
    pattern: Option<&Matcher>,
    limit: usize,
) -> Run {
    let opts = CommitSearchOptions {
        log,
        limit,
        ..CommitSearchOptions::default()
    };
    let start = Instant::now();
    let mut first = None;
    let mut oids = Vec::new();
    search_commits(repo, head, filters, pattern, &opts, |m| {
        first.get_or_insert_with(|| start.elapsed());
        oids.push(m.commit.oid);
        ControlFlow::Continue(())
    })
    .expect("search");
    Run {
        first,
        done: start.elapsed(),
        oids,
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e3)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let git_dir = args.get(1).expect("usage: commit_bench <git-dir> [runs]");
    let runs: usize = args.get(2).map_or(5, |r| r.parse().expect("runs"));
    let repo = gix::open(git_dir).expect("open repo");
    let head = repo.head_id().expect("HEAD").detach();

    let path = commitlog::log_path(repo.git_dir());
    let _ = std::fs::remove_file(&path);
    let t = Instant::now();
    let stats = commitlog::update(&repo, None).expect("build log");
    let build = t.elapsed();
    let size = std::fs::metadata(&path).expect("log").len();
    let t = Instant::now();
    let log = Arc::new(CommitLog::open(&path).expect("open").expect("log"));
    let open = t.elapsed();
    println!(
        "log: {} commits, {:.1} MB ({:.0} B/commit), built in {} ms, opened in {} ms, index {:.1} MB",
        stats.total,
        size as f64 / 1e6,
        size as f64 / stats.total.max(1) as f64,
        ms(build),
        ms(open),
        log.index_bytes() as f64 / 1e6
    );

    let atom = |r: &str| {
        Matcher::from_spec(
            &PatternSpec::Atom {
                regex: r.into(),
                negated: false,
            },
            false,
        )
        .expect("pattern")
    };
    let queries: Vec<(&str, CommitFilterSpec, Option<&str>, usize)> = vec![
        (
            "message:leak (limit 500)",
            CommitFilterSpec::default(),
            Some("leak"),
            500,
        ),
        (
            "message:nomatchxyz (whole history)",
            CommitFilterSpec::default(),
            Some("nomatchxyz"),
            usize::MAX,
        ),
        (
            "message:/memory leak/ (whole history)",
            CommitFilterSpec::default(),
            Some("memory leak"),
            usize::MAX,
        ),
        (
            "author:dev7@ message:fix (limit 500)",
            CommitFilterSpec {
                authors: vec!["dev7@".into()],
                ..Default::default()
            },
            Some("fix"),
            500,
        ),
        (
            "author:nobody (whole history)",
            CommitFilterSpec {
                authors: vec!["nobody".into()],
                ..Default::default()
            },
            None,
            usize::MAX,
        ),
    ];
    println!("query | git walk ms | log first ms | log done ms (p50 of {runs}) | matches");
    for (name, spec, pattern, limit) in &queries {
        let filters = CommitFilters::compile(spec, false).expect("filters");
        let m = pattern.map(atom);
        let git = run(&repo, head, None, &filters, m.as_ref(), *limit);
        let mut log_runs: Vec<Run> = (0..runs)
            .map(|_| run(&repo, head, Some(log.clone()), &filters, m.as_ref(), *limit))
            .collect();
        log_runs.sort_by_key(|r| r.done);
        let mid = &log_runs[log_runs.len() / 2];
        assert_eq!(git.oids, mid.oids, "{name}: git walk and log differ");
        println!(
            "{name} | {} | {} | {} | {}",
            ms(git.done),
            mid.first.map_or("-".into(), ms),
            ms(mid.done),
            mid.oids.len()
        );
    }
}
