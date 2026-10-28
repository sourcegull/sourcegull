//! Code intelligence configuration policies over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and a repo node started in-process: listing honours
//! repository permissions, site admins create, update and delete (not protected policies),
//! previews of repository and git object patterns, and an index's retention overview.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::{marshal_id, marshal_string_id};
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const PUBLIC: &str = "gitlab.example.com/acme/public";
const PRIVATE: &str = "gitlab.example.com/acme/private";

const LIST: &str = "query($repo: ID, $query: String, $protected: Boolean, $retention: Boolean) {
    codeIntelligenceConfigurationPolicies(repository: $repo, query: $query,
            protected: $protected, forDataRetention: $retention) {
        totalCount
        nodes { id name type pattern protected retentionDurationHours repository { name }
                repositoryPatterns embeddingsEnabled }
    }
}";

const CREATE: &str = "mutation($repo: ID, $patterns: [String!], $name: String!,
        $type: GitObjectType!, $pattern: String!, $hours: Int) {
    createCodeIntelligenceConfigurationPolicy(repository: $repo, repositoryPatterns: $patterns,
            name: $name, type: $type, pattern: $pattern, retentionEnabled: true,
            retentionDurationHours: $hours, retainIntermediateCommits: false,
            indexingEnabled: false, indexIntermediateCommits: false) {
        id name protected repository { name } repositoryPatterns
    }
}";

const UPDATE: &str = "mutation($id: ID!, $name: String!, $type: GitObjectType!,
        $pattern: String!, $hours: Int, $intermediate: Boolean!) {
    updateCodeIntelligenceConfigurationPolicy(id: $id, name: $name, type: $type,
            pattern: $pattern, retentionEnabled: true, retentionDurationHours: $hours,
            retainIntermediateCommits: $intermediate, indexingEnabled: false,
            indexIntermediateCommits: false) { alwaysNil }
}";

const DELETE: &str = "mutation($id: ID!) {
    deleteCodeIntelligenceConfigurationPolicy(policy: $id) { alwaysNil }
}";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    public: RepoId,
    private: RepoId,
    m1: String,
    m2: String,
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, private, external_id, external_service_type,
                external_service_id)
             VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(name)
        .bind(private)
        .bind(ext)
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
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
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let src = SourceRepo::new();
    let m1 = src.commit("m1", &[("a.txt", "1\n")]);
    src.git(&["tag", "v1"]);
    src.git(&["branch", "feature"]);
    let m2 = src.commit("m2", &[("a.txt", "2\n")]);
    let node = RepoNode::start(src.remote()).await;
    let public = insert_repo(&db, PUBLIC, "1", false).await;
    let private = insert_repo(&db, PRIVATE, "2", true).await;
    for (id, name) in [(public, PUBLIC), (private, PRIVATE)] {
        node.clone_repo(id, name).await;
    }
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz: Checker::new(db.clone(), config),
        app: None,
    });
    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        db,
        auth,
        schema,
        admin,
        alice,
        public,
        private,
        m1,
        m2,
    })
}

