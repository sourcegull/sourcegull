//! Index build throughput.
//!
//! For each repository: first a single-threaded breakdown of the per-file
//! work over the HEAD tree (tree listing, blob reads, language detection,
//! LZ4, newlines, trigrams, symbols, also as a rate on the files with a
//! grammar, and the writer: postings, FST and serialization), then full
//! builds with [`build_shard`] to a file (fsync included) on 1 and N
//! threads, with and without symbols, best of `reps`. MB are text bytes
//! indexed (binary and too-large files carry no content). MB/s/thread is
//! the wall-clock rate divided by the threads used; MB per CPU-s divides by
//! the CPU time the build consumed (user and system), which stays
//! meaningful on a busy machine.
//!
//! `cargo run --release -p sourcegull-indexer --example build_bench -- <out-dir> <threads> <reps> <git-dir>...`
//! (`scripts/bench/index-build.sh` fetches the pinned corpora and runs it).

use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant};

use sourcegull_gitstore::read::{ListTreeOptions, is_binary, list_tree};
use sourcegull_index_format::{Codec, DocInput, ShardWriter, prepare_doc, trigrams};
use sourcegull_indexer::{
    BuildOptions, DEFAULT_MAX_FILE_SIZE, KnownSymbols, ShardSpec, build_shard,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: build_bench <out-dir> <threads> <reps> <git-dir>...");
        std::process::exit(2);
    }
    let out_dir = Path::new(&args[1]);
    let threads: usize = args[2].parse().expect("thread count");
    let reps: usize = args[3].parse().expect("repetitions");
    std::fs::create_dir_all(out_dir).expect("out dir");
    for git_dir in &args[4..] {
        bench(git_dir, out_dir, threads, reps);
    }
}

