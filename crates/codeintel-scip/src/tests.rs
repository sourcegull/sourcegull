use std::io::Write;

use prost::Message;

use crate::graph::{CodeGraph, MonikerKind, Range, posting, relation};
use crate::process::{ProcessError, document_path, process_index};
use crate::proto::{self, role};
use crate::stream::{DecodeError, IndexReader, Item};
use crate::symbol::{Package, symbol_hash};

/// The whole `Index` message, for writing test indexes.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Index {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<proto::Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub documents: Vec<proto::Document>,
    #[prost(message, repeated, tag = "3")]
    pub external_symbols: Vec<proto::SymbolInformation>,
}

const FOO: &str = "scip-go gomod example.com/demo v1.0.0 `example.com/demo`/Foo().";
const BAR: &str = "scip-go gomod example.com/demo v1.0.0 `example.com/demo`/Bar#";
const IFACE: &str = "scip-go gomod example.com/demo v1.0.0 `example.com/demo`/Doer#";
const PRINTLN: &str = "scip-go gomod github.com/golang/go/src go1.22 fmt/Println().";

fn occ(range: &[i32], symbol: &str, roles: i32) -> proto::Occurrence {
    proto::Occurrence {
        range: range.to_vec(),
        symbol: symbol.into(),
        symbol_roles: roles,
        ..Default::default()
    }
}

fn info(symbol: &str, docs: &[&str]) -> proto::SymbolInformation {
    proto::SymbolInformation {
        symbol: symbol.into(),
        documentation: docs.iter().map(|d| (*d).to_owned()).collect(),
        display_name: symbol.rsplit('/').next().unwrap_or_default().into(),
        ..Default::default()
    }
}

fn sample() -> Index {
    let mut bar = info(BAR, &["Bar does things."]);
    bar.relationships.push(proto::Relationship {
        symbol: IFACE.into(),
        is_implementation: true,
        ..Default::default()
    });
    bar.signature_documentation = Some(proto::Signature {
        language: "go".into(),
        text: "type Bar struct{}".into(),
    });
    bar.kind = 7;
    Index {
        metadata: Some(proto::Metadata {
            tool_info: Some(proto::ToolInfo {
                name: "scip-go".into(),
                version: "0.1.0".into(),
                arguments: vec![],
            }),
            project_root: "file:///src".into(),
            ..Default::default()
        }),
        documents: vec![
            proto::Document {
                language: "go".into(),
                relative_path: "foo.go".into(),
                position_encoding: proto::encoding::UTF8,
                // Out of order on purpose; one is a duplicate to merge, one is invalid.
                occurrences: vec![
                    occ(&[4, 1, 8], PRINTLN, role::READ_ACCESS),
                    occ(&[2, 5, 8], FOO, role::DEFINITION),
                    occ(&[4, 1, 8], PRINTLN, role::IMPORT),
                    occ(&[3, 2, 9], "local 0", role::DEFINITION),
                    occ(&[5, 9, 3], FOO, 0),
                    proto::Occurrence {
                        single_line_range: Some(proto::SingleLineRange {
                            line: 6,
                            start_character: 1,
                            end_character: 4,
                        }),
                        symbol: BAR.into(),
                        diagnostics: vec![proto::Diagnostic {
                            severity: 2,
                            message: "deprecated".into(),
                            tags: vec![2],
                            ..Default::default()
                        }],
                        override_documentation: vec!["override".into()],
                        multi_line_enclosing_range: Some(proto::MultiLineRange {
                            start_line: 6,
                            start_character: 0,
                            end_line: 8,
                            end_character: 1,
                        }),
                        ..Default::default()
                    },
                ],
                symbols: vec![info(FOO, &["Foo does things.", "```go\nfunc Foo()\n```"])],
            },
            proto::Document {
                language: "go".into(),
                relative_path: "bar/bar.go".into(),
                occurrences: vec![
                    occ(&[1, 5, 1, 8], BAR, role::DEFINITION),
                    occ(&[7, 0, 3], FOO, role::READ_ACCESS),
                ],
                symbols: vec![bar, info("local 3", &[])],
                ..Default::default()
            },
            proto::Document {
                relative_path: "../escape.go".into(),
                occurrences: vec![occ(&[0, 0, 1], FOO, 0)],
                ..Default::default()
            },
            // A second chunk of foo.go.
            proto::Document {
                language: "go".into(),
                relative_path: "./foo.go".into(),
                occurrences: vec![occ(&[0, 0, 3], BAR, 0)],
                ..Default::default()
            },
        ],
        external_symbols: vec![
            info(PRINTLN, &["Println formats."]),
            info(IFACE, &["Doer is an interface."]),
        ],
    }
}

