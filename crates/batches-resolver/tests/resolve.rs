//! Workspace resolution against the test Postgres and an in-process repo node with one
//! source repository per seeded repository: `on` queries and explicit repositories with
//! branches, `rootAtLocationOf` workspaces and `in` globs, skips, repository permissions,
//! `importChangesets`, cache hits and re-resolution through the queue worker.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::time::Duration;

use sourcegull_batches::BatchSpec;
use sourcegull_batches::store::{
    BatchSpecStore, BatchSpecWorkspaceStore, ChangesetSpecStore, ChangesetSpecType,
    ExecutionCacheStore, ListChangesetSpecs, ListWorkspaces, NewBatchSpec, NewWorkspace,
};
use sourcegull_batches_resolver::{
    ARCHIVED_REASON, IGNORED_REASON, Options, ResolveError, Resolver, UNSUPPORTED_REASON,
    step_cache_key,
};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_pgqueue::JobState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const WEB: &str = "gitlab.example.com/acme/web";
const API: &str = "gitlab.example.com/acme/api";
const OLD: &str = "gitlab.example.com/acme/old";
const IGNORED: &str = "gitlab.example.com/acme/ignored";
const MIRROR: &str = "github.com/acme/mirror";
const SECRET: &str = "gitlab.example.com/acme/secret";

struct Env {
    t: TestDb,
    db: Db,
    node: RepoNode,
    sources: HashMap<&'static str, SourceRepo>,
    ids: HashMap<&'static str, RepoId>,
    heads: HashMap<&'static str, String>,
    alice: UserId,
    admin: UserId,
    resolver: Resolver,
    config: sourcegull_config::SiteConfigHandle,
}

impl Env {
    fn id(&self, name: &str) -> RepoId {
        self.ids[name]
    }

    fn head(&self, name: &str) -> &str {
        &self.heads[name]
    }

    async fn resolve(
        &self,
        yaml: &str,
        user: UserId,
        options: Options,
    ) -> Result<sourcegull_batches_resolver::Resolution, ResolveError> {
        let spec = BatchSpec::parse(yaml).expect("valid spec");
        self.resolver.resolve(&spec, user, options).await
    }

    /// The (repo name, path, branch) of each workspace.
    fn names(&self, ws: &[NewWorkspace]) -> Vec<(String, String, String)> {
        let by_id: HashMap<RepoId, &str> = self.ids.iter().map(|(n, i)| (*i, *n)).collect();
        ws.iter()
            .map(|w| {
                (
                    by_id[&w.repo_id].to_owned(),
                    w.path.clone(),
                    w.branch.clone(),
                )
            })
            .collect()
    }

    async fn create_spec(&self, yaml: &str, user: UserId) -> i64 {
        let spec = BatchSpec::parse(yaml).expect("valid spec");
        BatchSpecStore::new(&self.db)
            .create(&NewBatchSpec {
                raw_spec: yaml.to_owned(),
                spec: serde_json::to_value(&spec).unwrap(),
                namespace: Namespace::User(user),
                user_id: Some(user),
                created_from_raw: true,
                allow_unsupported: false,
                allow_ignored: false,
                no_cache: false,
                batch_change_id: None,
            })
            .await
            .unwrap()
            .id
    }
}

async fn insert_repo(db: &Db, name: &str, service: &str, ext: &str) -> RepoId {
    let id: i32 = sqlx_query_scalar(
        db,
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $2, $3, 'https://code.example.com/') RETURNING id",
        &[name, ext, service],
    )
    .await;
    RepoId(id)
}

async fn sqlx_query_scalar(db: &Db, sql: &str, args: &[&str]) -> i32 {
    let mut q = sourcegull_db::sqlx::query_scalar(sql);
    for a in args {
        q = q.bind(*a);
    }
    q.fetch_one(db.pool()).await.unwrap()
}

async fn exec(db: &Db, sql: &str, id: RepoId) {
    sourcegull_db::sqlx::query(sql)
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
}

