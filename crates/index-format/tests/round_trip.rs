use std::collections::BTreeSet;
use std::ops::ControlFlow;

use proptest::prelude::*;
use sourcegull_index_format::{
    BranchMeta, Codec, DocInput, DocSymbol, FLAG_BINARY, FLAG_TOO_LARGE, FormatError, RepoMeta,
    Shard, ShardWriter, prepare_copy, prepare_doc, trigram_bytes, trigrams,
};

#[derive(Debug)]
struct File {
    path: String,
    mask: u64,
    lang: Option<&'static str>,
    flags: u16,
    content: Vec<u8>,
    symbols: Vec<DocSymbol>,
}

fn repo() -> RepoMeta {
    RepoMeta {
        id: 42,
        name: "gitlab.example.com/g/r".into(),
        rank: 7,
        branches: vec![
            BranchMeta {
                name: "main".into(),
                commit: [1; 20],
            },
            BranchMeta {
                name: "dev".into(),
                commit: [2; 20],
            },
        ],
    }
}

fn build(files: &[File], codec: Codec) -> Vec<u8> {
    let mut w = ShardWriter::new(codec);
    let r = w.add_repo(repo()).unwrap();
    for (i, f) in files.iter().enumerate() {
        let doc = prepare_doc(
            &DocInput {
                repo: r,
                branch_mask: f.mask,
                path: &f.path,
                oid: [i as u8; 20],
                lang: f.lang,
                size: f.content.len() as u64,
                flags: f.flags,
                content: &f.content,
                symbols: &f.symbols,
            },
            codec,
        );
        w.push(doc).unwrap();
    }
    w.finish_to_vec().unwrap()
}

fn sample() -> Vec<File> {
    let f = |path: &str, mask, lang, flags, content: &str| File {
        path: path.into(),
        mask,
        lang,
        flags,
        content: content.as_bytes().to_vec(),
        symbols: Vec::new(),
    };
    let sym = |name: &str, container: Option<&str>, kind, range| DocSymbol {
        name: name.into(),
        container: container.map(Into::into),
        kind,
        range,
    };
    let mut files = vec![
        f("README.md", 3, Some("markdown"), 0, "# Hello\nworld\n"),
        f("data.bin", 3, None, FLAG_BINARY, ""),
        f(
            "src/main.go",
            1,
            Some("go"),
            0,
            "package main\n\nfunc Main() {}",
        ),
        f("src/main.go", 2, Some("go"), 0, "package main\n"),
        f("vendor/big.js", 1, Some("javascript"), FLAG_TOO_LARGE, ""),
    ];
    files[2].symbols = vec![
        sym("main", None, 4, 8..12),
        sym("Main", Some("main"), 12, 19..23),
    ];
    files[3].symbols = vec![sym("main", None, 4, 8..12)];
    files
}