fn run(bytes: Vec<u8>, root: &str) -> Result<(crate::Processed, CodeGraph), ProcessError> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph");
    let p = process_index(std::io::Cursor::new(bytes), root, &out, dir.path(), &|| {
        false
    })?;
    let g = CodeGraph::open(&out).unwrap();
    assert_eq!(std::fs::metadata(&out).unwrap().len(), p.graph.size);
    Ok((p, g))
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

#[test]
fn processes_documents_symbols_and_monikers() {
    let (p, g) = run(sample().encode_to_vec(), "").unwrap();
    assert_eq!(p.tool_name, "scip-go");
    assert_eq!(p.tool_version, "0.1.0");
    assert_eq!(p.skipped_documents, 1);
    assert_eq!(g.num_documents(), 2);
    assert_eq!(p.graph.num_documents, 2);
    assert_eq!(
        g.paths().into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
        ["bar/bar.go", "foo.go"]
    );
    assert_eq!(g.paths_with_prefix("bar/").len(), 1);

    let foo = g.document("foo.go").unwrap().unwrap();
    assert_eq!(foo.language, "go");
    assert_eq!(foo.position_encoding, 1);
    let ranges: Vec<(u32, u32)> = foo
        .occurrences
        .iter()
        .map(|o| (o.range.start_line, o.range.start_character))
        .collect();
    // Sorted, the duplicate merged, the reversed range dropped, the second chunk included.
    assert_eq!(ranges, [(0, 0), (2, 5), (3, 2), (4, 1), (6, 1)]);
    assert_eq!(p.graph.num_occurrences, 4 + 2 + 1);
    let println = &foo.occurrences[3];
    assert_eq!(println.roles, (role::READ_ACCESS | role::IMPORT) as u32);
    assert_eq!(g.symbol_name(println.symbol.unwrap()).unwrap(), PRINTLN);
    let bar_ref = &foo.occurrences[4];
    assert_eq!(bar_ref.override_documentation, ["override"]);
    assert_eq!(bar_ref.diagnostics[0].message, "deprecated");
    assert_eq!(bar_ref.diagnostics[0].tags, [2]);
    assert_eq!(
        bar_ref.enclosing_range,
        Some(Range {
            start_line: 6,
            start_character: 0,
            end_line: 8,
            end_character: 1
        })
    );
    let bar_def = &g.document("bar/bar.go").unwrap().unwrap().occurrences[0];
    assert_eq!(bar_def.range.end_line, 1);
    assert!(bar_def.is_definition());

    let at = g.occurrences_at("foo.go", 2, 6).unwrap();
    assert_eq!(at.len(), 1);
    assert_eq!(g.symbol_name(at[0].symbol.unwrap()).unwrap(), FOO);
    assert!(g.occurrences_at("foo.go", 2, 8).unwrap().is_empty());
    assert!(g.occurrences_at("missing.go", 0, 0).unwrap().is_empty());

    let foo_sym = g.symbol(FOO).unwrap().unwrap();
    let info = foo_sym.info.unwrap();
    assert_eq!(
        info.documentation,
        ["Foo does things.", "```go\nfunc Foo()\n```"]
    );
    let foo_id = g.document_id("foo.go").unwrap();
    let bar_id = g.document_id("bar/bar.go").unwrap();
    let mut postings: Vec<(u32, u8)> = foo_sym
        .postings
        .iter()
        .map(|p| (p.document, p.flags))
        .collect();
    postings.sort_unstable();
    let mut want = vec![(foo_id, posting::DEFINITION), (bar_id, posting::REFERENCE)];
    want.sort_unstable();
    assert_eq!(postings, want);

    let bar = g.symbol(BAR).unwrap().unwrap();
    let bar_info = bar.info.unwrap();
    assert_eq!(bar_info.kind, 7);
    assert_eq!(bar_info.signature.as_deref(), Some("type Bar struct{}"));
    assert_eq!(bar_info.relationships.len(), 1);
    assert_eq!(bar_info.relationships[0].flags, relation::IMPLEMENTATION);
    assert_eq!(
        g.symbol_name(bar_info.relationships[0].symbol).unwrap(),
        IFACE
    );
    let iface = g.symbol(IFACE).unwrap().unwrap();
    assert_eq!(iface.implementations, [bar.id]);
    assert_eq!(iface.postings[0].document, bar_id);
    assert_eq!(iface.postings[0].flags, posting::IMPLEMENTATION);
    assert_eq!(iface.info.unwrap().documentation, ["Doer is an interface."]);
    assert!(g.symbol("nope").unwrap().is_none());
    // The light postings read agrees with the full record for every symbol.
    for id in 0..g.num_symbols() {
        assert_eq!(
            g.symbol_postings(id).unwrap(),
            g.symbol_by_id(id).unwrap().postings
        );
    }

    // Postings: global symbols only, with their flag union.
    let mut want = vec![
        (symbol_hash(FOO), posting::DEFINITION | posting::REFERENCE),
        (symbol_hash(BAR), posting::DEFINITION | posting::REFERENCE),
        (symbol_hash(PRINTLN), posting::REFERENCE),
        (symbol_hash(IFACE), posting::IMPLEMENTATION),
    ];
    want.sort_unstable();
    assert_eq!(p.graph.postings, want);
    let pkg = |name: &str, version: &str| Package {
        scheme: "scip-go".into(),
        manager: "gomod".into(),
        name: name.into(),
        version: version.into(),
    };
    assert_eq!(
        p.graph.packages,
        [
            (MonikerKind::Export, pkg("example.com/demo", "v1.0.0")),
            (
                MonikerKind::Import,
                pkg("github.com/golang/go/src", "go1.22")
            ),
        ]
    );
}

