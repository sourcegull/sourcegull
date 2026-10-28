//! Git fixtures and an in-process repo node (feature `git`).
//!
//! [`SourceRepo`] builds a working repository with the git CLI using fixed identities and
//! dates, so oids and timestamps are deterministic. [`RepoNode`] serves a
//! `sourcegull.repo.v1.RepoService` and the `SearchService` (with a search index) over a temporary
//! [`GitStore`] on a local port, resolving every repo to one source repository, and hands out
//! routed [`RepoClients`].

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use sourcegull_gitstore::{
    GitStore, GitStoreConfig, Remote, RemoteAuth, RepoNodeService, ServiceConfig,
};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use sourcegull_types::RepoId;

/// A working repository acting as the code host side.
pub struct SourceRepo {
    pub dir: tempfile::TempDir,
    tick: std::cell::Cell<i64>,
}

impl Default for SourceRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceRepo {
    /// An empty repository with branch `main`.
    pub fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            tick: std::cell::Cell::new(0),
        };
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "commit.gpgsign", "false"]);
        s
    }

    /// The work tree.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The repository as a clone/fetch remote.
    pub fn remote(&self) -> Remote {
        Remote::new(&self.path().display().to_string(), RemoteAuth::None)
    }

    /// Runs git in the work tree (each call advances the fixed clock by a minute) and returns
    /// trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let t = self.tick.get() + 1;
        self.tick.set(t);
        let date = format!("{} +0000", 1_700_000_000 + t * 60);
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .env("GIT_AUTHOR_NAME", "Ann Author")
            .env("GIT_AUTHOR_EMAIL", "ann@example.com")
            .env("GIT_COMMITTER_NAME", "Cid Committer")
            .env("GIT_COMMITTER_EMAIL", "cid@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf-8 git output")
            .trim()
            .to_owned()
    }

    /// Writes a file in the work tree.
    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let p = self.path().join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, content).expect("write");
    }

    /// Writes files, commits everything and returns the commit oid.
    pub fn commit(&self, msg: &str, files: &[(&str, &str)]) -> String {
        for (p, c) in files {
            self.write(p, c);
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.head()
    }

    /// The current HEAD commit.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

/// Oids of the commits in [`browse_repo`].
#[derive(Debug, Clone)]
pub struct BrowseRepoOids {
    pub c1: String,
    pub c2: String,
    pub f1: String,
    pub c3: String,
}

/// A small repository for code browsing tests:
///
/// ```text
/// c1 "c1: initial"      README.md, src/main.rs, src/lib.rs, docs/guide.md, img.png (binary)
/// c2 "c2: greet\n\nLonger body."   edit src/main.rs
/// feature: f1 "f1: subtract"   edit src/lib.rs
/// c3 "c3: docs"         add docs/more.md            (main; tag v1.0 annotated on c3, light on c1)
/// ```
pub fn browse_repo() -> (SourceRepo, BrowseRepoOids) {
    let s = SourceRepo::new();
    s.write("img.png", [0x89u8, b'P', b'N', b'G', 0, 0, 1, 2]);
    let c1 = s.commit(
        "c1: initial",
        &[
            ("README.md", "# Demo\n\nHello.\n"),
            ("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n"),
            (
                "src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            ),
            ("docs/guide.md", "line 1\nline 2\nline 3\n"),
        ],
    );
    s.git(&["tag", "light", "HEAD"]);
    let c2 = s.commit(
        "c2: greet\n\nLonger body.",
        &[(
            "src/main.rs",
            "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n",
        )],
    );
    s.git(&["checkout", "-q", "-b", "feature"]);
    let f1 = s.commit(
        "f1: subtract",
        &[(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )],
    );
    s.git(&["checkout", "-q", "main"]);
    let c3 = s.commit("c3: docs", &[("docs/more.md", "more\n")]);
    s.git(&["tag", "-a", "v1.0", "-m", "release 1.0"]);
    (s, BrowseRepoOids { c1, c2, f1, c3 })
}

/// Records `Reindex` requests; a repo counts as queued until [`ReindexRequests::take`].
#[derive(Debug, Default)]
pub struct ReindexRequests(std::sync::Mutex<Vec<RepoId>>);

impl ReindexRequests {
    /// The repos asked for so far, oldest first; clears the list.
    pub fn take(&self) -> Vec<RepoId> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl sourcegull_search_exec::IndexControl for ReindexRequests {
    fn queued(&self, repo: RepoId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&repo)
    }

    fn reindex(&self, repo: RepoId) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(repo);
    }
}

/// An in-process repo node serving one source repository for every repo id.
pub struct RepoNode {
    /// Routed clients pointing at the node.
    pub clients: Arc<RepoClients>,
    /// The node's address (`http://127.0.0.1:port`).
    pub addr: String,
    /// The node's search index (empty until [`RepoNode::index_repo`]).
    pub index: Arc<sourcegull_search_exec::ShardIndex>,
    /// Repos the node was asked to reindex (`Reindex` RPC); nothing is rebuilt.
    pub reindex_requests: Arc<ReindexRequests>,
    store: Arc<GitStore>,
    _store_dir: tempfile::TempDir,
}

impl RepoNode {
    /// Starts a node on a free local port whose resolver returns `source` for any repo.
    pub async fn start(source: Remote) -> Self {
        Self::start_with(move |_: &sourcegull_gitstore::RepoRef| Ok(source.clone())).await
    }

    /// Starts a node whose repos clone from the remotes `resolver` returns (e.g. one source
    /// repository per repo name).
    pub async fn start_with(
        resolver: impl Fn(&sourcegull_gitstore::RepoRef) -> sourcegull_gitstore::Result<Remote>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(GitStore::open(GitStoreConfig::new(dir.path())).expect("open store"));
        let (index, _) = sourcegull_search_exec::ShardIndex::open(
            store.layout().index_dir(),
            Default::default(),
        )
        .expect("open index");
        let index = Arc::new(index);
        let reindex_requests = Arc::new(ReindexRequests::default());
        let search = sourcegull_search_exec::SearchNodeService::new(store.clone())
            .with_index(index.clone())
            .with_control(reindex_requests.clone());
        // Remotes are local paths here, so pushes (bulk changes) may go to them too.
        let config = ServiceConfig {
            allow_local_push: true,
            ..ServiceConfig::default()
        };
        let svc = RepoNodeService::new(store.clone(), resolver, config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(svc.into_server())
                .add_service(search.into_server())
                .serve_with_incoming(sourcegull_rpc::incoming(listener)),
        );
        let clients = Arc::new(RepoClients::new(
            Router::new([addr.clone()]),
            ClientConfig::default(),
        ));
        Self {
            clients,
            addr,
            index,
            reindex_requests,
            store,
            _store_dir: dir,
        }
    }

    /// Indexes repo `id` at HEAD plus `extra` branches, as the repo role's indexer does.
    pub fn index_repo(&self, id: RepoId, name: &str, extra: &[&str]) {
        use sourcegull_search_exec::index::BuildRequest;
        let git = self.store.repo(id).expect("cloned repo");
        let extra: Vec<String> = extra.iter().map(|b| (*b).to_owned()).collect();
        let req = BuildRequest {
            repo: id,
            name: name.to_owned(),
            git_dir: self.store.layout().repo_dir(id),
            branches: sourcegull_search_exec::branches_to_index(&git, &extra).expect("branches"),
            ranks: None,
            always_index: Default::default(),
        };
        sourcegull_search_exec::update_index(&*self.index, &req).expect("index");
    }

    /// Clones repo `id` (named `name`) onto the node.
    pub async fn clone_repo(&self, id: RepoId, name: &str) {
        self.clients
            .for_repo(id)
            .expect("client")
            .ensure_repo(pb::EnsureRepoRequest {
                repo: Some(pb::RepoRef {
                    id: id.get(),
                    name: name.to_owned(),
                }),
                fetch_if_cloned: false,
            })
            .await
            .expect("clone");
    }

    /// Fetches repo `id` (named `name`) from its source again.
    pub async fn fetch_repo(&self, id: RepoId, name: &str) {
        self.clients
            .for_repo(id)
            .expect("client")
            .ensure_repo(pb::EnsureRepoRequest {
                repo: Some(pb::RepoRef {
                    id: id.get(),
                    name: name.to_owned(),
                }),
                fetch_if_cloned: true,
            })
            .await
            .expect("fetch");
    }
}
