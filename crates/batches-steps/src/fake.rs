//! An in-process [`ContainerRuntime`] for tests: a handler plays the container, changing the
//! host working copy ([`ContainerSpec::workspace`]) directly and returning output and an exit
//! code. Every run is recorded.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::runtime::{ContainerRuntime, ContainerSpec, OutputFn, OutputStream, RuntimeError};

/// What a fake container does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl FakeOutput {
    pub fn ok() -> Self {
        Self::default()
    }

    pub fn stdout(s: impl Into<String>) -> Self {
        Self {
            stdout: s.into(),
            ..Self::default()
        }
    }

    pub fn failed(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            exit_code,
            stderr: stderr.into(),
            ..Self::default()
        }
    }
}

type Handler = Box<dyn Fn(&ContainerSpec) -> FakeOutput + Send + Sync>;

pub struct FakeRuntime {
    handler: Handler,
    digests: Mutex<HashMap<String, String>>,
    missing: Mutex<HashSet<String>>,
    runs: Mutex<Vec<ContainerSpec>>,
    resolved: Mutex<Vec<String>>,
}

impl FakeRuntime {
    /// A runtime whose containers run `handler`. Images resolve to `sha256:<hash of name>`
    /// unless set with [`set_digest`](Self::set_digest).
    pub fn new(handler: impl Fn(&ContainerSpec) -> FakeOutput + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            digests: Mutex::default(),
            missing: Mutex::default(),
            runs: Mutex::default(),
            resolved: Mutex::default(),
        }
    }

    /// Pins (or moves) an image's digest.
    pub fn set_digest(&self, image: &str, digest: &str) {
        lock(&self.digests).insert(image.to_owned(), digest.to_owned());
    }

    /// Makes resolving `image` fail, as for an image that cannot be pulled.
    pub fn set_missing(&self, image: &str) {
        lock(&self.missing).insert(image.to_owned());
    }

    /// The containers run so far.
    pub fn runs(&self) -> Vec<ContainerSpec> {
        lock(&self.runs).clone()
    }

    /// The images resolved so far.
    pub fn resolved(&self) -> Vec<String> {
        lock(&self.resolved).clone()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn resolve_image(
        &self,
        image: &str,
        _cancel: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        lock(&self.resolved).push(image.to_owned());
        if lock(&self.missing).contains(image) {
            return Err(RuntimeError::Image {
                image: image.to_owned(),
                message: "not found".into(),
            });
        }
        Ok(lock(&self.digests)
            .get(image)
            .cloned()
            .unwrap_or_else(|| format!("sha256:{}", hex::encode(Sha256::digest(image)))))
    }

    async fn run(
        &self,
        spec: &ContainerSpec,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Canceled);
        }
        lock(&self.runs).push(spec.clone());
        let out = (self.handler)(spec);
        if !out.stdout.is_empty() {
            output(OutputStream::Stdout, out.stdout.as_bytes());
        }
        if !out.stderr.is_empty() {
            output(OutputStream::Stderr, out.stderr.as_bytes());
        }
        Ok(out.exit_code)
    }
}
