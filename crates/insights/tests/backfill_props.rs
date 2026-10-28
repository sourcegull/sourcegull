//! Property tests of the incremental backfill: over generated git
//! histories (adds, edits, deletes, renames, files replaced by directories, binary files,
//! merges, identical files across repositories), counting each point from the previous one
//! through tree diffs gives exactly the full evaluation at every point, for content, path,
//! symbol, capture-group, grouped, commit and diff counts, also when counting resumes from a
//! stored point (recording) and when the blob cache is shared across repositories. For plain
//! content counts the full evaluation is also checked against unindexed search itself.

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;

use proptest::prelude::*;
use sourcegull_gitstore::ObjectId;
use sourcegull_search::Matcher;
use sourcegull_search_exec::series::{
    CountCache, Group, LeafKind, RepoPredicate, SeriesCounter, SeriesLeaf, SeriesPoint, SeriesRun,
    SeriesSpec, count_series,
};
use sourcegull_search_exec::{PathFilter, PatternSpec, UnindexedOptions, search_commit};

const PATHS: &[&str] = &[
    "a.go",
    "b.go",
    "src/c.go",
    "src/d.py",
    "src/e.txt",
    "docs/f.md",
    "x",
    "x/y.go",
    "x/z/w.py",
];

const LINES: &[&str] = &[
    "TODO(ann) fix this",
    "todo(bob)",
    "func Foo() {}",
    "func Bar() int { return 1 }",
    "def foo():\n    pass",
    "class Foo:\n    pass",
    "hello world",
    "TODO(ann) TODO(cid) todo",
    "bin\u{0}ary",
    "",
];

/// Epoch of the generated commits.
const BASE: i64 = 1_650_000_000;
/// Seconds between generated commits.
const STEP: i64 = 100;

#[derive(Debug, Clone)]
enum Op {
    Write(usize, Vec<usize>),
    Delete(usize),
    Rename(usize, usize),
    /// A commit on a side branch merged back with `--no-ff`.
    Merge(Vec<usize>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..PATHS.len(), prop::collection::vec(0..LINES.len(), 0..5))
            .prop_map(|(p, l)| Op::Write(p, l)),
        2 => (0..PATHS.len()).prop_map(Op::Delete),
        2 => (0..PATHS.len(), 0..PATHS.len()).prop_map(|(a, b)| Op::Rename(a, b)),
        1 => prop::collection::vec(0..LINES.len(), 1..4).prop_map(Op::Merge),
    ]
}

fn history() -> impl Strategy<Value = Vec<Vec<Op>>> {
    prop::collection::vec(prop::collection::vec(op(), 1..4), 2..9)
}

struct Repo {
    dir: tempfile::TempDir,
    clock: std::cell::Cell<i64>,
    commits: usize,
}

