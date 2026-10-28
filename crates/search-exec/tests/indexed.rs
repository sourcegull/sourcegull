//! Indexed search returns exactly what unindexed search returns on the same
//! commit.

use std::ops::{ControlFlow, Range};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use sourcegull_gitstore::ObjectId;
use sourcegull_index_format::{
    BranchMeta, Codec, DocInput, RepoMeta, Shard, ShardWriter, prepare_doc,
};
use sourcegull_indexer::BuildOptions;
use sourcegull_query::PatternType;
use sourcegull_search::index::{
    Branch, BuildRequest, Cancel, IndexQuery, IndexedFile, RepoBranches, SearchIndex,
};
use sourcegull_search::{FileFilter, Job, Limits, Matcher, PatternInfo, plan_query};
use sourcegull_search_exec::{
    ChunkMatch, CompiledQuery, IndexUpdate, PathFilter, PatternSpec, ShardIndex, UnindexedOptions,
    chunk_matches, search_commit, search_shard, update_index,
};
use sourcegull_types::{AlwaysIndex, RepoId};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A small deterministic generator for file bodies.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
}

const WORDS: &[&str] = &[
    "foo",
    "bar",
    "baz",
    "Foo",
    "BAR",
    "alpha",
    "beta",
    "gamma",
    "func",
    "Main",
    "main",
    "kelvin",
    "\u{212A}elvin",
    "café",
    "CAFÉ",
    "Straße",
    "ſtrasse",
    "STRASSE",
    "fooo",
    "x",
    "  ",
    "\n",
    "\n\n",
    "(",
    ")",
    "{",
    "}",
    "package",
    "return",
    "nil",
    "αβγ",
    "ΑΒΓ",
];

fn body(rng: &mut Lcg, words: usize) -> String {
    let mut s = String::new();
    for _ in 0..words {
        s.push_str(WORDS[rng.next() % WORDS.len()]);
        s.push(if rng.next() % 5 == 0 { '\n' } else { ' ' });
    }
    s
}

fn fixture() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    let mut rng = Lcg(7);
    for d in ["src", "src/util", "docs", "vendor/lib", "zz"] {
        std::fs::create_dir_all(p.join(d)).unwrap();
        for i in 0..30 {
            let ext = ["go", "rs", "md", "txt", "py"][i % 5];
            let words = 20 + rng.next() % 200;
            std::fs::write(p.join(format!("{d}/f{i:02}.{ext}")), body(&mut rng, words)).unwrap();
        }
    }
    std::fs::write(
        p.join("src/main.go"),
        "package main\n\nfunc Main() {\n\treturn nil\n}\n",
    )
    .unwrap();
    std::fs::write(p.join("empty.txt"), "").unwrap();
    std::fs::write(p.join("no-newline.txt"), "foo bar").unwrap();
    std::fs::write(p.join("data.bin"), b"foo\0bar baz").unwrap();
    std::fs::write(p.join("large.txt"), "foo bar\n".repeat(140_000)).unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "one"]);
    let main = git(p, &["rev-parse", "HEAD"]);
    git(p, &["checkout", "-qb", "dev"]);
    std::fs::write(p.join("src/main.go"), "package main\n\nfunc Dev() {}\n").unwrap();
    std::fs::write(p.join("docs/new.md"), "# alpha gamma foo\n").unwrap();
    git(p, &["rm", "-q", "zz/f00.go"]);
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "two"]);
    let dev = git(p, &["rev-parse", "HEAD"]);
    let oid = |s: String| ObjectId::from_hex(s.trim().as_bytes()).unwrap();
    (dir, oid(main), oid(dev))
}

/// The content or path pattern and file filters of a query.
fn plan(q: &str) -> (Option<PatternInfo>, bool, FileFilter) {
    let job = plan_query(
        q,
        PatternType::Keyword,
        &Limits::default(),
        chrono::Utc::now(),
    )
    .unwrap();
    fn find(j: &Job) -> Option<(Option<PatternInfo>, bool, FileFilter)> {
        match j {
            Job::Content { pattern, files, .. } => {
                Some((Some(pattern.clone()), false, files.clone()))
            }
            Job::Path { pattern, files, .. } => Some((pattern.clone(), true, files.clone())),
            Job::Parallel(js) => js.iter().find_map(find),
            Job::Select { child, .. } | Job::Limit { child, .. } | Job::Timeout { child, .. } => {
                find(child)
            }
            _ => None,
        }
    }
    find(&job).unwrap_or_else(|| panic!("no content or path job in {q:?}"))
}

type Row = (String, [u8; 20], Vec<ChunkMatch>, Vec<Range<usize>>);

fn unindexed(repo: &gix::Repository, commit: ObjectId, q: &str) -> Vec<Row> {
    unindexed_with(repo, commit, q, &UnindexedOptions::default())
}

fn unindexed_with(
    repo: &gix::Repository,
    commit: ObjectId,
    q: &str,
    opts: &UnindexedOptions,
) -> Vec<Row> {
    let (pattern, is_path, files) = plan(q);
    let cs = pattern.as_ref().is_some_and(|p| p.case_sensitive);
    let spec = pattern.map(|p| PatternSpec::from_pattern(&p).unwrap());
    let mut paths = PathFilter::compile(&files, cs).unwrap();
    let mut matcher = None;
    match (spec, is_path) {
        (Some(s), true) => paths = paths.with_pattern(Matcher::from_spec(&s, cs).unwrap()),
        (Some(s), false) => matcher = Some(Matcher::from_spec(&s, cs).unwrap()),
        (None, _) => {}
    }
    let mut out = Vec::new();
    search_commit(repo, commit, matcher.as_ref(), &paths, opts, |f| {
        out.push((
            f.path,
            f.oid.as_bytes().try_into().unwrap(),
            f.chunks,
            f.path_ranges,
        ));
        ControlFlow::Continue(())
    })
    .unwrap();
    out
}

fn index_query(q: &str, branch: &str) -> IndexQuery {
    let (pattern, is_path, files) = plan(q);
    let cs = pattern.as_ref().is_some_and(|p| p.case_sensitive);
    let spec = pattern.map(|p| PatternSpec::from_pattern(&p).unwrap());
    let (pattern, path_pattern) = if is_path { (None, spec) } else { (spec, None) };
    IndexQuery {
        repos: vec![RepoBranches {
            repo: RepoId(3),
            branches: vec![branch.into()],
        }],
        pattern,
        path_pattern,
        case_sensitive: cs,
        files,
        ..IndexQuery::default()
    }
}

