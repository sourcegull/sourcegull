//! Serving fetches over git's smart HTTP protocol (`/.executors/git/{repo}`).
//!
//! `git upload-pack --stateless-rpc` answers one request of the protocol: the ref
//! advertisement (`info/refs`) or a negotiation (`git-upload-pack` POST). Fetching a commit
//! by id is allowed when it is reachable from a ref, so executors can
//! `git fetch --depth=1 origin <sha>`.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tokio_stream::wrappers::ReceiverStream;

use crate::archive::stream_stdout;
use crate::command::GitCommand;
use crate::error::Result;

/// Largest negotiation request accepted (wants, haves and shallow lines); it travels in one
/// gRPC message, so it stays under the default 4 MiB message limit.
pub const MAX_REQUEST: usize = 2 * 1024 * 1024;

/// Runs one stateless upload-pack exchange in `repo_dir` and streams its response.
/// `advertise_refs` answers `info/refs` (without the `# service=` header, which the HTTP
/// layer adds); otherwise `request` is the client's POST body. `git_protocol` is the
/// client's `Git-Protocol` header (e.g. `version=2`), empty for v0.
pub fn upload_pack_stream(
    git_bin: &Path,
    repo_dir: &Path,
    advertise_refs: bool,
    request: Bytes,
    git_protocol: &str,
    timeout: Duration,
) -> Result<ReceiverStream<Result<Bytes>>> {
    let mut cmd = GitCommand::new(git_bin, "upload-pack")
        .config("uploadpack.allowReachableSHA1InWant=true")
        .config("uploadpack.allowFilter=true")
        .arg("--stateless-rpc");
    if advertise_refs {
        cmd = cmd.arg("--advertise-refs");
    } else {
        cmd = cmd.stdin(request);
    }
    if is_valid_protocol(git_protocol) {
        cmd = cmd.envs([("GIT_PROTOCOL".to_owned(), git_protocol.into())]);
    }
    let running = cmd.arg(repo_dir).timeout(timeout).spawn_streaming()?;
    Ok(stream_stdout(running, |e| e))
}

/// `Git-Protocol` values look like `version=2` or `version=2:object-format=sha1`.
fn is_valid_protocol(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"=:-_.".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_values() {
        assert!(is_valid_protocol("version=2"));
        assert!(is_valid_protocol("version=2:object-format=sha1"));
        assert!(!is_valid_protocol(""));
        assert!(!is_valid_protocol("version=2\nX=1"));
        assert!(!is_valid_protocol("a b"));
    }
}