impl Repo {
    fn new() -> Self {
        let r = Self {
            dir: tempfile::tempdir().unwrap(),
            clock: std::cell::Cell::new(BASE),
            commits: 0,
        };
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let date = format!("{} +0000", self.clock.get());
        let out = Command::new("git")
            .args(["-c", "user.name=Ann", "-c", "user.email=ann@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.path())
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn commit(&mut self, msg: &str) {
        self.clock.set(self.clock.get() + STEP);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.commits += 1;
    }

    /// Writes `path`, replacing a file where a directory must go and a directory where the
    /// file goes.
    fn write(&self, path: &str, content: &str) {
        let root = self.path();
        let mut prefix = String::new();
        for part in path.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
            prefix = if prefix.is_empty() {
                (*part).to_owned()
            } else {
                format!("{prefix}/{part}")
            };
            if root.join(&prefix).is_file() {
                std::fs::remove_file(root.join(&prefix)).unwrap();
            }
        }
        let p = root.join(path);
        if p.is_dir() {
            std::fs::remove_dir_all(&p).unwrap();
        }
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        let p = self.path().join(path);
        p.is_file().then(|| std::fs::read_to_string(p).unwrap())
    }

    fn delete(&self, path: &str) {
        let p = self.path().join(path);
        if p.is_file() {
            std::fs::remove_file(&p).unwrap();
        }
    }

    fn apply(&mut self, ops: &[Op], n: usize) {
        for op in ops {
            match op {
                Op::Write(p, lines) => self.write(PATHS[*p], &content(lines)),
                Op::Delete(p) => self.delete(PATHS[*p]),
                Op::Rename(a, b) if a != b => {
                    if let Some(c) = self.read(PATHS[*a]) {
                        self.delete(PATHS[*a]);
                        self.write(PATHS[*b], &c);
                    }
                }
                Op::Rename(..) => {}
                Op::Merge(lines) => {
                    let side = format!("side{}", self.commits);
                    self.git(&["checkout", "-q", "-b", &side]);
                    self.write(&format!("side/s{}.go", self.commits), &content(lines));
                    self.commit("side");
                    self.git(&["checkout", "-q", "main"]);
                    self.commit("main moves on");
                    self.clock.set(self.clock.get() + STEP);
                    self.git(&["merge", "-q", "--no-ff", "-m", "merge", &side]);
                    self.commits += 1;
                }
            }
        }
        self.commit(&format!("commit {n}: {}", content(&[n % LINES.len()])));
    }

    fn head(&self) -> ObjectId {
        ObjectId::from_hex(self.git(&["rev-parse", "HEAD"]).as_bytes()).unwrap()
    }
}

fn content(lines: &[usize]) -> String {
    let mut s = lines
        .iter()
        .map(|l| LINES[*l])
        .collect::<Vec<_>>()
        .join("\n");
    s.push('\n');
    s
}

fn atom(re: &str) -> PatternSpec {
    PatternSpec::Atom {
        regex: re.into(),
        negated: false,
    }
}

/// The series checked on every history.
fn specs() -> Vec<(&'static str, SeriesSpec)> {
    let content = |re: &str| SeriesLeaf::new(LeafKind::Content, Some(atom(re)));
    let mut go_funcs = content(r"func \w+");
    go_funcs.paths.include = vec![r"\.go$".into()];
    let mut not_bob = SeriesLeaf::new(
        LeafKind::Content,
        Some(PatternSpec::And(vec![
            atom("todo"),
            PatternSpec::Atom {
                regex: "bob".into(),
                negated: true,
            },
        ])),
    );
    not_bob.paths.exclude = vec!["^docs/".into()];
    let mut go_files = SeriesLeaf::new(LeafKind::Path, None);
    go_files.paths.langs = vec!["go".into()];
    let mut with_foo = SeriesLeaf::new(LeafKind::Path, Some(atom("/")));
    with_foo.file_contents = vec![(false, "(?i)foo".into()), (true, "bar".into())];
    let mut commits = SeriesLeaf::new(LeafKind::Commit, Some(atom("commit [0-9]*[13579]:")));
    commits.paths.include = vec![r"\.go$".into()];
    vec![
        ("content", SeriesSpec::count(vec![content("todo")])),
        ("content in go files", SeriesSpec::count(vec![go_funcs])),
        ("and/not", SeriesSpec::count(vec![not_bob])),
        (
            "paths",
            SeriesSpec::count(vec![
                SeriesLeaf::new(LeafKind::Path, Some(atom("src/"))),
                go_files,
                with_foo,
            ]),
        ),
        (
            "symbols",
            SeriesSpec::count(vec![SeriesLeaf::new(LeafKind::Symbol, Some(atom("foo")))]),
        ),
        (
            "capture",
            SeriesSpec {
                group: Group::Capture,
                capture: Some(r"TODO\((\w+)\)".into()),
                ..SeriesSpec::count(vec![content(r"TODO\(")])
            },
        ),
        (
            "by lang",
            SeriesSpec {
                group: Group::Lang,
                ..SeriesSpec::count(vec![content("o")])
            },
        ),
        (
            "by path",
            SeriesSpec {
                group: Group::Path,
                ..SeriesSpec::count(vec![content("todo|foo")])
            },
        ),
        (
            "commits and diffs",
            SeriesSpec {
                group: Group::Author,
                ..SeriesSpec::count(vec![
                    commits.clone(),
                    SeriesLeaf::new(LeafKind::Diff, Some(atom("todo"))),
                ])
            },
        ),
        (
            "commits by date",
            SeriesSpec {
                group: Group::Date,
                ..SeriesSpec::count(vec![commits])
            },
        ),
        (
            "repo predicate",
            SeriesSpec {
                repo_predicates: vec![RepoPredicate {
                    negated: false,
                    path: Some(r"^x/".into()),
                    content: Some("hello".into()),
                }],
                ..SeriesSpec::count(vec![content("o")])
            },
        ),
    ]
}

fn run(
    repo: &gix::Repository,
    head: ObjectId,
    counter: &SeriesCounter,
    times: &[i64],
    base: Option<&SeriesPoint>,
    full: bool,
    cache: Option<&CountCache>,
) -> Vec<SeriesPoint> {
    let run = SeriesRun {
        times: times.to_vec(),
        at_head: false,
        base: base.and_then(|b| Some((b.commit?, b.counts.clone()))),
        full,
    };
    count_series(repo, None, head, counter, &run, cache)
        .unwrap()
        .0
}

/// Plain content counts through unindexed search: matching ranges per file (1 for a file
/// matched only through negations).
fn searched(repo: &gix::Repository, commit: ObjectId, pattern: &PatternSpec) -> u64 {
    let matcher = Matcher::from_spec(pattern, false).unwrap();
    let paths = PathFilter::compile(&Default::default(), false).unwrap();
    let mut n = 0;
    search_commit(
        repo,
        commit,
        Some(&matcher),
        &paths,
        &UnindexedOptions::default(),
        |f| {
            let ranges: usize = f.chunks.iter().map(|c| c.ranges.len()).sum();
            n += ranges.max(1) as u64;
            ControlFlow::Continue(())
        },
    )
    .unwrap();
    n
}

fn build(history: &[Vec<Op>]) -> Repo {
    let mut r = Repo::new();
    for (n, ops) in history.iter().enumerate() {
        r.apply(ops, n);
    }
    r
}

fn times(repo: &Repo, picks: &[i64]) -> Vec<i64> {
    let end = repo.clock.get() + STEP;
    let set: BTreeSet<i64> = picks
        .iter()
        .map(|p| BASE - STEP + p.rem_euclid(end - BASE + 2 * STEP))
        .collect();
    set.into_iter().collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(40),
        // Failing cases are saved next to this file and re-run first (checked in).
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::SourceParallel("proptest-regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn incremental_backfill_equals_full_evaluation(
        h1 in history(),
        h2 in history(),
        picks in prop::collection::vec(any::<i64>(), 1..8),
        split in any::<prop::sample::Index>(),
    ) {
        let r1 = build(&h1);
        let r2 = build(&h2);
        let (g1, g2) = (gix::open(r1.path()).unwrap(), gix::open(r2.path()).unwrap());
        let (head1, head2) = (r1.head(), r2.head());
        let times1 = times(&r1, &picks);
        let times2 = times(&r2, &picks);
        // One cache across both repositories and every series, as on a repo node.
        let cache = CountCache::new(8 << 20);
        for (name, spec) in specs() {
            for (repo, head, times, label) in
                [(&g1, head1, &times1, "repo 1"), (&g2, head2, &times2, "repo 2")]
            {
                let counter = SeriesCounter::compile(&spec, label).unwrap().with_threads(2);
                let full = run(repo, head, &counter, times, None, true, None);
                let inc = run(repo, head, &counter, times, None, false, Some(&cache));
                prop_assert_eq!(&inc, &full, "{} in {}", name, label);
                // Resuming from a stored point (as recording does) gives the same.
                let at = split.index(times.len() + 1);
                let first = run(repo, head, &counter, &times[..at], None, false, Some(&cache));
                let rest = run(
                    repo, head, &counter, &times[at..], first.last(), false, Some(&cache),
                );
                let resumed: Vec<SeriesPoint> = first.into_iter().chain(rest).collect();
                prop_assert_eq!(&resumed, &full, "{} in {} resumed at {}", name, label, at);
                if name == "content" {
                    for p in &full {
                        let want = p.commit.map_or(0, |c| searched(repo, c, &atom("todo")));
                        prop_assert_eq!(p.counts.total(), want, "search at {:?}", p.commit);
                    }
                }
            }
        }
    }
}
