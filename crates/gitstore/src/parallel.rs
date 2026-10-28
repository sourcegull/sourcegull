//! Ordered parallel map over git objects.
//!
//! Unindexed search, diff search and the indexer read many objects
//! independently. Each worker opens its own thread-local state (for git, a
//! handle on the shared repository) and pulls the next item; results come
//! back in input order so streams stay deterministic.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default worker count: the available cores, at most 8.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(8))
}

/// Applies `f` to every item on up to `threads` threads and returns the
/// results in input order. One thread (or one item) runs inline.
pub fn map_ordered<T: Send, R: Send>(
    repo: &gix::ThreadSafeRepository,
    items: Vec<T>,
    threads: usize,
    f: impl Fn(&gix::Repository, T) -> R + Sync,
) -> Vec<R> {
    map_ordered_with(items, threads, || repo.to_thread_local(), f)
}

/// [`map_ordered`] with per-thread state made by `init`.
pub fn map_ordered_with<S, T: Send, R: Send>(
    items: Vec<T>,
    threads: usize,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&S, T) -> R + Sync,
) -> Vec<R> {
    let threads = threads.clamp(1, items.len().max(1));
    if threads == 1 {
        let local = init();
        return items.into_iter().map(|t| f(&local, t)).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let results: Vec<Mutex<Option<R>>> = slots.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let local = init();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(slot) = slots.get(i) else { break };
                    let item = slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .expect("each slot is taken once");
                    let r = f(&local, item);
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|r| {
            r.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every item is mapped")
        })
        .collect()
}