fn indexed(index: &ShardIndex, q: &str, branch: &str) -> Vec<Row> {
    let mut out = Vec::new();
    let stats = index
        .search(&index_query(q, branch), &mut |f: IndexedFile| {
            assert_eq!(
                f.branches
                    .iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>(),
                [branch]
            );
            assert_eq!(f.branches[0].commit.len(), 40);
            out.push((f.path, f.oid, f.chunks, f.path_ranges));
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(stats.missing.is_empty());
    // Ranked results come best first; compare in path order.
    out.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    out
}

const QUERIES: &[&str] = &[
    "foo",
    "Foo case:yes",
    "FOO",
    "foo bar",
    "func Main",
    "\"foo bar\"",
    "kelvin",
    "KELVIN",
    "kelvin case:yes",
    "café",
    "CAFÉ",
    "straße",
    "STRASSE",
    "strasse",
    "αβγ",
    "ΑΒΓ case:yes",
    "x",
    "fo",
    "patterntype:regexp fo+o",
    "patterntype:regexp ^package \\w+",
    "patterntype:standard /(alpha|beta)\\sgamma/",
    "patterntype:regexp [Kk]elvin",
    "patterntype:regexp Main\\(\\)",
    "patterntype:regexp gamma\\s+foo",
    "patterntype:regexp ^$",
    "patterntype:regexp \\bbar\\b",
    "patterntype:regexp BA[R] case:yes",
    "patterntype:regexp foo.*bar",
    "patterntype:standard /(foo\\s|bar\\s)+baz/",
    "foo or gamma",
    "(foo and bar) or kelvin",
    "foo not bar",
    "not foo",
    "foo file:^src/",
    "foo file:^src/util/",
    "foo -file:\\.md$",
    "foo lang:go",
    "foo -lang:go",
    "foo lang:markdown file:^docs/",
    "patterntype:structural func :[[f]]() {:[body]}",
    "patterntype:structural ( :[x] )",
    "patterntype:structural foo :[[w]] baz",
    "patterntype:structural { :[_] } lang:go",
    "patterntype:structural (:[x]) rule:'where match :[x] { | \"foo :[_]\" -> true }'",
    "type:path main",
    "type:path util/f0 case:yes",
    "type:path UTIL/f0 case:yes",
    "type:path \\.go$ patterntype:regexp",
    "file:\\.bin$ type:path",
    "nosuchstringanywhere",
];

#[test]
fn indexed_matches_unindexed() {
    let (dir, main, dev) = fixture();
    let repo = gix::open(dir.path()).unwrap();
    let index_dir = tempfile::tempdir().unwrap();
    let (index, failed) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    assert!(failed.is_empty());
    let built = index
        .build(&BuildRequest {
            repo: RepoId(3),
            name: "g/r".into(),
            git_dir: dir.path().join(".git"),
            branches: vec![
                Branch {
                    name: "main".into(),
                    commit: main.to_string(),
                },
                Branch {
                    name: "dev".into(),
                    commit: dev.to_string(),
                },
            ],
            ranks: None,
            always_index: Default::default(),
        })
        .unwrap();
    assert!(built.docs > 150);
    for q in QUERIES {
        for (branch, commit) in [("main", main), ("dev", dev)] {
            let want = unindexed(&repo, commit, q);
            let got = indexed(&index, q, branch);
            assert_eq!(
                got.iter().map(|r| &r.0).collect::<Vec<_>>(),
                want.iter().map(|r| &r.0).collect::<Vec<_>>(),
                "paths for {q:?} on {branch}"
            );
            assert_eq!(got, want, "matches for {q:?} on {branch}");
        }
    }
    // Something matched for most queries, so the comparison means something.
    let empty: Vec<_> = QUERIES
        .iter()
        .filter(|q| unindexed(&repo, main, q).is_empty() && unindexed(&repo, dev, q).is_empty())
        .collect();
    assert_eq!(
        empty,
        [&"type:path UTIL/f0 case:yes", &"nosuchstringanywhere"]
    );

    // Reopening finds the shard; unknown repos and branches are reported.
    drop(index);
    let (index, failed) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    assert!(failed.is_empty());
    assert_eq!(index.stats().repos, 1);
    let mut q = index_query("foo", "main");
    q.repos.push(RepoBranches {
        repo: RepoId(9),
        branches: vec!["main".into()],
    });
    q.repos[0].branches.push("gone".into());
    q.file_limit = 3;
    let mut n = 0;
    let stats = index
        .search(&q, &mut |_| {
            n += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(n, 3);
    assert!(stats.limit_hit);
    assert_eq!(
        stats.missing,
        [
            (RepoId(3), "gone".to_owned()),
            (RepoId(9), "main".to_owned())
        ]
    );

    let branches = index.branches(RepoId(3)).unwrap();
    assert_eq!(branches[0].name, "main");
    assert_eq!(branches[0].commit, main.to_string());
    assert!(index.branches(RepoId(9)).is_none());
    assert!(index.remove(RepoId(3)).unwrap());
    assert!(!index.remove(RepoId(3)).unwrap());
    assert!(index.branches(RepoId(3)).is_none());
    assert_eq!(index.stats().repos, 0);
}

// Property: on random corpora and random patterns, a shard search returns
// exactly the docs (and chunks) a brute-force scan with the matcher returns.

fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "a", "b", "ab", "A", "B", "k", "K", "\u{212A}", "s", "S", "ſ", "é", "É", " ", "\n",
            "ba", "abc", "xyz",
        ]),
        0..40,
    )
    .prop_map(|v| v.concat())
}

fn arb_regex() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        prop::sample::select(vec![
            "a", "b", "k", "s", "é", "ab", "abc", "ka", "sé", "xyz", "K", "ſ", "\u{212A}", " ",
        ])
        .prop_map(str::to_owned),
        prop::sample::select(vec![
            ".", "[ab]", "[^a]", "\\w", "\\s", "^", "$", "\\b", "[kK]", "(?-i)a", "\\n",
        ])
        .prop_map(str::to_owned),
    ];
    atom.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(|v| v.concat()),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("(?:{a}|{b})")),
            (
                inner.clone(),
                prop::sample::select(vec!["*", "+", "?", "{2}", "{1,3}"])
            )
                .prop_map(|(a, r)| format!("(?:{a}){r}")),
        ]
    })
}

