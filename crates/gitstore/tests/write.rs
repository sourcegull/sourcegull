//! Bulk changes writes: commits from patches in a temporary index,
//! patches applied without committing (commit-via-API mode) and pushes, against
//! real git repositories in temp dirs, through the store and through the gRPC
//! service.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::common::{SourceRepo, git_in, history_repo, store};
use sourcegull_gitstore::patch::{
    ApplyOptions, ChangedFile, FileAction, FileKind, Identity, PatchCommit,
};
use sourcegull_gitstore::push::PushSpec;
use sourcegull_gitstore::{
    GitError, GitStore, ObjectId, PushRejection, Remote, RemoteAuth, RepoNodeService, RepoRef,
    ServiceConfig,
};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ClientConfig, ErrorKind, RepoClients, Router};
use sourcegull_types::RepoId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::wrappers::TcpListenerStream;

const REPO: RepoId = RepoId(1);

fn oid(s: &str) -> ObjectId {
    ObjectId::from_hex(s.trim().as_bytes()).unwrap()
}

/// A store with the history fixture cloned as repo 1.
async fn cloned() -> (tempfile::TempDir, Arc<GitStore>, SourceRepo) {
    let (dir, store) = store();
    let src = history_repo();
    store.ensure(REPO, &src.remote(), false).await.unwrap();
    (dir, store, src)
}

/// Makes the working-tree changes `edit` in `src`, returns `git diff --cached
/// --binary <extra>` against HEAD and resets the work tree.
fn diff_of(src: &SourceRepo, extra: &[&str], edit: impl FnOnce(&SourceRepo)) -> Vec<u8> {
    edit(src);
    src.git(&["add", "-A"]);
    let mut args = vec![
        "-C",
        src.path().to_str().unwrap(),
        "diff",
        "--cached",
        "--binary",
    ];
    args.extend_from_slice(extra);
    let out = std::process::Command::new("git")
        .args(&args)
        .output()
        .unwrap();
    assert!(out.status.success());
    src.git(&["reset", "-q", "--hard", "HEAD"]);
    src.git(&["clean", "-qfd"]);
    out.stdout
}

fn ann() -> Identity {
    Identity {
        name: "Batch Bot".into(),
        email: "bot@example.com".into(),
    }
}

fn spec(base: &str, target_ref: &str, patch: Vec<u8>) -> PatchCommit {
    PatchCommit {
        base: oid(base),
        target_ref: target_ref.into(),
        patch: patch.into(),
        message: "Apply bulk change\n\nBody.".into(),
        author: ann(),
        committer: None,
        date: Some(1_700_100_000),
        apply: ApplyOptions::default(),
    }
}

fn show(dir: &Path, rev_path: &str) -> String {
    git_in(dir, &["show", rev_path])
}

#[tokio::test]
async fn text_patch_becomes_a_commit_on_the_target_ref() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let base = src.head();
    let head_before = git_in(&dir, &["rev-parse", "HEAD"]);
    let patch = diff_of(&src, &[], |s| {
        s.write("src/main.rs", "fn main() {}\n");
        s.write("NEW.md", "new file\n");
        std::fs::remove_file(s.path().join("README.md")).unwrap();
    });

    let c = store
        .create_commit_from_patch(REPO, &spec(&base, "refs/heads/batch-changes/foo", patch))
        .await
        .unwrap();

    assert_eq!(
        oid(&git_in(
            &dir,
            &["rev-parse", "refs/heads/batch-changes/foo"]
        )),
        c.commit
    );
    assert_eq!(
        oid(&git_in(
            &dir,
            &["rev-parse", &format!("{}^{{tree}}", c.commit)]
        )),
        c.tree
    );
    assert_eq!(
        show(&dir, &format!("{}:src/main.rs", c.commit)),
        "fn main() {}\n"
    );
    assert_eq!(show(&dir, &format!("{}:NEW.md", c.commit)), "new file\n");
    assert!(
        git_in(&dir, &["ls-tree", "--name-only", &c.commit.to_string()])
            .lines()
            .all(|l| l != "README.md")
    );
    let meta = git_in(
        &dir,
        &[
            "log",
            "-1",
            "--format=%P|%an|%ae|%cn|%ce|%at|%ct|%B",
            &c.commit.to_string(),
        ],
    );
    assert_eq!(
        meta.trim_end(),
        format!(
            "{base}|Batch Bot|bot@example.com|Batch Bot|bot@example.com|1700100000|1700100000|Apply bulk change\n\nBody."
        )
    );
    // Nothing else in the repo moved, and no index was left in the bare repo.
    assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), head_before);
    assert!(!dir.join("index").exists());
    let leftovers: Vec<_> = std::fs::read_dir(store.layout().tmp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("patch-"))
        .collect();
    assert!(leftovers.is_empty(), "temp index dirs left behind");

    // The same inputs give the same commit (fixed date), and a separate committer is kept.
    let mut again = spec(
        &base,
        "refs/heads/other",
        diff_of(&src, &[], |s| {
            s.write("src/main.rs", "fn main() {}\n");
        }),
    );
    again.committer = Some(Identity {
        name: "Cid".into(),
        email: "cid@example.com".into(),
    });
    let c2 = store.create_commit_from_patch(REPO, &again).await.unwrap();
    assert_eq!(
        git_in(
            &dir,
            &["log", "-1", "--format=%an|%cn", &c2.commit.to_string()]
        )
        .trim(),
        "Batch Bot|Cid"
    );
}

