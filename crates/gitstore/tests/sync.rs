//! Clone, fetch, ref updates, status, janitor and credential handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::common::{SourceRepo, git_in, history_repo, store};
use sourcegull_gitstore::janitor::{self, JanitorTask};
use sourcegull_gitstore::refs::RefUpdate;
use sourcegull_gitstore::{
    GitError, GitStore, GitStoreConfig, ObjectId, Remote, RemoteAuth, RemoteHint, SyncHooks,
};
use sourcegull_types::{RemoteUnavailable, RepoId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn oid(s: &str) -> ObjectId {
    ObjectId::from_hex(s.as_bytes()).unwrap()
}

#[tokio::test]
async fn clone_reports_all_refs_and_sets_head() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(7);
    assert!(!store.is_cloned(repo));
    assert!(matches!(store.repo(repo), Err(GitError::RepoNotFound(_))));

    let out = store
        .ensure(repo, &src.remote(), false)
        .await
        .unwrap()
        .unwrap();
    assert!(out.cloned);
    assert_eq!(out.head_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(out.head_oid, Some(oid(&src.head())));
    assert!(out.size_bytes > 0);
    let names: Vec<&str> = out.ref_updates.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "refs/heads/feature",
            "refs/heads/main",
            "refs/tags/light",
            "refs/tags/v1.0"
        ]
    );
    assert!(
        out.ref_updates
            .iter()
            .all(|u| u.old.is_none() && u.new.is_some())
    );

    // On disk: bare, sharded by id, no temp dirs left, commit-graph written,
    // and the stored remote URL has no credentials.
    let dir = store.repo_dir(repo);
    assert!(dir.ends_with("repos/07/7.git"));
    assert_eq!(
        git_in(&dir, &["rev-parse", "--is-bare-repository"]).trim(),
        "true"
    );
    assert_eq!(
        std::fs::read_dir(store.layout().tmp_dir()).unwrap().count(),
        1
    ); // askpass.sh
    assert!(janitor::fragmentation(&dir).unwrap().has_commit_graph);

    // A second ensure is a no-op.
    assert!(
        store
            .ensure(repo, &src.remote(), false)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn fetch_detects_created_updated_and_deleted_refs() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(1);
    store.ensure(repo, &src.remote(), false).await.unwrap();

    // Nothing changed.
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert!(!out.cloned);
    assert!(out.ref_updates.is_empty());

    let old_main = src.head();
    let old_feature = src.git(&["rev-parse", "feature"]);
    let new_main = src.commit("c5", &[("new.txt", "x\n")]);
    src.git(&["branch", "-D", "feature"]);
    src.git(&["branch", "topic"]);
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(
        out.ref_updates,
        vec![
            RefUpdate {
                name: "refs/heads/feature".into(),
                old: Some(oid(&old_feature)),
                new: None,
            },
            RefUpdate {
                name: "refs/heads/main".into(),
                old: Some(oid(&old_main)),
                new: Some(oid(&new_main)),
            },
            RefUpdate {
                name: "refs/heads/topic".into(),
                old: None,
                new: Some(oid(&new_main)),
            },
        ]
    );
    assert_eq!(out.head_oid, Some(oid(&new_main)));

    // Force-push (history rewrite) is a plain old→new update.
    src.git(&["reset", "-q", "--hard", "HEAD~1"]);
    let rewritten = src.commit("c5 rewritten", &[("other.txt", "y\n")]);
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(out.ref_updates.len(), 1);
    assert_eq!(out.ref_updates[0].old, Some(oid(&new_main)));
    assert_eq!(out.ref_updates[0].new, Some(oid(&rewritten)));

    // The read cache sees the new commit.
    let r = store.repo(repo).unwrap();
    assert_eq!(
        sourcegull_gitstore::read::resolve_revision(&r, "main").unwrap(),
        oid(&rewritten)
    );
}

#[tokio::test]
async fn head_follows_default_branch_rename() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(2);
    store.ensure(repo, &src.remote(), false).await.unwrap();
    src.git(&["branch", "-m", "main", "trunk"]);
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(out.head_ref.as_deref(), Some("refs/heads/trunk"));
    assert_eq!(out.head_oid, Some(oid(&src.head())));
}

#[tokio::test]
async fn empty_remote_clones_and_later_fetch_picks_up_first_push() {
    let src = SourceRepo::new();
    let (_d, store) = store();
    let repo = RepoId(3);
    let out = store
        .ensure(repo, &src.remote(), false)
        .await
        .unwrap()
        .unwrap();
    assert!(out.ref_updates.is_empty());
    assert_eq!(out.head_oid, None);
    let status = store.status(repo, true).await.unwrap();
    assert!(status.cloned);
    assert_eq!(status.head_oid, None);

    let c = src.commit("first", &[("a", "a\n")]);
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(out.ref_updates.len(), 1);
    assert_eq!(out.head_oid, Some(oid(&c)));
}

