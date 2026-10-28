//! The Postgres sink of the outbound request log: entries from every process go to
//! `outbound_requests`, which keeps the newest `outboundRequestLogLimit`.
//!
//! [`PgOutboundLog::record`] never blocks an API call: it hands the entry to a writer task over
//! a bounded channel and drops it when the channel is full.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sourcegull_codehost::outbound::{OutboundRequestEntry, OutboundRequestLog};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{Db, NewOutboundRequest};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Entries waiting for the writer; more are dropped.
const CHANNEL_CAPACITY: usize = 1024;
/// Entries written per transaction at most.
const BATCH: usize = 100;

/// The outbound request log in Postgres.
#[derive(Debug)]
pub struct PgOutboundLog {
    config: SiteConfigHandle,
    tx: mpsc::Sender<OutboundRequestEntry>,
}

impl PgOutboundLog {
    /// Starts the writer (until `cancel` fires) and returns the sink to install with
    /// [`sourcegull_codehost::outbound::set_global`].
    pub fn spawn(db: Db, config: SiteConfigHandle, cancel: CancellationToken) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        tokio::spawn(write_loop(db, config.clone(), rx, cancel));
        Arc::new(Self { config, tx })
    }
}

impl OutboundRequestLog for PgOutboundLog {
    fn record(&self, entry: OutboundRequestEntry) {
        if self.config.load().value.outbound_request_log_limit == 0 {
            return;
        }
        let _ = self.tx.try_send(entry);
    }
}

fn headers(h: &[(String, Vec<String>)]) -> Value {
    Value::Object(
        h.iter()
            .map(|(k, v)| (k.clone(), Value::from(v.clone())))
            .collect::<Map<String, Value>>(),
    )
}

/// The row for `e`.
pub fn row(e: &OutboundRequestEntry) -> NewOutboundRequest {
    NewOutboundRequest {
        started_at: e.started_at,
        method: e.method.clone(),
        url: e.url.clone(),
        request_headers: headers(&e.request_headers),
        request_body: e.request_body.clone(),
        status_code: i32::from(e.status_code),
        response_headers: headers(&e.response_headers),
        duration_ms: i64::try_from(e.duration.as_millis()).unwrap_or(i64::MAX),
        error_message: e.error.clone(),
        creation_stack_frame: e.client.clone(),
        call_stack: e.caller.clone(),
    }
}

async fn write_loop(
    db: Db,
    config: SiteConfigHandle,
    mut rx: mpsc::Receiver<OutboundRequestEntry>,
    cancel: CancellationToken,
) {
    let store = db.outbound_requests();
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let n = tokio::select! {
            () = cancel.cancelled() => return,
            n = rx.recv_many(&mut batch, BATCH) => n,
        };
        if n == 0 {
            return;
        }
        let rows: Vec<NewOutboundRequest> = batch.drain(..).map(|e| row(&e)).collect();
        let limit = i64::from(config.load().value.outbound_request_log_limit);
        if let Err(e) = store.insert_many(&rows, limit).await {
            tracing::warn!(error = %e, "writing the outbound request log failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
