//! `gull bulk preview|apply` end to end against a real server: the test
//! Postgres, an in-process repo node with one source repository per seeded repository, and the
//! public HTTP API with GraphQL, served over TCP. Steps run in the in-process `FakeRuntime`
//! (which edits the downloaded working copy as a container would) or, in
//! `real_docker_preview`, in `alpine` containers on the local Docker daemon (skipped when it is
//! not reachable; `scripts/dev test-batch-cli` mounts the host's socket).

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpecStore, ChangesetSpecStore, ListChangesetSpecs,
};
use sourcegull_batches_steps::ContainerRuntime;
use sourcegull_batches_steps::fake::{FakeOutput, FakeRuntime};
use sourcegull_cli::batch::{BatchCommand, ExecuteArgs};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;

const WEB: &str = "gitlab.example.com/acme/web";
const API: &str = "gitlab.example.com/acme/api";
const IGNORED: &str = "gitlab.example.com/acme/ignored";
const MIRROR: &str = "github.com/acme/mirror";

struct Server {
    _t: TestDb,
    _node: RepoNode,
    _sources: Vec<SourceRepo>,
    db: Db,
    port: u16,
    alice: UserId,
    token: String,
    stop: CancellationToken,
    ids: HashMap<&'static str, RepoId>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Server {
    async fn start() -> Option<Self> {
        let t = TestDb::new().await?;
        migrate::up(&t.pool).await.unwrap();
        let db = Db::from_pool(t.pool.clone());

        let mut remotes = HashMap::new();
        let mut sources = Vec::new();
        for name in [WEB, API, IGNORED, MIRROR] {
            let s = SourceRepo::new();
            let mut files = vec![("README.md", "# Demo\n"), ("src/main.go", "package main\n")];
            if name == IGNORED {
                files.push((".batchignore", ""));
            }
            s.commit(name, &files);
            if name == WEB {
                s.git(&["checkout", "-q", "-b", "dev"]);
                s.commit("dev", &[("dev.txt", "dev\n")]);
                s.git(&["checkout", "-q", "main"]);
            }
            remotes.insert(name.to_owned(), s.remote());
            sources.push(s);
        }
        let node = RepoNode::start_with(move |r| {
            Ok(remotes
                .get(&r.name)
                .unwrap_or_else(|| panic!("no source for {}", r.name))
                .clone())
        })
        .await;
        let mut ids = HashMap::new();
        for (i, name) in [WEB, API, IGNORED, MIRROR].into_iter().enumerate() {
            let service = if name == MIRROR { "github" } else { "gitlab" };
            let id: i32 = sourcegull_db::sqlx::query_scalar(
                "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
                 VALUES ($1, $2, $3, 'https://code.example.com/') RETURNING id",
            )
            .bind(name)
            .bind(i.to_string())
            .bind(service)
            .fetch_one(db.pool())
            .await
            .unwrap();
            node.clone_repo(RepoId(id), name).await;
            ids.insert(name, RepoId(id));
        }

        let config = ConfigHandle::new(1, SiteConfig::default());
        let auth = Authenticator::new(
            db.clone(),
            Arc::new(MemorySessionStore::new()),
            config.clone(),
            PasswordParams::TEST,
        );
        auth.sign_up("admin", "admin@example.com", "correct horse battery")
            .await
            .unwrap();
        let alice = db
            .users()
            .create(&sourcegull_db::NewUser {
                username: "alice".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id;
        let actor = auth
            .actor_for(alice, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let (_, token) = auth
            .create_access_token(&actor, alice, &["user:all".into()], "gull", None)
            .await
            .unwrap();

        let recorder = sourcegull_observability::build_recorder().unwrap();
        let state = AppState::new(
            db.clone(),
            config,
            sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle()),
            auth,
        )
        .with_repo_clients(node.clients.clone());
        let router = sourcegull_http_api::finish(
            sourcegull_http_api::public_routes()
                .merge(sourcegull::features::graphql_routes(&state)),
            state,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        tokio::spawn(sourcegull_http_api::serve(listener, router, stop.clone()));
        Some(Self {
            _t: t,
            _node: node,
            _sources: sources,
            db,
            port,
            alice,
            token,
            stop,
            ids,
        })
    }

    fn env(&self) -> HashMap<String, String> {
        [
            ("SRC_ENDPOINT", format!("http://127.0.0.1:{}", self.port)),
            ("SRC_ACCESS_TOKEN", self.token.clone()),
            ("GREETING", "hi from the environment".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }
}

/// `gull bulk preview|apply <args>`.
fn args(command: &str, extra: &[&str]) -> ExecuteArgs {
    let mut argv = vec!["gull", "bulk", command];
    argv.extend_from_slice(extra);
    match sourcegull_cli::Cli::try_parse_from(sourcegull_cli::args::normalize(argv))
        .unwrap()
        .command
    {
        sourcegull_cli::Command::Batch {
            command: BatchCommand::Preview(a) | BatchCommand::Apply(a),
        } => a,
        other => panic!("{other:?}"),
    }
}

fn spec(name: &str, run: &str) -> String {
    format!(
        "version: 2
name: {name}
description: Say hello
on:
  - repository: {WEB}
  - repository: {API}
  - repository: {IGNORED}
  - repository: {MIRROR}
steps:
  - run: {run}
    container: alpine:3
    env:
      - GREETING
    outputs:
      who:
        value: ${{{{ repository.name }}}}
changesetTemplate:
  title: Hello ${{{{ outputs.who }}}}
  body: ${{{{ batch_change.description }}}}
  branch: hello
  commit:
    message: Say hello
    author:
      name: Alice
      email: alice@example.com
  published: false
"
    )
}

/// A runtime that appends the step's `GREETING` to README.md, and fails in repositories whose
/// rendered script contains `fail-<repo basename>`.
fn fake_runtime() -> Arc<FakeRuntime> {
    Arc::new(FakeRuntime::new(|spec| {
        let readme = spec.workspace.join("README.md");
        let mut text = std::fs::read_to_string(&readme).unwrap();
        let repo = std::fs::read_to_string(spec.workspace.join("src/main.go")).unwrap();
        assert_eq!(repo, "package main\n");
        if spec.script.contains("fail-everything") {
            return FakeOutput::failed(3, "boom");
        }
        text.push_str(&format!("{}\n", spec.env["GREETING"]));
        std::fs::write(&readme, text).unwrap();
        FakeOutput::ok()
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_then_apply_with_the_cache() {
    let Some(server) = Server::start().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("batch.yaml");
    std::fs::write(&file, spec("hello", "echo hello >> README.md")).unwrap();
    let cache = dir.path().join("cache");
    let tmp = dir.path().join("tmp");
    let base = [
        "-f",
        file.to_str().unwrap(),
        "-cache",
        cache.to_str().unwrap(),
        "-tmp",
        tmp.to_str().unwrap(),
        "-j",
        "2",
    ];
    let env = server.env();
    let runtime = fake_runtime();

    // Preview: the GitHub mirror and the ignored repository are left out.
    let out = sourcegull_cli::batch::run::execute(
        &args("preview", &base),
        false,
        &env,
        runtime.clone() as Arc<dyn ContainerRuntime>,
    )
    .await
    .unwrap();
    assert_eq!(out.workspaces, 2);
    assert_eq!(out.changeset_specs.len(), 2);
    assert!(out.errors.is_empty());
    assert_eq!(runtime.runs().len(), 2);
    assert!(
        out.preview_url.starts_with(&format!(
            "http://127.0.0.1:{}/users/alice/bulk-changes/apply/",
            server.port
        )),
        "{}",
        out.preview_url
    );
    let rand = rand_id(&out.batch_spec_id);
    let spec_row = BatchSpecStore::new(&server.db)
        .get_by_rand_id(&rand)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(spec_row.user_id, Some(server.alice));
    assert!(!spec_row.created_from_raw);
    let specs = ChangesetSpecStore::new(&server.db)
        .list(&ListChangesetSpecs {
            batch_spec_id: Some(spec_row.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut repos: Vec<RepoId> = specs.iter().map(|s| s.repo_id).collect();
    repos.sort_by_key(|r| r.get());
    let mut want = vec![server.ids[WEB], server.ids[API]];
    want.sort_by_key(|r| r.get());
    assert_eq!(repos, want);
    for s in &specs {
        let diff = String::from_utf8_lossy(s.diff.as_deref().unwrap_or_default());
        assert!(diff.contains("+hi from the environment"), "{diff}");
        assert_eq!(s.head_ref.as_deref(), Some("refs/heads/hello"));
        let name = if s.repo_id == server.ids[WEB] {
            WEB
        } else {
            API
        };
        assert_eq!(s.title.as_deref(), Some(format!("Hello {name}").as_str()));
    }
    // Logs of successful workspaces are removed.
    assert_eq!(
        std::fs::read_dir(&tmp)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "log"))
            .count(),
        0
    );

    // Apply: every step result comes from the cache, so no container runs.
    let out = sourcegull_cli::batch::run::execute(
        &args("apply", &base),
        true,
        &env,
        runtime.clone() as Arc<dyn ContainerRuntime>,
    )
    .await
    .unwrap();
    assert_eq!(runtime.runs().len(), 2, "cached");
    assert_eq!(out.changeset_specs.len(), 2);
    assert_eq!(
        out.batch_change_url.as_deref(),
        Some(
            format!(
                "http://127.0.0.1:{}/users/alice/bulk-changes/hello",
                server.port
            )
            .as_str()
        )
    );
    let bc = BatchChangeStore::new(&server.db)
        .get_by_name(sourcegull_db::Namespace::User(server.alice), "hello")
        .await
        .unwrap()
        .unwrap();
    let applied = BatchSpecStore::new(&server.db)
        .get_by_rand_id(&rand_id(&out.batch_spec_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bc.batch_spec_id, applied.id);

    // -clear-cache runs the steps again; -allow-unsupported and -allow-ignored add the
    // other two repositories.
    let mut more = base.to_vec();
    more.extend([
        "-clear-cache",
        "-allow-unsupported",
        "-allow-ignored",
        "-keep-logs",
    ]);
    let out = sourcegull_cli::batch::run::execute(
        &args("preview", &more),
        false,
        &env,
        runtime.clone() as Arc<dyn ContainerRuntime>,
    )
    .await
    .unwrap();
    assert_eq!(out.workspaces, 4);
    assert_eq!(runtime.runs().len(), 6);
    assert_eq!(out.changeset_specs.len(), 4);
    assert_eq!(
        std::fs::read_dir(&tmp)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "log"))
            .count(),
        4,
        "-keep-logs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_namespaces_and_imports() {
    let Some(server) = Server::start().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("batch.yaml");
    let cache = dir.path().join("cache");
    let tmp = dir.path().join("tmp");
    let env = server.env();
    let runtime = fake_runtime();
    let run = |extra: Vec<&str>, apply: bool| {
        let mut argv = vec![
            "-f",
            file.to_str().unwrap(),
            "-cache",
            cache.to_str().unwrap(),
            "-tmp",
            tmp.to_str().unwrap(),
        ];
        argv.extend(extra);
        let a = args(if apply { "apply" } else { "preview" }, &argv);
        let env = env.clone();
        let runtime = runtime.clone() as Arc<dyn ContainerRuntime>;
        async move { sourcegull_cli::batch::run::execute(&a, apply, &env, runtime).await }
    };

    // Failing steps fail the run and keep the logs; nothing is created.
    std::fs::write(&file, spec("broken", "fail-everything")).unwrap();
    let e = run(vec![], false).await.unwrap_err().to_string();
    assert!(e.contains("2 workspaces failed"), "{e}");
    assert!(e.contains("step 1 failed with exit code 3"), "{e}");
    assert!(e.contains("-skip-errors"), "{e}");
    let logs: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    assert_eq!(logs.len(), 2);
    assert!(std::fs::read_to_string(&logs[0]).unwrap().contains("boom"));
    // With -skip-errors the (empty) spec is still created.
    let out = run(vec!["-skip-errors"], false).await.unwrap();
    assert_eq!(out.errors.len(), 2);
    assert!(out.changeset_specs.is_empty());

    // An unknown namespace; an organization namespace alice is not a member of.
    std::fs::write(&file, spec("org-change", "echo hi")).unwrap();
    let e = run(vec!["-namespace", "nobody"], false).await.unwrap_err();
    assert!(
        e.to_string().contains("namespace \"nobody\" not found"),
        "{e}"
    );
    let org = server.db.orgs().create("acme", None).await.unwrap();
    let e = run(vec!["-n", "acme"], false).await.unwrap_err();
    assert!(
        e.to_string().contains("creating the bulk change spec"),
        "{e}"
    );
    server
        .db
        .orgs()
        .add_member(org.id, server.alice, sourcegull_types::OrgRole::Member)
        .await
        .unwrap();
    let out = run(vec!["-n", "acme"], true).await.unwrap();
    assert!(
        out.batch_change_url
            .as_deref()
            .unwrap()
            .ends_with("/organizations/acme/bulk-changes/org-change")
    );

    // Two workspaces in one repository (branches main and dev) pushing to the same branch
    // are refused before anything is uploaded.
    std::fs::write(
        &file,
        spec("dup", "echo hi").replace(
            &format!("  - repository: {API}\n"),
            &format!("  - repository: {WEB}\n    branches: [main, dev]\n"),
        ),
    )
    .unwrap();
    let e = run(vec![], false).await.unwrap_err().to_string();
    assert!(
        e.starts_with("Some changeset specs would push to the same branch of a repository:"),
        "{e}"
    );
    assert!(
        e.contains(&format!("  - {WEB}: 2 specs push to branch")),
        "{e}"
    );

    // importChangesets only: no workspaces, one existing spec per ID.
    std::fs::write(
        &file,
        format!(
            "name: imports\nimportChangesets:\n  - repository: {WEB}\n    externalIDs: [12, \"13\"]\n"
        ),
    )
    .unwrap();
    let out = run(vec![], false).await.unwrap();
    assert_eq!(out.workspaces, 0);
    assert_eq!(out.changeset_specs.len(), 2);
    let spec_row = BatchSpecStore::new(&server.db)
        .get_by_rand_id(&rand_id(&out.batch_spec_id))
        .await
        .unwrap()
        .unwrap();
    let specs = ChangesetSpecStore::new(&server.db)
        .list(&ListChangesetSpecs {
            batch_spec_id: Some(spec_row.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut ext: Vec<_> = specs
        .iter()
        .map(|s| s.external_id.clone().unwrap())
        .collect();
    ext.sort();
    assert_eq!(ext, ["12", "13"]);

    // A spec that does not validate never reaches the instance.
    std::fs::write(&file, "name: x\nsteps: nope\n").unwrap();
    let e = run(vec![], false).await.unwrap_err();
    assert!(
        e.to_string().starts_with("bulk change spec is invalid"),
        "{e}"
    );
}

/// Real `alpine` steps (volume mode, so the daemon need not see this machine's paths).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_docker_preview() {
    use sourcegull_batches_steps::docker::{DockerOptions, DockerRuntime, WorkspaceMode};
    let Ok(docker) = DockerRuntime::connect(DockerOptions {
        workspace_mode: WorkspaceMode::Copy,
        ..DockerOptions::default()
    }) else {
        eprintln!("skipping: Docker not reachable");
        return;
    };
    if docker.version().await.is_empty() {
        eprintln!("skipping: Docker not reachable");
        return;
    }
    let Some(server) = Server::start().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("batch.yaml");
    std::fs::write(
        dir.path().join("greet.sh"),
        "echo \"$GREETING\" >> README.md\n",
    )
    .unwrap();
    std::fs::write(
        &file,
        spec("docker", "sh /tools/greet.sh").replace(
            "    env:\n",
            "    mount:\n      - path: ./greet.sh\n        mountpoint: /tools/greet.sh\n    env:\n",
        ),
    )
    .unwrap();
    let cache = dir.path().join("cache");
    let tmp = dir.path().join("tmp");
    let a = args(
        "preview",
        &[
            "-f",
            file.to_str().unwrap(),
            "-cache",
            cache.to_str().unwrap(),
            "-tmp",
            tmp.to_str().unwrap(),
            "-workspace",
            "volume",
        ],
    );
    let out = sourcegull_cli::batch::run::execute(&a, false, &server.env(), Arc::new(docker))
        .await
        .unwrap();
    assert_eq!(out.changeset_specs.len(), 2);
    let spec_row = BatchSpecStore::new(&server.db)
        .get_by_rand_id(&rand_id(&out.batch_spec_id))
        .await
        .unwrap()
        .unwrap();
    let specs = ChangesetSpecStore::new(&server.db)
        .list(&ListChangesetSpecs {
            batch_spec_id: Some(spec_row.id),
            ..Default::default()
        })
        .await
        .unwrap();
    for s in specs {
        let diff = String::from_utf8_lossy(s.diff.as_deref().unwrap_or_default()).into_owned();
        assert!(diff.contains("+hi from the environment"), "{diff}");
    }
}

/// The random id inside a `BatchSpec` GraphQL ID.
fn rand_id(id: &str) -> String {
    let (kind, key) = sourcegull_services::relay::unmarshal_id(id).unwrap();
    assert_eq!(kind, "BatchSpec");
    serde_json::from_str(&key).unwrap()
}
