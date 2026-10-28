//! Symbol extraction throughput: extracts the symbols of every file under
//! `<dir>` whose language has a grammar, on one thread, and prints bytes
//! per second, symbols and how files were parsed (bodies skipped, or in
//! full and why) per language.
//!
//! With `CHECK=1` it also extracts every file with a full parse, prints
//! that rate and how many symbols only one of the two has, and lists the
//! first files whose symbols differ (skipping function bodies should not
//! change the output; it can where the full parse recovers from syntax
//! errors differently); `DIFFS=1` prints every such symbol as a `DIFF`
//! line: `-` only in the full parse, `+` only with bodies skipped.
//!
//! `cargo run --release -p sourcegull-symbols --example extract_bench -- <dir>`

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use sourcegull_symbols::{Parsed, Symbol, extract_traced};

fn walk(dir: &Path, out: &mut Vec<(String, String, Vec<u8>)>) {
    for e in std::fs::read_dir(dir).expect("read dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            walk(&p, out);
        } else if let Some(lang) = sourcegull_languages::detect(&p.to_string_lossy(), None)
            && sourcegull_symbols::supported(lang.id)
        {
            let path = p.to_string_lossy().into_owned();
            out.push((lang.id.to_owned(), path, std::fs::read(&p).expect("read")));
        }
    }
}

#[derive(Default)]
struct Stats {
    files: usize,
    bytes: usize,
    symbols: usize,
    time: Duration,
    full_time: Duration,
    differ: usize,
    missing: usize,
    extra: usize,
    parsed: BTreeMap<Parsed, usize>,
}

fn key(s: &Symbol) -> (u32, u32, &str, u8, Option<&str>) {
    (
        s.range.start,
        s.range.end,
        &s.name,
        s.kind as u8,
        s.container.as_deref(),
    )
}

fn main() {
    let dir = std::env::args().nth(1).expect("<dir>");
    let check = std::env::var("CHECK").is_ok_and(|v| v == "1");
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    let mut per: BTreeMap<String, Stats> = BTreeMap::new();
    let mut shown = 0;
    let all_diffs = std::env::var("DIFFS").is_ok_and(|v| v == "1");
    for (lang, path, content) in &files {
        let t = Instant::now();
        let (syms, parsed) = extract_traced(lang, content, true);
        let e = per.entry(lang.clone()).or_default();
        e.time += t.elapsed();
        e.files += 1;
        e.bytes += content.len();
        e.symbols += syms.len();
        *e.parsed.entry(parsed).or_default() += 1;
        if check {
            let t = Instant::now();
            let (full, _) = extract_traced(lang, content, false);
            e.full_time += t.elapsed();
            if full != syms {
                e.differ += 1;
                let a: Vec<_> = syms.iter().map(key).collect();
                let b: Vec<_> = full.iter().map(key).collect();
                let missing: Vec<_> = b.iter().filter(|k| !a.contains(k)).collect();
                let extra: Vec<_> = a.iter().filter(|k| !b.contains(k)).collect();
                e.missing += missing.len();
                e.extra += extra.len();
                if all_diffs {
                    for (sign, list) in [("-", &missing), ("+", &extra)] {
                        for k in list.iter() {
                            println!("DIFF {sign} {path} {:?} {} {:?}", k.2, k.3, k.0);
                        }
                    }
                }
                if shown < 20 {
                    shown += 1;
                    let (missing, extra) = (
                        &missing[..missing.len().min(3)],
                        &extra[..extra.len().min(3)],
                    );
                    println!(
                        "differs ({parsed:?}): {path}\n  missing {missing:?}\n  extra {extra:?}"
                    );
                }
            }
        }
    }
    for (lang, s) in per {
        let mb = s.bytes as f64 / 1e6;
        let secs = s.time.as_secs_f64();
        print!(
            "{lang:12} {:6} files {mb:7.1} MB {:8} symbols {secs:6.2} s {:6.1} MB/s",
            s.files,
            s.symbols,
            mb / secs
        );
        if check {
            print!(
                " | full parse {:6.1} MB/s, {} files differ: {} symbols missing, {} extra",
                mb / s.full_time.as_secs_f64(),
                s.differ,
                s.missing,
                s.extra
            );
        }
        println!("\n{:12} {:?}", "", s.parsed);
    }
}
