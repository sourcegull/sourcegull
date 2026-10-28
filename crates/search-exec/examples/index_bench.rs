//! Indexed search on one repository against unindexed search on the same
//! commit: build time and size, then per query the warm latency to the
//! first result and to the end (default limit of 500 files), and a check
//! that both backends return the same files and chunks with no limit.
//! Shard-level numbers, not the end-to-end measurement.
//!
//! `cargo run --release -p sourcegull-search-exec --example index_bench -- <git-dir> <index-dir> [iterations]`

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use sourcegull_query::PatternType;
use sourcegull_search::index::{Branch, BuildRequest, IndexQuery, RepoBranches, SearchIndex};
use sourcegull_search::{FileFilter, Job, Limits, Matcher, PatternInfo, plan_query};
use sourcegull_search_exec::{
    FileMatch, PathFilter, PatternSpec, ShardIndex, TrigramQuery, UnindexedOptions, search_commit,
    search_commit_symbols, trigram_query,
};
use sourcegull_types::RepoId;

const QUERIES: &[&str] = &[
    // selective literals
    "ErrUnexpectedEOF",
    "func NewReader",
    "MaxHeaderBytes",
    // common literals
    "return nil",
    "err != nil",
    "if",
    // case-insensitive vs exact
    "ERRUNEXPECTEDEOF",
    "ErrUnexpectedEOF case:yes",
    // selective and broad regexps
    "patterntype:regexp func \\w+Reader\\(",
    "patterntype:regexp ErrUnexpected(EOF|Size)",
    "patterntype:regexp ^import \\($",
    "patterntype:regexp \\w+_test",
    // boolean
    "ReadFull and EOF",
    "ReadFull or WriteString",
    "ReadFull not EOF",
    // filters
    "context.Context file:^src/net/http/",
    "TODO lang:go -file:_test\\.go$",
    "nosuchidentifieranywhere",
    // path search
    "type:path http",
    // structural
    "patterntype:structural fmt.Errorf(:[msg], :[err])",
    "patterntype:structural if err != nil { return :[x] }",
    "patterntype:structural :[[f]](:[a]) rule:'where :[f] == \"panic\"' lang:go",
    // symbol search
    "type:symbol NewReader",
    "type:symbol patterntype:regexp ^Read",
    "type:symbol ServeHTTP file:^src/net/http/",
    "type:symbol nosuchidentifieranywhere",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Content,
    Path,
    Symbol,
}

