//! Delta shards against full rebuilds: builds the default branch at
//! `<base-rev>`, moves it to `<new-rev>` with a delta, then rebuilds
//! `<new-rev>` from scratch, and checks that both indexes hold the same
//! docs and return the same files for a few queries.
//!
//! `cargo run --release -p sourcegull-search-exec --example delta_bench -- <git-dir> <scratch-dir> <base-rev> <new-rev>`

use std::ops::ControlFlow;
use std::time::Instant;

use sourcegull_search::PatternSpec;
use sourcegull_search::index::{Branch, BuildRequest, IndexQuery, RepoBranches, SearchIndex};
use sourcegull_search_exec::ShardIndex;
use sourcegull_types::RepoId;

fn request(git_dir: &str, commit: String) -> BuildRequest {
    BuildRequest {
        repo: RepoId(1),
        name: "bench".into(),
        git_dir: git_dir.into(),
        branches: vec![Branch {
            name: "HEAD".into(),
            commit,
        }],
        ranks: None,
        always_index: Default::default(),
    }
}

fn rev_parse(git_dir: &str, rev: &str) -> String {
    let repo = gix::open(git_dir).expect("open repo");
    let id = repo.rev_parse_single(rev).expect("rev");
    id.object()
        .expect("object")
        .peel_to_commit()
        .expect("commit")
        .id
        .to_string()
}

fn files(index: &ShardIndex, pattern: &str) -> Vec<(String, [u8; 20])> {
    let q = IndexQuery {
        repos: vec![RepoBranches {
            repo: RepoId(1),
            branches: vec!["HEAD".into()],
        }],
        pattern: Some(PatternSpec::Atom {
            regex: regex_syntax::escape(pattern),
            negated: false,
        }),
        ..IndexQuery::default()
    };
    let mut out = Vec::new();
    index
        .search(&q, &mut |f| {
            out.push((f.path, f.oid));
            ControlFlow::Continue(())
        })
        .expect("search");
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (git_dir, scratch, base, new) = (&args[1], &args[2], &args[3], &args[4]);
    let (base, new) = (rev_parse(git_dir, base), rev_parse(git_dir, new));
    let _ = std::fs::remove_dir_all(scratch);
    let open = |name: &str| {
        let (index, _) =
            ShardIndex::open(format!("{scratch}/{name}"), Default::default()).expect("open index");
        index
    };

    let delta = open("delta");
    let t = Instant::now();
    let built = delta.build(&request(git_dir, base)).expect("base build");
    println!(
        "base build   {:8.1} ms  {} docs, {:.1} MB shard",
        t.elapsed().as_secs_f64() * 1e3,
        built.docs,
        built.index_bytes as f64 / 1e6
    );
    let t = Instant::now();
    let moved = delta
        .apply_delta(&request(git_dir, new.clone()))
        .expect("delta");
    let rs = delta.shards(RepoId(1)).expect("indexed");
    let (delta_docs, hidden) = rs.delta.as_ref().map_or((0, 0), |d| {
        (d.doc_count(), d.tombstones().map_or(0, |(_, h)| h.len()))
    });
    println!(
        "delta        {:8.1} ms  {delta_docs} docs, {hidden} tombstoned, {:.1} KB delta{}",
        t.elapsed().as_secs_f64() * 1e3,
        rs.delta.as_ref().map_or(0, |d| d.size_bytes()) as f64 / 1e3,
        if rs.delta.is_none() {
            " (compacted)"
        } else {
            ""
        }
    );

    let full = open("full");
    let t = Instant::now();
    let rebuilt = full.build(&request(git_dir, new)).expect("full build");
    println!(
        "full rebuild {:8.1} ms  {} docs",
        t.elapsed().as_secs_f64() * 1e3,
        rebuilt.docs
    );
    assert_eq!(moved.docs, rebuilt.docs);
    assert_eq!(moved.content_bytes, rebuilt.content_bytes);
    for p in ["func", "return nil", "ErrUnexpectedEOF", "TODO"] {
        assert_eq!(files(&delta, p), files(&full, p), "{p}");
    }
    println!("delta and full rebuild agree");
}
