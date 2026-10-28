//! Previews and applies of bulk change specs over GraphQL against the test
//! Postgres (`SOURCEGULL_TEST_DATABASE_URL`): the operations of a first apply and of re-applies
//! with added, changed and dropped specs, imports, `publicationStates` overrides and errors,
//! preview filters, the bulk change checks of `applyBatchChange`/`createBatchChange`, a
//! failing apply rolling back, hidden repositories, permissions, and spec diffs (with the
//! old and new file contents against an in-process repo node).

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{
    Actor, AuthVia, Authenticator, MemorySessionStore, PasswordParams, Permission, PermissionSet,
};
use sourcegull_authz::Checker;
use sourcegull_batches::store::{BatchChangeStore, Changeset, ChangesetStore, UiPublicationState};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoId, UserId};

const PUBLIC: &str = "gitlab.example.com/acme/public";
const OTHER: &str = "gitlab.example.com/acme/other";
const PRIVATE: &str = "gitlab.example.com/acme/private";

const DIFF: &str = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Title\n+Hello World\n";

/// `applyBatchChange` as a command-line client sends it.
const CLI_APPLY: &str = "
mutation ApplyBatchChange($batchSpec: ID!) {
    applyBatchChange(batchSpec: $batchSpec) {
        ...batchChangeFields
    }
}

fragment batchChangeFields on BatchChange {
    url
}
";

const APPLY: &str =
    "mutation($spec: ID!, $ensure: ID, $states: [ChangesetSpecPublicationStateInput!]) {
    applyBatchChange(batchSpec: $spec, ensureBatchChange: $ensure, publicationStates: $states) {
        id name state description url lastApplier { username } lastAppliedAt
        currentSpec { id }
    }
}";

const CREATE: &str = "mutation($spec: ID!, $states: [ChangesetSpecPublicationStateInput!]) {
    createBatchChange(batchSpec: $spec, publicationStates: $states) { id name }
}";

const PREVIEW: &str = "query($spec: ID!, $first: Int, $after: String, $search: String,
                       $state: ChangesetState, $action: ChangesetSpecOperation,
                       $states: [ChangesetSpecPublicationStateInput!]) {
    node(id: $spec) { ... on BatchSpec {
        applyPreview(first: $first, after: $after, search: $search, currentState: $state,
                     action: $action, publicationStates: $states) {
            totalCount
            pageInfo { hasNextPage endCursor }
            stats {
                push update undraft publish publishDraft sync import close reopen sleep
                detach archive reattach added modified removed
            }
            nodes {
                __typename
                ... on VisibleChangesetApplyPreview {
                    operations
                    delta { titleChanged bodyChanged diffChanged }
                    targets {
                        __typename
                        ... on VisibleApplyPreviewTargetsAttach {
                            changesetSpec { id description { ... on GitBranchChangesetDescription { title headRef } } }
                        }
                        ... on VisibleApplyPreviewTargetsUpdate {
                            changesetSpec { id description { ... on GitBranchChangesetDescription { title headRef } } }
                            changeset { id state }
                        }
                        ... on VisibleApplyPreviewTargetsDetach { changeset { id state } }
                    }
                }
                ... on HiddenChangesetApplyPreview {
                    operations
                    targets {
                        __typename
                        ... on HiddenApplyPreviewTargetsAttach { changesetSpec { id } }
                        ... on HiddenApplyPreviewTargetsUpdate { changesetSpec { id } changeset { id } }
                        ... on HiddenApplyPreviewTargetsDetach { changeset { id } }
                    }
                }
            }
        }
    } }
}";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    bob: UserId,
    public: RepoId,
    other: RepoId,
    private: RepoId,
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, private, external_id, external_service_type,
                 external_service_id, url)
             VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/', $4) RETURNING id",
        )
        .bind(name)
        .bind(private)
        .bind(ext)
        .bind(format!("https://{name}"))
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
}

async fn user(db: &Db, name: &str) -> UserId {
    let new = NewUser {
        username: name.into(),
        ..Default::default()
    };
    db.users().create(&new).await.unwrap().id
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, SiteConfig::default());
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let admin = auth
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let public = insert_repo(&db, PUBLIC, "1", false).await;
    let other = insert_repo(&db, OTHER, "2", false).await;
    let private = insert_repo(&db, PRIVATE, "3", true).await;
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        auth,
        config,
        schema,
        admin,
        alice,
        bob,
        public,
        other,
        private,
    })
}