fn arb_spec() -> impl Strategy<Value = PatternSpec> {
    let atom = (arb_regex(), prop::bool::weighted(0.2))
        .prop_map(|(regex, negated)| PatternSpec::Atom { regex, negated });
    atom.prop_recursive(2, 6, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..3).prop_map(PatternSpec::And),
            prop::collection::vec(inner, 1..3).prop_map(PatternSpec::Or),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn shard_search_equals_brute_force(
        texts in prop::collection::vec(arb_text(), 1..20),
        spec in arb_spec(),
        cs in any::<bool>(),
    ) {
        let mut w = ShardWriter::new(Codec::Lz4);
        let r = w.add_repo(RepoMeta {
            id: 1, name: "r".into(), rank: 0,
            branches: vec![BranchMeta { name: "main".into(), commit: [0; 20] }],
        }).unwrap();
        let paths: Vec<String> = (0..texts.len()).map(|i| format!("f{i:03}.txt")).collect();
        for (path, text) in paths.iter().zip(&texts) {
            w.push(prepare_doc(&DocInput {
                repo: r, branch_mask: 1, path, oid: [0; 20], lang: None,
                size: text.len() as u64, flags: 0, content: text.as_bytes(), symbols: &[],
            }, Codec::Lz4)).unwrap();
        }
        let shard = Shard::from_vec(w.finish_to_vec().unwrap()).unwrap();
        let Ok(matcher) = Matcher::from_spec(&spec, cs) else { return Ok(()) };
        let want: Vec<(String, Vec<ChunkMatch>)> = paths.iter().zip(&texts)
            .filter_map(|(p, t)| {
                let ranges = matcher.find(t.as_bytes())?;
                Some((p.clone(), chunk_matches(t.as_bytes(), &ranges, None)))
            })
            .collect();
        let q = IndexQuery { pattern: Some(spec.clone()), case_sensitive: cs, ..IndexQuery::default() };
        let compiled = CompiledQuery::compile(&q).unwrap();
        let mut got = Vec::new();
        search_shard(&shard, 0, 1, &compiled, &mut |m| {
            got.push((m.path, m.chunks));
            ControlFlow::Continue(())
        }).unwrap();
        prop_assert_eq!(got, want, "trigrams {:?}", compiled.trigrams());
    }
}

fn build_request(dir: &Path, main: ObjectId, dev: ObjectId) -> BuildRequest {
    BuildRequest {
        repo: RepoId(3),
        name: "g/r".into(),
        git_dir: dir.join(".git"),
        branches: vec![
            Branch {
                name: "main".into(),
                commit: main.to_string(),
            },
            Branch {
                name: "dev".into(),
                commit: dev.to_string(),
            },
        ],
        ranks: None,
        always_index: Default::default(),
    }
}

fn rev(dir: &Path, r: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", r]).trim().as_bytes()).unwrap()
}

#[test]
fn delta_shards_match_unindexed() {
    let (dir, main, dev) = fixture();
    let p = dir.path();
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index.build(&build_request(p, main, dev)).unwrap();

    // main: edit, add and delete files; dev: take one of main's edits, so a
    // path changes on one branch and its old version stays on the other.
    git(p, &["checkout", "-q", "main"]);
    std::fs::write(
        p.join("src/main.go"),
        "package main\n\nfunc Main() { kelvin() }\n",
    )
    .unwrap();
    std::fs::write(p.join("docs/added.md"), "# delta foo bar\n").unwrap();
    git(p, &["rm", "-q", "src/f01.rs"]);
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "three"]);
    git(p, &["checkout", "-q", "dev"]);
    std::fs::write(p.join("docs/added.md"), "# delta foo bar\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "four"]);
    let (main, dev) = (rev(p, "main"), rev(p, "dev"));
    let req = build_request(p, main, dev);
    let updated = index.apply_delta(&req).unwrap();
    let rs = index.shards(RepoId(3)).unwrap();
    let delta = rs.delta.as_ref().expect("a delta shard");
    assert!(delta.doc_count() <= 4, "{}", delta.doc_count());
    assert_eq!(index.branches(RepoId(3)).unwrap(), req.branches);
    assert_eq!(updated.docs as u64, rs.docs());

    let check = |index: &ShardIndex| {
        let repo = gix::open(p).unwrap();
        for q in QUERIES {
            for (branch, commit) in [("main", main), ("dev", dev)] {
                let want = unindexed(&repo, commit, q);
                assert_eq!(indexed(index, q, branch), want, "{q:?} on {branch}");
            }
        }
    };
    check(&index);
    // The same docs as a full build, and the delta survives a reopen.
    let full_dir = tempfile::tempdir().unwrap();
    let (full, _) = ShardIndex::open(full_dir.path(), Default::default()).unwrap();
    full.build(&req).unwrap();
    assert_eq!(full.stats().docs, index.stats().docs);
    assert_eq!(full.stats().content_bytes, index.stats().content_bytes);
    drop(index);
    let (index, failed) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    assert!(failed.is_empty());
    assert!(index.shards(RepoId(3)).unwrap().delta.is_some());
    check(&index);
    // The same when large repositories' candidates are made in parts.
    let index = index.with_candidate_part(7);
    check(&index);

    // A change touching most files compacts into a new base.
    git(p, &["checkout", "-q", "main"]);
    for d in ["src", "docs", "vendor/lib", "zz"] {
        for i in 0..30 {
            let f = p.join(format!("{d}/f{i:02}.txt"));
            std::fs::write(f, format!("rewritten {i}\n")).unwrap();
        }
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "five"]);
    index
        .apply_delta(&build_request(p, rev(p, "main"), dev))
        .unwrap();
    assert!(index.shards(RepoId(3)).unwrap().delta.is_none());
    assert!(!index_dir.path().join("3.delta.gullshard").exists());
}