impl Env {
    async fn run(&self, actor: UserId, query: &str, vars: Value) -> Response {
        let actor = self.auth.actor_for(actor, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, actor: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn code(&self, actor: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(actor, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        err.extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Names of the policies `actor` lists with `vars`.
    async fn names(&self, actor: UserId, vars: Value) -> Vec<String> {
        let data = self.ok(actor, LIST, vars).await;
        data["codeIntelligenceConfigurationPolicies"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A completed upload of `repo` at `commit`, uploaded `days` ago.
    async fn upload(&self, repo: RepoId, commit: &str, root: &str, days: i32) -> i64 {
        let id = CodeIntelStore::new(&self.db)
            .create_upload(&NewUpload {
                repo_id: repo,
                commit: commit.into(),
                root: root.into(),
                indexer: "scip-test".into(),
                indexer_version: "1".into(),
                num_parts: 1,
                uploader: None,
                associated_index_id: None,
                ci_job: None,
            })
            .await
            .unwrap();
        sourcegull_db::sqlx::query(
            "UPDATE codeintel_uploads SET state = 'completed', upload_size = 1,
                uploaded_at = now() - make_interval(days => $2)
             WHERE id = $1",
        )
        .bind(id)
        .bind(days)
        .execute(self.db.pool())
        .await
        .unwrap();
        id
    }
}

fn repo_id(id: RepoId) -> String {
    marshal_id("Repository", i64::from(id.get()))
}

fn index_id(id: i64) -> String {
    marshal_string_id("PreciseIndex", &format!("U:{id}"))
}

#[tokio::test]
async fn policies_crud_and_permissions() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let defaults = vec![
        "Default tip-of-branch retention policy".to_owned(),
        "Default tag retention policy".to_owned(),
        "Default commit retention policy".to_owned(),
    ];
    assert_eq!(env.names(env.alice, json!({})).await, defaults);
    let data = env.ok(env.alice, LIST, json!({})).await;
    let tip = &data["codeIntelligenceConfigurationPolicies"]["nodes"][0];
    assert_eq!(tip["type"], "GIT_TREE");
    assert_eq!(tip["pattern"], "*");
    assert_eq!(tip["protected"], true);
    assert_eq!(tip["retentionDurationHours"], 2016);
    assert_eq!(tip["embeddingsEnabled"], false);
    let tip_id = tip["id"].as_str().unwrap().to_owned();

    // Only site admins write.
    let vars = json!({"name": "Releases", "type": "GIT_TAG", "pattern": "v*", "hours": 24});
    assert_eq!(env.code(env.alice, CREATE, vars.clone()).await, "FORBIDDEN");
    let created = env.ok(env.admin, CREATE, vars).await;
    let releases = created["createCodeIntelligenceConfigurationPolicy"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A policy for the private repository is hidden from those who can't read it.
    let secret = env
        .ok(
            env.admin,
            CREATE,
            json!({"repo": repo_id(env.private), "name": "Secret", "type": "GIT_TREE",
                   "pattern": "main"}),
        )
        .await;
    let secret = &secret["createCodeIntelligenceConfigurationPolicy"];
    assert_eq!(secret["repository"]["name"], PRIVATE);
    let secret_id = secret["id"].as_str().unwrap().to_owned();
    assert!(
        env.names(env.admin, json!({}))
            .await
            .contains(&"Secret".to_owned())
    );
    assert!(
        !env.names(env.alice, json!({}))
            .await
            .contains(&"Secret".to_owned())
    );
    let node = "query($id: ID!) { node(id: $id) {
        ... on CodeIntelligenceConfigurationPolicy { name } } }";
    assert_eq!(
        env.ok(env.alice, node, json!({"id": secret_id})).await["node"],
        Value::Null
    );
    assert_eq!(
        env.ok(env.admin, node, json!({"id": secret_id})).await["node"]["name"],
        "Secret"
    );
    // Filtering by repository: the private one is unknown to alice.
    assert!(
        env.names(env.alice, json!({"repo": repo_id(env.private)}))
            .await
            .is_empty()
    );
    let public_names = env
        .names(
            env.admin,
            json!({"repo": repo_id(env.public), "protected": false}),
        )
        .await;
    assert_eq!(public_names, vec!["Releases".to_owned()]);
    assert_eq!(
        env.names(env.admin, json!({"query": "secret"})).await,
        vec!["Secret".to_owned()]
    );

    // Repository patterns apply by name.
    env.ok(
        env.admin,
        CREATE,
        json!({"patterns": ["*/public"], "name": "Public only", "type": "GIT_TREE",
               "pattern": "*"}),
    )
    .await;
    assert!(
        env.names(env.admin, json!({"repo": repo_id(env.public)}))
            .await
            .contains(&"Public only".to_owned())
    );
    assert!(
        !env.names(env.admin, json!({"repo": repo_id(env.private)}))
            .await
            .contains(&"Public only".to_owned())
    );

    // Validation.
    for vars in [
        json!({"name": " ", "type": "GIT_TAG", "pattern": "v*"}),
        json!({"name": "x", "type": "GIT_TAG", "pattern": "[v"}),
        json!({"name": "x", "type": "GIT_BLOB", "pattern": "v*"}),
        json!({"name": "x", "type": "GIT_TAG", "pattern": "v*", "hours": 0}),
        json!({"name": "x", "type": "GIT_TAG", "pattern": "v*", "patterns": ["{"]}),
        json!({"name": "x", "type": "GIT_TAG", "pattern": "v*", "patterns": ["*"],
               "repo": repo_id(env.public)}),
    ] {
        assert_eq!(env.code(env.admin, CREATE, vars).await, "VALIDATION");
    }

    // Protected policies: durations change, their shape and existence don't.
    let tip_update = json!({"id": tip_id, "name": "Default tip-of-branch retention policy",
        "type": "GIT_TREE", "pattern": "*", "hours": 100, "intermediate": false});
    env.ok(env.admin, UPDATE, tip_update.clone()).await;
    let mut changed = tip_update.clone();
    changed["pattern"] = json!("main");
    assert_eq!(env.code(env.admin, UPDATE, changed).await, "INVALID_INPUT");
    assert_eq!(
        env.code(env.alice, UPDATE, tip_update.clone()).await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(env.admin, DELETE, json!({"id": tip_id})).await,
        "INVALID_INPUT"
    );
    let data = env
        .ok(env.admin, LIST, json!({"query": "tip-of-branch"}))
        .await;
    assert_eq!(
        data["codeIntelligenceConfigurationPolicies"]["nodes"][0]["retentionDurationHours"],
        100
    );

    // Updating and deleting a policy of one's own.
    env.ok(
        env.admin,
        UPDATE,
        json!({"id": releases, "name": "Release tags", "type": "GIT_TAG", "pattern": "release-*",
               "hours": null, "intermediate": false}),
    )
    .await;
    assert!(
        env.names(env.admin, json!({}))
            .await
            .contains(&"Release tags".to_owned())
    );
    assert_eq!(
        env.code(env.alice, DELETE, json!({"id": releases})).await,
        "FORBIDDEN"
    );
    env.ok(env.admin, DELETE, json!({"id": releases})).await;
    assert_eq!(
        env.code(env.admin, DELETE, json!({"id": releases})).await,
        "NOT_FOUND"
    );
    // Only retention policies.
    assert_eq!(
        env.names(env.admin, json!({"retention": false})).await,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn previews_and_retention_overview() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let preview = "query($patterns: [String!]!) { previewRepositoryFilter(patterns: $patterns) {
        nodes { name } totalCount totalMatches matchesAllRepos limit } }";
    let data = env
        .ok(env.admin, preview, json!({"patterns": ["*/public"]}))
        .await;
    assert_eq!(
        data["previewRepositoryFilter"],
        json!({"nodes": [{"name": PUBLIC}], "totalCount": 1, "totalMatches": 1,
               "matchesAllRepos": false, "limit": null})
    );
    let data = env.ok(env.admin, preview, json!({"patterns": ["*"]})).await;
    assert_eq!(data["previewRepositoryFilter"]["matchesAllRepos"], true);
    assert_eq!(
        env.code(env.alice, preview, json!({"patterns": ["*"]}))
            .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(env.admin, preview, json!({"patterns": ["["]}))
            .await,
        "VALIDATION"
    );

    let objects = "query($name: String!, $type: GitObjectType!, $pattern: String!) {
        repository(name: $name) {
            previewGitObjectFilter(type: $type, pattern: $pattern,
                    countObjectsYoungerThanHours: 24) {
                nodes { name rev } totalCount totalCountYoungerThanThreshold
            }
        }
    }";
    let names = |data: &Value| -> Vec<(String, String)> {
        data["repository"]["previewGitObjectFilter"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["name"].as_str().unwrap().to_owned(),
                    n["rev"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let tags = env
        .ok(
            env.alice,
            objects,
            json!({"name": PUBLIC, "type": "GIT_TAG", "pattern": "v*"}),
        )
        .await;
    assert_eq!(names(&tags), vec![("v1".to_owned(), env.m1.clone())]);
    // The fixture's commits are from 2023.
    assert_eq!(
        tags["repository"]["previewGitObjectFilter"]["totalCountYoungerThanThreshold"],
        0
    );
    let branches = env
        .ok(
            env.alice,
            objects,
            json!({"name": PUBLIC, "type": "GIT_TREE", "pattern": "*"}),
        )
        .await;
    assert_eq!(
        names(&branches),
        vec![
            ("main".to_owned(), env.m2.clone()),
            ("feature".to_owned(), env.m1.clone())
        ]
    );
    let commit = env
        .ok(
            env.alice,
            objects,
            json!({"name": PUBLIC, "type": "GIT_COMMIT", "pattern": env.m1}),
        )
        .await;
    assert_eq!(names(&commit), vec![(env.m1.clone(), env.m1.clone())]);
    let missing = env
        .ok(
            env.alice,
            objects,
            json!({"name": PUBLIC, "type": "GIT_COMMIT", "pattern": "nope"}),
        )
        .await;
    assert_eq!(
        missing["repository"]["previewGitObjectFilter"]["totalCount"],
        0
    );

    // Retention: the head's upload is kept by the tip of the default branch; the tag and
    // the feature branch's tip keep the one at m1.
    let head = env.upload(env.public, &env.m2, "", 500).await;
    let tagged = env.upload(env.public, &env.m1, "", 10).await;
    let overview = "query($id: ID!, $matchesOnly: Boolean!, $query: String) {
        node(id: $id) { ... on PreciseIndex {
            expired
            retentionPolicyOverview(matchesOnly: $matchesOnly, query: $query) {
                totalCount
                nodes { configurationPolicy { name } matches protectingCommits }
            }
        } }
    }";
    let rows = |data: &Value| -> Vec<(Option<String>, bool, Vec<String>)> {
        data["node"]["retentionPolicyOverview"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["configurationPolicy"]["name"].as_str().map(str::to_owned),
                    n["matches"].as_bool().unwrap(),
                    serde_json::from_value(n["protectingCommits"].clone()).unwrap(),
                )
            })
            .collect()
    };
    let data = env
        .ok(
            env.alice,
            overview,
            json!({"id": index_id(head), "matchesOnly": true}),
        )
        .await;
    assert_eq!(data["node"]["expired"], false);
    assert_eq!(rows(&data), vec![(None, true, vec![])]);
    let data = env
        .ok(
            env.alice,
            overview,
            json!({"id": index_id(tagged), "matchesOnly": false}),
        )
        .await;
    assert_eq!(
        rows(&data),
        vec![
            (
                Some("Default tip-of-branch retention policy".to_owned()),
                true,
                vec![]
            ),
            (
                Some("Default tag retention policy".to_owned()),
                true,
                vec![]
            ),
            (
                Some("Default commit retention policy".to_owned()),
                false,
                vec![]
            ),
        ]
    );
    let data = env
        .ok(
            env.alice,
            overview,
            json!({"id": index_id(tagged), "matchesOnly": true, "query": "TAG"}),
        )
        .await;
    assert_eq!(
        rows(&data),
        vec![(
            Some("Default tag retention policy".to_owned()),
            true,
            vec![]
        )]
    );

    // Dependents: indexes that use a package the given one defines.
    for (upload, kind) in [(tagged, "export"), (head, "import")] {
        sourcegull_db::sqlx::query(
            "INSERT INTO codeintel_package_monikers (upload_id, kind, scheme, manager, name,
                version) VALUES ($1, $2, 'scip-test', 'cargo', 'calc', '1.0')",
        )
        .bind(upload)
        .bind(kind)
        .execute(env.db.pool())
        .await
        .unwrap();
    }
    let dependents = "query($id: ID!) { preciseIndexes(dependentOf: $id) {
        totalCount nodes { id } } }";
    let data = env
        .ok(env.alice, dependents, json!({"id": index_id(tagged)}))
        .await;
    assert_eq!(
        data["preciseIndexes"],
        json!({"totalCount": 1, "nodes": [{"id": index_id(head)}]})
    );
    let data = env
        .ok(env.alice, dependents, json!({"id": index_id(head)}))
        .await;
    assert_eq!(data["preciseIndexes"]["totalCount"], 0);
}
