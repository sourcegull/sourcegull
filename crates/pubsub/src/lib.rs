//! Postgres `LISTEN/NOTIFY` with one connection per process and database.
//!
//! Wake-ups (pgqueue workers, schedulers) and config propagation (site config, permissions,
//! the repo routing table) use `LISTEN/NOTIFY`. A `sqlx` `PgListener` built from a pool keeps
//! one of the pool's connections for as long as it lives, so when every subscriber had its own
//! listener on the request pool, a process with all roles pinned more connections than the pool
//! had and requests timed out waiting for one.
//!
//! Here every subscriber in a process shares one dedicated connection outside the caller's pool
//! (`application_name` `<pool's application_name>:listen`, visible in `pg_stat_activity`).
//! [`subscribe`] returns a [`Subscription`] whose [`recv`](Subscription::recv) yields the
//! notifications on its channels, and [`Event::Resync`] first and after every reconnect:
//! notifications are not durable, so a subscriber reloads its state then. Each subscriber should
//! still poll as a fallback.
//!
//! The shared connection is found by the pool's database and user, so callers need only their
//! pool. It follows the pool's server: when that changes (a replicated cluster's new primary,
//! see `sourcegull_db::cluster`), it reconnects there within [`FOLLOW_INTERVAL`]. It is closed
//! when the last subscription is dropped.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgListener, PgNotification, PgPool, PgPoolOptions};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

/// First delay before reconnecting after the connection failed; doubles up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_secs(1);
/// Longest delay between reconnection attempts.
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Bound on connecting and issuing `LISTEN`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the connection checks that it is still on its source pool's server.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
/// Events buffered per subscriber; one that falls further behind gets [`Event::Resync`].
const BUFFER: usize = 1024;

/// What a [`Subscription`] receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A notification on one of the subscription's channels.
    Notification {
        /// The channel it was sent on.
        channel: String,
        /// Its payload (may be empty).
        payload: String,
    },
    /// Notifications may have been missed (first event, reconnect, or the subscriber fell
    /// behind): reload the state the channels describe.
    Resync,
}

/// A subscription to one or more channels. Dropping it unsubscribes.
pub struct Subscription {
    hub: Arc<Hub>,
    rx: broadcast::Receiver<Event>,
    channels: HashSet<String>,
    resync: bool,
}

impl Subscription {
    /// The next event; [`Event::Resync`] first. Cancel-safe.
    pub async fn recv(&mut self) -> Event {
        if std::mem::take(&mut self.resync) {
            return Event::Resync;
        }
        loop {
            match self.rx.recv().await {
                Ok(Event::Notification { channel, .. }) if !self.channels.contains(&channel) => {}
                Ok(event) => return event,
                Err(broadcast::error::RecvError::Lagged(_)) => return Event::Resync,
                // The hub's task ended (its runtime is shutting down): only polling is left.
                Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
            }
        }
    }

    /// The channels this subscription receives.
    pub fn channels(&self) -> impl Iterator<Item = &str> {
        self.channels.iter().map(String::as_str)
    }

