//! Multi-host database URLs: finding the primary and following it when it goes away.
//!
//! Two TCP proxies in front of the test server stand in for two cluster members; stopping one
//! (and cutting its connections) is the primary failing.

use std::net::SocketAddr;
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use sourcegull_db::sqlx::postgres::PgConnectOptions;
use sourcegull_db::{Db, DbOptions};
use sourcegull_testutil::TestDb;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

/// Forwards connections to `upstream` until dropped or [`Proxy::stop`]ped.
struct Proxy {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            // Dropping the set (when this task is aborted) cuts every forwarded connection.
            let mut conns = JoinSet::new();
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                let upstream = upstream.clone();
                conns.spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        });
        Self { addr, task }
    }

    /// Closes the listener and every connection through it.
    async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The test database's URL with its host list replaced by `hosts`.
fn url_with_hosts(t: &TestDb, hosts: &[String]) -> String {
    let o = PgConnectOptions::from_str(&t.url).unwrap();
    let user = o.get_username();
    let db = o.get_database().unwrap();
    let password = t
        .url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map(|(_, p)| format!(":{p}"))
        .unwrap_or_default();
    format!("postgres://{user}{password}@{}/{db}", hosts.join(","))
}

fn upstream(t: &TestDb) -> String {
    let o = PgConnectOptions::from_str(&t.url).unwrap();
    format!("{}:{}", o.get_host(), o.get_port())
}

fn opts() -> DbOptions {
    DbOptions {
        max_connections: 4,
        acquire_timeout: Duration::from_secs(10),
        ..Default::default()
    }
}

async fn ping(db: &Db) -> Result<i32, sourcegull_db::sqlx::Error> {
    sourcegull_db::sqlx::query_scalar("SELECT 1")
        .fetch_one(db.pool())
        .await
}

#[tokio::test]
async fn connects_to_the_reachable_host() {
    let Some(t) = TestDb::new().await else { return };
    let real = upstream(&t);
    // Port 1 refuses connections.
    let url = url_with_hosts(&t, &["127.0.0.1:1".to_owned(), real.clone()]);
    let db = Db::connect(&url, &opts()).await.unwrap();
    assert_eq!(ping(&db).await.unwrap(), 1);
    let cluster = db.cluster().expect("a multi-host URL makes a cluster");
    assert_eq!(cluster.primary(), real);
}

#[tokio::test]
async fn no_writable_host_is_an_error() {
    let Some(t) = TestDb::new().await else { return };
    let url = url_with_hosts(&t, &["127.0.0.1:1".to_owned(), "127.0.0.1:2".to_owned()]);
    let err = Db::connect(&url, &opts()).await.unwrap_err().to_string();
    assert!(err.contains("no writable primary"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_the_primary_when_it_goes_away() {
    let Some(t) = TestDb::new().await else { return };
    let a = Proxy::start(upstream(&t)).await;
    let b = Proxy::start(upstream(&t)).await;
    let (a_addr, b_addr) = (a.addr.to_string(), b.addr.to_string());
    let url = url_with_hosts(&t, &[a_addr.clone(), b_addr.clone()]);

    let db = Db::connect(&url, &opts()).await.unwrap();
    let worker = db.sibling(&DbOptions {
        application_name: "sourcegull:worker".to_owned(),
        ..opts()
    });
    assert_eq!(ping(&db).await.unwrap(), 1);
    assert_eq!(ping(&worker).await.unwrap(), 1);
    let cluster = db.cluster().unwrap().clone();
    assert_eq!(cluster.primary(), a_addr);

    a.stop().await;
    let failed_at = Instant::now();

    // A request made right after the failure waits for a connection to the new primary
    // instead of failing.
    assert_eq!(ping(&db).await.unwrap(), 1);
    assert_eq!(ping(&worker).await.unwrap(), 1);
    let recovered = failed_at.elapsed();
    assert_eq!(cluster.primary(), b_addr);
    assert!(
        recovered < Duration::from_secs(6),
        "took {recovered:?} to reach the new primary"
    );

    drop(b);
}