const TODO: &str = "// TODO fixme\n";

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let mut sources = HashMap::new();
    let web = SourceRepo::new();
    web.commit(
        "web",
        &[
            ("README.md", "web\n"),
            ("package.json", "{}\n"),
            ("src/app.js", TODO),
            ("packages/a/package.json", "{}\n"),
            ("packages/a/index.js", TODO),
            ("packages/b/package.json", "{}\n"),
        ],
    );
    sources.insert(WEB, web);
    let api = SourceRepo::new();
    api.commit("api", &[("go.mod", "module api\n"), ("main.go", TODO)]);
    api.git(&["checkout", "-q", "-b", "feature"]);
    api.commit("feature", &[("feature.go", "package main\n")]);
    api.git(&["checkout", "-q", "main"]);
    sources.insert(API, api);
    for (name, extra) in [
        (OLD, None),
        (IGNORED, Some(".batchignore")),
        (MIRROR, None),
        (SECRET, None),
    ] {
        let s = SourceRepo::new();
        let mut files = vec![("main.go", TODO)];
        if let Some(f) = extra {
            files.push((f, ""));
        }
        s.commit(name, &files);
        sources.insert(name, s);
    }
    let remotes: HashMap<String, sourcegull_gitstore::Remote> = sources
        .iter()
        .map(|(n, s)| ((*n).to_owned(), s.remote()))
        .collect();
    let node = RepoNode::start_with(move |r| {
        Ok(remotes
            .get(&r.name)
            .unwrap_or_else(|| panic!("no source for {}", r.name))
            .clone())
    })
    .await;

    let mut ids = HashMap::new();
    for (i, name) in [WEB, API, OLD, IGNORED, MIRROR, SECRET]
        .into_iter()
        .enumerate()
    {
        let service = if name == MIRROR { "github" } else { "gitlab" };
        let id = insert_repo(&db, name, service, &i.to_string()).await;
        node.clone_repo(id, name).await;
        ids.insert(name, id);
    }
    exec(
        &db,
        "UPDATE repo SET archived = true WHERE id = $1",
        ids[OLD],
    )
    .await;
    exec(
        &db,
        "UPDATE repo SET private = true WHERE id = $1",
        ids[SECRET],
    )
    .await;
    let heads = sources.iter().map(|(n, s)| (*n, s.head())).collect();

    let alice = NewUser {
        username: "alice".into(),
        ..Default::default()
    };
    let alice = db.users().create(&alice).await.unwrap().id;
    let admin = NewUser {
        username: "admin".into(),
        ..Default::default()
    };
    let admin = db.users().create(&admin).await.unwrap().id;
    sourcegull_db::sqlx::query("UPDATE users SET site_admin = true WHERE id = $1")
        .bind(admin)
        .execute(db.pool())
        .await
        .unwrap();

    let config = ConfigHandle::new(1, SiteConfig::default());
    let authz = sourcegull_authz::Checker::new(db.clone(), config.clone());
    let resolver = Resolver::new(
        db.clone(),
        sourcegull_services::search_backend(db.clone(), authz, node.clients.clone()),
    );
    Some(Env {
        t,
        db,
        node,
        sources,
        ids,
        heads,
        alice,
        admin,
        resolver,
        config,
    })
}

const STEPS: &str = "steps:\n  - run: echo hi\n    container: alpine:3\n\
    changesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n";

