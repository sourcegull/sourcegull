//! A bulk changes scenario run against [`Server`] by the executor with any runtime (the
//! step runner's fake, Docker): server-side bulk change specs executed through GraphQL and
//! `execute_batch_spec`, waited for, and checked: checkout (full and sparse), secrets passed
//! through and redacted, the step cache uploaded and then served, the changeset specs
//! uploaded, auto-apply, a step mounting a file uploaded with the spec, a failing step and a
//! canceled execution.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_graphql::{Request, Variables};
use serde_json::{Value, json};
use sourcegull_authn::AuthVia;
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpec, BatchSpecStore, BatchSpecWorkspaceFileStore,
    BatchSpecWorkspaceStore, ChangesetSpecStore, ExecutionCacheStore, ExecutionJobRow,
    ListWorkspaces, NewBatchSpec, NewWorkspace,
};
use sourcegull_db::Namespace;
use sourcegull_executor::{ExecutorConfig, Runtime};
use sourcegull_pgqueue::JobState;
use sourcegull_services::batches::{ExecuteOptions, execute_batch_spec, step_cache_keys};
use sourcegull_types::UserId;
use tokio_util::sync::CancellationToken;

use super::{REPO, Server, TOKEN};

/// The `BATCHES` executor secret of [`Server::alice`] the steps pass through.
pub const SECRET_KEY: &str = "SOURCEGULL_BATCHES_SECRET";
pub const SECRET_VALUE: &str = "batches-secret-value-7";
/// The image of every step.
pub const IMAGE: &str = "alpine:latest";

/// Two steps: the first prints the secret, tells whether `src/` was checked out and changes
/// `README.md` in the workspace; the second appends the first one's output.
pub const SPEC: &str = r#"version: 2
name: hello
description: Say hello
on:
  - repository: gitlab.example.com/acme/demo
steps:
  - run: |
      echo "token is $SOURCEGULL_BATCHES_SECRET"
      test -d /work/src && echo "src checked out" || echo "src not checked out"
      echo Hello >> README.md
    container: alpine:latest
    env:
      - SOURCEGULL_BATCHES_SECRET
    outputs:
      greeting:
        value: hello
  - run: echo ${{ outputs.greeting }} from step 2 >> README.md
    container: alpine:latest
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
  published: false
"#;

/// A step that mounts a file uploaded with the spec ([`MOUNTED`] at `scripts/greeting.txt`).
pub const MOUNT_SPEC: &str = "version: 2
name: mounted
on:
  - repository: gitlab.example.com/acme/demo
steps:
  - run: cat /tmp/greeting.txt >> README.md
    container: alpine:latest
    mount:
      - path: ./scripts/greeting.txt
        mountpoint: /tmp/greeting.txt
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
";

/// The content of the mounted file.
pub const MOUNTED: &str = "Greetings from a mounted file\n";

/// A step that fails with status 3.
pub const FAILING_SPEC: &str = "version: 2
name: failing
on:
  - repository: gitlab.example.com/acme/demo
steps:
  - run: |
      echo about to fail >&2
      exit 3
    container: alpine:latest
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
";

/// A step that runs until it is canceled.
pub const SLOW_SPEC: &str = "version: 2
name: slow
on:
  - repository: gitlab.example.com/acme/demo
steps:
  - run: sleep 600
    container: alpine:latest
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
";

const EXECUTE: &str = "mutation($id: ID!) {
    executeBatchSpec(batchSpec: $id, autoApplyEnabled: true) { state autoApplyEnabled }
}";