#[tokio::test]
async fn binary_and_no_prefix_patches_apply() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let base = src.head();
    let blob: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let b2 = blob.clone();
    let patch = diff_of(&src, &[], move |s| {
        s.write("assets/logo.bin", &b2);
        s.write("src/lib.rs", "// changed\n");
    });
    assert!(String::from_utf8_lossy(&patch).contains("GIT binary patch"));
    let c = store
        .create_commit_from_patch(REPO, &spec(&base, "refs/heads/bin", patch))
        .await
        .unwrap();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["cat-file", "blob", &format!("{}:assets/logo.bin", c.commit)])
        .output()
        .unwrap();
    assert_eq!(out.stdout, blob);

    // A binary modification on top of the new commit (a delta against the stored blob).
    src.write("assets/logo.bin", &blob);
    src.commit("add logo", &[]);
    let base2 = src.head();
    store.fetch(REPO, &src.remote()).await.unwrap();
    let mut changed = blob.clone();
    changed[100] = 7;
    let c2 = changed.clone();
    let patch = diff_of(&src, &[], move |s| s.write("assets/logo.bin", &c2));
    let c = store
        .create_commit_from_patch(REPO, &spec(&base2, "refs/heads/bin2", patch))
        .await
        .unwrap();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["cat-file", "blob", &format!("{}:assets/logo.bin", c.commit)])
        .output()
        .unwrap();
    assert_eq!(out.stdout, changed);

    // `git diff --no-prefix` output applies with strip 0, and --directory roots it.
    let patch = diff_of(&src, &["--no-prefix", "--relative=src"], |s| {
        s.write("src/lib.rs", "// no prefix\n");
    });
    let mut p = spec(&base2, "refs/heads/np", patch);
    p.apply.strip = Some(0);
    p.apply.directory = "src".into();
    let c = store.create_commit_from_patch(REPO, &p).await.unwrap();
    assert_eq!(
        show(&dir, &format!("{}:src/lib.rs", c.commit)),
        "// no prefix\n"
    );
}

