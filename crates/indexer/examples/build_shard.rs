//! Builds a shard from a repository's HEAD and prints the build stats
//! (build throughput and index size). `no-symbols` skips symbol
//! extraction; `rebuild` builds a second time reusing the first shard's
//! symbols, as a rebuild of an indexed repository does.
//!
//! `cargo run --release -p sourcegull-indexer --example build_shard -- <git-dir> <out> [threads] [no-symbols|rebuild]`

use sourcegull_index_format::Shard;
use sourcegull_indexer::{BuildOptions, BuildStats, KnownSymbols, ShardSpec, build_shard};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (git_dir, out) = (&args[1], &args[2]);
    let mut opts = BuildOptions::default();
    if let Some(t) = args.get(3) {
        opts.threads = t.parse().expect("thread count");
    }
    opts.symbols = args.get(4).is_none_or(|a| a != "no-symbols");
    let repo = gix::open(git_dir).expect("open repo");
    let head = repo.head_id().expect("HEAD").detach();
    let spec = ShardSpec {
        repo_id: 1,
        name: git_dir.clone(),
        rank: 0,
        branches: vec![("HEAD".into(), head)],
        ranks: None,
        always_index: Default::default(),
    };
    let stats = build_shard(&repo, &spec, &opts, &Default::default(), out.as_ref()).expect("build");
    print(&stats, &opts);
    if args.get(4).is_some_and(|a| a == "rebuild") {
        let old = Shard::open(out.as_ref()).expect("open shard");
        let mut known = KnownSymbols::default();
        known.add(&old, 0..old.doc_count());
        let stats = build_shard(&repo, &spec, &opts, &known, out.as_ref()).expect("rebuild");
        print!("rebuild: ");
        print(&stats, &opts);
    }
}

fn print(stats: &BuildStats, opts: &BuildOptions) {
    let secs = stats.elapsed.as_secs_f64();
    let mb = stats.content_bytes as f64 / 1e6;
    println!(
        "docs {} (binary {}, large {}), content {:.1} MB, shard {:.1} MB ({:.2}x), {:.2}s, {:.0} MB/s total, {:.0} MB/s/thread ({} threads)",
        stats.docs,
        stats.skipped_binary,
        stats.skipped_large,
        mb,
        stats.shard_bytes as f64 / 1e6,
        stats.shard_bytes as f64 / stats.content_bytes as f64,
        secs,
        mb / secs,
        mb / secs / opts.threads as f64,
        opts.threads,
    );
}
