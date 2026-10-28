//! The node's search worker pool.
//!
//! Indexed searches verify candidates in parallel batches. Each search used
//! to start its own threads per batch, so concurrent searches ran several
//! threads per core between them (a many-repository query also runs several
//! indexed calls at once) and one broad search slowed every other one down.
//! All searches now share one pool of [`threads`] workers (the cores), and
//! a batch is a job on its queue: workers take a few items of the job at
//! the head of the queue and move it to the back, so concurrent searches
//! share the workers round robin, a new search's first batch is not queued
//! behind a broad search's work, and the CPU time of a node's searches
//! stays bounded by its cores. [`Lane::Urgent`] jobs (building the chunks
//! of a ranked search's first page) go before the others.
//!
//! `SOURCEGULL_SEARCH_THREADS` sets the worker count (default: the available
//! cores).

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Where a job queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Work a client is waiting on: taken before any [`Lane::Normal`] job.
    Urgent,
    Normal,
}

/// The number of workers in the pool.
pub fn threads() -> usize {
    pool().threads
}

/// Applies `f` to every item on the pool's workers and returns the results
/// in input order, once all are done. One item, a single-worker pool, or a
/// call from a worker runs inline on the caller's thread.
pub fn map_ordered<T: Send, R: Send>(
    items: Vec<T>,
    lane: Lane,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let pool = pool();
    if items.len() <= 1 || pool.threads <= 1 || IS_WORKER.with(|w| w.get()) {
        return items.into_iter().map(f).collect();
    }
    let slots: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let results: Vec<Mutex<Option<R>>> = slots.iter().map(|_| Mutex::new(None)).collect();
    let run = |i: usize| {
        let item = lock(&slots[i]).take().expect("each slot is taken once");
        let r = f(item);
        *lock(&results[i]) = Some(r);
    };
    pool.run(slots.len(), lane, &run);
    results
        .into_iter()
        .map(|r| {
            r.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every item is mapped")
        })
        .collect()
}

thread_local! {
    static IS_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("SOURCEGULL_SEARCH_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
        Pool::start(threads)
    })
}

/// The function a job runs per item, with its borrow's lifetime erased:
/// [`Pool::run`] waits until every item has run before it returns, and a
/// worker does not touch the function after counting its last items.
type ItemFn = &'static (dyn Fn(usize) + Sync);

/// A batch of items queued on the pool.
struct Job {
    run: ItemFn,
    len: usize,
    /// Items claimed per turn.
    step: usize,
    lane: Lane,
    /// Items finished, and the first panic, signalled through `finished`.
    done: Mutex<(usize, Option<Box<dyn Any + Send>>)>,
    finished: Condvar,
}

struct Pool {
    threads: usize,
    queue: Arc<Queue>,
}

#[derive(Default)]
struct Queue {
    state: Mutex<State>,
    ready: Condvar,
    /// Jobs with items left to claim, read without the lock by workers
    /// polling for work.
    queued: AtomicUsize,
}

/// How long an idle worker polls for a job (yielding the CPU) before it
/// sleeps. A search queues its batches one after another, with its own
/// work in between; waking sleeping workers for every batch made a
/// search alone 10–15% slower than threads started per batch.
const POLL: Duration = Duration::from_millis(1);

#[derive(Default)]
struct State {
    /// Jobs with items left to claim, and the next item of each.
    urgent: VecDeque<(Arc<Job>, usize)>,
    normal: VecDeque<(Arc<Job>, usize)>,
    /// Workers waiting on `ready`.
    sleeping: usize,
}

impl State {
    fn lane(&mut self, lane: Lane) -> &mut VecDeque<(Arc<Job>, usize)> {
        match lane {
            Lane::Urgent => &mut self.urgent,
            Lane::Normal => &mut self.normal,
        }
    }

    /// The next turn: `step` items of the job at the head of the queue,
    /// which moves to the back if it has items left.
    fn claim(&mut self) -> Option<(Arc<Job>, std::ops::Range<usize>)> {
        let (job, start) = self
            .urgent
            .pop_front()
            .or_else(|| self.normal.pop_front())?;
        let end = (start + job.step).min(job.len);
        if end < job.len {
            let lane = job.lane;
            self.lane(lane).push_back((job.clone(), end));
        }
        Some((job, start..end))
    }
}