fn spec(body: &str) -> String {
    format!("name: test\n{body}{STEPS}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_on_respects_permissions_and_tags_skips() {
    let Some(env) = setup().await else { return };
    let yaml = spec("on:\n  - repositoriesMatchingQuery: TODO fixme\n");
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    // Sorted by name; the private repository is invisible to alice and archived repositories
    // are not searched by default.
    assert_eq!(
        env.names(&r.workspaces),
        vec![
            (MIRROR.into(), "".into(), "refs/heads/main".into()),
            (API.into(), "".into(), "refs/heads/main".into()),
            (IGNORED.into(), "".into(), "refs/heads/main".into()),
            (WEB.into(), "".into(), "refs/heads/main".into()),
        ]
    );
    let by_repo: HashMap<RepoId, &NewWorkspace> =
        r.workspaces.iter().map(|w| (w.repo_id, w)).collect();
    let web = by_repo[&env.id(WEB)];
    assert_eq!(web.commit, env.head(WEB));
    assert_eq!(web.file_matches, vec!["packages/a/index.js", "src/app.js"]);
    assert!(!web.skipped && !web.ignored && !web.unsupported);
    let api = by_repo[&env.id(API)];
    assert_eq!(api.file_matches, vec!["main.go"]);
    let ignored = by_repo[&env.id(IGNORED)];
    assert!(ignored.ignored && ignored.skipped);
    assert_eq!(ignored.skipped_reason.as_deref(), Some(IGNORED_REASON));
    let mirror = by_repo[&env.id(MIRROR)];
    assert!(mirror.unsupported && mirror.skipped);
    assert_eq!(mirror.skipped_reason.as_deref(), Some(UNSUPPORTED_REASON));
    assert!(r.imports.is_empty());

    // A site admin sees the private repository; `archived:yes` adds the archived one.
    let yaml = spec("on:\n  - repositoriesMatchingQuery: TODO fixme archived:yes\n");
    let r = env
        .resolve(&yaml, env.admin, Options::default())
        .await
        .unwrap();
    let repos: Vec<RepoId> = r.workspaces.iter().map(|w| w.repo_id).collect();
    assert!(repos.contains(&env.id(SECRET)), "{repos:?}");
    let old = r.workspaces.iter().find(|w| w.repo_id == env.id(OLD));
    assert_eq!(
        old.unwrap().skipped_reason.as_deref(),
        Some(ARCHIVED_REASON)
    );

    // A query with its own count is left alone; a type:repo query has no paths.
    let yaml = spec("on:\n  - repositoriesMatchingQuery: repo:acme/web$ count:1\n");
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(r.workspaces.len(), 1);
    assert!(r.workspaces[0].file_matches.is_empty());

    // A query the search rejects fails the resolution.
    let yaml = spec("on:\n  - repositoriesMatchingQuery: foo count:0\n");
    let e = env.resolve(&yaml, env.alice, Options::default()).await;
    assert!(matches!(e, Err(ResolveError::Invalid(_))), "{e:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_repositories_and_branches() {
    let Some(env) = setup().await else { return };
    let feature = env.sources[API].git(&["rev-parse", "feature"]);
    let yaml = spec(&format!(
        "on:\n  - repository: {API}\n    branches: [main, feature]\n  - repository: {WEB}\n"
    ));
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(
        env.names(&r.workspaces),
        vec![
            (API.into(), "".into(), "refs/heads/feature".into()),
            (API.into(), "".into(), "refs/heads/main".into()),
            (WEB.into(), "".into(), "refs/heads/main".into()),
        ]
    );
    assert_eq!(r.workspaces[0].commit, feature);
    assert_eq!(r.workspaces[1].commit, env.head(API));
    assert!(r.workspaces.iter().all(|w| w.file_matches.is_empty()));

    // An explicit entry wins over a query, before or after it.
    for yaml in [
        spec(&format!(
            "on:\n  - repositoriesMatchingQuery: TODO fixme\n  - repository: {API}\n    branch: feature\n"
        )),
        spec(&format!(
            "on:\n  - repository: {API}\n    branch: feature\n  - repositoriesMatchingQuery: TODO fixme\n"
        )),
    ] {
        let r = env
            .resolve(&yaml, env.alice, Options::default())
            .await
            .unwrap();
        let api: Vec<_> = r
            .workspaces
            .iter()
            .filter(|w| w.repo_id == env.id(API))
            .collect();
        assert_eq!(api.len(), 1, "{yaml}");
        assert_eq!(api[0].branch, "refs/heads/feature");
        assert_eq!(api[0].commit, feature);
        assert!(r.workspaces.iter().any(|w| w.repo_id == env.id(WEB)));
    }

    // Unknown branches and repositories, and private ones, fail the resolution.
    let yaml = spec(&format!("on:\n  - repository: {API}\n    branch: nope\n"));
    let e = env.resolve(&yaml, env.alice, Options::default()).await;
    let Err(ResolveError::Invalid(msg)) = e else {
        panic!("{e:?}")
    };
    assert!(msg.contains("no branch matching \"nope\""), "{msg}");
    for name in ["gitlab.example.com/acme/nope", SECRET] {
        let yaml = spec(&format!("on:\n  - repository: {name}\n"));
        let e = env.resolve(&yaml, env.alice, Options::default()).await;
        let Err(ResolveError::Invalid(msg)) = e else {
            panic!("{e:?}")
        };
        assert!(msg.contains("not found"), "{msg}");
    }
    // ... but the admin may target the private repository.
    let yaml = spec(&format!("on:\n  - repository: {SECRET}\n"));
    let r = env
        .resolve(&yaml, env.admin, Options::default())
        .await
        .unwrap();
    assert_eq!(r.workspaces.len(), 1);

    // A spec without steps has no workspaces.
    let yaml = format!("name: test\non:\n  - repository: {WEB}\n");
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert!(r.workspaces.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_if_conditions_per_repository() {
    let Some(env) = setup().await else { return };
    let on = format!("on:\n  - repository: {API}\n  - repository: {WEB}\n");
    let with_steps = |steps: &str| {
        format!(
            "name: test\n{on}steps:\n{steps}changesetTemplate:\n  title: t\n  branch: b\n  \
             commit:\n    message: m\n"
        )
    };
    // Evaluated with the repository name: only web keeps a workspace.
    let yaml = with_steps(&format!(
        "  - run: echo hi\n    container: alpine:3\n    if: ${{{{ eq repository.name \"{WEB}\" }}}}\n"
    ));
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(
        env.names(&r.workspaces),
        vec![(WEB.into(), "".into(), "refs/heads/main".into())]
    );

    // Template functions over static values are static too.
    let yaml = with_steps(
        "  - run: echo hi\n    container: alpine:3\n    \
         if: ${{ matches repository.name \"*/api\" }}\n",
    );
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(
        env.names(&r.workspaces),
        vec![(API.into(), "".into(), "refs/heads/main".into())]
    );

    // A workspace stays when a later step may run; conditions on outputs are left to run
    // time.
    let yaml = with_steps(
        "  - run: echo hi\n    container: alpine:3\n    if: ${{ eq repository.name \"x\" }}\n  \
         - run: echo hi\n    container: alpine:3\n    if: ${{ eq outputs.x \"y\" }}\n",
    );
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(r.workspaces.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_at_location_of_and_in_globs() {
    let Some(env) = setup().await else { return };
    let yaml = spec(
        "on:\n  - repositoriesMatchingQuery: TODO fixme repo:acme/(web|api)$\n\
         workspaces:\n  - rootAtLocationOf: package.json\n    in: \"*/web\"\n    onlyFetchWorkspace: true\n",
    );
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(
        env.names(&r.workspaces),
        vec![
            // Outside the glob: the root.
            (API.into(), "".into(), "refs/heads/main".into()),
            (WEB.into(), "".into(), "refs/heads/main".into()),
            (WEB.into(), "packages/a".into(), "refs/heads/main".into()),
            (WEB.into(), "packages/b".into(), "refs/heads/main".into()),
        ]
    );
    let w = &r.workspaces;
    assert!(!w[0].only_fetch_workspace);
    // The root is fetched whole.
    assert!(!w[1].only_fetch_workspace);
    assert!(w[2].only_fetch_workspace && w[3].only_fetch_workspace);
    // Search result paths within each workspace.
    assert_eq!(w[1].file_matches, vec!["packages/a/index.js", "src/app.js"]);
    assert_eq!(w[2].file_matches, vec!["packages/a/index.js"]);
    assert!(w[3].file_matches.is_empty());

    // Repositories without the file get no workspace.
    let yaml = spec(
        "on:\n  - repositoriesMatchingQuery: TODO fixme repo:acme/(web|api)$\n\
         workspaces:\n  - rootAtLocationOf: go.mod\n",
    );
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    assert_eq!(
        env.names(&r.workspaces),
        vec![(API.into(), "".into(), "refs/heads/main".into())]
    );

    // A repository may match only one `in` glob.
    let yaml = spec(&format!(
        "on:\n  - repository: {WEB}\nworkspaces:\n  - rootAtLocationOf: package.json\n  \
         - rootAtLocationOf: README.md\n    in: \"gitlab.example.com/*\"\n"
    ));
    let e = env.resolve(&yaml, env.alice, Options::default()).await;
    let Err(ResolveError::Invalid(msg)) = e else {
        panic!("{e:?}")
    };
    assert!(
        msg.contains("matches multiple workspaces.in globs"),
        "{msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skips_and_allow_flags() {
    let Some(env) = setup().await else { return };
    let yaml = spec(&format!(
        "on:\n  - repository: {OLD}\n  - repository: {IGNORED}\n  - repository: {MIRROR}\n  - repository: {WEB}\n"
    ));
    let reasons = |r: &sourcegull_batches_resolver::Resolution| -> HashMap<RepoId, (bool, bool, bool, Option<String>)> {
        r.workspaces
            .iter()
            .map(|w| (w.repo_id, (w.ignored, w.unsupported, w.skipped, w.skipped_reason.clone())))
            .collect()
    };
    let r = env
        .resolve(&yaml, env.alice, Options::default())
        .await
        .unwrap();
    let got = reasons(&r);
    assert_eq!(
        got[&env.id(OLD)],
        (false, false, true, Some(ARCHIVED_REASON.into()))
    );
    assert_eq!(
        got[&env.id(IGNORED)],
        (true, false, true, Some(IGNORED_REASON.into()))
    );
    assert_eq!(
        got[&env.id(MIRROR)],
        (false, true, true, Some(UNSUPPORTED_REASON.into()))
    );
    assert_eq!(got[&env.id(WEB)], (false, false, false, None));

    let allow = Options {
        allow_ignored: true,
        allow_unsupported: true,
        no_cache: false,
    };
    let r = env.resolve(&yaml, env.alice, allow).await.unwrap();
    let got = reasons(&r);
    // Archived repositories stay skipped.
    assert_eq!(
        got[&env.id(OLD)],
        (false, false, true, Some(ARCHIVED_REASON.into()))
    );
    assert_eq!(got[&env.id(IGNORED)], (true, false, false, None));
    assert_eq!(got[&env.id(MIRROR)], (false, true, false, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_changesets() {
    let Some(env) = setup().await else { return };
    let yaml = format!(
        "name: test\nimportChangesets:\n  - repository: {WEB}\n    externalIDs: [1, \"2\"]\n  \
         - repository: {API}\n    externalIDs: [7]\n"
    );
    let id = env.create_spec(&yaml, env.alice).await;
    let r = env.resolver.resolve_and_store(id, env.alice).await.unwrap();
    assert!(r.workspaces.is_empty());
    let specs = ChangesetSpecStore::new(&env.db)
        .list(&ListChangesetSpecs {
            batch_spec_id: Some(id),
            ..Default::default()
        })
        .await
        .unwrap();
    let got: Vec<(RepoId, ChangesetSpecType, Option<String>)> = specs
        .iter()
        .map(|s| (s.repo_id, s.kind, s.external_id.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (env.id(WEB), ChangesetSpecType::Existing, Some("1".into())),
            (env.id(WEB), ChangesetSpecType::Existing, Some("2".into())),
            (env.id(API), ChangesetSpecType::Existing, Some("7".into())),
        ]
    );
    assert!(specs.iter().all(|s| s.user_id == Some(env.alice)));
    // Resolving again replaces them.
    env.resolver.resolve_and_store(id, env.alice).await.unwrap();
    let n = ChangesetSpecStore::new(&env.db)
        .count(&ListChangesetSpecs {
            batch_spec_id: Some(id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(n, 3);

    for (repo, ids, want) in [
        (SECRET, "[1]", "not found"),
        (MIRROR, "[1]", "does not support"),
        (WEB, "[\"abc\"]", "invalid external ID"),
    ] {
        let yaml = format!(
            "name: test\nimportChangesets:\n  - repository: {repo}\n    externalIDs: {ids}\n"
        );
        let e = env.resolve(&yaml, env.alice, Options::default()).await;
        let Err(ResolveError::Invalid(msg)) = e else {
            panic!("{e:?}")
        };
        assert!(msg.contains(want), "{msg}");
    }
}

async fn wait_job(
    db: &Db,
    spec: i64,
    want: JobState,
) -> sourcegull_batches::store::ResolutionJobRow {
    for _ in 0..200 {
        let job = BatchSpecStore::new(db)
            .resolution_job(spec)
            .await
            .unwrap()
            .unwrap();
        if job.state == want {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("resolution job of spec {spec} never reached {want:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_resolves_again_with_cache_hits() {
    let Some(env) = setup().await else { return };
    let cancel = tokio_util::sync::CancellationToken::new();
    let (worker, resetter) = sourcegull_batches_resolver::worker::resolution_workers(
        &env.db,
        env.config.clone(),
        env.node.clients.clone(),
        "test-worker",
    )
    .unwrap();
    tokio::spawn(worker.run(cancel.clone()));
    tokio::spawn(resetter.run(cancel.clone()));

    let yaml = spec(&format!(
        "on:\n  - repository: {WEB}\n  - repository: {API}\n"
    ));
    let id = env.create_spec(&yaml, env.alice).await;
    BatchSpecStore::new(&env.db)
        .enqueue_resolution(id, env.alice)
        .await
        .unwrap();
    wait_job(&env.db, id, JobState::Completed).await;
    let list = ListWorkspaces {
        batch_spec_id: id,
        ..Default::default()
    };
    let first = BatchSpecWorkspaceStore::new(&env.db)
        .list(&list)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|w| !w.cached_result_found));

    // The web workspace's step result is cached; api gets a new commit.
    let parsed = BatchSpec::parse(&yaml).unwrap();
    let key = step_cache_key(&parsed, WEB, env.head(WEB), &[], "", false, 0);
    ExecutionCacheStore::new(&env.db)
        .set(env.alice, &key, r#"{"diff":"x"}"#, 1)
        .await
        .unwrap();
    let new_head = env.sources[API].commit("more", &[("more.go", "package main\n")]);
    env.node.fetch_repo(env.id(API), API).await;
    BatchSpecStore::new(&env.db)
        .enqueue_resolution(id, env.alice)
        .await
        .unwrap();
    wait_job(&env.db, id, JobState::Completed).await;
    let second = BatchSpecWorkspaceStore::new(&env.db)
        .list(&list)
        .await
        .unwrap();
    assert_eq!(second.len(), 2);
    let web = second.iter().find(|w| w.repo_id == env.id(WEB)).unwrap();
    let api = second.iter().find(|w| w.repo_id == env.id(API)).unwrap();
    assert!(web.cached_result_found);
    assert_eq!(
        web.step_cache_results,
        serde_json::json!({ "1": { "key": key, "value": { "diff": "x" } } })
    );
    assert!(!api.cached_result_found);
    assert_eq!(api.commit, new_head);
    // The earlier workspaces were replaced.
    assert!(second.iter().all(|w| first.iter().all(|f| f.id != w.id)));

    // With noCache, hits are not looked up.
    BatchSpecStore::new(&env.db)
        .set_options(id, false, false, true)
        .await
        .unwrap();
    BatchSpecStore::new(&env.db)
        .enqueue_resolution(id, env.alice)
        .await
        .unwrap();
    wait_job(&env.db, id, JobState::Completed).await;
    let third = BatchSpecWorkspaceStore::new(&env.db)
        .list(&list)
        .await
        .unwrap();
    assert!(third.iter().all(|w| !w.cached_result_found));

    // A spec naming a missing repository fails its job with the reason.
    let yaml = spec("on:\n  - repository: gitlab.example.com/acme/nope\n");
    let bad = env.create_spec(&yaml, env.alice).await;
    BatchSpecStore::new(&env.db)
        .enqueue_resolution(bad, env.alice)
        .await
        .unwrap();
    let job = wait_job(&env.db, bad, JobState::Failed).await;
    let msg = job.failure_message.unwrap_or_default();
    assert!(msg.contains("not found"), "{msg}");
    cancel.cancel();
    drop(env.t);
}
