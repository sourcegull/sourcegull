//! Installs the process-wide recorders behind the site admin "Background jobs" and "Outbound
//! requests" pages, all in Postgres so the web process shows what every process did:
//! - routine runs and lease holders ([`sourcegull_db::PgRoutineRecorder`]; the lease runner reports
//!   acquisitions and releases through a [`sourcegull_leader::LeaseObserver`]);
//! - code host API calls ([`sourcegull_repos::outbound_log::PgOutboundLog`]).

use std::sync::Arc;

use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{
    Db, PgRoutineRecorder, global_recorder, routine_host_name, set_global_recorder,
};
use tokio_util::sync::CancellationToken;

/// Reports lease changes to the global routine recorder, off the lease loop.
struct RecorderObserver;

impl sourcegull_leader::LeaseObserver for RecorderObserver {
    fn acquired(&self, name: &str) {
        let name = name.to_owned();
        tokio::spawn(async move {
            if let Err(e) = global_recorder().started(&name, routine_host_name()).await {
                tracing::debug!(routine = name, error = %e, "recording a routine start failed");
            }
        });
    }

    fn released(&self, name: &str) {
        let name = name.to_owned();
        tokio::spawn(async move {
            if let Err(e) = global_recorder().stopped(&name, routine_host_name()).await {
                tracing::debug!(routine = name, error = %e, "recording a routine stop failed");
            }
        });
    }
}

/// Installs the recorders; the outbound log writer runs until `cancel` fires.
pub fn install(db: &Db, config: SiteConfigHandle, cancel: CancellationToken) {
    set_global_recorder(Arc::new(PgRoutineRecorder::new(db.pool().clone())));
    sourcegull_leader::set_lease_observer(Arc::new(RecorderObserver));
    sourcegull_codehost::outbound::set_global(
        sourcegull_repos::outbound_log::PgOutboundLog::spawn(db.clone(), config, cancel),
    );
}