#[test]
fn gzip_and_roots() {
    let (p, g) = run(gzip(&sample().encode_to_vec()), "sub/dir/").unwrap();
    assert_eq!(p.graph.num_documents, 2);
    assert!(g.document("sub/dir/foo.go").unwrap().is_some());
    // "../escape.go" stays inside the repository under a root.
    assert!(g.document("sub/escape.go").unwrap().is_none());
    assert_eq!(document_path("sub/", "../x.go"), None);
    assert_eq!(document_path("", "a/./b.go").as_deref(), Some("a/b.go"));
    assert_eq!(document_path("", ""), None);
}

#[test]
fn empty_index() {
    let (p, g) = run(Vec::new(), "").unwrap();
    assert_eq!(p.graph.num_documents, 0);
    assert_eq!(g.num_documents(), 0);
    assert!(g.paths().is_empty());
}

#[test]
fn rejects_bad_input() {
    let bytes = sample().encode_to_vec();
    let truncated = bytes[..bytes.len() - 5].to_vec();
    assert!(matches!(
        run(truncated, ""),
        Err(ProcessError::Decode(DecodeError::Truncated))
    ));
    // An LSIF dump (JSON lines).
    assert!(matches!(
        run(
            br#"{"id":1,"type":"vertex","label":"metaData"}"#.to_vec(),
            ""
        ),
        Err(ProcessError::Decode(DecodeError::Malformed(_)))
    ));
    // Corrupt gzip.
    let mut gz = gzip(&bytes);
    let n = gz.len();
    gz.truncate(n / 2);
    assert!(matches!(run(gz, ""), Err(ProcessError::Decode(_))));
}

