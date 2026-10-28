//! Serving side helpers.

use std::io;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::{Stream, StreamExt};

/// The connections accepted on `listener`, with `TCP_NODELAY` set, for tonic's
/// `serve_with_incoming*`.
///
/// Those ignore the server builder's `tcp_nodelay`, and without it Nagle's algorithm holds back
/// the small HTTP/2 frames that end a response until the client's delayed ACK: about 40 ms
/// added to every call (measured on repo node calls from `web` in Kubernetes).
pub fn incoming(listener: TcpListener) -> impl Stream<Item = io::Result<TcpStream>> {
    TcpListenerStream::new(listener).map(|conn| {
        let conn = conn?;
        // Not fatal: the connection only gets slower.
        let _ = conn.set_nodelay(true);
        Ok(conn)
    })
}

/// When the call's deadline passes: now plus the `grpc-timeout` the client sent (tonic's
/// `Request::set_timeout`). `None` without one. A streaming call's handler returns before
/// the stream ends, so long-running streams check this themselves.
pub fn request_deadline<T>(req: &tonic::Request<T>) -> Option<Instant> {
    let value = req.metadata().get("grpc-timeout")?.to_str().ok()?;
    parse_grpc_timeout(value).map(|t| Instant::now() + t)
}

/// A `grpc-timeout` value: at most 8 digits and a unit (`H`, `M`, `S`, `m`, `u`, `n`).
fn parse_grpc_timeout(value: &str) -> Option<Duration> {
    if value.len() < 2 || value.len() > 9 {
        return None;
    }
    let (digits, unit) = value.split_at(value.len() - 1);
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.checked_mul(3600)?),
        "M" => Duration::from_secs(n.checked_mul(60)?),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_timeouts_parse() {
        assert_eq!(parse_grpc_timeout("3S"), Some(Duration::from_secs(3)));
        assert_eq!(parse_grpc_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_grpc_timeout("2M"), Some(Duration::from_secs(120)));
        assert_eq!(parse_grpc_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_grpc_timeout("7u"), Some(Duration::from_micros(7)));
        assert_eq!(parse_grpc_timeout("9n"), Some(Duration::from_nanos(9)));
        assert_eq!(parse_grpc_timeout("S"), None);
        assert_eq!(parse_grpc_timeout("3s"), None);
        assert_eq!(parse_grpc_timeout("123456789S"), None);

        let mut req = tonic::Request::new(());
        assert!(request_deadline(&req).is_none());
        req.set_timeout(Duration::from_secs(10));
        let d = request_deadline(&req).unwrap();
        assert!(d > Instant::now() + Duration::from_secs(9));
    }

    #[tokio::test]
    async fn accepted_connections_have_nodelay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut conns = Box::pin(incoming(listener));
        let _client = TcpStream::connect(addr).await.unwrap();
        let conn = conns.next().await.unwrap().unwrap();
        assert!(conn.nodelay().unwrap());
    }
}