#[tokio::test]
async fn patches_that_do_not_apply_are_typed_errors() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let head = src.head();
    let first = git_in(src.path(), &["rev-list", "--max-parents=0", "HEAD"]);
    let patch = diff_of(&src, &[], |s| {
        s.write("src/main.rs", "fn main() { conflict() }\n")
    });

    // src/main.rs differs at the root commit, so the hunk does not apply there.
    let err = store
        .create_commit_from_patch(REPO, &spec(first.trim(), "refs/heads/x", patch.clone()))
        .await
        .unwrap_err();
    let GitError::PatchFailed { stderr } = &err else {
        panic!("{err}");
    };
    assert!(stderr.contains("src/main.rs"), "{stderr}");
    assert_eq!(err.kind(), ErrorKind::PatchFailed);
    assert!(
        git_in(&dir, &["for-each-ref", "refs/heads/x"]).is_empty(),
        "no ref on failure"
    );

    // Garbage and empty input.
    let err = store
        .create_commit_from_patch(REPO, &spec(&head, "refs/heads/x", b"not a diff\n".to_vec()))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::PatchFailed { .. }), "{err}");
    let err = store
        .create_commit_from_patch(REPO, &spec(&head, "refs/heads/x", Vec::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::PatchFailed { .. }), "{err}");
    let mut empty = spec(&head, "refs/heads/empty", Vec::new());
    empty.apply.allow_empty = true;
    let c = store.create_commit_from_patch(REPO, &empty).await.unwrap();
    assert_eq!(
        c.tree,
        oid(&git_in(&dir, &["rev-parse", &format!("{head}^{{tree}}")]))
    );

    // Missing base, missing repo, bad inputs.
    let err = store
        .create_commit_from_patch(REPO, &spec(&"1".repeat(40), "refs/heads/x", patch.clone()))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::RevisionNotFound(_)), "{err}");
    let err = store
        .create_commit_from_patch(RepoId(99), &spec(&head, "refs/heads/x", patch.clone()))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::RepoNotFound(_)), "{err}");
    for bad in [
        PatchCommit {
            target_ref: "refs/heads/a..b".into(),
            ..spec(&head, "", patch.clone())
        },
        PatchCommit {
            message: " ".into(),
            ..spec(&head, "refs/heads/x", patch.clone())
        },
        PatchCommit {
            author: Identity {
                name: "x".into(),
                email: String::new(),
            },
            ..spec(&head, "refs/heads/x", patch.clone())
        },
    ] {
        let err = store
            .create_commit_from_patch(REPO, &bad)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::InvalidArgument(_)), "{err}");
    }
}

fn set_executable(path: &Path, exec: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test]
async fn changed_files_without_committing() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let base = src.head();
    let refs_before = git_in(&dir, &["for-each-ref"]);
    let binary: Vec<u8> = vec![0, 1, 2, 0, 255, 10];
    let b2 = binary.clone();
    let manual = git_in(&dir, &["show", "HEAD:docs/manual.md"]);
    let patch = diff_of(&src, &[], move |s| {
        s.write("run.sh", "#!/bin/sh\necho hi\n");
        set_executable(&s.path().join("run.sh"), true);
        s.write("logo.bin", &b2);
        s.write("src/main.rs", "fn main() {}\n");
        std::fs::remove_file(s.path().join("src/lib.rs")).unwrap();
        s.git(&["mv", "README.md", "README.txt"]);
        s.git(&["mv", "docs/manual.md", "docs/handbook.md"]);
        s.write(
            "docs/handbook.md",
            manual.replace("line 3\n", "line three\n"),
        );
        std::os::unix::fs::symlink("src/main.rs", s.path().join("link")).unwrap();
    });
    let (tree, files) = store
        .changed_files(
            REPO,
            oid(&base),
            &patch.clone().into(),
            &ApplyOptions::default(),
            1 << 20,
            None,
        )
        .await
        .unwrap();
    let by_path: HashMap<&str, &ChangedFile> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    assert_eq!(files.len(), 7, "{files:#?}");
    let f = by_path["run.sh"];
    assert_eq!(
        (f.action, f.kind, f.previous_kind),
        (FileAction::Create, FileKind::Executable, None)
    );
    assert_eq!(f.content, b"#!/bin/sh\necho hi\n");
    let f = by_path["logo.bin"];
    assert_eq!((f.action, f.kind), (FileAction::Create, FileKind::Regular));
    assert_eq!(f.content, binary);
    let f = by_path["src/main.rs"];
    assert_eq!((f.action, f.content_changed), (FileAction::Update, true));
    assert_eq!(f.content, b"fn main() {}\n");
    let f = by_path["src/lib.rs"];
    assert_eq!((f.action, f.kind), (FileAction::Delete, FileKind::Regular));
    assert!(f.content.is_empty());
    let f = by_path["README.txt"];
    assert_eq!((f.action, f.content_changed), (FileAction::Move, false));
    assert_eq!(f.previous_path.as_deref(), Some("README.md"));
    assert!(f.content.is_empty());
    let f = by_path["docs/handbook.md"];
    assert_eq!((f.action, f.content_changed), (FileAction::Move, true));
    assert_eq!(f.previous_path.as_deref(), Some("docs/manual.md"));
    assert!(String::from_utf8_lossy(&f.content).contains("line three\n"));
    let f = by_path["link"];
    assert_eq!((f.action, f.kind), (FileAction::Create, FileKind::Symlink));
    assert_eq!(f.content, b"src/main.rs");

    // The same tree as committing the patch; no ref changed.
    assert_eq!(git_in(&dir, &["for-each-ref"]), refs_before);
    let c = store
        .create_commit_from_patch(REPO, &spec(&base, "refs/heads/cmp", patch.clone()))
        .await
        .unwrap();
    assert_eq!(c.tree, tree);

    // Only the mode changes.
    let mode = diff_of(&src, &[], |s| {
        set_executable(&s.path().join("src/lib.rs"), true)
    });
    let (_, files) = store
        .changed_files(
            REPO,
            oid(&base),
            &mode.into(),
            &ApplyOptions::default(),
            1 << 20,
            None,
        )
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(
        (f.action, f.kind, f.previous_kind, f.content_changed),
        (
            FileAction::Update,
            FileKind::Executable,
            Some(FileKind::Regular),
            false
        )
    );

    // Limits on the number of files and on the contents' size.
    let err = store
        .changed_files(
            REPO,
            oid(&base),
            &patch.clone().into(),
            &ApplyOptions::default(),
            1 << 20,
            Some(6),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::TooLarge { limit: 6, .. }), "{err}");
    let err = store
        .changed_files(
            REPO,
            oid(&base),
            &patch.clone().into(),
            &ApplyOptions::default(),
            20,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::TooLarge { limit: 20, .. }), "{err}");

    // A patch that does not apply.
    let root = git_in(src.path(), &["rev-list", "--max-parents=0", "HEAD"]);
    let err = store
        .changed_files(
            REPO,
            oid(&root),
            &patch.into(),
            &ApplyOptions::default(),
            1 << 20,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::PatchFailed { .. }), "{err}");
}