fn repo_id(r: RepoId) -> String {
    marshal_id("Repository", i64::from(r.get()))
}

fn ns(u: UserId) -> String {
    marshal_id("User", u.get())
}

fn bc_key(id: &Value) -> i64 {
    unmarshal_int_id("BatchChange", id.as_str().unwrap()).unwrap()
}

/// A bulk change spec named `name`.
fn spec_yaml(name: &str, description: &str) -> String {
    format!(
        "version: 2
name: {name}
description: {description}
on:
  - repositoriesMatchingQuery: file:README.md
steps:
  - run: echo Hello >> README.md
    container: alpine:3
changesetTemplate:
  title: Hello World
  body: My first bulk change!
  branch: hello-world
  commit:
    message: Append Hello World
"
    )
}

/// A branch changeset spec in the uploaded JSON form; `published` null leaves it unset.
fn branch_spec(repo: RepoId, branch: &str, title: &str, published: Value) -> String {
    let mut v = json!({
        "baseRepository": repo_id(repo),
        "baseRef": "refs/heads/main",
        "baseRev": "0123456789abcdef0123456789abcdef01234567",
        "headRepository": repo_id(repo),
        "headRef": format!("refs/heads/{branch}"),
        "title": title,
        "body": "Body",
        "commits": [{
            "message": "Append Hello World",
            "diff": DIFF,
            "authorName": "Alice",
            "authorEmail": "alice@example.com"
        }],
    });
    if !published.is_null() {
        v["published"] = published;
    }
    v.to_string()
}

fn import_spec(repo: RepoId, external_id: &str) -> String {
    json!({"baseRepository": repo_id(repo), "externalID": external_id}).to_string()
}

impl Env {
    async fn actor(&self, user: UserId) -> Actor {
        self.auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap()
    }

    async fn run_as(&self, actor: Actor, query: &str, vars: Value) -> Response {
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
        let actor = self.actor(user).await;
        self.run_as(actor, query, vars).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, user: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(user, query, vars).await;
        first_error(query, &resp)
    }

    /// Uploads changeset specs as `user`, returning their IDs.
    async fn upload(&self, user: UserId, specs: &[String]) -> Vec<String> {
        let data = self
            .ok(
                user,
                "mutation($specs: [String!]!) { createChangesetSpecs(changesetSpecs: $specs) { id } }",
                json!({ "specs": specs }),
            )
            .await;
        data["createChangesetSpecs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Creates a bulk change spec in `namespace` with the changeset specs `ids`.
    async fn batch_spec(
        &self,
        user: UserId,
        namespace: UserId,
        yaml: &str,
        ids: &[String],
    ) -> String {
        let data = self
            .ok(
                user,
                "mutation($namespace: ID!, $spec: String!, $ids: [ID!]!) {
                    createBatchSpec(namespace: $namespace, batchSpec: $spec, changesetSpecs: $ids) { id }
                }",
                json!({"namespace": ns(namespace), "spec": yaml, "ids": ids}),
            )
            .await;
        data["createBatchSpec"]["id"].as_str().unwrap().to_owned()
    }

    /// Uploads `specs` as alice into a bulk change spec named `name` of hers.
    async fn alice_spec(&self, name: &str, specs: &[String]) -> (String, Vec<String>) {
        let ids = self.upload(self.alice, specs).await;
        let spec = self
            .batch_spec(self.alice, self.alice, &spec_yaml(name, "Fix things"), &ids)
            .await;
        (spec, ids)
    }

    async fn preview(&self, user: UserId, spec: &str, vars: Value) -> Value {
        let mut v = json!({ "spec": spec });
        v.as_object_mut()
            .unwrap()
            .extend(vars.as_object().unwrap().clone());
        let data = self.ok(user, PREVIEW, v).await;
        data["node"]["applyPreview"].clone()
    }

    /// The changesets of a bulk change (archived ones too), in id order.
    async fn changesets(&self, bc: i64) -> Vec<Changeset> {
        let ids: Vec<i64> = sourcegull_db::sqlx::query_scalar(
            "SELECT id FROM changesets WHERE batch_change_ids ? $1::text OR owned_by_batch_change_id = $2
             ORDER BY id",
        )
        .bind(bc.to_string())
        .bind(bc)
        .fetch_all(self.db.pool())
        .await
        .unwrap();
        ChangesetStore::new(&self.db).get_many(&ids).await.unwrap()
    }

    /// What the reconciler would leave: published and open (with an MR number) or still
    /// unpublished, completed.
    async fn reconciled(&self, id: i64, published: bool) {
        sourcegull_db::sqlx::query(
            "UPDATE changesets SET state = 'completed',
                 publication_state = CASE WHEN $2 THEN 'PUBLISHED' ELSE 'UNPUBLISHED' END,
                 external_state = CASE WHEN $2 THEN 'OPEN' END,
                 external_id = CASE WHEN $2 THEN id::text END
             WHERE id = $1",
        )
        .bind(id)
        .bind(published)
        .execute(self.db.pool())
        .await
        .unwrap();
    }
}

fn first_error(query: &str, resp: &Response) -> (String, String) {
    let err = resp
        .errors
        .first()
        .unwrap_or_else(|| panic!("{query}: no error"));
    let code = err
        .extensions
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.clone().into_json().ok())
        .and_then(|c| c.as_str().map(str::to_owned))
        .unwrap_or_default();
    (code, err.message.clone())
}