#[test]
fn always_index_reads_large_files_in_both_searches() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    std::fs::create_dir_all(p.join("gen")).unwrap();
    let large = format!("{}needle\n", "filler line\n".repeat(20));
    std::fs::write(p.join("gen/api.ts"), &large).unwrap();
    std::fs::write(p.join("other.ts"), &large).unwrap();
    std::fs::write(p.join("small.ts"), "needle\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "one"]);
    let main = rev(p, "main");
    let repo = gix::open(p).unwrap();
    let max_file_size = 64;
    let index_dir = tempfile::tempdir().unwrap();
    let opts = BuildOptions {
        max_file_size,
        ..BuildOptions::default()
    };
    let (index, _) = ShardIndex::open(index_dir.path(), opts).unwrap();
    let mut req = BuildRequest {
        repo: RepoId(3),
        name: "g/r".into(),
        git_dir: p.join(".git"),
        branches: vec![Branch {
            name: "main".into(),
            commit: main.to_string(),
        }],
        ranks: None,
        always_index: AlwaysIndex::default(),
    };
    let search = |rule: &AlwaysIndex| {
        let opts = UnindexedOptions {
            max_file_size,
            always_index: rule.clone(),
            ..UnindexedOptions::default()
        };
        let want = unindexed_with(&repo, main, "needle", &opts);
        assert_eq!(indexed(&index, "needle", "main"), want, "{rule:?}");
        want.into_iter().map(|r| r.0).collect::<Vec<_>>()
    };

    update_index(&index, &req).unwrap();
    assert_eq!(search(&req.always_index), ["small.ts"]);
    let rule = AlwaysIndex::new(["gen/**"]).unwrap();
    assert!(index.follows_always_index(RepoId(3), &req.always_index));
    assert!(!index.follows_always_index(RepoId(3), &rule));

    // A new rule rebuilds the repository although its branches did not move.
    req.always_index = rule;
    assert!(matches!(
        update_index(&index, &req).unwrap(),
        IndexUpdate::Built(_)
    ));
    assert_eq!(update_index(&index, &req).unwrap(), IndexUpdate::Unchanged);
    assert_eq!(search(&req.always_index), ["gen/api.ts", "small.ts"]);
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn compound_shards_match_unindexed() {
    let (dir, main, dev) = fixture();
    let p = dir.path();
    let index_dir = tempfile::tempdir().unwrap();
    let idx = index_dir.path();
    let (index, _) = ShardIndex::open(idx, Default::default()).unwrap();
    // Three repositories with the same history; 3 is the one queried.
    let request = |id: i32, main: ObjectId, dev: ObjectId| BuildRequest {
        repo: RepoId(id),
        name: format!("g/r{id}"),
        ..build_request(p, main, dev)
    };
    for id in [1, 3, 5] {
        index.build(&request(id, main, dev)).unwrap();
    }
    let docs = index.stats().docs;
    let report = index.merge_compounds().unwrap();
    assert_eq!((report.shards, report.repos, report.removed), (1, 3, 3));
    assert_eq!(files_in(idx), ["compound-0.gullshard"]);
    assert_eq!(index.stats().docs, docs);
    assert_eq!(
        index.stats().index_bytes,
        std::fs::metadata(idx.join("compound-0.gullshard"))
            .unwrap()
            .len()
    );
    let rs = index.shards(RepoId(3)).unwrap();
    assert_eq!((rs.compound, rs.repo), (Some(0), 1));
    // A settled index is left alone.
    assert_eq!(index.merge_compounds().unwrap(), Default::default());

    let check = |index: &ShardIndex, ids: &[i32], main: ObjectId, dev: ObjectId| {
        let repo = gix::open(p).unwrap();
        for q in QUERIES {
            for (branch, commit) in [("main", main), ("dev", dev)] {
                let want = unindexed(&repo, commit, q);
                // Every repository asked for, in request order.
                let mut query = index_query(q, branch);
                query.repos = ids
                    .iter()
                    .map(|&id| RepoBranches {
                        repo: RepoId(id),
                        branches: vec![branch.into()],
                    })
                    .collect();
                let mut got = Vec::new();
                index
                    .search(&query, &mut |f| {
                        got.push((f.repo, (f.path, f.oid, f.chunks, f.path_ranges)));
                        ControlFlow::Continue(())
                    })
                    .unwrap();
                // Repositories in request order; ranked files in each.
                got.sort_by(|(ra, a), (rb, b)| {
                    let pos = |r: &RepoId| ids.iter().position(|&id| id == r.0);
                    (pos(ra), &a.0, a.1).cmp(&(pos(rb), &b.0, b.1))
                });
                let want: Vec<_> = ids
                    .iter()
                    .flat_map(|&id| want.iter().map(move |w| (RepoId(id), w.clone())))
                    .collect();
                assert_eq!(got, want, "{q:?} on {branch} in {ids:?}");
            }
        }
    };
    check(&index, &[5, 1, 3], main, dev);

    // A delta over a repository in a compound shard.
    git(p, &["checkout", "-q", "main"]);
    std::fs::write(p.join("docs/added.md"), "# delta foo bar\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "three"]);
    let main2 = rev(p, "main");
    index.apply_delta(&request(3, main2, dev)).unwrap();
    let rs = index.shards(RepoId(3)).unwrap();
    assert_eq!(rs.compound, Some(0));
    assert!(rs.delta.is_some());
    check(&index, &[3], main2, dev);
    drop(index);
    let (index, failed) = ShardIndex::open(idx, Default::default()).unwrap();
    assert!(failed.is_empty());
    assert!(index.shards(RepoId(3)).unwrap().delta.is_some());
    check(&index, &[3], main2, dev);
    check(&index, &[1, 5], main, dev);
    assert_eq!(index.merge_compounds().unwrap(), Default::default());

    // A full build moves a repository out of its compound, a removal drops
    // it; both are remembered across a reopen.
    index.build(&request(5, main2, dev)).unwrap();
    assert!(index.remove(RepoId(1)).unwrap());
    assert_eq!(
        std::fs::read_to_string(idx.join("compound-0.dead")).unwrap(),
        "1\n5\n"
    );
    drop(index);
    let (index, failed) = ShardIndex::open(idx, Default::default()).unwrap();
    assert!(failed.is_empty());
    assert_eq!(index.repos(), [RepoId(3), RepoId(5)]);
    assert_eq!(index.shards(RepoId(5)).unwrap().compound, None);
    check(&index, &[3, 5], main2, dev);

    // Two thirds of the compound are dead: it is repacked with repository
    // 5's own shard, and repository 3's delta is folded in.
    let saved = tempfile::tempdir().unwrap();
    for f in ["compound-0.gullshard", "compound-0.dead"] {
        std::fs::copy(idx.join(f), saved.path().join(f)).unwrap();
    }
    let report = index.merge_compounds().unwrap();
    assert_eq!((report.shards, report.repos), (1, 2));
    assert_eq!(files_in(idx), ["compound-1.gullshard"]);
    let rs = index.shards(RepoId(3)).unwrap();
    assert_eq!((rs.compound, rs.delta.is_none()), (Some(1), true));
    check(&index, &[3, 5], main2, dev);

    // A crash before the old compound was deleted: the newer one wins and
    // the old one's copies are marked dead, then deleted by the next merge.
    drop(index);
    for f in ["compound-0.gullshard", "compound-0.dead"] {
        std::fs::copy(saved.path().join(f), idx.join(f)).unwrap();
    }
    let (index, failed) = ShardIndex::open(idx, Default::default()).unwrap();
    assert!(failed.is_empty());
    assert_eq!(index.repos(), [RepoId(3), RepoId(5)]);
    assert_eq!(index.shards(RepoId(3)).unwrap().compound, Some(1));
    assert_eq!(
        std::fs::read_to_string(idx.join("compound-0.dead")).unwrap(),
        "1\n3\n5\n"
    );
    check(&index, &[3, 5], main2, dev);
    let report = index.merge_compounds().unwrap();
    assert_eq!((report.shards, report.removed), (0, 1));
    assert_eq!(files_in(idx), ["compound-1.gullshard"]);
}

// Symbol search: the index (full, delta and compound shards) returns the
// symbols unindexed search extracts on the fly.

fn symbol_fixture(dir: &Path) {
    git(dir, &["init", "-q"]);
    for (path, text) in [
        (
            "src/server.go",
            "package srv\n\ntype Server struct {\n\tAddr string\n}\n\n\
             func NewServer() *Server { return nil }\n\n\
             func (s *Server) Run() error { return nil }\n",
        ),
        (
            "lib/cache.py",
            "class Cache:\n    def get(self, key):\n        return None\n\ndef make_cache():\n    pass\n",
        ),
        (
            "core/conn.rs",
            "pub struct Conn { fd: i32 }\n\nimpl Conn {\n    pub fn open() -> Conn { Conn { fd: 0 } }\n}\n",
        ),
        (
            "web/app.ts",
            "interface Shape { area(): number }\nexport class App { run(): void {} }\n",
        ),
        ("docs/server.md", "# Server\n\nNewServer and Run.\n"),
    ] {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "symbols"]);
}

const SYMBOL_QUERIES: &[&str] = &[
    "type:symbol Server",
    "type:symbol server case:yes",
    "type:symbol patterntype:regexp ^(get|open|run)$",
    "type:symbol Run file:\\.go$",
    "type:symbol lang:python get",
    "type:symbol",
    "type:symbol make or area",
    "type:symbol Server not New",
    "type:symbol nosuchsymbol",
];

type SymbolRow = (String, Vec<sourcegull_search_exec::SymbolMatch>);

fn symbol_plan(q: &str) -> (Option<PatternSpec>, bool, FileFilter) {
    let job = plan_query(
        q,
        PatternType::Keyword,
        &Limits::default(),
        chrono::Utc::now(),
    )
    .unwrap();
    fn find(j: &Job) -> Option<(Option<PatternInfo>, FileFilter)> {
        match j {
            Job::Symbol { pattern, files, .. } => Some((pattern.clone(), files.clone())),
            Job::Parallel(js) => js.iter().find_map(find),
            Job::Select { child, .. } | Job::Limit { child, .. } | Job::Timeout { child, .. } => {
                find(child)
            }
            _ => None,
        }
    }
    let (pattern, files) = find(&job).unwrap_or_else(|| panic!("no symbol job in {q:?}"));
    let cs = pattern.as_ref().is_some_and(|p| p.case_sensitive);
    let spec = pattern.map(|p| PatternSpec::from_pattern(&p).unwrap());
    (spec, cs, files)
}

fn unindexed_symbols(repo: &gix::Repository, commit: ObjectId, q: &str) -> Vec<SymbolRow> {
    let (spec, cs, files) = symbol_plan(q);
    let paths = PathFilter::compile(&files, cs).unwrap();
    let matcher = spec.as_ref().map(|s| Matcher::from_spec(s, cs).unwrap());
    let prefilter = spec
        .as_ref()
        .map_or(sourcegull_search_exec::TrigramQuery::All, |s| {
            sourcegull_search_exec::trigram_query(s, cs)
        });
    let mut out = Vec::new();
    sourcegull_search_exec::search_commit_symbols(
        repo,
        commit,
        matcher.as_ref(),
        &prefilter,
        &paths,
        &UnindexedOptions::default(),
        |f| {
            assert!(f.chunks.is_empty());
            out.push((f.path, f.symbols));
            ControlFlow::Continue(())
        },
    )
    .unwrap();
    out
}

fn indexed_symbols(index: &ShardIndex, repo: i32, q: &str) -> Vec<SymbolRow> {
    let (pattern, case_sensitive, files) = symbol_plan(q);
    let query = IndexQuery {
        repos: vec![RepoBranches {
            repo: RepoId(repo),
            branches: vec!["HEAD".into()],
        }],
        pattern,
        case_sensitive,
        files,
        symbols: true,
        ..IndexQuery::default()
    };
    let mut out = Vec::new();
    let stats = index
        .search(&query, &mut |f| {
            out.push((f.path, f.symbols));
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(stats.missing.is_empty());
    // Ranked results come best first; compare in path order.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn symbol_search_matches_unindexed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    symbol_fixture(p);
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    let request = |id: i32| BuildRequest {
        repo: RepoId(id),
        name: format!("g/s{id}"),
        git_dir: p.join(".git"),
        branches: vec![Branch {
            name: "HEAD".into(),
            commit: rev(p, "HEAD").to_string(),
        }],
        ranks: None,
        always_index: Default::default(),
    };
    let check = |index: &ShardIndex, repos: &[i32]| {
        let git = gix::open(p).unwrap();
        let head = rev(p, "HEAD");
        for q in SYMBOL_QUERIES {
            let want = unindexed_symbols(&git, head, q);
            for &id in repos {
                assert_eq!(indexed_symbols(index, id, q), want, "{q:?} in repo {id}");
            }
        }
    };
    index.build(&request(3)).unwrap();
    check(&index, &[3]);
    let repo = gix::open(p).unwrap();
    let head = rev(p, "HEAD");
    let found = |q| unindexed_symbols(&repo, head, q);
    // What the queries find, so the comparison means something.
    let names = |rows: Vec<SymbolRow>| -> Vec<(String, String, String)> {
        rows.into_iter()
            .flat_map(|(path, syms)| {
                syms.into_iter().map(move |s| {
                    let kind = sourcegull_types::SymbolKind::from_u8(s.kind)
                        .name()
                        .to_owned();
                    (path.clone(), s.name, kind)
                })
            })
            .collect()
    };
    let row = |p: &str, n: &str, k: &str| (p.to_owned(), n.to_owned(), k.to_owned());
    assert_eq!(
        names(found("type:symbol Server")),
        [
            row("src/server.go", "Server", "STRUCT"),
            row("src/server.go", "NewServer", "FUNCTION"),
        ]
    );
    assert_eq!(
        names(found("type:symbol patterntype:regexp ^(get|open|run)$")),
        [
            row("core/conn.rs", "open", "METHOD"),
            row("lib/cache.py", "get", "METHOD"),
            row("src/server.go", "Run", "METHOD"),
            row("web/app.ts", "run", "METHOD"),
        ]
    );
    let run = &found("type:symbol Run file:\\.go$")[0].1[0];
    assert_eq!(run.container.as_deref(), Some("Server"));
    assert_eq!((run.range.start.line, run.range.start.column), (8, 17));
    assert!(found("type:symbol nosuchsymbol").is_empty());

    // A delta over the base, then a compound shard with a second repo.
    std::fs::write(
        p.join("src/server.go"),
        "package srv\n\ntype Server struct{}\n\nfunc (s *Server) Stop() {}\n",
    )
    .unwrap();
    git(p, &["commit", "-qam", "stop"]);
    index.apply_delta(&request(3)).unwrap();
    assert!(index.shards(RepoId(3)).unwrap().delta.is_some());
    check(&index, &[3]);
    index.build(&request(4)).unwrap();
    let report = index.merge_compounds().unwrap();
    assert_eq!(report.repos, 2);
    check(&index, &[3, 4]);
}

// Ranking: ranked indexed results come best first, the limit keeps
// the best files rather than the first ones in path order.

fn ranking_fixture(dir: &Path) {
    git(dir, &["init", "-q"]);
    for (path, text) in [
        ("a/notes.txt", "a server here and a server there\n"),
        ("a/servers.txt", "the serverside of a thing\n"),
        ("pkg/server.go", "package pkg\n\ntype Server struct{}\n"),
        ("pkg/client.go", "package pkg\n\nvar c Server\n"),
        ("pkg/client_test.go", "package pkg\n\nvar c Server\n"),
        ("vendor/x/client.go", "package pkg\n\nvar c Server\n"),
    ] {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "ranking"]);
}

#[test]
fn ranked_results_come_best_first() {
    let dir = tempfile::tempdir().unwrap();
    ranking_fixture(dir.path());
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index
        .build(&BuildRequest {
            repo: RepoId(1),
            name: "r".into(),
            git_dir: dir.path().join(".git"),
            branches: vec![Branch {
                name: "HEAD".into(),
                commit: head.trim().into(),
            }],
            ranks: None,
            always_index: Default::default(),
        })
        .unwrap();
    let search = |q: &str, limit: usize| {
        let mut query = index_query(q, "HEAD");
        query.repos[0].repo = RepoId(1);
        query.file_limit = limit;
        query.bm25 = !q.contains("patterntype:");
        let mut out = Vec::new();
        index
            .search(&query, &mut |f| {
                out.push((f.path, f.score));
                ControlFlow::Continue(())
            })
            .unwrap();
        assert!(out.windows(2).all(|w| w[0].1 >= w[1].1), "{out:?}");
        out.into_iter().map(|(p, _)| p).collect::<Vec<_>>()
    };
    for q in ["server", "server patterntype:literal"] {
        let got = search(q, 100);
        // The definition in a file named after it first; test and vendored
        // copies after the same text elsewhere; whole words before parts.
        assert_eq!(got[0], "pkg/server.go", "{q}: {got:?}");
        let pos = |p: &str| got.iter().position(|g| g == p).unwrap();
        assert!(
            pos("pkg/client.go") < pos("pkg/client_test.go"),
            "{q}: {got:?}"
        );
        assert!(
            pos("pkg/client.go") < pos("vendor/x/client.go"),
            "{q}: {got:?}"
        );
        assert!(pos("a/notes.txt") < pos("a/servers.txt"), "{q}: {got:?}");
        // A limit keeps the best, not the first in path order.
        assert_eq!(search(q, 1), ["pkg/server.go"]);
    }
}

// Static ranks: among otherwise equal matches the file the
// code graph ranks higher comes first; a new ranks generation rebuilds the
// shard, and a delta keeps the base's ranks.
#[test]
fn static_ranks_break_ties() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    std::fs::create_dir_all(p.join("x")).unwrap();
    for name in ["a.go", "b.go", "c.go"] {
        std::fs::write(p.join("x").join(name), "package x\n\nvar v = Thing\n").unwrap();
    }
    // Enough other files that one changed file makes a delta, not a rebuild.
    for i in 0..10 {
        std::fs::write(p.join(format!("f{i}.txt")), format!("filler {i}\n")).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "one"]);
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    let request = |ranks: Option<sourcegull_types::PathRanks>| BuildRequest {
        repo: RepoId(1),
        name: "r".into(),
        git_dir: p.join(".git"),
        branches: vec![Branch {
            name: "HEAD".into(),
            commit: git(p, &["rev-parse", "HEAD"]).trim().into(),
        }],
        ranks: ranks.map(std::sync::Arc::new),
        always_index: Default::default(),
    };
    let search = |index: &ShardIndex| {
        let mut query = index_query("Thing", "HEAD");
        query.repos[0].repo = RepoId(1);
        let mut out = Vec::new();
        index
            .search(&query, &mut |f| {
                out.push((f.path, f.score));
                ControlFlow::Continue(())
            })
            .unwrap();
        out
    };
    let update = |req: &BuildRequest| sourcegull_search_exec::update_index(&index, req).unwrap();
    assert!(matches!(
        update(&request(None)),
        sourcegull_search_exec::IndexUpdate::Built(_)
    ));
    let plain = search(&index);
    let paths: Vec<&str> = plain.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["x/a.go", "x/b.go", "x/c.go"]);
    assert_eq!(plain[0].1, plain[1].1);
    assert_eq!(index.ranks_generation(RepoId(1)), Some(0));

    let ranks = |generation| {
        sourcegull_types::PathRanks::new(
            generation,
            [("x/b.go".to_owned(), 1.0), ("x/c.go".to_owned(), 0.5)],
        )
    };
    // Same commit, new ranks: a full rebuild.
    assert!(matches!(
        update(&request(Some(ranks(3)))),
        sourcegull_search_exec::IndexUpdate::Built(_)
    ));
    assert_eq!(index.ranks_generation(RepoId(1)), Some(3));
    assert_eq!(
        update(&request(Some(ranks(3)))),
        sourcegull_search_exec::IndexUpdate::Unchanged
    );
    let ranked = search(&index);
    let paths: Vec<&str> = ranked.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["x/b.go", "x/c.go", "x/a.go"]);
    let bonus = ranked[0].1 - plain[1].1;
    assert!(
        (bonus - sourcegull_search::ranking::DOC_RANK).abs() < 1e-3,
        "{bonus}"
    );

    // A new commit with the same generation writes a delta that keeps the ranks.
    std::fs::write(p.join("x/b.go"), "package x\n\nvar v = Thing // changed\n").unwrap();
    git(p, &["commit", "-qam", "two"]);
    update(&request(Some(ranks(3))));
    assert!(index.shards(RepoId(1)).unwrap().delta.is_some());
    let after: Vec<String> = search(&index).into_iter().map(|(p, _)| p).collect();
    assert_eq!(after, ["x/b.go", "x/c.go", "x/a.go"]);
    // Ranks dropped: rebuilt without them.
    update(&request(None));
    assert_eq!(index.ranks_generation(RepoId(1)), Some(0));
    assert!(index.shards(RepoId(1)).unwrap().delta.is_none());
}