fn bench(git_dir: &str, out_dir: &Path, threads: usize, reps: usize) {
    let repo = gix::open(git_dir).expect("open repo");
    let head = repo.head_id().expect("HEAD").detach();
    println!("== {git_dir} @ {head}");

    // Phases, one thread.
    let t = Instant::now();
    let mut entries = Vec::new();
    let opts = ListTreeOptions {
        recursive: true,
        limit: usize::MAX,
        ..ListTreeOptions::default()
    };
    list_tree(&repo, head, &opts, |e| {
        if e.kind.is_blob() {
            entries.push((e.path, e.oid));
        }
        ControlFlow::Continue(())
    })
    .expect("list tree");
    entries.sort();
    let list = t.elapsed();

    let t = Instant::now();
    let mut small = Vec::new();
    for (path, oid) in &entries {
        if repo.find_header(*oid).expect("header").size() <= DEFAULT_MAX_FILE_SIZE {
            small.push((path, oid));
        }
    }
    let headers = t.elapsed();
    let t = Instant::now();
    let mut blobs = Vec::new();
    for (path, oid) in small {
        let blob = repo.find_blob(*oid).expect("blob");
        if !is_binary(&blob.data) {
            blobs.push((path.clone(), *oid, blob.data.clone()));
        }
    }
    let read = t.elapsed();
    let bytes: u64 = blobs.iter().map(|b| b.2.len() as u64).sum();
    let mb = bytes as f64 / 1e6;
    println!(
        "{} files, {} text files, {mb:.1} MB of text",
        entries.len(),
        blobs.len()
    );

    let phase = |name: &str, d: Duration| {
        println!(
            "  {name:<28} {:>8.3} s {:>8.0} MB/s",
            d.as_secs_f64(),
            mb / d.as_secs_f64()
        );
    };
    phase("list tree", list);
    phase("object headers", headers);
    phase("read blobs", read);
    let t = Instant::now();
    let langs: Vec<_> = blobs
        .iter()
        .map(|b| sourcegull_languages::detect(&b.0, None).map(|l| l.id))
        .collect();
    phase("language", t.elapsed());
    let t = Instant::now();
    let mut stored = 0usize;
    for b in &blobs {
        stored += lz4_flex::block::compress(&b.2).len();
    }
    phase("lz4", t.elapsed());
    let t = Instant::now();
    let mut lines = 0usize;
    for b in &blobs {
        lines += memchr::memchr_iter(b'\n', &b.2).count();
    }
    phase("newlines (count only)", t.elapsed());
    let t = Instant::now();
    let mut distinct = 0usize;
    for b in &blobs {
        distinct += trigrams(&b.2).len();
    }
    phase("trigrams", t.elapsed());
    let t = Instant::now();
    let mut symbols = Vec::with_capacity(blobs.len());
    for (b, lang) in blobs.iter().zip(&langs) {
        symbols.push(match lang {
            Some(l) if with_symbols() => sourcegull_indexer::doc_symbols(l, &b.2),
            _ => Vec::new(),
        });
    }
    if with_symbols() {
        let d = t.elapsed();
        phase("symbols", d);
        // The rate on the files symbols are extracted from (a grammar, not
        // over sourcegull_symbols::MAX_BYTES), which is what the extractor sees.
        let (files, bytes) = blobs
            .iter()
            .zip(&langs)
            .filter(|(b, l)| {
                l.is_some_and(sourcegull_symbols::supported)
                    && b.2.len() <= sourcegull_symbols::MAX_BYTES
            })
            .fold((0, 0), |(n, s), (b, _)| (n + 1, s + b.2.len()));
        println!(
            "  {:<28} {:>8.1} MB in {files} files, {:>6.1} MB/s",
            "symbols: files with a grammar",
            bytes as f64 / 1e6,
            bytes as f64 / 1e6 / d.as_secs_f64()
        );
    }
    let t = Instant::now();
    let prepared: Vec<_> = blobs
        .iter()
        .zip(&langs)
        .zip(&symbols)
        .map(|((b, lang), syms)| {
            prepare_doc(
                &DocInput {
                    repo: 0,
                    branch_mask: 1,
                    path: &b.0,
                    oid: b.1.as_bytes().try_into().expect("SHA-1"),
                    lang: *lang,
                    size: b.2.len() as u64,
                    flags: 0,
                    content: &b.2,
                    symbols: syms,
                },
                Codec::Lz4,
            )
        })
        .collect();
    phase("prepare_doc (lz4+nl+trigrams)", t.elapsed());
    let t = Instant::now();
    let mut writer = ShardWriter::new(Codec::Lz4);
    writer
        .add_repo(sourcegull_index_format::RepoMeta {
            id: 1,
            name: git_dir.into(),
            rank: 0,
            branches: vec![sourcegull_index_format::BranchMeta {
                name: "HEAD".into(),
                commit: head.as_bytes().try_into().expect("SHA-1"),
            }],
        })
        .expect("repo");
    let mut prepared = prepared;
    while !prepared.is_empty() {
        let rest = prepared.split_off(prepared.len().min(2048));
        writer
            .push_batch(std::mem::replace(&mut prepared, rest))
            .expect("push");
    }
    let push = t.elapsed();
    let t = Instant::now();
    let shard = writer.finish_to_vec().expect("finish").len();
    phase("writer: postings fill", push);
    phase("writer: encode + serialize", t.elapsed());
    println!(
        "  ({lines} lines, {distinct} doc-trigrams, lz4 {:.2}x, shard {:.2}x of text)",
        stored as f64 / bytes as f64,
        shard as f64 / bytes as f64
    );
    drop((blobs, symbols));

    // Full builds.
    let spec = ShardSpec {
        repo_id: 1,
        name: git_dir.into(),
        rank: 0,
        branches: vec![("HEAD".into(), head)],
        ranks: None,
        always_index: Default::default(),
    };
    let out = out_dir.join("bench.gullshard");
    let mut counts = vec![1];
    if threads > 1 {
        counts.push(threads);
    }
    for symbols in [false, true] {
        if symbols && !with_symbols() {
            continue;
        }
        for &n in &counts {
            let opts = BuildOptions {
                threads: n,
                symbols,
                ..BuildOptions::default()
            };
            let (mut best, mut best_cpu) = (Duration::MAX, f64::MAX);
            let mut stats = None;
            for _ in 0..reps {
                let cpu = cpu_secs();
                let s = build_shard(&repo, &spec, &opts, &KnownSymbols::default(), &out)
                    .expect("build");
                best = best.min(s.elapsed);
                best_cpu = best_cpu.min(cpu_secs() - cpu);
                stats = Some(s);
            }
            let s = stats.expect("one rep");
            let mb = s.content_bytes as f64 / 1e6;
            let rate = mb / best.as_secs_f64();
            println!(
                "  build {:<10} {n} thread(s): {:>7.3} s, {:>6.0} MB/s, {:>6.1} MB/s/thread, {:>6.1} MB per CPU-s, shard {:.2}x",
                if symbols { "+symbols" } else { "content" },
                best.as_secs_f64(),
                rate,
                rate / n as f64,
                mb / best_cpu,
                s.shard_bytes as f64 / s.content_bytes as f64,
            );
        }
    }
    let _ = std::fs::remove_file(&out);
}

/// `SYMBOLS=0` skips symbol extraction (the slowest part, reported apart).
fn with_symbols() -> bool {
    std::env::var("SYMBOLS").map_or(true, |v| v != "0")
}

/// User and system CPU time of this process so far (Linux, 10 ms ticks).
fn cpu_secs() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the parenthesized command name; utime and stime are 14 and 15.
    let rest = stat.rsplit_once(')').map_or("", |r| r.1);
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (tick(11) + tick(12)) / 100.0
}