fn ops(node: &Value) -> Vec<&str> {
    node["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o.as_str().unwrap())
        .collect()
}

fn targets(preview: &Value) -> Vec<(String, Vec<&str>)> {
    preview["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["targets"]["__typename"].as_str().unwrap().to_owned(),
                ops(n),
            )
        })
        .collect()
}

#[tokio::test]
async fn first_apply_and_reapply() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let alice = env.alice;

    // First apply: every spec is new.
    let (spec1, ids1) = env
        .alice_spec(
            "fix",
            &[
                branch_spec(env.public, "fix", "Fix it", json!(true)),
                branch_spec(env.other, "fix", "Fix it", json!(false)),
            ],
        )
        .await;
    let p = env.preview(alice, &spec1, json!({})).await;
    assert_eq!(p["totalCount"], 2);
    assert_eq!(
        targets(&p),
        vec![
            (
                "VisibleApplyPreviewTargetsAttach".into(),
                vec!["PUSH", "PUBLISH"]
            ),
            ("VisibleApplyPreviewTargetsAttach".into(), vec![]),
        ]
    );
    let first = &p["nodes"][0]["targets"]["changesetSpec"];
    assert_eq!(first["id"], ids1[0].as_str());
    assert_eq!(first["description"]["headRef"], "fix");
    let stats = &p["stats"];
    assert_eq!(
        (
            stats["added"].clone(),
            stats["modified"].clone(),
            stats["removed"].clone()
        ),
        (json!(2), json!(0), json!(0))
    );
    assert_eq!(
        (stats["push"].clone(), stats["publish"].clone()),
        (json!(1), json!(1))
    );

    // Pagination.
    let page = env.preview(alice, &spec1, json!({"first": 1})).await;
    assert_eq!(page["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(page["totalCount"], 2);
    assert!(page["pageInfo"]["hasNextPage"].as_bool().unwrap());
    let next = env
        .preview(
            alice,
            &spec1,
            json!({"first": 1, "after": page["pageInfo"]["endCursor"]}),
        )
        .await;
    assert_eq!(
        next["nodes"][0]["targets"]["changesetSpec"]["id"],
        ids1[1].as_str()
    );
    assert!(!next["pageInfo"]["hasNextPage"].as_bool().unwrap());

    // The command-line client's operation.
    let data = env.ok(alice, CLI_APPLY, json!({"batchSpec": spec1})).await;
    assert_eq!(
        data["applyBatchChange"]["url"],
        "/users/alice/bulk-changes/fix"
    );
    let bc = BatchChangeStore::new(&env.db)
        .get_by_name(Namespace::User(alice), "fix")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bc.last_applier_id, Some(alice));
    assert!(bc.last_applied_at.is_some());
    assert_eq!(bc.description, "Fix things");
    let cs = env.changesets(bc.id).await;
    assert_eq!(cs.len(), 2);
    assert!(cs.iter().all(|c| c.owned_by_batch_change_id == Some(bc.id)
        && c.reconciler_state == sourcegull_batches::store::ReconcilerState::Queued
        && c.current_spec_id.is_some()));
    // Applying the same spec again changes nothing.
    let again = env.ok(alice, APPLY, json!({"spec": spec1})).await;
    assert_eq!(bc_key(&again["applyBatchChange"]["id"]), bc.id);
    assert_eq!(env.changesets(bc.id).await, cs);
    let (public_cs, other_cs) = (cs[0].id, cs[1].id);
    env.reconciled(public_cs, true).await;
    env.reconciled(other_cs, false).await;

    // Re-apply: the public changeset's title changes, a new branch appears, the other
    // repository's (unpublished) changeset is dropped.
    let (spec2, _) = env
        .alice_spec(
            "fix",
            &[
                branch_spec(env.public, "fix", "Fix it better", json!(true)),
                branch_spec(env.public, "more", "More fixes", json!(true)),
            ],
        )
        .await;
    let p = env.preview(alice, &spec2, json!({})).await;
    assert_eq!(
        targets(&p),
        vec![
            ("VisibleApplyPreviewTargetsUpdate".into(), vec!["UPDATE"]),
            (
                "VisibleApplyPreviewTargetsAttach".into(),
                vec!["PUSH", "PUBLISH"]
            ),
            ("VisibleApplyPreviewTargetsDetach".into(), vec!["DETACH"]),
        ]
    );
    let update = &p["nodes"][0];
    assert_eq!(update["delta"]["titleChanged"], true);
    assert_eq!(update["delta"]["diffChanged"], false);
    assert_eq!(update["targets"]["changeset"]["state"], "OPEN");
    let stats = &p["stats"];
    assert_eq!(
        (
            stats["added"].clone(),
            stats["modified"].clone(),
            stats["removed"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    assert_eq!(
        (stats["update"].clone(), stats["detach"].clone()),
        (json!(1), json!(1))
    );

    // Filters.
    let f = env
        .preview(alice, &spec2, json!({"action": "PUBLISH"}))
        .await;
    assert_eq!(f["totalCount"], 1);
    assert_eq!(f["stats"]["added"], 1, "stats ignore filters");
    let f = env.preview(alice, &spec2, json!({"state": "OPEN"})).await;
    assert_eq!(
        targets(&f),
        vec![("VisibleApplyPreviewTargetsUpdate".into(), vec!["UPDATE"])]
    );
    let f = env
        .preview(alice, &spec2, json!({"search": "acme/other"}))
        .await;
    assert_eq!(f["totalCount"], 1);
    let f = env
        .preview(alice, &spec2, json!({"search": "fix -better"}))
        .await;
    assert_eq!(
        targets(&f),
        vec![(
            "VisibleApplyPreviewTargetsAttach".into(),
            vec!["PUSH", "PUBLISH"]
        )]
    );

    let data = env.ok(alice, APPLY, json!({"spec": spec2})).await;
    assert_eq!(
        data["applyBatchChange"]["currentSpec"]["id"],
        spec2.as_str()
    );
    let after = env.changesets(bc.id).await;
    assert_eq!(after.len(), 3);
    let public = after.iter().find(|c| c.id == public_cs).unwrap();
    assert_eq!(public.previous_spec_id, cs[0].current_spec_id);
    assert_ne!(public.current_spec_id, cs[0].current_spec_id);
    assert_eq!(
        public.reconciler_state,
        sourcegull_batches::store::ReconcilerState::Queued
    );
    let other = after.iter().find(|c| c.id == other_cs).unwrap();
    assert!(other.batch_change_ids[&bc.id].detach);
    assert!(
        after
            .iter()
            .any(|c| c.id > other_cs && c.repo_id == env.public)
    );

    // Re-apply without the published changeset: closed and archived.
    let (spec3, _) = env
        .alice_spec(
            "fix",
            &[branch_spec(env.public, "more", "More fixes", json!(true))],
        )
        .await;
    let p = env.preview(alice, &spec3, json!({})).await;
    let detach: Vec<_> = targets(&p)
        .into_iter()
        .filter(|(t, _)| t == "VisibleApplyPreviewTargetsDetach")
        .collect();
    assert_eq!(
        detach,
        vec![(
            "VisibleApplyPreviewTargetsDetach".into(),
            vec!["ARCHIVE", "CLOSE"]
        )]
    );
    assert_eq!(p["stats"]["archive"], 1);
    env.ok(alice, APPLY, json!({"spec": spec3})).await;
    let public = ChangesetStore::new(&env.db)
        .get(public_cs)
        .await
        .unwrap()
        .unwrap();
    assert!(public.closing);
    assert!(public.archived_in(bc.id));

    // The bulk change lists it as archived only.
    let data = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { ... on BatchChange {
                changesets { totalCount }
                archived: changesets(onlyArchived: true) { totalCount nodes { id } }
            } } }",
            json!({"id": marshal_id("BatchChange", bc.id)}),
        )
        .await;
    assert_eq!(data["node"]["archived"]["totalCount"], 1);
    assert_eq!(
        data["node"]["archived"]["nodes"][0]["id"],
        marshal_id("Changeset", public_cs)
    );

    // Bringing it back re-attaches it and cancels the close.
    let (spec4, _) = env
        .alice_spec(
            "fix",
            &[
                branch_spec(env.public, "fix", "Fix it better", json!(true)),
                branch_spec(env.public, "more", "More fixes", json!(true)),
            ],
        )
        .await;
    env.ok(alice, APPLY, json!({"spec": spec4})).await;
    let public = ChangesetStore::new(&env.db)
        .get(public_cs)
        .await
        .unwrap()
        .unwrap();
    assert!(!public.closing && !public.archived_in(bc.id));
}

#[tokio::test]
async fn publication_states_and_imports() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let alice = env.alice;
    let (spec, ids) = env
        .alice_spec(
            "ui",
            &[
                branch_spec(env.public, "ui", "Decided in the UI", Value::Null),
                branch_spec(env.other, "ui", "Decided in the spec", json!(true)),
                import_spec(env.public, "42"),
            ],
        )
        .await;
    let p = env.preview(alice, &spec, json!({})).await;
    assert_eq!(
        targets(&p),
        vec![
            ("VisibleApplyPreviewTargetsAttach".into(), vec![]),
            (
                "VisibleApplyPreviewTargetsAttach".into(),
                vec!["PUSH", "PUBLISH"]
            ),
            ("VisibleApplyPreviewTargetsAttach".into(), vec!["IMPORT"]),
        ]
    );
    let states = json!([{"changesetSpec": ids[0], "publicationState": "draft"}]);
    let p = env.preview(alice, &spec, json!({"states": states})).await;
    assert_eq!(ops(&p["nodes"][0]), vec!["PUSH", "PUBLISH_DRAFT"]);
    assert_eq!(p["stats"]["publishDraft"], 1);

    // Errors: a spec that sets `published`, a spec twice, a spec of another bulk change spec.
    let bad = json!([{"changesetSpec": ids[1], "publicationState": true}]);
    let (code, msg) = env
        .err(alice, PREVIEW, json!({"spec": spec, "states": bad}))
        .await;
    assert_eq!(code, "INVALID_INPUT");
    assert!(msg.contains("published field"), "{msg}");
    let twice = json!([
        {"changesetSpec": ids[0], "publicationState": true},
        {"changesetSpec": ids[0], "publicationState": false}
    ]);
    let (code, msg) = env
        .err(alice, APPLY, json!({"spec": spec, "states": twice}))
        .await;
    assert_eq!(code, "INVALID_INPUT");
    assert!(msg.contains("duplicate"), "{msg}");
    let stray = env
        .upload(alice, &[branch_spec(env.public, "x", "Stray", Value::Null)])
        .await;
    let foreign = json!([{"changesetSpec": stray[0], "publicationState": true}]);
    let (code, msg) = env
        .err(alice, APPLY, json!({"spec": spec, "states": foreign}))
        .await;
    assert_eq!(code, "INVALID_INPUT");
    assert!(msg.contains("not found"), "{msg}");

    // Applied with the override; the import is tracked, not owned.
    let states = json!([{"changesetSpec": ids[0], "publicationState": true}]);
    let data = env
        .ok(alice, CREATE, json!({"spec": spec, "states": states}))
        .await;
    let bc = bc_key(&data["createBatchChange"]["id"]);
    let cs = env.changesets(bc).await;
    assert_eq!(cs.len(), 3);
    assert_eq!(
        cs[0].ui_publication_state,
        Some(UiPublicationState::Published)
    );
    assert_eq!(cs[1].ui_publication_state, None);
    let imported = &cs[2];
    assert_eq!(imported.external_id.as_deref(), Some("42"));
    assert_eq!(imported.owned_by_batch_change_id, None);
    assert!(imported.current_spec_id.is_none());

    // Another bulk change importing the same merge request shares the changeset.
    let (other_spec, _) = env
        .alice_spec("other", &[import_spec(env.public, "42")])
        .await;
    let p = env.preview(alice, &other_spec, json!({})).await;
    assert_eq!(
        targets(&p),
        vec![("VisibleApplyPreviewTargetsUpdate".into(), vec!["IMPORT"])]
    );
    let data = env.ok(alice, APPLY, json!({"spec": other_spec})).await;
    let bc2 = bc_key(&data["applyBatchChange"]["id"]);
    let shared = ChangesetStore::new(&env.db)
        .get(imported.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        shared.batch_change_ids.contains_key(&bc) && shared.batch_change_ids.contains_key(&bc2)
    );

    // Dropped from the first bulk change: detached, not closed.
    let (spec2, _) = env
        .alice_spec(
            "ui",
            &[
                branch_spec(env.public, "ui", "Decided in the UI", Value::Null),
                branch_spec(env.other, "ui", "Decided in the spec", json!(true)),
            ],
        )
        .await;
    let p = env
        .preview(alice, &spec2, json!({"action": "DETACH"}))
        .await;
    // Still waiting to be imported for the other bulk change.
    assert_eq!(
        targets(&p),
        vec![(
            "VisibleApplyPreviewTargetsDetach".into(),
            vec!["DETACH", "IMPORT"]
        )]
    );
    env.ok(alice, APPLY, json!({"spec": spec2})).await;
    let shared = ChangesetStore::new(&env.db)
        .get(imported.id)
        .await
        .unwrap()
        .unwrap();
    assert!(shared.batch_change_ids[&bc].detach && !shared.closing);
    // The UI choice is kept when a re-apply does not repeat it.
    let kept = ChangesetStore::new(&env.db)
        .get(cs[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.ui_publication_state,
        Some(UiPublicationState::Published)
    );
}

#[tokio::test]
async fn batch_change_checks_permissions_and_rollback() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let alice = env.alice;
    let (spec1, _) = env
        .alice_spec("checks", &[branch_spec(env.public, "a", "A", json!(false))])
        .await;

    // Someone else may not apply alice's spec, nor anyone without BATCH_CHANGES#WRITE.
    let (code, _) = env.err(env.bob, APPLY, json!({"spec": spec1})).await;
    assert_eq!(code, "FORBIDDEN");
    let mut read_only = env.actor(alice).await;
    read_only.permissions = [Permission::BatchChangesRead]
        .into_iter()
        .collect::<PermissionSet>();
    let resp = env.run_as(read_only, APPLY, json!({"spec": spec1})).await;
    assert_eq!(first_error(APPLY, &resp).0, "FORBIDDEN");
    env.config.store(
        2,
        SiteConfig {
            batch_changes_enabled: false,
            ..SiteConfig::default()
        },
    );
    let (code, _) = env.err(alice, APPLY, json!({"spec": spec1})).await;
    assert_eq!(code, "ErrBatchChangesDisabled");
    env.config.store(3, SiteConfig::default());

    let data = env.ok(alice, CREATE, json!({"spec": spec1})).await;
    let bc = bc_key(&data["createBatchChange"]["id"]);

    // createBatchChange fails once it exists; ensureBatchChange must name it.
    let (spec2, _) = env
        .alice_spec(
            "checks",
            &[branch_spec(env.public, "a", "A2", json!(false))],
        )
        .await;
    let (code, _) = env.err(alice, CREATE, json!({"spec": spec2})).await;
    assert_eq!(code, "ErrMatchingBatchChangeExists");
    let (other_spec, _) = env
        .alice_spec("other", &[branch_spec(env.other, "a", "A", json!(false))])
        .await;
    let data = env.ok(alice, APPLY, json!({"spec": other_spec})).await;
    let other_bc = data["applyBatchChange"]["id"].clone();
    let (code, _) = env
        .err(alice, APPLY, json!({"spec": spec2, "ensure": other_bc}))
        .await;
    assert_eq!(code, "ErrEnsureBatchChangeFailed");
    let data = env
        .ok(
            alice,
            APPLY,
            json!({"spec": spec2, "ensure": marshal_id("BatchChange", bc)}),
        )
        .await;
    assert_eq!(data["applyBatchChange"]["lastApplier"]["username"], "alice");

    // A closed bulk change cannot be applied to.
    BatchChangeStore::new(&env.db)
        .close(bc, false)
        .await
        .unwrap();
    let (spec3, _) = env
        .alice_spec(
            "checks",
            &[branch_spec(env.public, "a", "A3", json!(false))],
        )
        .await;
    let (code, _) = env.err(alice, APPLY, json!({"spec": spec3})).await;
    assert_eq!(code, "ErrApplyClosedBatchChange");

    // A failure half way leaves nothing behind.
    sourcegull_db::sqlx::query(&format!(
        "CREATE FUNCTION fail_other() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.repo_id = {} THEN RAISE EXCEPTION 'boom'; END IF;
             RETURN NEW;
         END $$",
        env.other.get()
    ))
    .execute(env.db.pool())
    .await
    .unwrap();
    sourcegull_db::sqlx::query(
        "CREATE TRIGGER fail_other BEFORE INSERT ON changesets
         FOR EACH ROW EXECUTE FUNCTION fail_other()",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    let (failing, _) = env
        .alice_spec(
            "atomic",
            &[
                branch_spec(env.public, "a", "A", json!(false)),
                branch_spec(env.other, "a", "A", json!(false)),
            ],
        )
        .await;
    let resp = env.run(alice, APPLY, json!({"spec": failing})).await;
    assert!(!resp.errors.is_empty());
    assert!(
        BatchChangeStore::new(&env.db)
            .get_by_name(Namespace::User(alice), "atomic")
            .await
            .unwrap()
            .is_none()
    );
    let attached: Option<i64> = sourcegull_db::sqlx::query_scalar(
        "SELECT batch_change_id FROM batch_specs WHERE spec->>'name' = 'atomic'",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(attached, None);
    let total: i64 = sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM changesets")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(total, 2, "only the earlier bulk changes' changesets");
}

#[tokio::test]
async fn hidden_repositories_and_diffs() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let (alice, admin) = (env.alice, env.admin);
    // A site admin prepares a spec in alice's namespace that touches a private repository.
    let ids = env
        .upload(
            admin,
            &[
                branch_spec(env.public, "h", "Visible", json!(true)),
                branch_spec(env.private, "h", "Secret", json!(true)),
            ],
        )
        .await;
    let spec = env
        .batch_spec(admin, alice, &spec_yaml("hidden", "Hidden"), &ids)
        .await;

    let p = env.preview(alice, &spec, json!({})).await;
    assert_eq!(p["totalCount"], 2);
    let hidden = &p["nodes"][1];
    assert_eq!(hidden["__typename"], "HiddenChangesetApplyPreview");
    assert_eq!(
        hidden["targets"]["__typename"],
        "HiddenApplyPreviewTargetsAttach"
    );
    assert_eq!(ops(hidden), vec!["PUSH", "PUBLISH"]);
    assert!(
        hidden["targets"]
            .get("changesetSpec")
            .unwrap()
            .get("description")
            .is_none()
    );
    assert_eq!(p["stats"]["added"], 2);
    // A text search leaves hidden entries out, even when it would match them.
    let f = env.preview(alice, &spec, json!({"search": "Secret"})).await;
    assert_eq!(f["totalCount"], 0);
    let f = env.preview(admin, &spec, json!({"search": "Secret"})).await;
    assert_eq!(f["totalCount"], 1);

    // Alice cannot apply changes to a repository she cannot read; the admin can.
    let (code, _) = env.err(alice, APPLY, json!({"spec": spec})).await;
    assert_eq!(code, "NOT_FOUND");
    let data = env.ok(admin, APPLY, json!({"spec": spec})).await;
    assert_eq!(
        data["applyBatchChange"]["url"],
        "/users/alice/bulk-changes/hidden"
    );

    // Changeset spec diffs, parsed from the uploaded diff.
    let data = env
        .ok(
            alice,
            "query($id: ID!) { node(id: $id) { ... on VisibleChangesetSpec {
                description { ... on GitBranchChangesetDescription {
                    diff {
                        baseRepository { name }
                        fileDiffs(first: 10) {
                            totalCount rawDiff diffStat { added deleted }
                            nodes {
                                oldPath newPath internalID stat { added deleted }
                                hunks { oldRange { startLine lines } newRange { startLine lines } body }
                            }
                        }
                    }
                } }
            } } }",
            json!({"id": ids[0]}),
        )
        .await;
    let diff = &data["node"]["description"]["diff"];
    assert_eq!(diff["baseRepository"]["name"], PUBLIC);
    let files = &diff["fileDiffs"];
    assert_eq!(files["totalCount"], 1);
    assert_eq!(files["rawDiff"], DIFF);
    assert_eq!(files["diffStat"], json!({"added": 1, "deleted": 0}));
    let f = &files["nodes"][0];
    assert_eq!(
        (f["oldPath"].clone(), f["newPath"].clone()),
        (json!("README.md"), json!("README.md"))
    );
    assert_eq!(
        f["hunks"][0]["oldRange"],
        json!({"startLine": 1, "lines": 1})
    );
    assert_eq!(
        f["hunks"][0]["newRange"],
        json!({"startLine": 1, "lines": 2})
    );
    assert_eq!(f["hunks"][0]["body"], " # Title\n+Hello World\n");
}