#[tokio::test]
async fn concurrent_commits_on_one_repo_are_independent() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let base = src.head();
    let patches: Vec<Vec<u8>> = (0..12)
        .map(|i| {
            diff_of(&src, &[], |s| {
                s.write(&format!("gen/file{i}.txt"), format!("content {i}\n"));
                s.write("src/lib.rs", format!("// version {i}\n"));
            })
        })
        .collect();
    let mut tasks = tokio::task::JoinSet::new();
    for (i, patch) in patches.into_iter().enumerate() {
        let store = store.clone();
        // Half the calls share one ref; the last writer wins there.
        let target = if i % 2 == 0 {
            format!("refs/heads/batch/{i}")
        } else {
            "refs/heads/batch/shared".to_owned()
        };
        let p = spec(&base, &target, patch);
        tasks.spawn(async move { (i, target, store.create_commit_from_patch(REPO, &p).await) });
    }
    let mut shared = Vec::new();
    while let Some(r) = tasks.join_next().await {
        let (i, target, created) = r.unwrap();
        let c = created.unwrap();
        let files = git_in(
            &dir,
            &["ls-tree", "-r", "--name-only", &c.commit.to_string()],
        );
        let generated: Vec<&str> = files.lines().filter(|l| l.starts_with("gen/")).collect();
        assert_eq!(
            generated,
            [format!("gen/file{i}.txt")],
            "commit {i} has only its own file"
        );
        assert_eq!(
            show(&dir, &format!("{}:src/lib.rs", c.commit)),
            format!("// version {i}\n")
        );
        if i % 2 == 0 {
            assert_eq!(oid(&git_in(&dir, &["rev-parse", &target])), c.commit);
        } else {
            shared.push(c.commit);
        }
    }
    let tip = oid(&git_in(&dir, &["rev-parse", "refs/heads/batch/shared"]));
    assert!(shared.contains(&tip));
    git_in(&dir, &["fsck", "--connectivity-only", "--no-dangling"]);
}

fn bare(dir: &Path) {
    git_in(dir, &["init", "-q", "--bare"]);
}