impl Server {
    /// Runs `query` as `user`; fails on errors.
    pub async fn graphql(&self, user: UserId, query: &str, vars: Value) -> Value {
        let actor = self
            .state
            .auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        let resp = sourcegull_graphql::execute(&self.schema, req, Some(actor)).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    /// The root workspace of the repository at `c3`.
    pub fn workspace(&self) -> NewWorkspace {
        let mut ws = NewWorkspace::new(self.repo, "refs/heads/main", &self.oids.c3);
        ws.file_matches = vec!["README.md".into()];
        ws
    }

    /// A resolved server-side spec of alice with `workspaces`.
    pub async fn batch_spec(&self, raw: &str, workspaces: Vec<NewWorkspace>) -> BatchSpec {
        let parsed = sourcegull_batches::BatchSpec::parse(raw).unwrap();
        let spec = BatchSpecStore::new(&self.db)
            .create(&NewBatchSpec {
                raw_spec: raw.into(),
                spec: serde_json::to_value(&parsed).unwrap(),
                namespace: Namespace::User(self.alice),
                user_id: Some(self.alice),
                created_from_raw: true,
                allow_unsupported: false,
                allow_ignored: false,
                no_cache: false,
                batch_change_id: None,
            })
            .await
            .unwrap();
        BatchSpecStore::new(&self.db)
            .enqueue_resolution(spec.id, self.alice)
            .await
            .unwrap();
        sourcegull_db::sqlx::query(
            "UPDATE batch_spec_resolution_jobs SET state = 'completed' WHERE batch_spec_id = $1",
        )
        .bind(spec.id)
        .execute(self.db.pool())
        .await
        .unwrap();
        BatchSpecWorkspaceStore::new(&self.db)
            .create_many(spec.id, &workspaces)
            .await
            .unwrap();
        spec
    }

    /// Executes `spec` as `executeBatchSpec` does; returns the number of jobs.
    pub async fn execute(&self, spec: &BatchSpec) -> u64 {
        execute_batch_spec(
            &self.db,
            self.state.auto_applier.as_ref(),
            spec,
            ExecuteOptions::default(),
        )
        .await
        .unwrap()
        .jobs
    }

    /// The execution job of `spec`'s only workspace.
    pub async fn execution(&self, spec: &BatchSpec) -> Option<ExecutionJobRow> {
        let ws = self.workspaces(spec).await;
        BatchSpecWorkspaceStore::new(&self.db)
            .execution_jobs_for_workspaces(&[ws[0].id])
            .await
            .unwrap()
            .pop()
    }

    pub async fn workspaces(
        &self,
        spec: &BatchSpec,
    ) -> Vec<sourcegull_batches::store::BatchSpecWorkspace> {
        BatchSpecWorkspaceStore::new(&self.db)
            .list(&ListWorkspaces {
                batch_spec_id: spec.id,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    /// Waits until the execution of `spec`'s only workspace ended.
    pub async fn wait_execution(&self, spec: &BatchSpec, timeout: Duration) -> ExecutionJobRow {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(row) = self.execution(spec).await
                && matches!(
                    row.state,
                    JobState::Completed | JobState::Failed | JobState::Errored | JobState::Canceled
                )
            {
                return row;
            }
            assert!(
                Instant::now() < deadline,
                "the execution did not end in time"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The changeset specs uploaded for `spec`'s only workspace.
    pub async fn changeset_specs(
        &self,
        spec: &BatchSpec,
    ) -> Vec<sourcegull_batches::store::ChangesetSpec> {
        let ws = self.workspaces(spec).await.remove(0);
        let mut out = Vec::new();
        for id in ws.changeset_spec_ids {
            out.push(
                ChangesetSpecStore::new(&self.db)
                    .get(id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        out
    }
}

fn keys(row: &ExecutionJobRow) -> Vec<&str> {
    row.execution_logs.iter().map(|e| e.key.as_str()).collect()
}

fn entry<'a>(row: &'a ExecutionJobRow, key: &str) -> &'a sourcegull_pgqueue::ExecutionLogEntry {
    row.execution_logs
        .iter()
        .find(|e| e.key == key)
        .unwrap_or_else(|| panic!("no {key} in {:?}", keys(row)))
}

fn completed(row: &ExecutionJobRow) {
    assert_eq!(
        row.state,
        JobState::Completed,
        "{:?}: {:#?}",
        row.failure_message,
        row.execution_logs
    );
    for e in &row.execution_logs {
        assert!(
            !e.out.contains(SECRET_VALUE),
            "secret in {}: {}",
            e.key,
            e.out
        );
        assert!(!e.out.contains("sjt_"), "job token in {}: {}", e.key, e.out);
    }
}

/// Runs the scenario with an executor using `runtime`; `timeout` bounds each execution.
/// Returns the executor's name.
pub async fn run_scenario(server: &Server, runtime: Arc<dyn Runtime>, timeout: Duration) -> String {
    let runtime_name = runtime.name();
    let scratch = tempfile::tempdir().unwrap();
    let name = format!("test-batches-{}", uuid::Uuid::new_v4().simple());
    let stop = CancellationToken::new();
    let executor = tokio::spawn(sourcegull_executor::run(
        ExecutorConfig {
            url: format!("http://127.0.0.1:{}", server.port).parse().unwrap(),
            token: TOKEN.into(),
            name: name.clone(),
            queues: vec!["batches".into()],
            max_jobs: 2,
            poll_interval: Duration::from_millis(200),
            heartbeat_interval: Duration::from_millis(500),
            scratch_dir: scratch.path().to_owned(),
            git: "git".into(),
        },
        runtime,
        stop.clone(),
    ));
    let alice = server.alice;

    // executeBatchSpec with autoApplyEnabled: the executor checks out the workspace, runs
    // both steps, stores their results in alice's cache and uploads the changeset spec; the
    // spec is then applied as alice.
    let first = server.batch_spec(SPEC, vec![server.workspace()]).await;
    let id = sourcegull_graphql::batches::batch_spec_id(&first.rand_id);
    let data = server
        .graphql(alice, EXECUTE, json!({ "id": id.as_str() }))
        .await;
    // The executor polls every 200 ms and may already have claimed the job.
    let state = &data["executeBatchSpec"]["state"];
    assert!(state == "QUEUED" || state == "PROCESSING", "{state}");
    let row = server.wait_execution(&first, timeout).await;
    completed(&row);
    let logged = keys(&row);
    for key in [
        "setup.git.fetch",
        "setup.git.checkout",
        &format!("setup.image.{IMAGE}"),
        "step.0",
        "step.1",
        "teardown.upload",
    ] {
        assert!(logged.contains(&key), "{key} in {logged:?}");
    }
    let step = entry(&row, "step.0");
    assert_eq!(step.command[0], IMAGE);
    assert!(step.command[1].contains("echo Hello >> README.md"));
    assert_eq!(step.exit_code, Some(0));
    assert!(step.duration_ms.is_some());
    assert!(
        step.out.contains(&format!(
            "stdout: token is ${{{{ secrets.{SECRET_KEY} }}}}\n"
        )),
        "{}",
        step.out
    );
    assert!(
        step.out.contains("stdout: src checked out\n"),
        "{}",
        step.out
    );
    let step = entry(&row, "step.1");
    assert_eq!(
        step.command[1].trim(),
        "echo hello from step 2 >> README.md"
    );

    let specs = server.changeset_specs(&first).await;
    assert_eq!(specs.len(), 1);
    let diff = String::from_utf8(specs[0].diff.clone().unwrap()).unwrap();
    assert!(diff.contains("+Hello\n+hello from step 2\n"), "{diff}");
    assert_eq!(specs[0].head_ref.as_deref(), Some("refs/heads/hello"));
    assert_eq!(
        specs[0].commit_author_email.as_deref(),
        Some("alice@example.com")
    );
    let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
    let ws = server.workspaces(&first).await.remove(0);
    let cache_keys = step_cache_keys(&parsed, REPO, &ws);
    let cached = ExecutionCacheStore::new(&server.db)
        .get_many(alice, &cache_keys)
        .await
        .unwrap();
    assert_eq!(cached.len(), 2);
    for e in &cached {
        assert!(!e.value.contains(SECRET_VALUE), "secret in the cache");
    }
    // Applied as alice once the execution completed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let bc = loop {
        if let Some(bc) = BatchChangeStore::new(&server.db)
            .get_by_name(Namespace::User(alice), "hello")
            .await
            .unwrap()
        {
            break bc;
        }
        assert!(Instant::now() < deadline, "the spec was not applied");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(bc.batch_spec_id, first.id);
    assert_eq!(bc.last_applier_id, Some(alice));

    // `onlyFetchWorkspace`: a sparse checkout of the workspace directory.
    let mut docs = server.workspace();
    docs.path = "docs".into();
    docs.only_fetch_workspace = true;
    docs.file_matches = vec!["docs/guide.md".into()];
    let sparse = server.batch_spec(SPEC, vec![docs]).await;
    assert_eq!(server.execute(&sparse).await, 1);
    let row = server.wait_execution(&sparse, timeout).await;
    completed(&row);
    let out = &entry(&row, "step.0").out;
    assert!(out.contains("stdout: src not checked out\n"), "{out}");
    let diff = String::from_utf8(
        server.changeset_specs(&sparse).await[0]
            .diff
            .clone()
            .unwrap(),
    )
    .unwrap();
    assert!(diff.contains("docs/README.md"), "{diff}");

    // The first step's result in the payload (as the resolver finds it): the run resumes
    // after it and builds the same diff.
    let value: Value = serde_json::from_str(
        &cached
            .iter()
            .find(|e| e.key == cache_keys[0])
            .unwrap()
            .value,
    )
    .unwrap();
    let mut partial = server.workspace();
    partial.step_cache_results = json!({ "1": { "key": cache_keys[0], "value": value } });
    let resumed = server.batch_spec(SPEC, vec![partial]).await;
    assert_eq!(server.execute(&resumed).await, 1);
    let row = server.wait_execution(&resumed, timeout).await;
    completed(&row);
    let logged = keys(&row);
    assert!(logged.contains(&"step.0.cached"), "{logged:?}");
    assert!(!logged.contains(&"step.0"), "{logged:?}");
    assert!(logged.contains(&"step.1"), "{logged:?}");
    assert_eq!(
        server.changeset_specs(&resumed).await[0].diff,
        specs[0].diff
    );

    // A step mounting a file uploaded with the spec: the executor downloads it with the job
    // token and mounts it.
    let mounted = server
        .batch_spec(MOUNT_SPEC, vec![server.workspace()])
        .await;
    BatchSpecWorkspaceFileStore::new(&server.db)
        .upsert(&sourcegull_batches::store::NewBatchSpecWorkspaceFile {
            batch_spec_id: mounted.id,
            path: "scripts",
            filename: "greeting.txt",
            content: MOUNTED.as_bytes(),
            modified_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    assert_eq!(server.execute(&mounted).await, 1);
    let row = server.wait_execution(&mounted, timeout).await;
    completed(&row);
    let files = entry(&row, "setup.workspace-files");
    assert!(files.out.contains("scripts/greeting.txt"), "{}", files.out);
    let diff = String::from_utf8(
        server.changeset_specs(&mounted).await[0]
            .diff
            .clone()
            .unwrap(),
    )
    .unwrap();
    assert!(diff.contains("+Greetings from a mounted file\n"), "{diff}");

    // A failing step fails the execution.
    let failing = server
        .batch_spec(FAILING_SPEC, vec![server.workspace()])
        .await;
    assert_eq!(server.execute(&failing).await, 1);
    let row = server.wait_execution(&failing, timeout).await;
    assert_eq!(row.state, JobState::Failed, "{:?}", row.failure_message);
    assert_eq!(
        row.failure_message.as_deref(),
        Some("step 1 failed with exit code 3")
    );
    let step = entry(&row, "step.0");
    assert_eq!(step.exit_code, Some(3));
    assert!(step.out.contains("stderr: about to fail\n"), "{}", step.out);
    assert!(server.changeset_specs(&failing).await.is_empty());

    // Canceling a running execution stops the step.
    let slow = server.batch_spec(SLOW_SPEC, vec![server.workspace()]).await;
    assert_eq!(server.execute(&slow).await, 1);
    let deadline = Instant::now() + timeout;
    loop {
        let row = server.execution(&slow).await.unwrap();
        if row.execution_logs.iter().any(|e| e.key == "step.0") {
            break;
        }
        assert!(Instant::now() < deadline, "the slow step did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    BatchSpecWorkspaceStore::new(&server.db)
        .cancel_executions(slow.id)
        .await
        .unwrap();
    let row = server.wait_execution(&slow, Duration::from_secs(30)).await;
    assert_eq!(row.state, JobState::Canceled, "{:?}", row.failure_message);
    let step = entry(&row, "step.0");
    assert_eq!(step.exit_code, None);

    server.assert_executor(&name, runtime_name).await;
    stop.cancel();
    executor.await.unwrap().unwrap();
    name
}