#[test]
fn round_trip() {
    for codec in [Codec::None, Codec::Lz4] {
        let files = sample();
        let shard = Shard::from_vec(build(&files, codec)).unwrap();
        assert_eq!(shard.codec(), codec);
        assert_eq!(shard.repos(), [repo()]);
        assert_eq!(shard.doc_count(), 5);
        for (i, f) in files.iter().enumerate() {
            let d = shard.doc(i as u32);
            assert_eq!(shard.path(&d).unwrap(), f.path);
            assert_eq!(d.branch_mask, f.mask);
            assert_eq!(d.oid, [i as u8; 20]);
            assert_eq!(shard.lang(&d), f.lang);
            assert_eq!(d.has_content(), f.flags == 0);
            assert_eq!(shard.content(&d).unwrap().as_ref(), f.content.as_slice());
            assert_eq!(shard.symbols(&d).unwrap(), f.symbols);
            let ranges: Vec<_> = f
                .symbols
                .iter()
                .map(|s| (s.kind, s.range.clone()))
                .collect();
            assert_eq!(shard.symbol_ranges(&d).unwrap(), ranges);
            assert_eq!(shard.has_symbols(&d), !f.symbols.is_empty());
        }
        // Two versions of src/main.go share one path entry.
        assert_eq!(shard.doc(2).path, shard.doc(3).path);
        let main = shard.doc(2);
        assert_eq!(main.line_count, 3);
        assert_eq!(shard.newlines(&main).unwrap(), [12, 13]);
        assert_eq!(shard.doc(0).line_count, 2);

        let mut walked = Vec::new();
        shard
            .walk_docs(1, &mut |d, p| {
                walked.push((d.id, p.to_owned()));
                ControlFlow::Continue(())
            })
            .unwrap();
        let expected: Vec<_> = (1..5)
            .map(|i| (i, files[i as usize].path.clone()))
            .collect();
        assert_eq!(walked, expected);

        let pkg = trigram_bytes(*b"pac");
        assert_eq!(shard.trigram_count(pkg).unwrap(), 2);
        assert_eq!(
            shard.postings(pkg).unwrap().iter().collect::<Vec<_>>(),
            [2, 3]
        );
        // Folded: "MAIN" and "Main" are the same trigrams.
        let ain = trigram_bytes(*b"AIN");
        assert_eq!(
            shard.postings(ain).unwrap().iter().collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(shard.trigram_count(trigram_bytes(*b"zzz")).unwrap(), 0);
        assert!(shard.postings(trigram_bytes(*b"zzz")).unwrap().is_empty());
    }
}

#[test]
fn open_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("1.gullshard");
    let mut w = ShardWriter::new(Codec::Lz4);
    let r = w.add_repo(repo()).unwrap();
    let content = b"fn main() {}\n";
    w.push(prepare_doc(
        &DocInput {
            repo: r,
            branch_mask: 1,
            path: "main.rs",
            oid: [9; 20],
            lang: Some("rust"),
            size: content.len() as u64,
            flags: 0,
            content,
            symbols: &[],
        },
        Codec::Lz4,
    ))
    .unwrap();
    let n = w.finish(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), n);
    let shard = Shard::open(&path).unwrap();
    assert_eq!(shard.size_bytes(), n);
    assert_eq!(shard.content(&shard.doc(0)).unwrap().as_ref(), content);
    // Only the shard is left in the directory.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn compound_shards_copy_docs() {
    for (from, to) in [
        (Codec::Lz4, Codec::Lz4),
        (Codec::None, Codec::Lz4),
        (Codec::Lz4, Codec::None),
    ] {
        let single = Shard::from_vec(build(&sample(), from)).unwrap();
        assert_eq!(single.repo_docs(0), 0..5);
        // Two repos holding the same files, and an empty one between them.
        let mut w = ShardWriter::new(to);
        let mut metas = Vec::new();
        for id in [1, 2, 3] {
            let meta = RepoMeta { id, ..repo() };
            metas.push(meta.clone());
            let r = w.add_repo(meta).unwrap();
            if id == 2 {
                continue;
            }
            for d in single.docs() {
                w.push(prepare_copy(&single, &d, r, to).unwrap()).unwrap();
            }
        }
        let compound = Shard::from_vec(w.finish_to_vec().unwrap()).unwrap();
        assert_eq!(compound.repos(), metas);
        assert_eq!(compound.repo_docs(0), 0..5);
        assert!(compound.repo_docs(1).is_empty());
        assert_eq!(compound.repo_docs(2), 5..10);
        for d in compound.docs() {
            let orig = single.doc(d.id % 5);
            assert_eq!(d.repo, if d.id < 5 { 0 } else { 2 });
            assert_eq!(compound.path(&d).unwrap(), single.path(&orig).unwrap());
            assert_eq!(
                compound.content(&d).unwrap(),
                single.content(&orig).unwrap()
            );
            assert_eq!(
                compound.newlines(&d).unwrap(),
                single.newlines(&orig).unwrap()
            );
            assert_eq!(compound.lang(&d), single.lang(&orig));
            assert_eq!(
                compound.symbols(&d).unwrap(),
                single.symbols(&orig).unwrap()
            );
            assert_eq!(
                (d.branch_mask, d.flags, d.size, d.line_count, d.oid),
                (
                    orig.branch_mask,
                    orig.flags,
                    orig.size,
                    orig.line_count,
                    orig.oid
                )
            );
        }
        let pkg = trigram_bytes(*b"pac");
        assert_eq!(
            compound.postings(pkg).unwrap().iter().collect::<Vec<_>>(),
            [2, 3, 7, 8]
        );
    }
}