#[test]
fn cancels_between_documents() {
    let dir = tempfile::tempdir().unwrap();
    let r = process_index(
        std::io::Cursor::new(sample().encode_to_vec()),
        "",
        &dir.path().join("g"),
        dir.path(),
        &|| true,
    );
    assert!(matches!(r, Err(ProcessError::Canceled)));
}

#[test]
fn skips_unknown_fields_and_streams_items() {
    let mut bytes = Vec::new();
    // Field 15, varint; field 14, fixed64; field 13, fixed32; field 12, bytes.
    bytes.extend_from_slice(&[15 << 3, 0x96, 0x01]);
    bytes.push(14 << 3 | 1);
    bytes.extend_from_slice(&[0; 8]);
    bytes.push(13 << 3 | 5);
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&[12 << 3 | 2, 2, 0xaa, 0xbb]);
    bytes.extend_from_slice(&sample().encode_to_vec());
    let mut r = IndexReader::new(bytes.as_slice());
    let mut kinds = Vec::new();
    while let Some(item) = r.next_item().unwrap() {
        kinds.push(match item {
            Item::Metadata(_) => 'm',
            Item::Document(_) => 'd',
            Item::ExternalSymbol(_) => 'e',
        });
    }
    assert_eq!(kinds.iter().collect::<String>(), "mddddee");
}

#[test]
fn rejects_corrupt_graph_files() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph");
    process_index(
        std::io::Cursor::new(sample().encode_to_vec()),
        "",
        &out,
        dir.path(),
        &|| false,
    )
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert!(CodeGraph::from_vec(bytes.clone()).is_ok());
    assert!(CodeGraph::from_vec(bytes[..bytes.len() - 1].to_vec()).is_err());
    let mut flipped = bytes.clone();
    let at = flipped.len() - 20;
    flipped[at] ^= 0xff;
    assert!(CodeGraph::from_vec(flipped).is_err());
    assert!(CodeGraph::from_vec(b"not a graph".to_vec()).is_err());
}

mod prop {
    use proptest::prelude::*;

    use super::*;

    fn occurrence() -> impl Strategy<Value = (u32, u32, u32, u32, u8, i32)> {
        (0u32..200, 0u32..120, 0u32..3, 0u32..120, 0u8..6, 0i32..128)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn documents_round_trip(occs in proptest::collection::vec(occurrence(), 0..80)) {
            let symbols = ["local 1", FOO, BAR, IFACE, PRINTLN, ""];
            let doc = proto::Document {
                relative_path: "a.go".into(),
                occurrences: occs
                    .iter()
                    .map(|&(l, c, dl, ec, s, roles)| {
                        occ(&[l as i32, c as i32, (l + dl) as i32, ec as i32], symbols[s as usize], roles)
                    })
                    .collect(),
                ..Default::default()
            };
            let index = Index { documents: vec![doc.clone()], ..Default::default() };
            let (_, g) = run(index.encode_to_vec(), "").unwrap();
            let got = g.document("a.go").unwrap().unwrap();
            // Expected: valid ranges, sorted, same (range, symbol) merged.
            let mut want: Vec<(Range, String, u32)> = Vec::new();
            for o in &doc.occurrences {
                let Some(r) = o.range4() else { continue };
                let r = Range { start_line: r[0], start_character: r[1], end_line: r[2], end_character: r[3] };
                want.push((r, o.symbol.clone(), o.symbol_roles as u32));
            }
            want.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            let mut merged: Vec<(Range, String, u32)> = Vec::new();
            for w in want {
                match merged.last_mut() {
                    Some(m) if m.0 == w.0 && m.1 == w.1 => m.2 |= w.2,
                    _ => merged.push(w),
                }
            }
            let mut got: Vec<(Range, String, u32)> = got
                .occurrences
                .iter()
                .map(|o| (o.range, o.symbol.map(|s| g.symbol_name(s).unwrap().to_owned()).unwrap_or_default(), o.roles))
                .collect();
            got.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            prop_assert_eq!(got, merged);
        }
    }
}
