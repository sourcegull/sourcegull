//! Read APIs against a cloned fixture, using the git CLI as the oracle.

use std::collections::HashMap;
use std::sync::Arc;

use crate::common::{SourceRepo, git_in, history_repo, store};
use futures::StreamExt;
use sourcegull_gitstore::archive::{ArchiveFormat, archive_stream};
use sourcegull_gitstore::blame::{BlameOptions, blame_gix, blame_stream};
use sourcegull_gitstore::diff::{ChangeStatus, DiffOptions, compare, diff_commits_vec};
use sourcegull_gitstore::read::{self, EntryKind, ListTreeOptions, LogOptions};
use sourcegull_gitstore::refs::{ListRefsFilter, RefKind, list_refs};
use sourcegull_gitstore::{GitError, GitStore, ObjectId, graph};
use sourcegull_types::RepoId;

const REPO: RepoId = RepoId(1);

async fn cloned() -> (SourceRepo, tempfile::TempDir, Arc<GitStore>) {
    let src = history_repo();
    let (d, store) = store();
    store.ensure(REPO, &src.remote(), false).await.unwrap();
    (src, d, store)
}

fn oid(s: &str) -> ObjectId {
    ObjectId::from_hex(s.trim().as_bytes()).unwrap()
}

#[tokio::test]
async fn resolves_revisions() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let head = oid(&src.head());
    assert_eq!(read::resolve_revision(&r, "").unwrap(), head);
    assert_eq!(read::resolve_revision(&r, "HEAD").unwrap(), head);
    assert_eq!(read::resolve_revision(&r, "main").unwrap(), head);
    // Annotated tag peels to its commit.
    assert_eq!(read::resolve_revision(&r, "v1.0").unwrap(), head);
    assert_eq!(
        read::resolve_revision(&r, "HEAD~1").unwrap(),
        oid(&src.git(&["rev-parse", "HEAD~1"]))
    );
    assert_eq!(read::resolve_revision(&r, &src.head()[..10]).unwrap(), head);
    for bad in ["nope", "--upload-pack=x", "main..feature", "HEAD:README.md"] {
        assert!(
            matches!(
                read::resolve_revision(&r, bad),
                Err(GitError::RevisionNotFound(_))
            ),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn lists_trees_and_stats_paths() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let head = oid(&src.head());

    let (root, truncated) = read::list_tree_vec(&r, head, &ListTreeOptions::default()).unwrap();
    assert!(!truncated);
    let names: Vec<(&str, EntryKind)> = root.iter().map(|e| (e.path.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [
            ("README.md", EntryKind::File),
            ("docs", EntryKind::Directory),
            ("src", EntryKind::Directory)
        ]
    );
    assert!(root.iter().all(|e| e.size.is_none()));

    let opts = ListTreeOptions {
        recursive: true,
        include_sizes: true,
        ..Default::default()
    };
    let (all, _) = read::list_tree_vec(&r, head, &opts).unwrap();
    let paths: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "docs",
            "docs/manual.md",
            "src",
            "src/lib.rs",
            "src/main.rs"
        ]
    );
    // Sizes and oids match `git ls-tree -r -l`.
    let ls = git_in(&store.repo_dir(REPO), &["ls-tree", "-r", "-l", "HEAD"]);
    for line in ls.lines() {
        let (meta, path) = line.split_once('\t').unwrap();
        let f: Vec<&str> = meta.split_whitespace().collect();
        let e = all.iter().find(|e| e.path == path).unwrap();
        assert_eq!(e.oid.to_string(), f[2]);
        assert_eq!(e.size, Some(f[3].parse().unwrap()));
        assert_eq!(format!("{:06o}", e.mode), f[0]);
    }

    // Subdirectory, limit, and wrong types.
    let (src_dir, _) = read::list_tree_vec(
        &r,
        head,
        &ListTreeOptions {
            path: "/src/".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(src_dir.len(), 2);
    assert_eq!(src_dir[0].name, "lib.rs");
    let (limited, truncated) = read::list_tree_vec(
        &r,
        head,
        &ListTreeOptions {
            recursive: true,
            limit: 3,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(truncated);
    assert_eq!(limited.len(), 3);
    assert!(matches!(
        read::list_tree_vec(
            &r,
            head,
            &ListTreeOptions {
                path: "README.md".into(),
                ..Default::default()
            }
        ),
        Err(GitError::WrongEntryType { .. })
    ));
    assert!(matches!(
        read::stat(&r, head, "missing/file"),
        Err(GitError::PathNotFound(_))
    ));
    let st = read::stat(&r, head, "src/main.rs").unwrap();
    assert_eq!(st.kind, EntryKind::File);
    assert_eq!(st.name, "main.rs");
    assert!(st.size.unwrap() > 0);
    assert_eq!(read::stat(&r, head, "").unwrap().kind, EntryKind::Directory);
}

#[tokio::test]
async fn reads_blobs_with_limits_and_binary_detection() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let head = oid(&src.head());
    let b = read::read_blob(&r, head, "README.md", 1024).unwrap();
    assert_eq!(b.content, b"# Demo\n");
    assert!(!b.binary && !b.truncated);
    assert_eq!(b.size, 7);
    let t = read::read_blob(&r, head, "README.md", 3).unwrap();
    assert_eq!(t.content, b"# D");
    assert!(t.truncated);
    assert_eq!(t.size, 7);
    // bin.dat exists at c2.
    let c2 = second_main_commit(&src);
    let bin = read::read_blob(&r, c2, "bin.dat", 1024).unwrap();
    assert!(bin.binary);
    assert!(matches!(
        read::read_blob(&r, head, "src", 10),
        Err(GitError::WrongEntryType { .. })
    ));
}

#[tokio::test]
async fn commit_info_matches_git() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let merge = oid(&src.git(&["rev-parse", "HEAD~1"]));
    let c = read::commit_info(&r, merge).unwrap();
    assert_eq!(c.parents.len(), 2);
    assert_eq!(c.subject(), "merge feature");
    assert_eq!(c.author.name, "Ann Author");
    assert_eq!(c.author.email, "ann@example.com");
    assert_eq!(c.committer.name, "Cid Committer");
    let ts: i64 = src
        .git(&["log", "-1", "--format=%at", "HEAD~1"])
        .parse()
        .unwrap();
    assert_eq!(c.author.time, ts);
    assert_eq!(c.tree, oid(&src.git(&["rev-parse", "HEAD~1^{tree}"])));
    assert!(matches!(
        read::commit_info(&r, oid(&"12".repeat(20))),
        Err(GitError::RevisionNotFound(_))
    ));
}

fn second_main_commit(src: &SourceRepo) -> ObjectId {
    let list = src.git(&["rev-list", "--reverse", "--first-parent", "main"]);
    oid(list.lines().nth(1).unwrap())
}

#[tokio::test]
async fn log_matches_git_log_for_paths() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let head = oid(&src.head());
    let dir = store.repo_dir(REPO);
    for path in [
        "",
        "src",
        "src/lib.rs",
        "src/main.rs",
        "docs/manual.md",
        "docs",
        "bin.dat",
        "README.md",
    ] {
        let mut args = vec!["log", "--format=%H", "HEAD"];
        if !path.is_empty() {
            args.extend(["--", path]);
        }
        let expected: Vec<String> = git_in(&dir, &args).lines().map(str::to_owned).collect();
        let page = read::log(
            &r,
            head,
            &LogOptions {
                path: path.into(),
                ..Default::default()
            },
        )
        .unwrap();
        let got: Vec<String> = page.commits.iter().map(|c| c.oid.to_string()).collect();
        assert_eq!(got, expected, "path {path:?}");
        assert!(!page.has_more);
    }
    // Paging.
    let all = read::log(&r, head, &LogOptions::default()).unwrap().commits;
    let p1 = read::log(
        &r,
        head,
        &LogOptions {
            limit: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(p1.has_more);
    let p2 = read::log(
        &r,
        head,
        &LogOptions {
            skip: 2,
            limit: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(p1.commits, all[..2]);
    assert_eq!(p2.commits, all[2..4]);
    // First parent skips the feature branch commit.
    let fp = read::log(
        &r,
        head,
        &LogOptions {
            first_parent: true,
            ..Default::default()
        },
    )
    .unwrap();
    let expected: Vec<String> = git_in(&dir, &["log", "--first-parent", "--format=%H"])
        .lines()
        .map(str::to_owned)
        .collect();
    let got: Vec<String> = fp.commits.iter().map(|c| c.oid.to_string()).collect();
    assert_eq!(got, expected);
    // Hiding a commit leaves out it and its ancestors (`HEAD ^second`).
    let second = second_main_commit(&src);
    let hidden = read::log(
        &r,
        head,
        &LogOptions {
            hide: vec![second],
            ..Default::default()
        },
    )
    .unwrap();
    let expected: Vec<String> =
        git_in(&dir, &["log", "--format=%H", "HEAD", &format!("^{second}")])
            .lines()
            .map(str::to_owned)
            .collect();
    let got: Vec<String> = hidden.commits.iter().map(|c| c.oid.to_string()).collect();
    assert!(!expected.is_empty() && expected.len() < all.len());
    assert_eq!(got, expected);
    // `before` (`rev:at.time()`): as `git log --first-parent --before=<t>`, at every
    // commit's own time and just before it.
    for c in &fp.commits {
        for t in [c.committer.time, c.committer.time - 1] {
            let at = read::log(
                &r,
                head,
                &LogOptions {
                    first_parent: true,
                    before: Some(t),
                    limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            let expected = git_in(
                &dir,
                &[
                    "log",
                    "--first-parent",
                    "--format=%H",
                    "-n1",
                    &format!("--before=@{t}"),
                ],
            );
            let got = at.commits.first().map(|c| c.oid.to_string());
            assert_eq!(got.as_deref().unwrap_or(""), expected.trim(), "before {t}");
        }
    }
    // `after` (`repo:has.commit.after()`): as `git log --since=<t>`, at every commit's own
    // time and just after it; past the newest commit nothing is left.
    let newest = all.iter().map(|c| c.committer.time).max().unwrap();
    for t in all
        .iter()
        .flat_map(|c| [c.committer.time, c.committer.time + 1])
        .chain([newest + 1])
    {
        let after = read::log(
            &r,
            head,
            &LogOptions {
                after: Some(t),
                limit: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        let expected: Vec<String> = git_in(&dir, &["log", "--format=%H", &format!("--since=@{t}")])
            .lines()
            .map(str::to_owned)
            .collect();
        let got: Vec<String> = after.commits.iter().map(|c| c.oid.to_string()).collect();
        assert_eq!(got, expected, "after {t}");
        assert!(!after.has_more);
    }
}

#[tokio::test]
async fn refs_listing() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let all = list_refs(
        &r,
        ListRefsFilter {
            branches: true,
            tags: true,
            prefix: "",
        },
    )
    .unwrap();
    let by_name: HashMap<&str, _> = all.iter().map(|r| (r.short_name.as_str(), r)).collect();
    assert_eq!(by_name.len(), 4);
    assert_eq!(by_name["main"].kind, RefKind::Branch);
    assert_eq!(by_name["main"].commit, Some(oid(&src.head())));
    let v1 = by_name["v1.0"];
    assert!(v1.annotated);
    assert_ne!(v1.target, oid(&src.head()));
    assert_eq!(v1.commit, Some(oid(&src.head())));
    assert!(!by_name["light"].annotated);
    // Newest first.
    assert!(all.windows(2).all(|w| w[0].commit_time >= w[1].commit_time));
    let tags = list_refs(
        &r,
        ListRefsFilter {
            branches: false,
            tags: true,
            prefix: "v",
        },
    )
    .unwrap();
    assert_eq!(tags.len(), 1);
}

#[tokio::test]
async fn diffs_match_git() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let dir = store.repo_dir(REPO);
    let rev = |s: &str| oid(&src.git(&["rev-parse", s]));
    let c1 = rev("light");
    let head = rev("HEAD");

    // c1..HEAD: lib/main modified, docs renamed; bin.dat added then removed.
    let (files, truncated) = diff_commits_vec(&r, Some(c1), head, &DiffOptions::default()).unwrap();
    assert!(!truncated);
    let summary: Vec<(ChangeStatus, &str)> = files.iter().map(|f| (f.status, f.path())).collect();
    assert_eq!(
        summary,
        [
            (ChangeStatus::Renamed, "docs/manual.md"),
            (ChangeStatus::Modified, "src/lib.rs"),
            (ChangeStatus::Modified, "src/main.rs"),
        ]
    );
    let renamed = &files[0];
    assert_eq!(renamed.old_path.as_deref(), Some("docs/guide.md"));
    assert!(renamed.hunks.is_empty() && renamed.additions == 0);

    // Hunks and stats equal `git diff` for each file.
    let numstat = git_in(
        &dir,
        &["diff", "--numstat", &c1.to_string(), &head.to_string()],
    );
    for f in files.iter().filter(|f| f.status == ChangeStatus::Modified) {
        let line = numstat.lines().find(|l| l.ends_with(f.path())).unwrap();
        let n: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.additions.to_string(), n[0], "{}", f.path());
        assert_eq!(f.deletions.to_string(), n[1], "{}", f.path());
        let patch = git_in(
            &dir,
            &[
                "diff",
                "--no-color",
                &c1.to_string(),
                &head.to_string(),
                "--",
                f.path(),
            ],
        );
        let git_hunks: Vec<&str> = patch.split("\n@@").skip(1).collect();
        assert_eq!(f.hunks.len(), git_hunks.len(), "{}", f.path());
        for (h, g) in f.hunks.iter().zip(git_hunks) {
            let (header, body) = g.split_once('\n').unwrap();
            let expected_header = format!(
                " -{},{} +{},{} @@",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            );
            assert!(
                header.starts_with(&expected_header),
                "{header} vs {expected_header}"
            );
            let section = header[expected_header.len()..].trim_start_matches(' ');
            assert_eq!(h.section, section, "{}", f.path());
            let body = format!("{}\n", body.trim_end_matches('\n'));
            assert_eq!(h.body, body, "{}", f.path());
        }
    }

    // Default old = first parent; binary detection; path filter; stats only.
    let c2 = second_main_commit(&src);
    let (files, _) = diff_commits_vec(&r, None, c2, &DiffOptions::default()).unwrap();
    let bin = files.iter().find(|f| f.path() == "bin.dat").unwrap();
    assert_eq!(bin.status, ChangeStatus::Added);
    assert!(bin.binary && bin.hunks.is_empty());
    let (files, _) = diff_commits_vec(
        &r,
        Some(c1),
        head,
        &DiffOptions {
            paths: vec!["src".into()],
            stats_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.hunks.is_empty() && f.additions > 0));
    // Root commit diffs against the empty tree.
    let (files, _) = diff_commits_vec(&r, None, c1, &DiffOptions::default()).unwrap();
    assert_eq!(files.len(), 4);
    assert!(files.iter().all(|f| f.status == ChangeStatus::Added));
    // Without rename detection the rename is a delete + add.
    let (files, _) = diff_commits_vec(
        &r,
        Some(c1),
        head,
        &DiffOptions {
            detect_renames: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(files.len(), 4);
}

#[tokio::test]
async fn missing_final_newline_is_marked() {
    let src = SourceRepo::new();
    let a = src.commit("a", &[("f.txt", "one\ntwo")]);
    let b = src.commit("b", &[("f.txt", "one\nthree")]);
    let (_d, store) = store();
    store.ensure(REPO, &src.remote(), false).await.unwrap();
    let r = store.repo(REPO).unwrap();
    let (files, _) = diff_commits_vec(&r, Some(oid(&a)), oid(&b), &DiffOptions::default()).unwrap();
    assert_eq!(
        files[0].hunks[0].body,
        " one\n-two\n\\ No newline at end of file\n+three\n\\ No newline at end of file\n"
    );
}

#[tokio::test]
async fn compare_merge_base_and_ancestry() {
    let (src, _d, store) = cloned().await;
    let r = store.repo(REPO).unwrap();
    let rev = |s: &str| oid(&src.git(&["rev-parse", s]));
    src.git(&["checkout", "-q", "-b", "side", "light"]);
    let side = src.commit("side work", &[("side.txt", "s\n")]);
    src.git(&["checkout", "-q", "main"]);
    store.fetch(REPO, &src.remote()).await.unwrap();
    let r2 = store.repo(REPO).unwrap();
    drop(r);
    let r = r2;

    let (main, light, feature) = (rev("main"), rev("light"), rev("feature"));
    let side = oid(&side);
    assert_eq!(graph::merge_base(&r, main, side).unwrap(), Some(light));
    assert_eq!(graph::merge_base(&r, main, main).unwrap(), Some(main));
    assert!(graph::is_ancestor(&r, light, main).unwrap());
    assert!(graph::is_ancestor(&r, feature, main).unwrap());
    assert!(!graph::is_ancestor(&r, main, feature).unwrap());
    assert!(!graph::is_ancestor(&r, side, main).unwrap());

    let c = compare(&r, main, side, 100).unwrap();
    assert_eq!(c.merge_base, Some(light));
    assert_eq!(c.commits.len(), 1);
    assert_eq!(c.commits[0].oid, side);
    assert_eq!(c.files.len(), 1);
    assert_eq!(c.files[0].path(), "side.txt");
    let c = compare(&r, light, main, 2).unwrap();
    assert!(c.commits_truncated);
    assert_eq!(c.commits.len(), 2);

    // Unrelated history.
    let other = SourceRepo::new();
    let orphan = other.commit("orphan", &[("o", "o\n")]);
    src.git(&[
        "fetch",
        "-q",
        &other.path().display().to_string(),
        "main:orphan",
    ]);
    store.fetch(REPO, &src.remote()).await.unwrap();
    let r = store.repo(REPO).unwrap();
    assert_eq!(graph::merge_base(&r, main, oid(&orphan)).unwrap(), None);
    assert!(
        compare(&r, main, oid(&orphan), 10)
            .unwrap()
            .files
            .is_empty()
    );
}

#[tokio::test]
async fn blame_backends_agree_with_git() {
    let (src, _d, store) = cloned().await;
    let dir = store.repo_dir(REPO);
    let head = oid(&src.head());
    for path in ["src/main.rs", "src/lib.rs", "docs/manual.md"] {
        // Oracle: per-line commit from `git blame --line-porcelain`.
        let porcelain = git_in(&dir, &["blame", "--line-porcelain", "HEAD", "--", path]);
        let expected: Vec<String> = porcelain
            .lines()
            .filter(|l| {
                l.len() > 40
                    && l.as_bytes()[40] == b' '
                    && l[..40].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|l| l[..40].to_owned())
            .collect();

        let mut hunks: Vec<_> =
            blame_stream(store.git_bin(), &dir, head, path, &BlameOptions::default())
                .unwrap()
                .map(|h| h.unwrap())
                .collect()
                .await;
        hunks.sort_by_key(|h| h.start_line);
        let per_line = |hunks: &[sourcegull_gitstore::blame::BlameHunk]| -> Vec<String> {
            let mut v = Vec::new();
            for h in hunks {
                for _ in 0..h.line_count {
                    v.push(h.commit.to_string());
                }
            }
            v
        };
        assert_eq!(per_line(&hunks), expected, "cli {path}");
        assert!(
            hunks
                .iter()
                .all(|h| !h.summary.is_empty() && h.author.name == "Ann Author")
        );

        let r = store.repo(REPO).unwrap();
        let gix_hunks = blame_gix(&r, head, path, &BlameOptions::default()).unwrap();
        assert_eq!(per_line(&gix_hunks), expected, "gix {path}");
    }
    // The renamed file's early lines are attributed to the original path.
    let hunks: Vec<_> = blame_stream(
        store.git_bin(),
        &dir,
        head,
        "docs/manual.md",
        &BlameOptions::default(),
    )
    .unwrap()
    .map(|h| h.unwrap())
    .collect()
    .await;
    assert!(hunks.iter().any(|h| h.orig_path == "docs/guide.md"));

    // Line range.
    let ranged: Vec<_> = blame_stream(
        store.git_bin(),
        &dir,
        head,
        "docs/manual.md",
        &BlameOptions {
            start_line: 5,
            end_line: 7,
            ..Default::default()
        },
    )
    .unwrap()
    .map(|h| h.unwrap())
    .collect()
    .await;
    assert_eq!(ranged.iter().map(|h| h.line_count).sum::<u32>(), 3);
    assert_eq!(ranged.iter().map(|h| h.start_line).min(), Some(5));

    // Errors end the stream.
    let errs: Vec<_> = blame_stream(
        store.git_bin(),
        &dir,
        head,
        "nope.rs",
        &BlameOptions::default(),
    )
    .unwrap()
    .collect()
    .await;
    assert!(
        matches!(errs.last(), Some(Err(GitError::PathNotFound(_)))),
        "{errs:?}"
    );
}

#[tokio::test]
async fn archives_stream_tar_and_zip() {
    let (src, _d, store) = cloned().await;
    let dir = store.repo_dir(REPO);
    let head = oid(&src.head());
    let collect = |format, paths: Vec<String>| {
        let dir = dir.clone();
        let store = store.clone();
        async move {
            let chunks: Vec<_> = archive_stream(
                store.git_bin(),
                &dir,
                head,
                format,
                &paths,
                std::time::Duration::from_secs(30),
            )
            .unwrap()
            .collect()
            .await;
            let mut out = Vec::new();
            for c in chunks {
                out.extend_from_slice(&c.unwrap());
            }
            out
        }
    };
    let tar = collect(ArchiveFormat::Tar, vec![]).await;
    let names = list_archive(&tar, "tar");
    assert!(names.contains(&"src/main.rs".to_owned()), "{names:?}");
    assert!(names.contains(&"docs/manual.md".to_owned()));
    let tar = collect(ArchiveFormat::Tar, vec!["src".into()]).await;
    let names = list_archive(&tar, "tar");
    assert!(names.iter().all(|n| n.starts_with("src")), "{names:?}");
    let zip = collect(ArchiveFormat::Zip, vec![]).await;
    assert_eq!(&zip[..2], b"PK");
}

/// Lists archive members with the system `tar`.
fn list_archive(data: &[u8], _kind: &str) -> Vec<String> {
    use std::io::Write;
    let mut child = std::process::Command::new("tar")
        .arg("-tf")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_end_matches('/').to_owned())
        .collect()
}

#[test]
fn hunk_sections_match_git() {
    let src = SourceRepo::new();
    let mut v1 = String::from("// header comment\n\n");
    for f in 0..6 {
        v1.push_str(&format!("func f{f}(a int) int {{\t \n"));
        for l in 0..12 {
            v1.push_str(&format!("\tx{l} := a + {l}\n"));
        }
        v1.push_str("}\n\n");
    }
    v1.push_str(&format!("type {} struct{{}}\n", "Long".repeat(30)));
    v1.push_str("\tpad := 1\n\tpad := 2\n\tpad := 3\n\tpad := 4\n\tlast := 1\n");
    let c1 = src.commit("one", &[("a.go", &v1)]);
    // Change lines inside f0 (no heading above but the comment), f2, f5,
    // and after the long type line.
    let v2 = v1
        .replace("\tx1 := a + 1\n", "\tx1 := a + 100\n")
        .replace("func f2(a int) int {\t \n\tx0 := a + 0\n\tx1 := a + 1\n\tx2 := a + 2\n\tx3 := a + 3\n\tx4 := a + 4\n\tx5 := a + 5\n\tx6", "func f2(a int) int {\t \n\tx0 := a + 0\n\tx1 := a + 1\n\tx2 := a + 2\n\tx3 := a + 3\n\tx4 := a + 4\n\tx5 := a + 5\n\ty6")
        .replace("\tlast := 1\n", "\tlast := 2\n");
    let c2 = src.commit("two", &[("a.go", &v2)]);
    let r = gix::open(src.path()).unwrap();
    let (files, _) =
        diff_commits_vec(&r, Some(oid(&c1)), oid(&c2), &DiffOptions::default()).unwrap();
    let patch = src.git(&["diff", "--no-color", &c1, &c2]);
    let git_sections: Vec<String> = patch
        .lines()
        .filter(|l| l.starts_with("@@ "))
        .map(|l| {
            l.split_once(" @@")
                .unwrap()
                .1
                .trim_start_matches(' ')
                .to_owned()
        })
        .collect();
    let ours: Vec<&str> = files[0].hunks.iter().map(|h| h.section.as_str()).collect();
    assert!(git_sections.len() >= 4, "{patch}");
    assert_eq!(ours, git_sections);
    assert!(ours.iter().any(|s| s.starts_with("func f2")));
    assert!(ours.iter().any(|s| s.len() == 80));
}
