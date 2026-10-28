//! What the runner reports while it works.
//!
//! Output is passed on raw, as the container writes it: callers that forward it (the
//! executor's job log, a terminal UI) redact secrets themselves.

use crate::runtime::OutputStream;

/// An event of a run. Steps are numbered from 0.
#[derive(Debug, Clone, PartialEq)]
pub enum StepEvent<'a> {
    /// An image was resolved to a digest (pulled if needed).
    ImageResolved { image: &'a str, digest: &'a str },
    /// Steps up to and including `step` were restored from the cache.
    CacheHit { step: usize },
    /// The step's `if:` evaluated to false.
    Skipped { step: usize },
    /// The step's container is about to start.
    Started {
        step: usize,
        image: &'a str,
        digest: &'a str,
        /// The rendered `run`.
        script: &'a str,
    },
    /// Output of the step's container, as it arrives (chunks, not lines).
    Output {
        step: usize,
        stream: OutputStream,
        data: &'a [u8],
    },
    /// The step's container exited.
    Finished { step: usize, exit_code: i32 },
    /// Something went wrong that does not fail the run (a cache read or write).
    Warning { message: &'a str },
}

/// Receives [`StepEvent`]s. Implemented for closures.
pub trait LogSink: Send + Sync {
    fn event(&self, event: &StepEvent<'_>);
}

impl<F: Fn(&StepEvent<'_>) + Send + Sync> LogSink for F {
    fn event(&self, event: &StepEvent<'_>) {
        self(event)
    }
}

/// A sink that drops everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLog;

impl LogSink for NoLog {
    fn event(&self, _: &StepEvent<'_>) {}
}
