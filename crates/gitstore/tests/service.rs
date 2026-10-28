//! The repo node gRPC service end to end: a tonic server over a GitStore,
//! called through the routed client from sourcegull-rpc.

use std::collections::HashMap;

use crate::common::{SourceRepo, history_repo, store};
use sourcegull_gitstore::{GitError, Remote, RepoNodeService, RepoRef, ServiceConfig};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ClientConfig, ErrorKind, RepoClients, Router};
use sourcegull_types::RepoId;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;

struct Node {
    clients: RepoClients,
    _store_dir: tempfile::TempDir,
    _sources: Vec<SourceRepo>,
}

/// Starts a node serving repo 1 (history fixture) and repo 2 (empty source);
/// repo 3 has no remote.
async fn start() -> Node {
    let (dir, store) = store();
    let src = history_repo();
    let empty = SourceRepo::new();
    let remotes: HashMap<i32, Remote> = [(1, src.remote()), (2, empty.remote())].into();
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
            tree_batch: 2,
            log_batch: 2,
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
    let clients = RepoClients::new(
        Router::new([format!("http://{addr}")]),
        ClientConfig::default(),
    );
    Node {
        clients,
        _store_dir: dir,
        _sources: vec![src, empty],
    }
}

fn repo(id: i32) -> Option<pb::RepoRef> {
    Some(pb::RepoRef {
        id,
        name: format!("gitlab.example.com/g/r{id}"),
    })
}