    /// Whether the shared connection is currently listening.
    pub fn is_connected(&self) -> bool {
        self.hub
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Subscribes to `channels` on the database `pool` connects to.
///
/// Returns once `LISTEN` is in effect, or once the shared connection is known to be down (an
/// [`Event::Resync`] then follows when it is back), so nothing sent after this returns is
/// missed without a resync.
pub fn subscribe(
    pool: &PgPool,
    channels: &[&str],
) -> impl Future<Output = Subscription> + Send + 'static {
    // Nothing borrowed is kept, so callers can pass temporaries.
    let source = pool.clone();
    let channels: HashSet<String> = channels.iter().map(|c| (*c).to_owned()).collect();
    async move {
        let hub = hub(source);
        let (ack, done) = oneshot::channel();
        if hub
            .commands
            .send(Command::Listen(channels.iter().cloned().collect(), ack))
            .is_ok()
        {
            let _ = done.await;
        }
        // Subscribing after the acknowledgement is safe: the initial resync reloads everything
        // sent before this point.
        let rx = hub.events.subscribe();
        Subscription {
            hub,
            rx,
            channels,
            resync: true,
        }
    }
}

/// The shared connection of one database.
struct Hub {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<Event>,
    connected: Arc<std::sync::atomic::AtomicBool>,
}

enum Command {
    /// Listen on these channels too; acknowledged once in effect (or the connection is down).
    Listen(Vec<String>, oneshot::Sender<()>),
}

/// Live hubs by database. Entries die with their last subscription.
static HUBS: LazyLock<Mutex<HashMap<String, Weak<Hub>>>> = LazyLock::new(Default::default);

/// The server is left out: a pool following a replicated cluster's primary changes host, and
/// keeps its hub.
fn hub_key(o: &PgConnectOptions) -> String {
    format!(
        "{:?}/{}/{}",
        o.get_socket(),
        o.get_database().unwrap_or_default(),
        o.get_username()
    )
}

fn hub(source: PgPool) -> Arc<Hub> {
    let key = hub_key(&source.connect_options());
    let mut hubs = HUBS.lock().unwrap_or_else(PoisonError::into_inner);
    hubs.retain(|_, hub| hub.strong_count() > 0);
    // A hub whose task is gone (it ran on a runtime that has shut down) is replaced.
    if let Some(hub) = hubs.get(&key).and_then(Weak::upgrade)
        && !hub.commands.is_closed()
    {
        return hub;
    }
    let (commands, rx) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(BUFFER);
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn(run_hub(source, rx, events.clone(), connected.clone()));
    let hub = Arc::new(Hub {
        commands,
        events,
        connected,
    });
    hubs.insert(key, Arc::downgrade(&hub));
    hub
}

/// Owns the connection: listens on every channel asked for, forwards notifications and
/// reconnects with backoff. Ends when every subscription is dropped.
async fn run_hub(
    source: PgPool,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<Event>,
    connected: Arc<std::sync::atomic::AtomicBool>,
) {
    let listen_options = || {
        let options = (*source.connect_options()).clone();
        let name = format!(
            "{}:listen",
            options.get_application_name().unwrap_or("sourcegull")
        );
        options.application_name(&name)
    };
    // A one-connection pool only because `PgListener` wants one; it is never shared.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(CONNECT_TIMEOUT)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_lazy_with(listen_options());
    let mut follow = tokio::time::interval(FOLLOW_INTERVAL);
    follow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut channels: HashSet<String> = HashSet::new();
    let mut listener: Option<PgListener> = None;
    let mut retry = RETRY_MIN;
    let mut next_attempt = Instant::now();
    let set_connected = |listener: &Option<PgListener>| {
        connected.store(listener.is_some(), std::sync::atomic::Ordering::Relaxed);
    };

    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None => break,
                Some(Command::Listen(wanted, ack)) => {
                    let new: Vec<String> = wanted
                        .into_iter()
                        .filter(|c| channels.insert(c.clone()))
                        .collect();
                    if let Some(l) = listener.as_mut() {
                        if !new.is_empty()
                            && let Err(e) = bounded(l.listen_all(new.iter().map(String::as_str))).await
                        {
                            tracing::warn!(error = %e, "LISTEN failed; reconnecting");
                            listener = None;
                            next_attempt = Instant::now();
                        }
                    } else if Instant::now() >= next_attempt {
                        // First subscription (or the backoff has passed): connect now, so the
                        // caller's initial resync happens after LISTEN.
                        listener = connect(&pool, &channels, &events, &mut retry, &mut next_attempt).await;
                    }
                    set_connected(&listener);
                    let _ = ack.send(());
                }
            },
            r = try_recv(listener.as_mut()) => {
                match r {
                    Ok(Some(n)) => {
                        let _ = events.send(Event::Notification {
                            channel: n.channel().to_owned(),
                            payload: n.payload().to_owned(),
                        });
                    }
                    // sqlx reconnected and listens again; anything sent in between is lost.
                    Ok(None) => {
                        tracing::info!("notification listener reconnected");
                        let _ = events.send(Event::Resync);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "notification listener failed; reconnecting");
                        listener = None;
                        next_attempt = Instant::now() + retry;
                    }
                }
                set_connected(&listener);
            }
            _ = follow.tick() => {
                let wanted = listen_options();
                let current = pool.connect_options();
                if (wanted.get_host(), wanted.get_port()) != (current.get_host(), current.get_port()) {
                    tracing::info!(
                        host = wanted.get_host(),
                        "database server changed; moving the notification listener"
                    );
                    pool.set_connect_options(wanted);
                    if listener.take().is_some() {
                        next_attempt = Instant::now();
                        retry = RETRY_MIN;
                    }
                    set_connected(&listener);
                }
            }
            () = tokio::time::sleep_until(next_attempt), if listener.is_none() && !channels.is_empty() => {
                listener = connect(&pool, &channels, &events, &mut retry, &mut next_attempt).await;
                set_connected(&listener);
            }
        }
    }
    drop(listener);
    pool.close().await;
}

/// Connects and listens on `channels`; on success broadcasts a resync (anything sent while
/// disconnected is lost), on failure schedules the next attempt.
async fn connect(
    pool: &PgPool,
    channels: &HashSet<String>,
    events: &broadcast::Sender<Event>,
    retry: &mut Duration,
    next_attempt: &mut Instant,
) -> Option<PgListener> {
    let attempt = async {
        let mut l = PgListener::connect_with(pool).await?;
        l.listen_all(channels.iter().map(String::as_str)).await?;
        Ok::<_, sqlx::Error>(l)
    };
    match bounded(attempt).await {
        Ok(l) => {
            *retry = RETRY_MIN;
            let _ = events.send(Event::Resync);
            Some(l)
        }
        Err(e) => {
            tracing::warn!(error = %e, retry_in = ?*retry, "connecting the notification listener failed");
            *next_attempt = Instant::now() + *retry;
            *retry = (*retry * 2).min(RETRY_MAX);
            None
        }
    }
}

async fn bounded<T>(fut: impl Future<Output = Result<T, sqlx::Error>>) -> Result<T, sqlx::Error> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .unwrap_or(Err(sqlx::Error::PoolTimedOut))
}

/// `try_recv` on the listener, or never without one. `PgListener::try_recv` is cancel-safe.
async fn try_recv(l: Option<&mut PgListener>) -> Result<Option<PgNotification>, sqlx::Error> {
    match l {
        Some(l) => l.try_recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;

    #[test]
    fn hubs_are_per_database_and_user() {
        let a = PgConnectOptions::from_str("postgres://u:p@h:5432/a").unwrap();
        let a2 =
            PgConnectOptions::from_str("postgres://u:other@h:5432/a?application_name=x").unwrap();
        let b = PgConnectOptions::from_str("postgres://u:p@h:5432/b").unwrap();
        let c = PgConnectOptions::from_str("postgres://v:p@h:5432/a").unwrap();
        let moved = PgConnectOptions::from_str("postgres://u:p@h2:5433/a").unwrap();
        assert_eq!(hub_key(&a), hub_key(&a2));
        assert_eq!(hub_key(&a), hub_key(&moved));
        assert_ne!(hub_key(&a), hub_key(&b));
        assert_ne!(hub_key(&a), hub_key(&c));
    }
}
