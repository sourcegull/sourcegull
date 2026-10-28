//! Monitoring: the domain gauges read from Postgres, and `sourcegull monitoring generate`.

use std::process::Command;

use sourcegull_db::{ActiveJob, Db, ExecutorInfo, migrate};
use sourcegull_testutil::TestDb;

#[tokio::test]
async fn collector_exports_domain_gauges() {
    let Some(t) = TestDb::new().await else {
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    db.executors()
        .heartbeat(
            &ExecutorInfo {
                hostname: "executor-1".to_owned(),
                queue_names: vec!["codeintel".to_owned()],
                ..Default::default()
            },
            &[ActiveJob {
                queue: "codeintel".to_owned(),
                id: 7,
            }],
        )
        .await
        .unwrap();

    let recorder = sourcegull_observability::build_recorder().unwrap();
    let handle = recorder.handle();
    {
        // The test runtime is single-threaded, so the thread-local recorder sees every gauge.
        let _guard = metrics::set_default_local_recorder(&recorder);
        sourcegull::metrics_collector::collect(db.pool())
            .await
            .unwrap();
        sourcegull::metrics_collector::collect_coverage(&db, true, true)
            .await
            .unwrap();
    }
    let out = handle.render();
    for line in [
        "sourcegull_executors_active 1",
        "sourcegull_executor_running_jobs 1",
        r#"sourcegull_queue_jobs{queue="repo_update_jobs",state="queued"} 0"#,
        r#"sourcegull_queue_jobs{queue="codeintel_index_jobs",state="failed"} 0"#,
        r#"sourcegull_queue_oldest_queued_seconds{queue="external_service_sync_jobs"} 0"#,
        r#"sourcegull_repos{clone_status="cloned"} 0"#,
        "sourcegull_repos_corrupted 0",
        r#"sourcegull_index_repos{status="indexed"} 0"#,
        "sourcegull_index_repos_errored 0",
        r#"sourcegull_repo_moves{state="failed"} 0"#,
        r#"sourcegull_codeintel_coverage_repos{state="failing"} 0"#,
        r#"sourcegull_codeintel_coverage_failing_repos{source="index_job"} 0"#,
    ] {
        assert!(out.contains(line), "missing {line:?} in\n{out}");
    }
    // This test's own connection is active; the server allows some connections.
    let gauge = |name: &str| -> f64 {
        out.lines()
            .find_map(|l| l.strip_prefix(name)?.trim().parse().ok())
            .unwrap_or_else(|| panic!("{name} missing in\n{out}"))
    };
    assert!(gauge(r#"sourcegull_postgres_connections{state="active"}"#) >= 1.0);
    assert!(gauge("sourcegull_postgres_max_connections") > 1.0);
    assert!(gauge("sourcegull_postgres_database_size_bytes") > 0.0);
    assert!(gauge("sourcegull_postgres_oldest_transaction_seconds") >= 0.0);
}

#[test]
fn generate_writes_dashboards_rules_and_docs() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_sourcegull"))
        .args(["--log-format=pretty", "monitoring", "generate", "--out"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    for path in [
        "grafana/sourcegull-web.json",
        "grafana/sourcegull-postgres.json",
        "prometheus/sourcegull-alerts.yml",
        "dashboards.md",
    ] {
        assert!(dir.path().join(path).is_file(), "{path} missing");
    }
    // The same content as the checked-in docs.
    let generated = std::fs::read_to_string(dir.path().join("alerts.md")).unwrap();
    let checked_in = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/observability/alerts.md"
    ))
    .unwrap();
    assert_eq!(generated, checked_in);
}