#[test]
fn delta_shards_carry_tombstones() {
    let base = Shard::from_vec(build(&sample(), Codec::Lz4)).unwrap();
    assert!(base.tombstones().is_none());
    // A delta with no docs (only the branch heads moved) is valid.
    let mut w = ShardWriter::new(Codec::Lz4);
    w.add_repo(repo()).unwrap();
    let hidden: roaring::RoaringBitmap = [0u32, 2].into_iter().collect();
    w.set_tombstones(base.id(), hidden.clone());
    let delta = Shard::from_vec(w.finish_to_vec().unwrap()).unwrap();
    assert_eq!(delta.doc_count(), 0);
    let (of, docs) = delta.tombstones().unwrap();
    assert_eq!(of, base.id());
    assert_eq!(docs, &hidden);
    assert_ne!(delta.id(), base.id());
    // Ids follow content.
    let again = Shard::from_vec(build(&sample(), Codec::Lz4)).unwrap();
    assert_eq!(again.id(), base.id());
}

/// `build` with ranks: doc `i` ranked `i / 10`, generation 9.
fn build_ranked(files: &[File]) -> Vec<u8> {
    let mut w = ShardWriter::new(Codec::Lz4);
    let r = w.add_repo(repo()).unwrap();
    w.set_ranks_generation(r, 9).unwrap();
    assert!(w.set_ranks_generation(r + 1, 1).is_err());
    for (i, f) in files.iter().enumerate() {
        let doc = prepare_doc(
            &DocInput {
                repo: r,
                branch_mask: f.mask,
                path: &f.path,
                oid: [i as u8; 20],
                lang: f.lang,
                size: f.content.len() as u64,
                flags: f.flags,
                content: &f.content,
                symbols: &f.symbols,
            },
            Codec::Lz4,
        );
        w.push(doc.with_rank(i as f32 / 10.0)).unwrap();
    }
    w.finish_to_vec().unwrap()
}

#[test]
fn ranks_round_trip() {
    let ranked = Shard::from_vec(build_ranked(&sample())).unwrap();
    assert_eq!(ranked.ranks_generation(0), 9);
    let ranks: Vec<f32> = (0..ranked.doc_count())
        .map(|i| ranked.doc_rank(i))
        .collect();
    assert_eq!(ranks, [0.0, 0.1, 0.2, 0.3, 0.4]);

    // Without ranks there is no RANKS section: every rank and generation is 0.
    let plain_bytes = build(&sample(), Codec::Lz4);
    let plain = Shard::from_vec(plain_bytes.clone()).unwrap();
    assert_eq!(plain.ranks_generation(0), 0);
    assert!((0..plain.doc_count()).all(|i| plain.doc_rank(i) == 0.0));
    assert!(plain.size_bytes() < ranked.size_bytes());

    // Compound shards copy each doc's rank and take each repository's generation.
    let mut w = ShardWriter::new(Codec::Lz4);
    for (id, gen_) in [(1, 9), (2, 0)] {
        let r = w.add_repo(RepoMeta { id, ..repo() }).unwrap();
        w.set_ranks_generation(r, gen_).unwrap();
        for d in ranked.docs() {
            w.push(prepare_copy(&ranked, &d, r, Codec::None).unwrap())
                .unwrap();
        }
    }
    let compound = Shard::from_vec(w.finish_to_vec().unwrap()).unwrap();
    assert_eq!(
        (compound.ranks_generation(0), compound.ranks_generation(1)),
        (9, 0)
    );
    assert_eq!(compound.doc_rank(3), 0.3);
    assert_eq!(compound.doc_rank(8), 0.3);

    // Shards written by version 2 (no RANKS section) still open.
    let mut old = plain_bytes;
    old[8..12].copy_from_slice(&2u32.to_le_bytes());
    let sections = u32::from_le_bytes(old[16..20].try_into().unwrap()) as usize;
    let header = 24 + 32 * sections;
    let crc = crc32fast::hash(&old[..header]);
    old[header..header + 4].copy_from_slice(&crc.to_le_bytes());
    let old = Shard::from_vec(old).unwrap();
    assert_eq!(old.doc_count(), plain.doc_count());
    assert_eq!(old.doc_rank(1), 0.0);
    let mut v1 = build(&sample(), Codec::Lz4);
    v1[8..12].copy_from_slice(&1u32.to_le_bytes());
    assert!(Shard::from_vec(v1).is_err());
}

