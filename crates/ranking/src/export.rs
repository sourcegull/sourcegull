//! The reference graph: file → file edges weighted by reference count, exported
//! from code graph files.
//!
//! [`export_upload`] reads one upload's code graph: each document's non-definition
//! occurrences, counted per symbol. A symbol defined in the same upload gives an edge to each
//! document defining it (the count split between them); a global symbol defined elsewhere is
//! kept by its hash, the one in `codeintel_symbol_postings`. Local symbols stay inside their
//! document and give no edge.
//!
//! [`assemble`] merges the exports of the uploads serving each repository's default branch
//! head into one graph whose nodes are (repository, path): two uploads of one repository
//! (different roots or indexers) covering the same file share its node. A global symbol
//! reaches the documents defining it in any of these uploads (in the same repository or
//! another one, as cross-repository navigation reaches them through package monikers), its
//! count split between them.

use std::collections::HashMap;

use sourcegull_codeintel_scip::CodeGraph;
use sourcegull_codeintel_scip::graph::{GraphError, posting};
use sourcegull_codeintel_scip::symbol::{is_local, symbol_hash};
use sourcegull_types::RepoId;

use crate::pagerank::{Graph, GraphBuilder};

/// What ranking needs from one upload's code graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UploadExport {
    /// Document id → repository-relative path.
    pub paths: Vec<String>,
    /// (referencing document, defining document, weight) within the upload.
    pub edges: Vec<(u32, u32, f32)>,
    /// (global symbol hash, defining document), sorted.
    pub definitions: Vec<(i64, u32)>,
    /// (referencing document, global symbol hash, references) for global symbols the upload
    /// does not define.
    pub external: Vec<(u32, i64, u32)>,
}

/// Exports one upload's references. Reads every document of the graph.
pub fn export_upload(graph: &CodeGraph) -> Result<UploadExport, GraphError> {
    let mut out = UploadExport {
        paths: (0..graph.num_documents())
            .map(|id| graph.document_path(id).map(str::to_owned))
            .collect::<Result<_, _>>()?,
        ..UploadExport::default()
    };
    // Per symbol: the documents defining it, and the hash of a global one.
    let symbols = graph.num_symbols() as usize;
    let mut defs: Vec<Vec<u32>> = vec![Vec::new(); symbols];
    let mut hashes: Vec<Option<i64>> = vec![None; symbols];
    for id in 0..graph.num_symbols() {
        let i = id as usize;
        defs[i] = graph
            .symbol_postings(id)?
            .into_iter()
            .filter(|p| p.flags & posting::DEFINITION != 0)
            .map(|p| p.document)
            .collect();
        let name = graph.symbol_name(id)?;
        if !is_local(name) {
            let h = symbol_hash(name);
            hashes[i] = Some(h);
            out.definitions.extend(defs[i].iter().map(|&doc| (h, doc)));
        }
    }
    out.definitions.sort_unstable();
    out.definitions.dedup();
    let mut counts: Vec<(u32, u32)> = Vec::new();
    for doc in 0..graph.num_documents() {
        counts.clear();
        for occ in graph.document_by_id(doc)?.occurrences {
            match occ.symbol {
                Some(s) if !occ.is_definition() && (s as usize) < symbols => counts.push((s, 1)),
                _ => {}
            }
        }
        counts.sort_unstable();
        counts.dedup_by(|b, a| {
            if a.0 == b.0 {
                a.1 += b.1;
                true
            } else {
                false
            }
        });
        for &(s, n) in &counts {
            let targets = &defs[s as usize];
            if !targets.is_empty() {
                let w = n as f32 / targets.len() as f32;
                out.edges
                    .extend(targets.iter().filter(|&&t| t != doc).map(|&t| (doc, t, w)));
            } else if let Some(h) = hashes[s as usize] {
                out.external.push((doc, h, n));
            }
        }
    }
    Ok(out)
}

/// The reference graph over every exported upload.
#[derive(Debug, Clone, Default)]
pub struct ReferenceGraph {
    /// Node id → file, sorted by (repository, path).
    pub nodes: Vec<(RepoId, String)>,
    pub graph: Graph,
}

/// Merges the exports of uploads (each with its repository) into one graph. Node ids follow
/// (repository, path) order, so the graph does not depend on the order of `uploads`.
pub fn assemble(uploads: &[(RepoId, UploadExport)]) -> ReferenceGraph {
    let mut nodes: Vec<(RepoId, &str)> = uploads
        .iter()
        .flat_map(|(repo, e)| e.paths.iter().map(move |p| (*repo, p.as_str())))
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    let ids: HashMap<(RepoId, &str), u32> = nodes
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, i as u32))
        .collect();
    // Per upload, document id → node id.
    let maps: Vec<Vec<u32>> = uploads
        .iter()
        .map(|(repo, e)| e.paths.iter().map(|p| ids[&(*repo, p.as_str())]).collect())
        .collect();
    let mut defined: HashMap<i64, Vec<u32>> = HashMap::new();
    for ((_, e), map) in uploads.iter().zip(&maps) {
        for &(h, doc) in &e.definitions {
            defined.entry(h).or_default().push(map[doc as usize]);
        }
    }
    for v in defined.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    let mut b = GraphBuilder::new(nodes.len() as u32);
    b.reserve(
        uploads
            .iter()
            .map(|(_, e)| e.edges.len() + e.external.len())
            .sum(),
    );
    for ((_, e), map) in uploads.iter().zip(&maps) {
        for &(s, d, w) in &e.edges {
            b.add_edge(map[s as usize], map[d as usize], w);
        }
        for &(s, h, n) in &e.external {
            let Some(targets) = defined.get(&h) else {
                continue;
            };
            let w = n as f32 / targets.len() as f32;
            for &t in targets {
                b.add_edge(map[s as usize], t, w);
            }
        }
    }
    ReferenceGraph {
        nodes: nodes.into_iter().map(|(r, p)| (r, p.to_owned())).collect(),
        graph: b.build(),
    }
}