fn local(dir: &Path) -> Remote {
    Remote::new(&format!("file://{}", dir.display()), RemoteAuth::None)
}

#[tokio::test]
async fn push_to_a_local_bare_repo() {
    let (_d, store, src) = cloned().await;
    let base = src.head();
    let dest = tempfile::tempdir().unwrap();
    bare(dest.path());
    let p1 = diff_of(&src, &[], |s| s.write("a.txt", "a\n"));
    let p2 = diff_of(&src, &[], |s| s.write("b.txt", "b\n"));
    let c1 = store
        .create_commit_from_patch(REPO, &spec(&base, "refs/heads/bc/x", p1))
        .await
        .unwrap();
    let c2 = store
        .create_commit_from_patch(REPO, &spec(&base, "refs/heads/bc/y", p2))
        .await
        .unwrap();
    let push = |commit, force| PushSpec {
        remote: local(dest.path()),
        commit,
        destination_ref: "refs/heads/bc/x".into(),
        force,
    };

    // Local remotes need the opt-in.
    let err = store
        .push(REPO, &push(c1.commit, false), false)
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::InvalidArgument(_)), "{err}");

    store
        .push(REPO, &push(c1.commit, false), true)
        .await
        .unwrap();
    assert_eq!(
        oid(&git_in(dest.path(), &["rev-parse", "refs/heads/bc/x"])),
        c1.commit
    );
    // c2 is a sibling of c1: not a fast-forward unless forced.
    let err = store
        .push(REPO, &push(c2.commit, false), true)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GitError::PushRejected {
                reason: PushRejection::NonFastForward,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(err.kind(), ErrorKind::NonFastForward);
    store
        .push(REPO, &push(c2.commit, true), true)
        .await
        .unwrap();
    assert_eq!(
        oid(&git_in(dest.path(), &["rev-parse", "refs/heads/bc/x"])),
        c2.commit
    );

    // A protected branch, as GitLab's pre-receive hook reports it.
    let hook = dest.path().join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'GitLab: You are not allowed to push code to protected branches on this project.' >&2\nexit 1\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let err = store
        .push(REPO, &push(c1.commit, true), true)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GitError::PushRejected {
                reason: PushRejection::PermissionDenied,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);

    // A commit that is not in the repo.
    let err = store
        .push(REPO, &push(oid(&"2".repeat(40)), false), true)
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::RevisionNotFound(_)), "{err}");
}

/// A fake smart-HTTP remote answering every request with `status` (401 with a
/// Basic challenge, or 403). Returns its address and the Authorization headers seen.
async fn http_remote(
    status: u16,
) -> (
    std::net::SocketAddr,
    Arc<std::sync::Mutex<Vec<String>>>,
    Arc<AtomicBool>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen2, stop2) = (seen.clone(), stop.clone());
    tokio::spawn(async move {
        while !stop2.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            if let Some(a) = req.lines().find_map(|l| l.strip_prefix("Authorization: ")) {
                seen2.lock().unwrap().push(a.to_owned());
            }
            let resp = match status {
                401 => {
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"GitLab\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
                _ => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            };
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (addr, seen, stop)
}

#[tokio::test]
async fn http_push_errors_are_classified_and_redacted() {
    let (_d, store, src) = cloned().await;
    let head = oid(&src.head());
    let token = "glpat-PushSecret987";

    let (addr, seen, stop) = http_remote(401).await;
    let spec = PushSpec {
        remote: Remote::new(
            &format!("http://{addr}/g/p.git"),
            RemoteAuth::gitlab_token(token),
        ),
        commit: head,
        destination_ref: "refs/heads/bc".into(),
        force: true,
    };
    let err = tokio::time::timeout(Duration::from_secs(30), store.push(REPO, &spec, false))
        .await
        .unwrap()
        .unwrap_err();
    stop.store(true, Ordering::Relaxed);
    assert!(
        matches!(
            err,
            GitError::PushRejected {
                reason: PushRejection::Unauthenticated,
                ..
            }
        ),
        "{err}"
    );
    assert!(!err.to_string().contains(token), "{err}");
    // base64("oauth2:glpat-PushSecret987"): the token went through askpass.
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|a| a == "Basic b2F1dGgyOmdscGF0LVB1c2hTZWNyZXQ5ODc="),
        "{:?}",
        seen.lock().unwrap()
    );

    // Token in the URL instead: moved to askpass, still redacted.
    let (addr, _seen, stop) = http_remote(403).await;
    let spec = PushSpec {
        remote: Remote::new(
            &format!("http://oauth2:{token}@{addr}/g/p.git"),
            RemoteAuth::None,
        ),
        ..spec
    };
    let err = tokio::time::timeout(Duration::from_secs(30), store.push(REPO, &spec, false))
        .await
        .unwrap()
        .unwrap_err();
    stop.store(true, Ordering::Relaxed);
    assert!(
        matches!(
            err,
            GitError::PushRejected {
                reason: PushRejection::PermissionDenied,
                ..
            }
        ),
        "{err}"
    );
    assert!(!err.to_string().contains(token), "{err}");
}

