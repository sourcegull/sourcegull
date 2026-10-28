//! The worker role's feature tasks: which singletons (by lease name) and how many per-process
//! tasks start, with and without repository nodes; and the features' parts of the GraphQL
//! schema.

use std::sync::Arc;

use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::Db;
use sourcegull_feature::WorkerContext;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry};

fn context(dir: &std::path::Path, with_nodes: bool) -> WorkerContext {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://sourcegull@127.0.0.1:1/unused")
        .unwrap();
    let blobs: sourcegull_blobstore::Blobs =
        Arc::new(sourcegull_blobstore::FsBlobStore::open(dir).unwrap());
    WorkerContext {
        db: Db::from_pool(pool),
        config: ConfigHandle::new(1, SiteConfig::default()),
        keyring: None,
        blobs,
        codeintel_dir: dir.join("codeintel"),
        repo_clients: with_nodes
            .then(|| sourcegull::routing::repo_clients(&["127.0.0.1:1".into()])),
        worker_id: "test".into(),
    }
}

/// Lease names of the singletons that run without repository nodes.
const ALWAYS: &[&str] = &[
    sourcegull_authz::worker::PERMS_SYNC_SCHEDULER,
    sourcegull_codeintel_scip::janitor::UPLOAD_JANITOR,
    sourcegull_searchjobs::janitor::JANITOR,
    sourcegull_outbound_webhooks::JANITOR,
    sourcegull_oauth_server::janitor::JANITOR,
    sourcegull_batches_reconciler::RETENTION_JANITOR,
    sourcegull_batches_reconciler::PREVIEWS_JANITOR,
    sourcegull_batches_reconciler::SYNCER,
    sourcegull_batches_reconciler::SCHEDULER,
    sourcegull_insights::JANITOR,
    sourcegull_own::teamsync::TEAM_SYNC,
    sourcegull_oobmigration::OOB_MIGRATION_RUNNER,
    sourcegull_analytics::ROLLUP,
    sourcegull_repos::worker::SYNC_SCHEDULER,
];

/// Lease names of the singletons that need repository nodes.
const WITH_NODES: &[&str] = &[
    sourcegull_ranking::job::RANKER,
    sourcegull_autoindex::scheduler::AUTOINDEX_SCHEDULER,
    sourcegull_codeintel_policies::expirer::UPLOAD_EXPIRER,
    sourcegull_monitors::EVALUATOR,
    sourcegull_insights::ENQUEUER,
    sourcegull_own::jobs::SCHEDULER,
];

fn sorted(mut names: Vec<&'static str>) -> Vec<&'static str> {
    names.sort_unstable();
    names
}

#[tokio::test]
async fn worker_tasks_without_repo_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = sourcegull::worker::feature_tasks(&context(dir.path(), false)).unwrap();
    assert_eq!(sorted(tasks.singleton_names()), sorted(ALWAYS.to_vec()));
    // Queue workers and resetters: permissions, uploads, outbound webhooks, changesets,
    // changeset jobs, watch actions, discovery syncs.
    assert_eq!(tasks.workers.len(), 14);
}

#[tokio::test]
async fn worker_tasks_with_repo_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = sourcegull::worker::feature_tasks(&context(dir.path(), true)).unwrap();
    let names = tasks.singleton_names();
    let mut expected = ALWAYS.to_vec();
    expected.extend_from_slice(WITH_NODES);
    assert_eq!(names.len(), expected.len(), "lease names are unique");
    assert_eq!(sorted(names), sorted(expected));
    // Also: spec resolution, search jobs (one task for its three queues), watch triggers,
    // insights (one task for both queues), ownership signals.
    assert_eq!(tasks.workers.len(), 14 + 2 + 1 + 2 + 1 + 2);
}

#[test]
fn graphql_features() {
    type Graphql = sourcegull::features::Graphql;
    assert_eq!(
        Graphql::names(),
        [
            "saved-searches",
            "searchjobs",
            "codeintel",
            "executors",
            "batches",
            "outbound-webhooks",
            "notebooks",
            "monitors",
            "own",
            "insights",
        ]
    );
    let mut loaders = NodeRegistry::<Node>::default();
    Graphql::node_loaders(&mut loaders);
    assert_eq!(
        loaders.kinds(),
        sorted(vec![
            "SavedSearch",
            "SearchContext",
            "SearchJob",
            "PreciseIndex",
            "CodeIntelligenceConfigurationPolicy",
            "ExecutorSecret",
            "ExecutorSecretAccessLog",
            "BatchChange",
            "BatchSpec",
            "ChangesetSpec",
            "Changeset",
            "BatchSpecWorkspace",
            "BulkOperation",
            "BatchChangesCredential",
            "OutboundWebhook",
            "OutboundWebhookLog",
            "Notebook",
            "CodeMonitor",
            "Team",
            "CodeownersIngestedFile",
            "insight_view",
            "dashboard",
        ])
    );
    // The features' node types implement `Node` in the schema.
    let sdl = sourcegull::features::graphql_sdl();
    for ty in [
        "SavedSearch",
        "SearchJob",
        "PreciseIndex",
        "ExecutorSecretAccessLog",
        "ChangesetEvent",
        "OutboundWebhookLog",
        "Notebook",
        "Monitor",
        "CodeownersIngestedFile",
        "InsightsDashboard",
    ] {
        assert!(
            sdl.contains(&format!("type {ty} implements Node {{")),
            "{ty} implements Node"
        );
    }
}