impl Pool {
    fn start(threads: usize) -> Pool {
        let queue = Arc::new(Queue::default());
        if threads > 1 {
            for i in 0..threads {
                let queue = queue.clone();
                std::thread::Builder::new()
                    .name(format!("search-{i}"))
                    .spawn(move || {
                        IS_WORKER.with(|w| w.set(true));
                        work(&queue);
                    })
                    .expect("search worker thread starts");
            }
        }
        Pool { threads, queue }
    }

    /// Runs `f(0..len)` on the workers and waits for all of it.
    fn run(&self, len: usize, lane: Lane, f: &(dyn Fn(usize) + Sync)) {
        // SAFETY: the job's function is only called for items below `len`,
        // and this function does not return before all `len` items have
        // finished (`done.0 == len`); a worker stops using the reference
        // before it counts its items (see `work`), so the borrow outlives
        // every use.
        #[allow(unsafe_code)]
        let run: ItemFn = unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), ItemFn>(f) };
        // A few items per turn: small enough to interleave with other jobs
        // and to spread one job over every worker, large enough that the
        // queue's lock is not taken per file.
        let step = len.div_ceil(self.threads * 8).clamp(1, 8);
        let job = Arc::new(Job {
            run,
            len,
            step,
            lane,
            done: Mutex::new((0, None)),
            finished: Condvar::new(),
        });
        let mut state = lock(&self.queue.state);
        state.lane(lane).push_back((job.clone(), 0));
        self.queue.queued.fetch_add(1, Ordering::Relaxed);
        // Wake as many sleeping workers as the job has turns.
        let wake = state.sleeping.min(len.div_ceil(step));
        drop(state);
        for _ in 0..wake {
            self.queue.ready.notify_one();
        }
        let mut done = lock(&job.done);
        while done.0 < len {
            done = job.finished.wait(done).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(panic) = done.1.take() {
            drop(done);
            resume_unwind(panic);
        }
    }
}

/// A worker: runs turns ([`State::claim`]) until the process exits,
/// polling for a while ([`POLL`]) before it sleeps.
fn work(queue: &Queue) {
    loop {
        let (job, items) = {
            let mut state = lock(&queue.state);
            let mut polled = false;
            loop {
                if let Some(turn) = state.claim() {
                    if turn.1.end == turn.0.len {
                        queue.queued.fetch_sub(1, Ordering::Relaxed);
                    }
                    break turn;
                }
                if !polled {
                    polled = true;
                    drop(state);
                    let t = Instant::now();
                    while t.elapsed() < POLL && queue.queued.load(Ordering::Relaxed) == 0 {
                        std::thread::yield_now();
                    }
                    state = lock(&queue.state);
                    continue;
                }
                state.sleeping += 1;
                state = queue.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.sleeping -= 1;
            }
        };
        let n = items.len();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            for i in items {
                (job.run)(i);
            }
        }))
        .err();
        // The last use of `job.run`: after this the caller may return.
        let mut done = lock(&job.done);
        done.0 += n;
        if done.1.is_none() {
            done.1 = panic;
        }
        if done.0 == job.len {
            job.finished.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_in_order() {
        let out = map_ordered((0..1000).collect(), Lane::Normal, |i: u64| i * 2);
        assert_eq!(out, (0..1000).map(|i| i * 2).collect::<Vec<_>>());
        assert!(map_ordered(Vec::<u8>::new(), Lane::Normal, |i| i).is_empty());
    }

    #[test]
    fn concurrent_callers_share_the_workers() {
        let calls = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let calls = &calls;
                s.spawn(move || {
                    for _ in 0..20 {
                        let items: Vec<u64> = (0..100).map(|i| i + t).collect();
                        let out = map_ordered(items, Lane::Normal, |i| {
                            calls.fetch_add(1, Ordering::Relaxed);
                            i + 1
                        });
                        assert_eq!(out, (0..100).map(|i| i + t + 1).collect::<Vec<_>>());
                    }
                });
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 8 * 20 * 100);
    }

    #[test]
    fn nested_calls_run_inline() {
        let out = map_ordered((0..10).collect(), Lane::Urgent, |i: usize| {
            map_ordered((0..i).collect(), Lane::Normal, |j: usize| j).len()
        });
        assert_eq!(out, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn a_panic_reaches_the_caller() {
        let r = catch_unwind(|| {
            map_ordered((0..100).collect(), Lane::Normal, |i: usize| {
                assert_ne!(i, 57, "boom");
                i
            })
        });
        assert!(r.is_err());
        // The pool still works.
        assert_eq!(
            map_ordered(vec![1, 2, 3], Lane::Urgent, |i: i32| i),
            vec![1, 2, 3]
        );
    }
}