/// `repos_only` (`repo:has.file()` sub-searches): each requested branch once, with a
/// file that matches there, and nothing for branches without one.
#[test]
fn repos_only_reports_each_branch_once() {
    let (dir, main, dev) = fixture();
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index.build(&build_request(dir.path(), main, dev)).unwrap();
    let run = |q: &str| {
        let mut iq = index_query(q, "main");
        iq.repos[0].branches = vec!["main".into(), "dev".into()];
        iq.repos_only = true;
        let mut got = Vec::new();
        index
            .search(&iq, &mut |f: IndexedFile| {
                assert!(f.chunks.is_empty());
                for b in f.branches {
                    got.push((b.name, f.path.clone()));
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        got.sort();
        got
    };
    // Common files: both branches, each once.
    let got = run("foo");
    assert_eq!(
        got.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(),
        ["dev", "main"]
    );
    // Only on dev.
    assert_eq!(
        run("func Dev"),
        [("dev".to_owned(), "src/main.go".to_owned())]
    );
    assert_eq!(run("type:path new\\.md patterntype:regexp").len(), 1);
    assert_eq!(run("file:^docs/new\\.md$").len(), 1);
    assert!(run("nosuchstringanywhere").is_empty());
}

// Flush wall time: a ranked search holds its matches to send
// the best first only until the flush wall time; after that it streams them
// as found, with chunks, and still stops at the file limit.
#[test]
fn ranked_search_streams_after_the_flush_wall_time() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    ranking_fixture(p);
    for i in 0..30 {
        let text = format!("server number {i}\n{}\n", "a server line\n".repeat(i % 4));
        std::fs::write(p.join(format!("n{i:02}.txt")), text).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "more"]);
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index
        .build(&BuildRequest {
            repo: RepoId(1),
            name: "r".into(),
            git_dir: p.join(".git"),
            branches: vec![Branch {
                name: "HEAD".into(),
                commit: git(p, &["rev-parse", "HEAD"]).trim().into(),
            }],
            ranks: None,
            always_index: Default::default(),
        })
        .unwrap();
    drop(index);
    let open = |flush: std::time::Duration| {
        let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
        index.with_flush_wall_time(flush)
    };
    let held = open(std::time::Duration::from_secs(3600));
    let streamed = open(std::time::Duration::ZERO);
    let search = |index: &ShardIndex, limit: usize| {
        let mut query = index_query("server", "HEAD");
        query.repos[0].repo = RepoId(1);
        query.file_limit = limit;
        query.bm25 = true;
        let mut out = Vec::new();
        let stats = index
            .search(&query, &mut |f| {
                assert!(!f.chunks.is_empty(), "{} has no chunks", f.path);
                out.push((f.path, f.chunks, f.score));
                ControlFlow::Continue(())
            })
            .unwrap();
        (out, stats.limit_hit)
    };
    // No limit: the same files and chunks; held ones best first, streamed
    // ones in path order.
    let (all_held, hit) = search(&held, 0);
    assert!(!hit);
    let (all_streamed, hit) = search(&streamed, 0);
    assert!(!hit);
    assert_eq!(all_held.len(), 36);
    assert!(all_held.windows(2).all(|w| w[0].2 >= w[1].2));
    assert!(all_streamed.windows(2).all(|w| w[0].0 < w[1].0));
    let mut sorted = all_held.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(sorted, all_streamed);
    // A limit keeps the best files while holding, the first ones found
    // once streaming.
    let (best, hit) = search(&held, 5);
    assert!(hit);
    assert_eq!(best, all_held[..5]);
    let (first, hit) = search(&streamed, 5);
    assert!(hit);
    assert_eq!(first, all_streamed[..5]);
}

// Standard queries (the default) read `copyright (c) 2005` as one phrase with
// literal parentheses. Both backends agree.
#[test]
fn standard_phrases_keep_parentheses() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    let files = [
        ("a.c", "/*\n * Copyright (C) 2005 Jane Doe\n */\n"),
        ("b.c", "// copyright (c) 2005, 2006 ACME\n"),
        ("c.c", "// copyright c 2005\n"),
        ("d.c", "// Copyright (c) 2006\nint 2005;\n"),
        ("e.c", "int f(a,  b);\n"),
        ("f.c", "int f(a, b);\n"),
    ];
    for (name, text) in files {
        std::fs::write(p.join(name), text).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "one"]);
    let head = git(p, &["rev-parse", "HEAD"]);
    let head = ObjectId::from_hex(head.trim().as_bytes()).unwrap();
    let repo = gix::open(p).unwrap();
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index.build(&build_request(p, head, head)).unwrap();
    let cases: &[(&str, &[&str])] = &[
        ("copyright (c) 2005", &["a.c", "b.c"]),
        ("copyright (c) 2005 case:yes", &["b.c"]),
        ("(c) 2005", &["a.c", "b.c"]),
        ("copyright c 2005", &["c.c"]),
        ("(copyright or int) and 2005", &["a.c", "b.c", "c.c", "d.c"]),
        ("/copyright/ (c) 2006", &["d.c"]),
        ("f(a,  b)", &["e.c"]),
        ("f(a, b)", &["f.c"]),
        ("patterntype:regexp copyright 2005", &["a.c", "b.c", "c.c"]),
        ("patterntype:regexp 2005 copyright", &[]),
    ];
    for (q, want) in cases {
        let q = if q.contains("patterntype:") {
            q.to_string()
        } else {
            format!("patterntype:standard {q}")
        };
        let paths = |rows: Vec<Row>| rows.into_iter().map(|r| r.0).collect::<Vec<_>>();
        let got = paths(unindexed(&repo, head, &q));
        assert_eq!(got, *want, "unindexed {q:?}");
        assert_eq!(paths(indexed(&index, &q, "main")), *want, "indexed {q:?}");
    }
}