#[tokio::test]
async fn control_and_read_rpcs() {
    let node = start().await;
    let mut c = node.clients.for_repo(RepoId(1)).unwrap();

    // Reads before cloning report RepoNotFound.
    let err = c
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: repo(1),
            rev: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::RepoNotFound));

    let ensured = c
        .ensure_repo(pb::EnsureRepoRequest {
            repo: repo(1),
            fetch_if_cloned: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!ensured.already_cloned);
    let result = ensured.result.unwrap();
    assert!(result.cloned);
    assert_eq!(result.ref_updates.len(), 4);
    assert_eq!(result.head_ref, "refs/heads/main");
    let again = c
        .ensure_repo(pb::EnsureRepoRequest {
            repo: repo(1),
            fetch_if_cloned: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(again.already_cloned && again.result.is_none());
    let fetched = c
        .fetch_repo(pb::FetchRepoRequest { repo: repo(1) })
        .await
        .unwrap()
        .into_inner()
        .result
        .unwrap();
    assert!(!fetched.cloned && fetched.ref_updates.is_empty());

    let head = c
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: repo(1),
            rev: "v1.0".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .commit_oid;
    assert_eq!(head, result.head_oid);
    let err = c
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: repo(1),
            rev: "nope".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::RevisionNotFound));
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Streaming tree in batches of 2.
    let mut stream = c
        .list_tree(pb::ListTreeRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            recursive: true,
            include_sizes: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut entries = Vec::new();
    let mut messages = 0;
    while let Some(m) = stream.next().await {
        let m = m.unwrap();
        messages += 1;
        entries.extend(m.entries);
    }
    assert_eq!(entries.len(), 6);
    assert_eq!(messages, 3);
    assert!(
        entries
            .iter()
            .any(|e| e.path == "src/main.rs" && e.size.is_some())
    );

    let file = c
        .read_file(pb::ReadFileRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            path: "README.md".into(),
            max_bytes: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(&file.content[..], b"# Demo\n");
    let err = c
        .read_file(pb::ReadFileRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            path: "missing".into(),
            max_bytes: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::PathNotFound));
    let stat = c
        .stat(pb::StatRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            path: "src".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(stat.kind, pb::EntryKind::Directory as i32);

    // Log streams with has_more on the last message.
    let mut stream = c
        .commit_log(pb::CommitLogRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            limit: 3,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut commits = Vec::new();
    let mut has_more = false;
    while let Some(m) = stream.next().await {
        let m = m.unwrap();
        commits.extend(m.commits);
        has_more = m.has_more;
    }
    assert_eq!(commits.len(), 3);
    assert!(has_more);
    assert_eq!(commits[0].oid, head);
    let commit = c
        .get_commit(pb::GetCommitRequest {
            repo: repo(1),
            commit_oid: head.clone(),
        })
        .await
        .unwrap()
        .into_inner()
        .commit
        .unwrap();
    assert!(commit.message.starts_with("c4: run"));

    let refs = c
        .list_refs(pb::ListRefsRequest {
            repo: repo(1),
            branches: true,
            tags: true,
            prefix: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(refs.refs.len(), 4);
    assert_eq!(refs.head_ref, "refs/heads/main");

    // Diff / compare / merge base / ancestry.
    let light = c
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: repo(1),
            rev: "light".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .commit_oid;
    let mut stream = c
        .diff(pb::DiffRequest {
            repo: repo(1),
            old_commit_oid: light.clone(),
            new_commit_oid: head.clone(),
            detect_renames: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut files = Vec::new();
    while let Some(m) = stream.next().await {
        files.extend(m.unwrap().files);
    }
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].status, pb::ChangeStatus::Renamed as i32);
    assert!(!files[2].hunks.is_empty());
    let cmp = c
        .compare(pb::CompareRequest {
            repo: repo(1),
            base_commit_oid: light.clone(),
            head_commit_oid: head.clone(),
            max_commits: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cmp.merge_base_oid, light);
    assert_eq!(cmp.commits.len(), 5);
    let mb = c
        .merge_base(pb::MergeBaseRequest {
            repo: repo(1),
            a: light.clone(),
            b: head.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mb.oid, light);
    let anc = c
        .is_ancestor(pb::IsAncestorRequest {
            repo: repo(1),
            ancestor: head.clone(),
            descendant: light.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!anc.is_ancestor);
    let err = c
        .merge_base(pb::MergeBaseRequest {
            repo: repo(1),
            a: "zz".into(),
            b: head.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::InvalidArgument));

    // Blame and archive stream.
    let mut stream = c
        .blame(pb::BlameRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            path: "src/main.rs".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut lines = 0;
    while let Some(m) = stream.next().await {
        lines += m.unwrap().hunks.iter().map(|h| h.line_count).sum::<u32>();
    }
    assert_eq!(lines, 6);
    let err = c
        .blame(pb::BlameRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            path: "src".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::WrongEntryType));
    let mut stream = c
        .archive(pb::ArchiveRequest {
            repo: repo(1),
            commit_oid: head.clone(),
            format: pb::ArchiveFormat::Zip as i32,
            paths: vec![],
        })
        .await
        .unwrap()
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(m) = stream.next().await {
        bytes.extend_from_slice(&m.unwrap().chunk);
    }
    assert_eq!(&bytes[..2], b"PK");

    // Status, janitor, delete.
    let status = c
        .repo_status(pb::RepoStatusRequest {
            repo: repo(1),
            include_size: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(status.cloned && status.size_bytes > 0);
    let jan = c
        .run_janitor(pb::RunJanitorRequest {
            repo: repo(1),
            task: pb::JanitorTask::Fsck as i32,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!jan.corrupt);
    let err = c
        .run_janitor(pb::RunJanitorRequest {
            repo: repo(1),
            task: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::InvalidArgument));
    let del = c
        .delete_repo(pb::DeleteRepoRequest { repo: repo(1) })
        .await
        .unwrap()
        .into_inner();
    assert!(del.existed);
    let status = c
        .repo_status(pb::RepoStatusRequest {
            repo: repo(1),
            include_size: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!status.cloned);
}

#[tokio::test]
async fn resolver_errors_and_bad_requests() {
    let node = start().await;
    let mut c = node.clients.for_repo(RepoId(3)).unwrap();
    let err = c
        .fetch_repo(pb::FetchRepoRequest { repo: repo(3) })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::InvalidArgument));
    let err = c
        .fetch_repo(pb::FetchRepoRequest { repo: None })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    // An empty source clones fine and has no HEAD commit.
    let mut c2 = node.clients.for_repo(RepoId(2)).unwrap();
    let out = c2
        .clone_repo(pb::CloneRepoRequest { repo: repo(2) })
        .await
        .unwrap()
        .into_inner()
        .result
        .unwrap();
    assert!(out.cloned && out.head_oid.is_empty());
    let err = c2
        .resolve_revision(pb::ResolveRevisionRequest {
            repo: repo(2),
            rev: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(ErrorKind::of(&err), Some(ErrorKind::RevisionNotFound));
}