fn plan(q: &str) -> (Option<PatternInfo>, Kind, FileFilter) {
    let job = plan_query(
        q,
        PatternType::Keyword,
        &Limits::default(),
        chrono::Utc::now(),
    )
    .expect("query plans");
    fn find(j: &Job) -> Option<(Option<PatternInfo>, Kind, FileFilter)> {
        match j {
            Job::Content { pattern, files, .. } => {
                Some((Some(pattern.clone()), Kind::Content, files.clone()))
            }
            Job::Path { pattern, files, .. } => Some((pattern.clone(), Kind::Path, files.clone())),
            Job::Symbol { pattern, files, .. } => {
                Some((pattern.clone(), Kind::Symbol, files.clone()))
            }
            Job::Parallel(js) => js.iter().find_map(find),
            Job::Select { child, .. } | Job::Limit { child, .. } | Job::Timeout { child, .. } => {
                find(child)
            }
            _ => None,
        }
    }
    find(&job).expect("content, path or symbol job")
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (git_dir, index_dir) = (&args[1], &args[2]);
    let iterations: usize = args.get(3).map_or(50, |n| n.parse().expect("iterations"));
    let repo = gix::open(git_dir).expect("open repo");
    let head = repo.head_id().expect("HEAD").detach();

    let (index, _) = ShardIndex::open(index_dir, Default::default()).expect("open index");
    let start = Instant::now();
    let built = index
        .build(&BuildRequest {
            repo: RepoId(1),
            name: git_dir.clone(),
            git_dir: git_dir.into(),
            branches: vec![Branch {
                name: "HEAD".into(),
                commit: head.to_string(),
            }],
            ranks: None,
            always_index: Default::default(),
        })
        .expect("build");
    let secs = start.elapsed().as_secs_f64();
    println!(
        "build: {} docs, {:.1} MB text, shard {:.1} MB ({:.2}x), {:.2}s, {:.0} MB/s",
        built.docs,
        built.content_bytes as f64 / 1e6,
        built.index_bytes as f64 / 1e6,
        built.index_bytes as f64 / built.content_bytes as f64,
        secs,
        built.content_bytes as f64 / 1e6 / secs,
    );
    println!(
        "{:<44} {:>6} {:>8} {:>8} {:>8} {:>8} {:>9}",
        "query", "files", "first50", "first99", "done50", "done99", "unindexed"
    );

    for q in QUERIES {
        let (pattern, kind, files) = plan(q);
        let is_path = kind == Kind::Path;
        let cs = pattern.as_ref().is_some_and(|p| p.case_sensitive);
        let bm25 = pattern
            .as_ref()
            .is_some_and(|p| p.pattern_type == sourcegull_query::PatternType::Keyword);
        let spec = pattern.map(|p| PatternSpec::from_pattern(&p).expect("regexp pattern"));
        let (content, path) = if is_path { (None, spec) } else { (spec, None) };
        let mut iq = IndexQuery {
            repos: vec![RepoBranches {
                repo: RepoId(1),
                branches: vec!["HEAD".into()],
            }],
            pattern: content.clone(),
            path_pattern: path.clone(),
            case_sensitive: cs,
            files: files.clone(),
            file_limit: Limits::default().default_count,
            max_line_len: None,
            symbols: kind == Kind::Symbol,
            bm25,
            repos_only: false,
            flush_wall_time: None,
            cancel: Default::default(),
        };
        let mut first = Vec::new();
        let mut done = Vec::new();
        let mut n = 0;
        for _ in 0..iterations {
            let t = Instant::now();
            let mut t_first = None;
            n = 0;
            index
                .search(&iq, &mut |_| {
                    t_first.get_or_insert_with(|| t.elapsed());
                    n += 1;
                    ControlFlow::Continue(())
                })
                .expect("search");
            done.push(t.elapsed());
            first.push(t_first.unwrap_or_else(|| t.elapsed()));
        }
        first.sort();
        done.sort();

        // Parity with no limit.
        iq.file_limit = 0;
        let mut indexed = Vec::new();
        index
            .search(&iq, &mut |f| {
                indexed.push((f.path, f.chunks, f.path_ranges, f.symbols));
                ControlFlow::Continue(())
            })
            .expect("search");
        let mut paths = PathFilter::compile(&files, cs).expect("filters");
        if let Some(p) = &path {
            paths = paths.with_pattern(Matcher::from_spec(p, cs).expect("compiles"));
        }
        let matcher = content
            .as_ref()
            .map(|c| Matcher::from_spec(c, cs).expect("compiles"));
        let mut unindexed = Vec::new();
        let t = Instant::now();
        let opts = UnindexedOptions::default();
        let emit = |f: FileMatch| {
            unindexed.push((f.path, f.chunks, f.path_ranges, f.symbols));
            ControlFlow::Continue(())
        };
        if kind == Kind::Symbol {
            let prefilter = content
                .as_ref()
                .map_or(TrigramQuery::All, |c| trigram_query(c, cs));
            let m = matcher.as_ref();
            search_commit_symbols(&repo, head, m, &prefilter, &paths, &opts, emit)
        } else {
            search_commit(&repo, head, matcher.as_ref(), &paths, &opts, emit)
        }
        .expect("unindexed search");
        let unindexed_ms = t.elapsed().as_secs_f64() * 1e3;
        // Ranked results come best first; compare in path order.
        indexed.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(indexed == unindexed, "results differ for {q:?}");

        println!(
            "{:<44} {:>6} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.1}",
            q,
            n,
            pct(&first, 0.5),
            pct(&first, 0.99),
            pct(&done, 0.5),
            pct(&done, 0.99),
            unindexed_ms,
        );
    }
    println!("(ms; first/done = to the first result / to the end at the default 500-file limit)");
}
