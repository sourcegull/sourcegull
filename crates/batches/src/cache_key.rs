//! The cache key of step results, shared by workspace
//! resolution (`sourcegull-batches-resolver`, which looks keys up to report cache hits) and the step
//! runner (`sourcegull-batches-steps`, which extends the key with what only it knows).

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::BatchSpec;

/// Format version of [`step_cache_key`]; bump when its input changes.
pub const STEP_CACHE_KEY_VERSION: u32 = 1;

/// The cache key of the result of steps `0..=step` of `spec` in one workspace: a hex
/// SHA-256 over the bulk change's name and description, the repository name, base commit
/// and matched paths, the workspace path, `onlyFetchWorkspace`, and the definitions of the
/// steps up to `step` (command, container, env as written, files, outputs, `if`, mounts).
///
/// So editing step 4 changes the keys of steps 4 and later only. Not covered: the values of
/// pass-through env variables (executor secrets), mounted file contents and the image digest,
/// which the step runner knows and the resolver does not. Entries are stored under this key;
/// `sourcegull_batches_steps::cache` records a fingerprint of those inputs in each entry and only
/// uses an entry whose fingerprint still matches.
pub fn step_cache_key(
    spec: &BatchSpec,
    repo_name: &str,
    commit: &str,
    file_matches: &[String],
    path: &str,
    only_fetch_workspace: bool,
    step: usize,
) -> String {
    let steps = &spec.steps[..(step + 1).min(spec.steps.len())];
    let input = json!({
        "version": STEP_CACHE_KEY_VERSION,
        "batchChange": { "name": spec.name, "description": spec.description },
        "repository": { "name": repo_name, "baseRev": commit, "fileMatches": file_matches },
        "path": path,
        "onlyFetchWorkspace": only_fetch_workspace,
        "steps": steps,
    });
    hex::encode(Sha256::digest(input.to_string().as_bytes()))
}
