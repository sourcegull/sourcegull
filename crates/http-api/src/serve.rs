//! Running a router on a listener with graceful shutdown.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// How long in-flight requests may run after shutdown starts. Long-lived streams (SSE) are
/// cut off after this; it stays below Kubernetes' default 30 s termination grace period.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// Binds a TCP listener on `addr`.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Serves `router` on `listener` until `cancel` fires, then stops accepting connections and
/// waits up to [`DRAIN_TIMEOUT`] for in-flight requests. Handlers can read the client address
/// with `ConnectInfo<SocketAddr>`. Connections get `TCP_NODELAY`, so that streamed responses
/// (search events, SCIP and blob transfers) are not held back by Nagle's algorithm.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "listening");
    let listener = listener.tap_io(|tcp| {
        // Not fatal: the connection only gets slower.
        let _ = tcp.set_nodelay(true);
    });
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(cancel.clone().cancelled_owned())
    .into_future();
    tokio::select! {
        res = server => res?,
        () = async {
            cancel.cancelled().await;
            tokio::time::sleep(DRAIN_TIMEOUT).await;
        } => {
            tracing::warn!(%addr, "requests still running after {DRAIN_TIMEOUT:?}; closing them");
        }
    }
    tracing::info!(%addr, "listener stopped");
    Ok(())
}
