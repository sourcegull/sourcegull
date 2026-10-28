//! Per-repository build overhead: builds the shards of many small
//! repositories one after another, as a repo node's index queue does, and
//! splits the time per repository into opening the repository and resolving
//! HEAD, the build into memory, writing the shard file (with its fsync and
//! rename) and opening the shard.
//!
//! `cargo run --release -p sourcegull-indexer --example small_bench -- <out-dir> <threads> <git-dir>...`

use std::path::Path;
use std::time::{Duration, Instant};

use sourcegull_index_format::Shard;
use sourcegull_indexer::{BuildOptions, KnownSymbols, ShardSpec, build_shard, build_shard_to_vec};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: small_bench <out-dir> <threads> <git-dir>...");
        std::process::exit(2);
    }
    let out_dir = Path::new(&args[1]);
    let threads: usize = args[2].parse().expect("thread count");
    std::fs::create_dir_all(out_dir).expect("out dir");
    let repos = &args[3..];
    for symbols in [false, true] {
        let opts = BuildOptions {
            threads,
            symbols,
            ..BuildOptions::default()
        };
        let mut t = [Duration::ZERO; 4];
        let mut bytes = 0u64;
        let cpu = cpu_secs();
        let start = Instant::now();
        for (i, git_dir) in repos.iter().enumerate() {
            let s = Instant::now();
            let repo = gix::open(git_dir).expect("open repo");
            let head = repo.head_id().expect("HEAD").detach();
            let spec = ShardSpec {
                repo_id: i as i32 + 1,
                name: git_dir.clone(),
                rank: 0,
                branches: vec![("HEAD".into(), head)],
                ranks: None,
                always_index: Default::default(),
            };
            t[0] += s.elapsed();
            let s = Instant::now();
            let (shard, stats) = build_shard_to_vec(&repo, &spec, &opts).expect("build");
            t[1] += s.elapsed();
            bytes += stats.content_bytes;
            drop(shard);
            let out = out_dir.join(format!("{i}.gullshard"));
            let s = Instant::now();
            build_shard(&repo, &spec, &opts, &KnownSymbols::default(), &out).expect("build");
            t[2] += s.elapsed();
            let s = Instant::now();
            let shard = Shard::open(&out).expect("open shard");
            t[3] += s.elapsed();
            drop(shard);
            let _ = std::fs::remove_file(&out);
        }
        let n = repos.len() as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1e3 / n;
        let wall = start.elapsed().as_secs_f64();
        println!(
            "{} repos, {:.1} MB, symbols {symbols}, {threads} thread(s): open {:.2} ms, build in memory {:.2} ms, build to a file {:.2} ms (so writing and fsync {:.2} ms), shard open {:.3} ms per repo; {:.1} s wall, {:.1} s CPU",
            repos.len(),
            bytes as f64 / 1e6,
            ms(t[0]),
            ms(t[1]),
            ms(t[2]),
            ms(t[2]) - ms(t[1]),
            ms(t[3]),
            wall,
            cpu_secs() - cpu,
        );
    }
}

/// User and system CPU time of this process so far (Linux, 10 ms ticks).
fn cpu_secs() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map_or("", |r| r.1);
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (tick(11) + tick(12)) / 100.0
}
