//! Processes a SCIP index file into a code graph and reports throughput:
//! `cargo run --release -p sourcegull-codeintel-scip --example process -- index.scip [root]`.

use std::time::Instant;

use sourcegull_codeintel_scip::{CodeGraph, process_index};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: process <index.scip> [root]")?;
    let root = args.next().unwrap_or_default();
    let size = std::fs::metadata(&path)?.len();
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("graph");
    let started = Instant::now();
    let p = process_index(
        std::fs::File::open(&path)?,
        &root,
        &out,
        dir.path(),
        &|| false,
    )?;
    let secs = started.elapsed().as_secs_f64();
    let g = &p.graph;
    println!(
        "{} by {} {}: {} documents, {} occurrences, {} symbols, {} postings, {} packages",
        path,
        p.tool_name,
        p.tool_version,
        g.num_documents,
        g.num_occurrences,
        g.num_symbols,
        g.postings.len(),
        g.packages.len()
    );
    println!(
        "index {:.1} MB -> graph {:.1} MB in {:.2} s ({:.1} MB/s)",
        size as f64 / 1e6,
        g.size as f64 / 1e6,
        secs,
        size as f64 / 1e6 / secs
    );

    let started = Instant::now();
    let graph = CodeGraph::open(&out)?;
    let mut occurrences = 0usize;
    let mut lookups = 0usize;
    for (path, _) in graph.paths() {
        let doc = graph.document(&path)?.ok_or("missing document")?;
        occurrences += doc.occurrences.len();
        for o in doc.occurrences.iter().filter(|o| o.is_definition()) {
            if let Some(s) = o.symbol {
                graph.symbol_by_id(s)?;
                lookups += 1;
            }
        }
    }
    println!(
        "read back {} documents, {occurrences} occurrences, {lookups} symbol records in {:.2} s",
        graph.num_documents(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