// Whitespace inside a phrase matches as written, and empty quotes are the text `""` or `''`.
// Both backends agree.
#[test]
fn phrases_match_whitespace_and_empty_quotes_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    let files = [
        ("one.txt", "alpha beta\n"),
        ("two.txt", "alpha  beta\n"),
        ("tab.txt", "alpha\tbeta\n"),
        ("dq.txt", "x = \"\";\n"),
        ("sq.txt", "x = '';\n"),
    ];
    for (name, text) in files {
        std::fs::write(p.join(name), text).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "one"]);
    let head = git(p, &["rev-parse", "HEAD"]);
    let head = ObjectId::from_hex(head.trim().as_bytes()).unwrap();
    let repo = gix::open(p).unwrap();
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index.build(&build_request(p, head, head)).unwrap();
    let cases: &[(&str, &[&str])] = &[
        ("patterntype:standard alpha beta", &["one.txt"]),
        ("patterntype:standard alpha  beta", &["two.txt"]),
        ("patterntype:standard alpha\tbeta", &["tab.txt"]),
        ("patterntype:literal alpha  beta", &["two.txt"]),
        ("patterntype:literal alpha\tbeta", &["tab.txt"]),
        // A field between the words: they are joined on one space.
        ("patterntype:standard alpha file:txt  beta", &["one.txt"]),
        // A regexp next to a word is ANDed with it.
        (
            "patterntype:standard /alph[a]/  beta",
            &["one.txt", "tab.txt", "two.txt"],
        ),
        (r#"patterntype:keyword "alpha  beta""#, &["two.txt"]),
        (r#"patterntype:keyword "alpha\tbeta""#, &["tab.txt"]),
        (r#"patterntype:keyword """#, &["dq.txt"]),
        ("patterntype:keyword ''", &["sq.txt"]),
        (r#"patterntype:regexp """#, &["dq.txt"]),
    ];
    for (q, want) in cases {
        let paths = |rows: Vec<Row>| {
            let mut paths = rows.into_iter().map(|r| r.0).collect::<Vec<_>>();
            paths.sort();
            paths
        };
        assert_eq!(paths(unindexed(&repo, head, q)), *want, "unindexed {q:?}");
        assert_eq!(paths(indexed(&index, q, "main")), *want, "indexed {q:?}");
    }
}

/// A repository of `n` one-line files, indexed as repository 3.
fn many_files(n: usize) -> (tempfile::TempDir, ObjectId, tempfile::TempDir, ShardIndex) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    for i in 0..n {
        std::fs::write(p.join(format!("f{i:05}.txt")), format!("line {i}\n")).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "many"]);
    let head = git(p, &["rev-parse", "HEAD"]);
    let index_dir = tempfile::tempdir().unwrap();
    let (index, _) = ShardIndex::open(index_dir.path(), Default::default()).unwrap();
    index
        .build(&BuildRequest {
            repo: RepoId(3),
            name: "r".into(),
            git_dir: p.join(".git"),
            branches: vec![Branch {
                name: "HEAD".into(),
                commit: head.trim().into(),
            }],
            ranks: None,
            always_index: Default::default(),
        })
        .unwrap();
    let commit = ObjectId::from_hex(head.trim().as_bytes()).unwrap();
    // Matches stream as they are found.
    (
        dir,
        commit,
        index_dir,
        index.with_flush_wall_time(Duration::ZERO),
    )
}

// Patterns without trigrams: every file is verified.
const NO_MATCH: &str = "patterntype:regexp q.z";
const EVERY_FILE: &str = "patterntype:regexp l.n";

#[test]
fn canceled_indexed_search_stops_within_a_batch() {
    let (_dir, _, _index_dir, index) = many_files(3000);
    // Without a cancel, every file is verified.
    let stats = index
        .search(&index_query(NO_MATCH, "HEAD"), &mut |_| {
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(stats.files_searched, 3000);
    assert!(!stats.canceled && !stats.limit_hit);

    // A deadline already past: nothing is verified.
    let mut query = index_query(NO_MATCH, "HEAD");
    query.cancel = Cancel::new().with_deadline(Some(Instant::now()));
    let stats = index
        .search(&query, &mut |_| ControlFlow::Continue(()))
        .unwrap();
    assert!(stats.canceled && stats.limit_hit);
    assert_eq!(stats.files_searched, 0);

    // Canceled at the first match: the batch being verified ends, no other starts.
    let mut query = index_query(EVERY_FILE, "HEAD");
    let cancel = Cancel::new();
    query.cancel = cancel.clone();
    let mut emitted = 0;
    let stats = index
        .search(&query, &mut |_| {
            emitted += 1;
            cancel.cancel();
            ControlFlow::Continue(())
        })
        .unwrap();
    assert!(stats.canceled && stats.limit_hit);
    assert!(
        (1..=16).contains(&stats.files_searched),
        "{}",
        stats.files_searched
    );
    assert_eq!(emitted, stats.files_searched);

    // Path search and symbol search check the flag too.
    for (q, symbols) in [("type:path f0", false), ("line", true)] {
        let mut query = index_query(q, "HEAD");
        query.symbols = symbols;
        query.cancel = Cancel::new().with_deadline(Some(Instant::now()));
        let mut emitted = 0;
        let stats = index
            .search(&query, &mut |_| {
                emitted += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert!(stats.canceled, "{q}");
        assert_eq!(emitted, 0, "{q}");
    }
}

#[test]
fn canceled_unindexed_search_stops_within_a_batch() {
    let (dir, commit, _index_dir, _index) = many_files(3000);
    let repo = gix::open(dir.path()).unwrap();
    let search = |q: &str, cancel: Cancel, on_match: &dyn Fn()| {
        let (pattern, _, files) = plan(q);
        let spec = PatternSpec::from_pattern(&pattern.unwrap()).unwrap();
        let matcher = Matcher::from_spec(&spec, false).unwrap();
        let paths = PathFilter::compile(&files, false).unwrap();
        let opts = UnindexedOptions {
            cancel,
            ..UnindexedOptions::default()
        };
        let mut emitted = 0;
        let stats = search_commit(&repo, commit, Some(&matcher), &paths, &opts, |_| {
            emitted += 1;
            on_match();
            ControlFlow::Continue(())
        })
        .unwrap();
        (stats, emitted)
    };
    let (stats, _) = search(NO_MATCH, Cancel::new(), &|| {});
    assert_eq!(stats.files_searched, 3000);
    assert!(!stats.canceled);

    let expired = Cancel::new().with_deadline(Some(Instant::now()));
    let (stats, emitted) = search(NO_MATCH, expired, &|| {});
    assert!(stats.canceled && stats.limit_hit);
    assert_eq!((stats.files_searched, emitted), (0, 0));

    let cancel = Cancel::new();
    let (stats, emitted) = search(EVERY_FILE, cancel.clone(), &|| cancel.cancel());
    assert!(stats.canceled && stats.limit_hit);
    assert!(
        (1..=16).contains(&stats.files_searched),
        "{}",
        stats.files_searched
    );
    assert_eq!(emitted, stats.files_searched);
}
