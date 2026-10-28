use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sourcegull_pgqueue::*;
use sourcegull_testutil::TestDb;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, sqlx::FromRow)]
struct Payload {
    n: i32,
}

async fn setup(config: QueueConfig) -> Option<(TestDb, Queue<Payload>)> {
    let db = TestDb::new().await?;
    let sql = create_queue_table_sql(&config.table, &["n integer NOT NULL"]).unwrap();
    sqlx::raw_sql(&sql).execute(&db.pool).await.unwrap();
    let q = Queue::new(db.pool.clone(), config).unwrap();
    Some((db, q))
}

async fn insert(pool: &PgPool, n: i32) -> i64 {
    sqlx::query_scalar("INSERT INTO jobs (n) VALUES ($1) RETURNING id")
        .bind(n)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn state(q: &Queue<Payload>, id: i64) -> JobState {
    q.get(id).await.unwrap().unwrap().state
}

async fn wait_for<F: AsyncFn() -> bool>(timeout: Duration, f: F) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_dequeuers_never_share_rows() {
    let Some((db, q)) = setup(QueueConfig::new("jobs")).await else {
        return;
    };
    sqlx::query("INSERT INTO jobs (n) SELECT g FROM generate_series(1, 300) g")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut handles = Vec::new();
    for w in 0..16 {
        let q = q.clone();
        handles.push(tokio::spawn(async move {
            let mut got = Vec::new();
            loop {
                let batch = q.dequeue(&format!("w{w}"), 3).await.unwrap();
                if batch.is_empty() {
                    return got;
                }
                for r in batch {
                    assert_eq!(r.state, JobState::Processing);
                    assert_eq!(r.worker_hostname, format!("w{w}"));
                    got.push(r.id);
                }
            }
        }));
    }
    let mut seen = HashSet::new();
    for h in handles {
        for id in h.await.unwrap() {
            assert!(seen.insert(id), "row {id} dequeued twice");
        }
    }
    assert_eq!(seen.len(), 300);
    assert_eq!(q.queue_depth().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dequeue_order_and_payload() {
    let Some((db, q)) = setup(QueueConfig::new("jobs")).await else {
        return;
    };
    let a = insert(&db.pool, 7).await;
    let b = insert(&db.pool, 8).await;
    let got = q.dequeue("w", 10).await.unwrap();
    assert_eq!(got.iter().map(|r| r.id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(got[0].payload.n, 7);
    assert!(got[0].started_at.is_some() && got[0].last_heartbeat_at.is_some());
    assert!(q.mark_complete(a, "w").await.unwrap());
    assert!(!q.mark_complete(a, "w").await.unwrap(), "already completed");
    assert!(
        !q.mark_complete(b, "other").await.unwrap(),
        "not this worker's job"
    );
    assert_eq!(state(&q, a).await, JobState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_with_backoff_then_fails() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.max_failures = 3;
    cfg.backoff_base = Duration::from_secs(60);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let id = insert(&db.pool, 1).await;
    for attempt in 1..=3 {
        let got = q.dequeue("w", 1).await.unwrap();
        assert_eq!(got.len(), 1, "attempt {attempt}");
        let st = q
            .mark_errored(id, "w", &format!("boom {attempt}"))
            .await
            .unwrap();
        let rec = q.get(id).await.unwrap().unwrap();
        assert_eq!(rec.num_failures, attempt);
        assert_eq!(
            rec.failure_message.as_deref(),
            Some(format!("boom {attempt}").as_str())
        );
        if attempt < 3 {
            assert_eq!(st, Some(JobState::Queued));
            // Backoff: 60 s, then 120 s.
            let delay: f64 = sqlx::query_scalar(
                "SELECT extract(epoch FROM process_after - now())::float8 FROM jobs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            let want = 60.0 * f64::from(1 << (attempt - 1));
            assert!((delay - want).abs() < 5.0, "delay {delay}, want {want}");
            assert!(q.dequeue("w", 1).await.unwrap().is_empty(), "delayed");
            sqlx::query("UPDATE jobs SET process_after = now() WHERE id = $1")
                .bind(id)
                .execute(&db.pool)
                .await
                .unwrap();
        } else {
            assert_eq!(st, Some(JobState::Failed));
            assert!(rec.finished_at.is_some());
        }
    }
    assert!(q.dequeue("w", 1).await.unwrap().is_empty());

    let id2 = insert(&db.pool, 2).await;
    q.dequeue("w", 1).await.unwrap();
    assert!(q.mark_failed(id2, "w", "fatal").await.unwrap());
    assert_eq!(state(&q, id2).await, JobState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_jobs_reset_then_fail() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.stalled_after = Duration::from_millis(200);
    cfg.max_resets = 1;
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let id = insert(&db.pool, 1).await;
    let live = insert(&db.pool, 2).await;

    q.dequeue("w", 2).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    q.heartbeat(&[live], "w").await.unwrap();
    let r = q.reset_stalled().await.unwrap();
    assert_eq!((r.reset, r.failed), (vec![id], vec![]));
    let rec = q.get(id).await.unwrap().unwrap();
    assert_eq!((rec.state, rec.num_resets), (JobState::Queued, 1));
    assert_eq!(state(&q, live).await, JobState::Processing);
    assert!(
        !q.mark_complete(id, "w").await.unwrap(),
        "stale worker must not complete"
    );

    assert_eq!(q.dequeue("w2", 1).await.unwrap()[0].id, id);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = q.reset_stalled().await.unwrap();
    assert!(r.failed.contains(&id));
    let rec = q.get(id).await.unwrap().unwrap();
    assert_eq!((rec.state, rec.num_resets), (JobState::Failed, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphans_of_a_remote_worker_are_reset() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.max_resets = 1;
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let kept = insert(&db.pool, 1).await;
    let lost = insert(&db.pool, 2).await;
    q.dequeue("exec-1", 2).await.unwrap();
    let other = insert(&db.pool, 3).await;
    q.dequeue("exec-2", 1).await.unwrap();

    // Within the grace period nothing is reset: the executor may not have the job yet.
    let none = q
        .reset_orphans("exec-1", &[kept], Duration::from_secs(60))
        .await
        .unwrap();
    assert!(none.is_empty());
    let reset = q
        .reset_orphans("exec-1", &[kept], Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(reset, vec![lost]);
    assert_eq!(state(&q, lost).await, JobState::Queued);
    assert_eq!(state(&q, kept).await, JobState::Processing);
    assert_eq!(state(&q, other).await, JobState::Processing);

    q.dequeue("exec-1", 1).await.unwrap();
    q.reset_orphans("exec-1", &[kept], Duration::ZERO)
        .await
        .unwrap();
    let rec = q.get(lost).await.unwrap().unwrap();
    assert_eq!((rec.state, rec.num_resets), (JobState::Failed, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_heartbeat() {
    let Some((db, q)) = setup(QueueConfig::new("jobs")).await else {
        return;
    };
    let queued = insert(&db.pool, 1).await;
    assert!(q.cancel(queued).await.unwrap());
    assert_eq!(state(&q, queued).await, JobState::Canceled);
    assert!(!q.cancel(queued).await.unwrap());

    let a = insert(&db.pool, 2).await;
    let b = insert(&db.pool, 3).await;
    let got = q.dequeue("w", 10).await.unwrap();
    assert_eq!(got.len(), 2, "canceled row is not dequeued");
    assert!(q.cancel(b).await.unwrap());
    let hb = q.heartbeat(&[a, b, 999], "w").await.unwrap();
    assert_eq!(hb.known, vec![a, b]);
    assert_eq!(hb.canceled, vec![b]);
    assert!(q.heartbeat(&[a], "other").await.unwrap().known.is_empty());
    assert!(q.mark_canceled(b, "w").await.unwrap());
    assert_eq!(state(&q, b).await, JobState::Canceled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_logs_append() {
    let Some((db, q)) = setup(QueueConfig::new("jobs")).await else {
        return;
    };
    let id = insert(&db.pool, 1).await;
    for i in 0..2 {
        let e = ExecutionLogEntry {
            key: format!("step.{i}"),
            out: "ok".into(),
            ..Default::default()
        };
        assert_eq!(q.add_execution_log(id, &e).await.unwrap(), Some(i));
    }
    assert_eq!(
        q.add_execution_log(12345, &Default::default())
            .await
            .unwrap(),
        None
    );
    let logs = q.get(id).await.unwrap().unwrap().execution_logs;
    assert_eq!(
        logs.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
        ["step.0", "step.1"]
    );

    let grown = ExecutionLogEntry {
        key: "step.1".into(),
        out: "ok\nmore".into(),
        exit_code: Some(0),
        ..Default::default()
    };
    assert!(q.update_execution_log(id, 1, &grown).await.unwrap());
    assert!(!q.update_execution_log(id, 2, &grown).await.unwrap());
    assert!(!q.update_execution_log(id, -1, &grown).await.unwrap());
    let logs = q.get(id).await.unwrap().unwrap().execution_logs;
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1], grown);
}

/// Sends each handled job id; behaviour depends on the payload `n`:
/// 0 = succeed, 1 = wait for cancellation then error, 2 = ignore cancellation.
struct TestHandler(mpsc::UnboundedSender<i64>);

#[async_trait]
impl Handler<Payload> for TestHandler {
    async fn handle(&self, ctx: JobContext, record: Record<Payload>) -> Result<(), JobError> {
        let _ = self.0.send(record.id);
        ctx.log(&ExecutionLogEntry {
            key: "run".into(),
            ..Default::default()
        })
        .await
        .map_err(JobError::retry)?;
        match record.payload.n {
            0 => Ok(()),
            1 => {
                ctx.cancelled().await;
                Err(JobError::retry("stopped"))
            }
            _ => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            }
        }
    }
}

fn start_worker(
    q: &Queue<Payload>,
    concurrency: usize,
) -> (
    mpsc::UnboundedReceiver<i64>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut opts = WorkerOptions::new("worker-1");
    opts.concurrency = concurrency;
    opts.poll_interval = Duration::from_secs(30);
    opts.cancel_grace = Duration::from_millis(200);
    let cancel = CancellationToken::new();
    let worker = Worker::new(q.clone(), TestHandler(tx), opts);
    let handle = tokio::spawn(worker.run(cancel.clone()));
    (rx, cancel, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_picks_up_via_notify() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.heartbeat_interval = Duration::from_millis(100);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let early = insert(&db.pool, 0).await; // present before start: found by the first poll
    let (mut rx, cancel, handle) = start_worker(&q, 2);
    assert_eq!(rx.recv().await, Some(early));
    // Let LISTEN take effect (the listener resyncs on connect anyway).
    tokio::time::sleep(Duration::from_millis(300)).await;

    for _ in 0..5 {
        let t = Instant::now();
        let mut tx = db.pool.begin().await.unwrap();
        let id: i64 = sqlx::query_scalar("INSERT INTO jobs (n) VALUES (0) RETURNING id")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        notify_enqueued(&mut *tx, "jobs").await.unwrap();
        tx.commit().await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
        assert_eq!(got, Some(id));
        // Poll interval is 30 s, so this pickup came from NOTIFY.
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "pickup took {:?}",
            t.elapsed()
        );
        assert!(
            wait_for(Duration::from_secs(2), async || state(&q, id).await
                == JobState::Completed)
            .await
        );
    }
    assert_eq!(q.get(early).await.unwrap().unwrap().execution_logs.len(), 1);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_cancels_job_flagged_in_db() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.heartbeat_interval = Duration::from_millis(100);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let id = insert(&db.pool, 1).await;
    let (mut rx, cancel, handle) = start_worker(&q, 1);
    assert_eq!(rx.recv().await, Some(id));
    assert!(q.cancel(id).await.unwrap());
    assert!(
        wait_for(Duration::from_secs(3), async || state(&q, id).await
            == JobState::Canceled)
        .await
    );

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_shutdown_requeues_unfinished_jobs() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.heartbeat_interval = Duration::from_millis(100);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let cooperative = insert(&db.pool, 1).await;
    let stubborn = insert(&db.pool, 2).await;
    let (mut rx, cancel, handle) = start_worker(&q, 2);
    let mut started = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    started.sort();
    assert_eq!(started, vec![cooperative, stubborn]);

    let t = Instant::now();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "shutdown took {:?}",
        t.elapsed()
    );
    for id in [cooperative, stubborn] {
        let rec = q.get(id).await.unwrap().unwrap();
        assert_eq!(rec.state, JobState::Queued, "job {id}");
        assert_eq!((rec.num_failures, rec.num_resets), (0, 0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resetter_task_resets_stalled() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.stalled_after = Duration::from_millis(100);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let id = insert(&db.pool, 0).await;
    q.dequeue("dead-worker", 1).await.unwrap();
    let cancel = CancellationToken::new();
    let h = tokio::spawn(Resetter::new(q.clone(), Duration::from_millis(50)).run(cancel.clone()));
    let q2 = q.clone();
    assert!(
        wait_for(Duration::from_secs(3), async || state(&q2, id).await
            == JobState::Queued)
        .await
    );
    cancel.cancel();
    h.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_queues_dequeue_highest_priority_first() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let sql = create_queue_table_sql(
        "pjobs",
        &["n integer NOT NULL", "priority smallint NOT NULL DEFAULT 0"],
    )
    .unwrap();
    sqlx::raw_sql(&sql).execute(&db.pool).await.unwrap();
    let q: Queue<Payload> =
        Queue::new(db.pool.clone(), QueueConfig::new("pjobs").with_priority()).unwrap();
    for (n, priority) in [(1, 0i16), (2, 20), (3, 10), (4, 20)] {
        sqlx::query("INSERT INTO pjobs (n, priority) VALUES ($1, $2)")
            .bind(n)
            .bind(priority)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let mut order = Vec::new();
    while let Some(r) = q.dequeue("w", 1).await.unwrap().pop() {
        order.push(r.payload.n);
    }
    assert_eq!(order, [2, 4, 3, 1]);
}

/// Sends the job id when it starts and its negation when it is stopped.
struct StopHandler(mpsc::UnboundedSender<i64>);

#[async_trait]
impl Handler<Payload> for StopHandler {
    async fn handle(&self, ctx: JobContext, record: Record<Payload>) -> Result<(), JobError> {
        let _ = self.0.send(record.id);
        ctx.cancelled().await;
        let _ = self.0.send(-record.id);
        Err(JobError::retry("stopped"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_heartbeats_stop_jobs_before_they_stall() {
    let mut cfg = QueueConfig::new("jobs");
    cfg.heartbeat_interval = Duration::from_millis(100);
    cfg.stalled_after = Duration::from_secs(2);
    let Some((db, q)) = setup(cfg).await else {
        return;
    };
    let id = insert(&db.pool, 0).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut opts = WorkerOptions::new("worker-1");
    opts.poll_interval = Duration::from_secs(30);
    opts.cancel_grace = Duration::from_millis(500);
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(Worker::new(q.clone(), StopHandler(tx), opts).run(cancel.clone()));
    assert_eq!(rx.recv().await, Some(id));

    // Heartbeats fail while the table is away; the job keeps running until it has gone
    // unconfirmed for stalled_after less the grace and an interval (1.4 s), well before
    // another process could reset it.
    let t = Instant::now();
    sqlx::query("ALTER TABLE jobs RENAME TO jobs_away")
        .execute(&db.pool)
        .await
        .unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap();
    assert_eq!(stopped, Some(-id));
    assert!(
        t.elapsed() >= Duration::from_millis(1100) && t.elapsed() < Duration::from_secs(2),
        "stopped after {:?}",
        t.elapsed()
    );
    sqlx::query("ALTER TABLE jobs_away RENAME TO jobs")
        .execute(&db.pool)
        .await
        .unwrap();
    // Abandoned, not recorded: the row is left to the resetter.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state(&q, id).await, JobState::Processing);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}
