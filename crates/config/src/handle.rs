//! Lock-free hot config.
//!
//! Readers call [`ConfigHandle::load`] on every request; it is a single atomic load. Writers
//! (the NOTIFY listener or the periodic poll fallback) call [`ConfigHandle::store`], which
//! only replaces the value when the version is newer, so out-of-order reloads never roll
//! the config back.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use tokio::sync::watch;

use crate::site::SiteConfig;

/// A value tagged with the version it was loaded at (e.g. the `critical_and_site_config.id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versioned<T> {
    /// Monotonically increasing version.
    pub version: i64,
    /// The config value.
    pub value: T,
}

/// Shared, atomically swappable config with change notifications. Cheap to clone.
#[derive(Debug)]
pub struct ConfigHandle<T = SiteConfig> {
    inner: Arc<Inner<T>>,
}

#[derive(Debug)]
struct Inner<T> {
    current: ArcSwap<Versioned<T>>,
    /// Serializes writers so the version check and swap are atomic together.
    write_lock: Mutex<()>,
    changes: watch::Sender<i64>,
}

impl<T> Clone for ConfigHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Handle to the site config.
pub type SiteConfigHandle = ConfigHandle<SiteConfig>;

impl<T> ConfigHandle<T> {
    /// Creates a handle holding `value` at `version`.
    pub fn new(version: i64, value: T) -> Self {
        let (changes, _) = watch::channel(version);
        Self {
            inner: Arc::new(Inner {
                current: ArcSwap::from_pointee(Versioned { version, value }),
                write_lock: Mutex::new(()),
                changes,
            }),
        }
    }

    /// Returns the current value. Lock-free.
    pub fn load(&self) -> Arc<Versioned<T>> {
        self.inner.current.load_full()
    }

    /// Returns the current version. Lock-free.
    pub fn version(&self) -> i64 {
        self.inner.current.load().version
    }

    /// Replaces the value if `version` is newer than the current one, notifying
    /// subscribers. Returns whether the value was replaced.
    pub fn store(&self, version: i64, value: T) -> bool {
        // A poisoned lock only means another writer panicked; the guarded data is `()`.
        let _guard = self
            .inner
            .write_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if version <= self.inner.current.load().version {
            return false;
        }
        self.inner
            .current
            .store(Arc::new(Versioned { version, value }));
        self.inner.changes.send_replace(version);
        true
    }

    /// Subscribes to changes. The receiver yields the new version after each successful
    /// [`store`](Self::store); read the value with [`load`](Self::load).
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.inner.changes.subscribe()
    }
}

impl<T: Default> Default for ConfigHandle<T> {
    /// A handle holding `T::default()` at version 0.
    fn default() -> Self {
        Self::new(0, T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_versions_are_stored() {
        let h = ConfigHandle::new(5, "five");
        assert!(!h.store(4, "four"));
        assert!(!h.store(5, "five again"));
        assert_eq!(h.load().value, "five");
        assert!(h.store(7, "seven"));
        assert!(!h.store(6, "six"));
        let cur = h.load();
        assert_eq!((cur.version, cur.value), (7, "seven"));
    }

    #[test]
    fn clones_share_state() {
        let a = ConfigHandle::new(1, 10u32);
        let b = a.clone();
        assert!(b.store(2, 20));
        assert_eq!(a.load().value, 20);
        assert_eq!(a.version(), 2);
    }

    #[tokio::test]
    async fn subscribers_are_notified() {
        let h = SiteConfigHandle::default();
        let mut rx = h.subscribe();
        let writer = h.clone();
        tokio::spawn(async move {
            writer.store(
                3,
                SiteConfig {
                    auth_public: true,
                    ..SiteConfig::default()
                },
            );
        });
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), 3);
        assert!(h.load().value.auth_public);
    }

    #[test]
    fn concurrent_stores_keep_the_highest_version() {
        let h = ConfigHandle::new(0, 0i64);
        std::thread::scope(|s| {
            for t in 0..8 {
                let h = h.clone();
                s.spawn(move || {
                    for v in (0..1000).map(|i| i * 8 + t) {
                        h.store(v, v);
                    }
                });
            }
        });
        let cur = h.load();
        assert_eq!(cur.version, 7999);
        assert_eq!(cur.value, 7999);
    }
}