/// A `git` wrapper whose fetches fail the way GitLab answers for a project whose repository
/// was never created; everything else runs the real git.
fn gitlab_missing_repo_git(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("git-missing-repo.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor a; do\n  if [ \"$a\" = fetch ]; then\n    \
         echo \"remote: A repository for this project does not exist yet.\" >&2\n    \
         echo \"fatal: repository 'https://gitlab.example.com/g/p.git/' not found\" >&2\n    \
         exit 128\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn missing_remote_repository_clones_empty_and_later_fetch_fills_it() {
    let src = history_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut config = GitStoreConfig::new(dir.path().join("store"));
    config.git_bin = gitlab_missing_repo_git(dir.path());
    let store = GitStore::open(config).unwrap();
    let repo = RepoId(31);

    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert!(out.cloned);
    assert_eq!(out.remote_unavailable, Some(RemoteUnavailable::Missing));
    assert!(out.ref_updates.is_empty());
    assert_eq!(out.head_oid, None);
    assert!(store.is_cloned(repo));
    // A later fetch while the repository is still missing is not an error either.
    let out = store.fetch(repo, &src.remote()).await.unwrap();
    assert!(!out.cloned);
    assert_eq!(out.remote_unavailable, Some(RemoteUnavailable::Missing));

    // Once the repository exists (first push), the normal fetch fills the empty copy.
    let real = GitStore::open(GitStoreConfig::new(dir.path().join("store"))).unwrap();
    let out = real.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(out.remote_unavailable, None);
    assert_eq!(out.ref_updates.len(), 4);
    assert_eq!(out.head_oid, Some(oid(&src.head())));
}

/// A `git` wrapper whose fetches fail with GitLab's "not allowed to download code" (repository
/// feature disabled, or the token below Reporter on a private project).
fn gitlab_denied_git(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("git-denied.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor a; do\n  if [ \"$a\" = fetch ]; then\n    \
         echo \"remote: You are not allowed to download code from this project.\" >&2\n    \
         echo \"fatal: unable to access 'https://gitlab.example.com/g/p.git/': The requested URL returned error: 403\" >&2\n    \
         exit 128\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn disabled_or_unreadable_repository_clones_empty() {
    let src = history_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut config = GitStoreConfig::new(dir.path().join("store"));
    config.git_bin = gitlab_denied_git(dir.path());
    let store = GitStore::open(config).unwrap();

    // The listing said the repository feature is disabled.
    let hooks = SyncHooks {
        remote_hint: RemoteHint {
            repository_disabled: true,
            ..RemoteHint::default()
        },
        ..SyncHooks::default()
    };
    let out = store
        .sync_with(RepoId(41), &src.remote(), hooks)
        .await
        .unwrap();
    assert!(out.cloned);
    assert_eq!(out.remote_unavailable, Some(RemoteUnavailable::Disabled));
    assert_eq!(out.head_oid, None);
    assert!(store.is_cloned(RepoId(41)));

    // No hint: the token is blamed, still not an error.
    let out = store.fetch(RepoId(42), &src.remote()).await.unwrap();
    assert_eq!(
        out.remote_unavailable,
        Some(RemoteUnavailable::NoCodeAccess)
    );
    let out = store.fetch(RepoId(42), &src.remote()).await.unwrap();
    assert!(!out.cloned);
    assert_eq!(
        out.remote_unavailable,
        Some(RemoteUnavailable::NoCodeAccess)
    );
}

fn fetch_error(stderr: &str) -> GitError {
    GitError::Command {
        op: "fetch".into(),
        status: "exit status: 128".into(),
        stderr: stderr.into(),
    }
}

#[test]
fn classifies_unavailable_remote_repositories() {
    let none = RemoteHint::default();
    let empty = RemoteHint {
        empty: true,
        ..RemoteHint::default()
    };
    let disabled = RemoteHint {
        repository_disabled: true,
        ..RemoteHint::default()
    };
    let guest = RemoteHint {
        below_reporter: true,
        ..RemoteHint::default()
    };
    let missing =
        fetch_error("remote: A repository for this project does not exist yet.\nfatal: ...");
    assert_eq!(
        missing.remote_unavailable(&none),
        Some(RemoteUnavailable::Missing)
    );
    let not_found = fetch_error("fatal: repository 'https://h/g/p.git/' not found");
    assert_eq!(not_found.remote_unavailable(&none), None);
    assert_eq!(
        not_found.remote_unavailable(&empty),
        Some(RemoteUnavailable::Missing)
    );
    assert_eq!(
        not_found.remote_unavailable(&disabled),
        Some(RemoteUnavailable::Disabled)
    );
    assert_eq!(
        not_found.remote_unavailable(&guest),
        Some(RemoteUnavailable::NoCodeAccess)
    );
    // GitLab says the same for a disabled repository feature and for too little access; the
    // listing's `repositoryAccessLevel` tells them apart, and without it the token is blamed.
    let denied = fetch_error(
        "remote: You are not allowed to download code from this project.\n\
         fatal: unable to access 'https://gitlab.lo.example/lentochka/qa/archived.git/': \
         The requested URL returned error: 403",
    );
    assert_eq!(
        denied.remote_unavailable(&none),
        Some(RemoteUnavailable::NoCodeAccess)
    );
    assert_eq!(
        denied.remote_unavailable(&guest),
        Some(RemoteUnavailable::NoCodeAccess)
    );
    assert_eq!(
        denied.remote_unavailable(&disabled),
        Some(RemoteUnavailable::Disabled)
    );
    // Other failures stay errors.
    let auth = fetch_error("fatal: Authentication failed for 'https://h/g/p.git/'");
    assert_eq!(auth.remote_unavailable(&guest), None);
    assert_eq!(
        GitError::Cancelled("fetch".into()).remote_unavailable(&disabled),
        None
    );
    let other_op = GitError::Command {
        op: "gc".into(),
        status: "exit status: 128".into(),
        stderr: "remote: You are not allowed to download code from this project.".into(),
    };
    assert_eq!(other_op.remote_unavailable(&none), None);
}

#[tokio::test]
async fn reclone_swaps_atomically_and_reports_changes_against_old_copy() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(4);
    store.ensure(repo, &src.remote(), false).await.unwrap();
    let old = src.head();
    let new = src.commit("c5", &[("z", "z\n")]);
    let out = store.clone_repo(repo, &src.remote()).await.unwrap();
    assert!(out.cloned);
    assert_eq!(
        out.ref_updates,
        vec![RefUpdate {
            name: "refs/heads/main".into(),
            old: Some(oid(&old)),
            new: Some(oid(&new)),
        }]
    );
    // Temp dirs (including the replaced copy) are gone.
    assert_eq!(
        std::fs::read_dir(store.layout().tmp_dir()).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn failed_clone_leaves_nothing_behind() {
    let (_d, store) = store();
    let repo = RepoId(5);
    let remote = Remote::new("/nonexistent/repo.git", RemoteAuth::None);
    let err = store.ensure(repo, &remote, false).await.unwrap_err();
    assert!(matches!(err, GitError::Command { .. }), "{err}");
    assert!(!store.is_cloned(repo));
    assert!(!store.is_cloning(repo));
    assert_eq!(
        std::fs::read_dir(store.layout().tmp_dir()).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn concurrent_ensures_clone_once() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(6);
    let remote = src.remote();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (store, remote) = (store.clone(), remote.clone());
        tasks.push(tokio::spawn(async move {
            store.ensure(repo, &remote, false).await.unwrap()
        }));
    }
    let mut clones = 0;
    for t in tasks {
        if t.await.unwrap().is_some_and(|o| o.cloned) {
            clones += 1;
        }
    }
    assert_eq!(clones, 1);
}

#[tokio::test]
async fn status_delete_and_janitor() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(8);
    store.ensure(repo, &src.remote(), false).await.unwrap();
    let s = store.status(repo, true).await.unwrap();
    assert!(s.cloned && !s.clone_in_progress);
    assert_eq!(s.head_ref.as_deref(), Some("refs/heads/main"));
    assert!(s.size_bytes > 0);

    for task in [
        JanitorTask::Maintenance,
        JanitorTask::Gc,
        JanitorTask::Repack,
        JanitorTask::Prune,
        JanitorTask::CommitGraph,
        JanitorTask::Fsck,
    ] {
        let out = store.run_janitor(repo, task).await.unwrap();
        assert!(!out.corrupt, "{task:?}: {}", out.output);
        assert!(out.size_bytes_after > 0);
    }
    // Small fetches are unpacked to loose objects; after gc everything is packed.
    assert_eq!(
        store.status(repo, false).await.unwrap().fragmentation.packs,
        1
    );
    // Reads still work after repacking.
    let r = store.repo(repo).unwrap();
    sourcegull_gitstore::read::resolve_revision(&r, "v1.0").unwrap();
    assert!(
        store
            .janitor_sweep(&Default::default())
            .await
            .unwrap()
            .is_empty()
    );

    assert!(store.delete(repo).await.unwrap());
    assert!(!store.is_cloned(repo));
    assert!(!store.delete(repo).await.unwrap());
    assert!(matches!(
        store.run_janitor(repo, JanitorTask::Gc).await,
        Err(GitError::RepoNotFound(_))
    ));
    assert!(!store.status(repo, false).await.unwrap().cloned);
}

#[tokio::test]
async fn fsck_detects_corruption() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(9);
    store.ensure(repo, &src.remote(), false).await.unwrap();
    let dir = store.repo_dir(repo);
    // Unpack everything to loose objects, then delete one blob.
    let packs: Vec<_> = std::fs::read_dir(dir.join("objects/pack"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    for p in &packs {
        if p.extension().is_some_and(|e| e == "pack") {
            let data = std::fs::read(p).unwrap();
            let mut child = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .arg("unpack-objects")
                .arg("-q")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&data).unwrap();
            assert!(child.wait().unwrap().success());
        }
    }
    for p in packs {
        std::fs::remove_file(p).unwrap();
    }
    let blob = git_in(&dir, &["rev-parse", "HEAD:README.md"]);
    let blob = blob.trim();
    std::fs::remove_file(dir.join("objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    let out = store.run_janitor(repo, JanitorTask::Fsck).await.unwrap();
    assert!(out.corrupt, "{}", out.output);
}

#[tokio::test]
async fn clone_concurrency_is_bounded() {
    let src = history_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = GitStoreConfig::new(dir.path());
    cfg.max_concurrent_clones = 1;
    let store = Arc::new(GitStore::open(cfg).unwrap());
    let mut tasks = Vec::new();
    for id in 10..14 {
        let (store, remote) = (store.clone(), src.remote());
        tasks.push(tokio::spawn(async move {
            store.ensure(RepoId(id), &remote, false).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(store.layout().list_repos().unwrap().len(), 4);
}

/// A fake HTTPS-less "GitLab" that demands basic auth, records the
/// Authorization header git sends after the askpass helper answered, and then
/// fails. Checks that credentials reach git only through the askpass helper,
/// never through argv/URL, and that error output is redacted.
#[tokio::test]
async fn credentials_go_through_askpass_and_are_redacted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_auth = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = seen_auth.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    tokio::spawn(async move {
        while !stop2.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let auth = req
                .lines()
                .find_map(|l| l.strip_prefix("Authorization: "))
                .map(str::to_owned);
            let resp = match auth {
                None => "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                Some(a) => {
                    seen.lock().unwrap().push(a);
                    // Echo something token-like in the body to check redaction.
                    "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                }
            };
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    let (_d, store) = store();
    let token = "glpat-SuperSecret123";
    let remote = Remote::new(
        &format!("http://{addr}/group/project.git"),
        RemoteAuth::gitlab_token(token),
    );
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        store.ensure(RepoId(20), &remote, false),
    )
    .await
    .unwrap()
    .unwrap_err();
    stop.store(true, Ordering::Relaxed);
    let msg = err.to_string();
    assert!(!msg.contains(token), "{msg}");

    let seen = seen_auth.lock().unwrap();
    assert!(!seen.is_empty(), "git never retried with credentials");
    // base64("oauth2:glpat-SuperSecret123")
    assert_eq!(seen[0], "Basic b2F1dGgyOmdscGF0LVN1cGVyU2VjcmV0MTIz");

    // URL-embedded credentials are moved out of the URL as well.
    let r = Remote::new(
        &format!("http://u:{token}@{addr}/g/p.git"),
        RemoteAuth::None,
    );
    assert!(!r.url().contains(token));
}

#[tokio::test]
async fn clone_and_fetch_maintain_the_commit_log() {
    let src = history_repo();
    let (_d, store) = store();
    let repo = RepoId(1);
    store.ensure(repo, &src.remote(), false).await.unwrap();
    let count = |rev: &str| {
        src.git(&["rev-list", "--count", rev])
            .parse::<usize>()
            .unwrap()
    };
    let log = store
        .commit_log(repo)
        .unwrap()
        .expect("clone writes the log");
    assert_eq!(log.len(), count("--all"));
    let head = oid(&src.head());
    let walked: Vec<ObjectId> = log
        .walk(log.lookup(&head).unwrap())
        .map(|c| c.oid())
        .collect();
    let expected: Vec<ObjectId> = src
        .git(&["rev-list", "--date-order", "HEAD"])
        .lines()
        .map(oid)
        .collect();
    assert_eq!(walked, expected);

    let new_main = oid(&src.commit("c5", &[("new.txt", "x\n")]));
    store.fetch(repo, &src.remote()).await.unwrap();
    let log = store.commit_log(repo).unwrap().unwrap();
    assert_eq!(log.len(), count("--all"));
    assert!(log.lookup(&new_main).is_some());

    // Repos cloned before logs existed get one on their next fetch.
    std::fs::remove_file(sourcegull_gitstore::commitlog::log_path(
        &store.repo_dir(repo),
    ))
    .unwrap();
    assert!(store.commit_log(repo).unwrap().is_none());
    store.fetch(repo, &src.remote()).await.unwrap();
    assert_eq!(
        store.commit_log(repo).unwrap().unwrap().len(),
        count("--all")
    );
}