/// The gRPC service with local pushes allowed; repo 1 is the history fixture.
async fn start(store: Arc<GitStore>, src: &SourceRepo) -> RepoClients {
    let remotes: HashMap<i32, Remote> = [(1, src.remote())].into();
    let resolver = move |r: &RepoRef| {
        remotes
            .get(&r.id.get())
            .cloned()
            .ok_or_else(|| GitError::InvalidArgument(format!("no remote for {}", r.name)))
    };
    let svc = RepoNodeService::new(
        store,
        resolver,
        ServiceConfig {
            allow_local_push: true,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(svc.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    RepoClients::new(
        Router::new([format!("http://{addr}")]),
        ClientConfig::default(),
    )
}

#[tokio::test]
async fn create_commit_and_push_over_grpc() {
    let (_d, store, src) = cloned().await;
    let dir = store.repo_dir(REPO);
    let clients = start(store.clone(), &src).await;
    let mut c = clients.for_repo(REPO).unwrap();
    let base = src.head();
    let dest = tempfile::tempdir().unwrap();
    bare(dest.path());
    let dest_url = dest.path().display().to_string();
    let patch = diff_of(&src, &[], |s| {
        s.write("src/main.rs", "fn main() { batch() }\n")
    });
    let repo = Some(pb::RepoRef {
        id: 1,
        name: "gitlab.example.com/g/r1".into(),
    });
    let target = |url: &str, force| {
        Some(pb::PushTarget {
            remote_url: url.to_owned(),
            credentials: Some(pb::PushCredentials {
                username: String::new(),
                token: "glpat-NeverLogged".into(),
            }),
            force,
        })
    };
    let request = |push| pb::CreateCommitFromPatchRequest {
        repo: repo.clone(),
        base_commit_oid: base.clone(),
        target_ref: "batch-changes/foo".into(),
        patch: patch.clone().into(),
        message: "Batch: greet".into(),
        author: Some(pb::CommitIdentity {
            name: "Ann".into(),
            email: "ann@example.com".into(),
        }),
        committer: None,
        date: Some(1_700_200_000),
        apply: None,
        push,
    };

    // The token never shows in Debug output of the request.
    let dbg = format!("{:?}", request(target(&dest_url, true)));
    assert!(
        !dbg.contains("glpat-NeverLogged") && dbg.contains("[redacted]"),
        "{dbg}"
    );

    // Commit only.
    let r = c
        .create_commit_from_patch(request(None))
        .await
        .unwrap()
        .into_inner();
    assert!(!r.pushed);
    assert_eq!(
        git_in(&dir, &["rev-parse", "refs/heads/batch-changes/foo"]).trim(),
        r.commit_oid
    );

    // Commit and push in one call (same inputs, so the same commit).
    let r2 = c
        .create_commit_from_patch(request(target(&dest_url, true)))
        .await
        .unwrap()
        .into_inner();
    assert!(r2.pushed);
    assert_eq!(r2.commit_oid, r.commit_oid);
    assert_eq!(
        git_in(dest.path(), &["rev-parse", "refs/heads/batch-changes/foo"]).trim(),
        r.commit_oid
    );

    // Push alone, by local ref name, to another branch; empty URL = the repo's own remote.
    let p = c
        .push(pb::PushRequest {
            repo: repo.clone(),
            source: "refs/heads/batch-changes/foo".into(),
            destination_ref: "refs/heads/copy".into(),
            target: target(&dest_url, false),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.commit_oid, r.commit_oid);
    c.push(pb::PushRequest {
        repo: repo.clone(),
        source: r.commit_oid.clone(),
        destination_ref: "batch-changes/own".into(),
        target: target("", false),
    })
    .await
    .unwrap();
    assert_eq!(
        src.git(&["rev-parse", "refs/heads/batch-changes/own"]),
        r.commit_oid
    );

    // Typed errors.
    let kind = |s: &tonic::Status| ErrorKind::of(s);
    let mut bad = request(None);
    bad.base_commit_oid = git_in(src.path(), &["rev-list", "--max-parents=0", "HEAD"])
        .trim()
        .to_owned();
    bad.patch = diff_of(&src, &[], |s| s.write("src/main.rs", "x\n")).into();
    let err = c.create_commit_from_patch(bad).await.unwrap_err();
    assert_eq!(kind(&err), Some(ErrorKind::PatchFailed), "{err:?}");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("src/main.rs"), "{}", err.message());

    let mut no_author = request(None);
    no_author.author = None;
    let err = c.create_commit_from_patch(no_author).await.unwrap_err();
    assert_eq!(kind(&err), Some(ErrorKind::InvalidArgument), "{err:?}");

    // Apply without committing: the changed files and their contents, or TOO_LARGE.
    let apply = |max_files| pb::ApplyPatchRequest {
        repo: repo.clone(),
        base_commit_oid: base.clone(),
        patch: patch.clone().into(),
        max_files,
        ..Default::default()
    };
    let a = c.apply_patch(apply(0)).await.unwrap().into_inner();
    assert_eq!(a.files.len(), 1);
    assert_eq!(a.files[0].action(), pb::changed_file::Action::Update);
    assert_eq!(a.files[0].kind(), pb::changed_file::Kind::Regular);
    assert_eq!(a.files[0].path, "src/main.rs");
    assert_eq!(a.files[0].content, b"fn main() { batch() }\n");
    assert!(a.files[0].content_changed);
    assert_eq!(
        a.tree_oid,
        git_in(&dir, &["rev-parse", &format!("{}^{{tree}}", r.commit_oid)]).trim()
    );
    let mut small = apply(0);
    small.max_content_bytes = 3;
    let err = c.apply_patch(small).await.unwrap_err();
    assert_eq!(kind(&err), Some(ErrorKind::TooLarge), "{err:?}");

    // Pushes only go to the repo's code host, and only over http(s) or opted-in local paths.
    for url in [
        "https://elsewhere.example.com/g/p.git",
        "ssh://git@x/y",
        "ext::sh",
    ] {
        let err = c
            .create_commit_from_patch(request(target(url, true)))
            .await
            .unwrap_err();
        assert_eq!(
            kind(&err),
            Some(ErrorKind::InvalidArgument),
            "{url}: {err:?}"
        );
    }

    // Non-fast-forward over gRPC.
    let other = c
        .create_commit_from_patch(pb::CreateCommitFromPatchRequest {
            target_ref: "refs/heads/sibling".into(),
            patch: diff_of(&src, &[], |s| s.write("z.txt", "z\n")).into(),
            ..request(None)
        })
        .await
        .unwrap()
        .into_inner();
    let err = c
        .push(pb::PushRequest {
            repo: repo.clone(),
            source: other.commit_oid,
            destination_ref: "refs/heads/batch-changes/foo".into(),
            target: target(&dest_url, false),
        })
        .await
        .unwrap_err();
    assert_eq!(kind(&err), Some(ErrorKind::NonFastForward), "{err:?}");
    assert_eq!(err.code(), tonic::Code::Aborted);
    let err = c
        .push(pb::PushRequest {
            repo: repo.clone(),
            source: "refs/heads/nope".into(),
            destination_ref: "x".into(),
            target: target(&dest_url, false),
        })
        .await
        .unwrap_err();
    assert_eq!(kind(&err), Some(ErrorKind::RevisionNotFound), "{err:?}");
}