/// A diff changing, deleting and adding a file of [`preview_file_contents`]' repository.
const FILES_DIFF: &str = "diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -2,2 +2,3 @@ # Title
 two
-three
+3
+four
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-old
diff --git a/docs/new.txt b/docs/new.txt
new file mode 100644
--- /dev/null
+++ b/docs/new.txt
@@ -0,0 +1 @@
+new
\\ No newline at end of file
";

#[tokio::test]
async fn preview_file_contents() {
    use sourcegull_testutil::git::{RepoNode, SourceRepo};
    let Some(env) = setup().await else { return };
    let src = SourceRepo::new();
    let base = src.commit(
        "base",
        &[
            ("README.md", "# Title\ntwo\nthree\nend\n"),
            ("old.txt", "old\n"),
        ],
    );
    let node = RepoNode::start(src.remote()).await;
    node.clone_repo(env.public, PUBLIC).await;
    let schema = build_schema(Services {
        db: env.db.clone(),
        auth: env.auth.clone(),
        config: env.config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz: Checker::new(env.db.clone(), env.config.clone()),
        app: None,
    });
    let env = Env { schema, ..env };
    let mut spec: Value =
        serde_json::from_str(&branch_spec(env.public, "files", "Files", json!(true))).unwrap();
    spec["baseRev"] = json!(base);
    spec["commits"][0]["diff"] = json!(FILES_DIFF);
    let ids = env.upload(env.admin, &[spec.to_string()]).await;

    let query = "query($id: ID!, $paths: [String!]) { node(id: $id) { ... on VisibleChangesetSpec {
        description { ... on GitBranchChangesetDescription { diff { fileDiffs(paths: $paths) {
            totalCount
            nodes {
                oldPath newPath
                oldFile { path content commit { oid } }
                newFile { path content byteSize url }
                mostRelevantFile { __typename path url }
            }
        } } } }
    } } }";
    let data = env
        .ok(env.alice, query, json!({"id": ids[0], "paths": null}))
        .await;
    let files = &data["node"]["description"]["diff"]["fileDiffs"];
    assert_eq!(files["totalCount"], 3);
    let [readme, old, new] = &files["nodes"].as_array().unwrap()[..] else {
        panic!("{files}")
    };
    // A changed file: the old file at the base revision, the new one with the diff applied.
    assert_eq!(readme["oldFile"]["content"], "# Title\ntwo\nthree\nend\n");
    assert_eq!(readme["oldFile"]["commit"]["oid"], json!(base));
    assert_eq!(readme["newFile"]["content"], "# Title\ntwo\n3\nfour\nend\n");
    assert_eq!(readme["newFile"]["url"], "");
    assert_eq!(readme["mostRelevantFile"]["__typename"], "VirtualFile");
    // A deleted file has no new file; the most relevant one is the old one, with its page.
    assert!(old["newFile"].is_null());
    assert_eq!(old["oldFile"]["content"], "old\n");
    assert_eq!(old["mostRelevantFile"]["__typename"], "GitBlob");
    assert_eq!(old["mostRelevantFile"]["path"], "old.txt");
    assert!(
        old["mostRelevantFile"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/-/blob/old.txt"),
        "{old}"
    );
    // An added file has no old file.
    assert!(new["oldFile"].is_null());
    assert_eq!(new["newFile"]["content"], "new");
    assert_eq!(new["newFile"]["byteSize"], 3);

    // `paths` narrows the files to those at or under the paths.
    let data = env
        .ok(
            env.alice,
            query,
            json!({"id": ids[0], "paths": ["docs", "old.txt"]}),
        )
        .await;
    let files = &data["node"]["description"]["diff"]["fileDiffs"];
    assert_eq!(files["totalCount"], 2);
    assert_eq!(files["nodes"][0]["oldPath"], "old.txt");
    assert_eq!(files["nodes"][1]["newPath"], "docs/new.txt");
}