#[test]
fn writer_rejects_bad_input() {
    let mut w = ShardWriter::new(Codec::None);
    let r = w.add_repo(repo()).unwrap();
    let doc = |path: &str, mask| {
        prepare_doc(
            &DocInput {
                repo: r,
                branch_mask: mask,
                path,
                oid: [0; 20],
                lang: None,
                size: 0,
                flags: 0,
                content: b"",
                symbols: &[],
            },
            Codec::None,
        )
    };
    w.push(doc("b", 1)).unwrap();
    assert!(matches!(w.push(doc("a", 1)), Err(FormatError::Invalid(_))));
    assert!(matches!(w.push(doc("c", 4)), Err(FormatError::Invalid(_))));
    let mut repo = repo();
    repo.branches = (0..65)
        .map(|i| BranchMeta {
            name: format!("b{i}"),
            commit: [0; 20],
        })
        .collect();
    assert!(w.add_repo(repo).is_err());
}

#[test]
fn corruption_is_an_error() {
    let bytes = build(&sample(), Codec::Lz4);
    // Every single-byte flip and every truncation is detected, never a panic.
    for i in 0..bytes.len() {
        let mut b = bytes.clone();
        b[i] ^= 0x5a;
        assert!(Shard::from_vec(b).is_err(), "flip at {i} not detected");
    }
    for len in (0..bytes.len()).step_by(7) {
        assert!(Shard::from_vec(bytes[..len].to_vec()).is_err());
    }
    let mut v = bytes.clone();
    v[8] = 9;
    assert!(matches!(
        Shard::from_vec(v),
        Err(FormatError::Version(9)) | Err(FormatError::Corrupt(_))
    ));
}

fn arb_files() -> impl Strategy<Value = Vec<File>> {
    let content = prop::collection::vec(
        prop_oneof![
            Just(b'a'),
            Just(b'B'),
            Just(b'\n'),
            Just(0xc3u8),
            Just(0xa9u8),
            any::<u8>()
        ],
        0..300,
    );
    prop::collection::btree_map("[a-c]{1,3}(/[a-c]{1,3}){0,2}", (1u64..4, content), 0..60).prop_map(
        |m| {
            m.into_iter()
                .map(|(path, (mask, content))| File {
                    path,
                    mask,
                    lang: Some("text"),
                    flags: 0,
                    content,
                    symbols: Vec::new(),
                })
                .collect()
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn postings_are_exact(files in arb_files(), lz4 in any::<bool>()) {
        let codec = if lz4 { Codec::Lz4 } else { Codec::None };
        let shard = Shard::from_vec(build(&files, codec)).unwrap();
        prop_assert_eq!(shard.doc_count() as usize, files.len());
        let mut all = BTreeSet::new();
        let per_file: Vec<Vec<u32>> = files.iter().map(|f| trigrams(&f.content)).collect();
        for (i, f) in files.iter().enumerate() {
            let d = shard.doc(i as u32);
            prop_assert_eq!(shard.path(&d).unwrap(), f.path.clone());
            let content = shard.content(&d).unwrap();
            prop_assert_eq!(content.as_ref(), f.content.as_slice());
            let nl: Vec<u32> = f.content.iter().enumerate()
                .filter(|(_, b)| **b == b'\n').map(|(i, _)| i as u32).collect();
            prop_assert_eq!(shard.newlines(&d).unwrap(), nl);
            all.extend(per_file[i].iter().copied());
        }
        prop_assert_eq!(shard.trigram_total(), all.len());
        for t in all {
            let expected: Vec<u32> = per_file.iter().enumerate()
                .filter(|(_, ts)| ts.binary_search(&t).is_ok())
                .map(|(i, _)| i as u32).collect();
            prop_assert_eq!(shard.postings(t).unwrap().iter().collect::<Vec<_>>(), expected.clone());
            prop_assert_eq!(shard.trigram_count(t).unwrap(), expected.len() as u64);
        }
    }
}
