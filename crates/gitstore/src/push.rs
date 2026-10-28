//! Pushes with per-request credentials (the reconciler's `Push`).
//!
//! Credentials reach git only through the askpass helper (as for fetches,
//! [`crate::remote`]); the URL passed to git has no userinfo, and stderr goes
//! through the remote's [`Redactor`](crate::remote::Redactor) before it is
//! returned. Only http(s) transports are enabled (`protocol.allow=never`
//! plus explicit allows), and local remotes only when the node allows them
//! (tests and development), so a request cannot make the node push into
//! another repository on its own disk.
//!
//! git's error output is classified into [`PushRejection`]s the reconciler can
//! act on: bad credentials, missing permission (protected branch, archived or
//! missing project, no write access) and non-fast-forward.

use std::path::Path;
use std::time::Duration;

use gix::ObjectId;

use crate::command::GitCommand;
use crate::error::{GitError, PushRejection, Result};
use crate::remote::Remote;

/// What to push.
#[derive(Debug, Clone)]
pub struct PushSpec {
    /// The remote, with credentials.
    pub remote: Remote,
    /// The local commit to push.
    pub commit: ObjectId,
    /// Full ref name on the remote.
    pub destination_ref: String,
    /// Overwrite the remote ref even if this is not a fast-forward.
    pub force: bool,
}

/// Whether `url` is a local path or `file://` URL.
pub fn is_local_url(url: &str) -> bool {
    url.starts_with('/') || url.to_ascii_lowercase().starts_with("file://")
}

/// Checks that `url` uses a transport pushes may use: http(s), or a local path
/// when `allow_local` is set.
pub fn check_push_url(url: &str, allow_local: bool) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(());
    }
    if allow_local && is_local_url(url) {
        return Ok(());
    }
    Err(GitError::InvalidArgument(
        "push remote must be an http(s) URL".into(),
    ))
}

/// The host (without port and userinfo, lowercased) of an http(s), ssh or
/// scp-style (`git@host:path`) URL; `None` for local paths.
pub fn url_host(url: &str) -> Option<String> {
    let authority = if let Some(i) = url.find("://") {
        let rest = &url[i + 3..];
        &rest[..rest.find('/').unwrap_or(rest.len())]
    } else if !url.starts_with('/') && url.contains(':') {
        &url[..url.find(':').unwrap_or(url.len())]
    } else {
        return None;
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = host.strip_prefix('[') {
        // [ipv6]:port
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Classifies `git push` stderr. `None` means some other failure (network,
/// remote error) that stays a plain command failure.
pub fn classify_push_error(stderr: &str) -> Option<PushRejection> {
    let s = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    if any(&[
        "non-fast-forward",
        "(fetch first)",
        "(stale info)",
        "updates were rejected because the tip",
        "updates were rejected because the remote contains",
    ]) {
        return Some(PushRejection::NonFastForward);
    }
    if any(&[
        "authentication failed",
        "http basic: access denied",
        "could not read username",
        "could not read password",
        "invalid username or password",
        "requested url returned error: 401",
        "terminal prompts disabled",
    ]) {
        return Some(PushRejection::Unauthenticated);
    }
    if any(&[
        "not allowed to push",
        "not allowed to force push",
        "protected branch",
        "archived",
        "pre-receive hook declined",
        "permission denied",
        "permission to push",
        "requested url returned error: 403",
        "requested url returned error: 404",
        "could not be found",
        "repository not found",
        "does not appear to be a git repository",
    ]) {
        return Some(PushRejection::PermissionDenied);
    }
    None
}

/// Pushes `spec.commit` to `spec.destination_ref` from the bare repo at
/// `repo_dir`. `askpass` is the store's helper script.
pub async fn push(
    git_bin: &Path,
    askpass: &Path,
    repo_dir: &Path,
    spec: &PushSpec,
    allow_local: bool,
    timeout: Duration,
) -> Result<()> {
    let url = spec.remote.url();
    check_push_url(url, allow_local)?;
    let mut cmd = GitCommand::new(git_bin, "push")
        .repo(repo_dir)
        .timeout(timeout)
        .redactor(spec.remote.redactor())
        .envs(crate::patch::isolated_config_env())
        .envs(spec.remote.git_env(askpass))
        // Credentials come only from the askpass helper.
        .config("credential.helper=")
        .config("protocol.allow=never")
        .config("protocol.https.allow=always")
        .config("protocol.http.allow=always");
    if allow_local {
        cmd = cmd.config("protocol.file.allow=always");
    }
    cmd = cmd.arg("--no-verify");
    if spec.force {
        cmd = cmd.arg("--force");
    }
    let refspec = format!("{}:{}", spec.commit, spec.destination_ref);
    match cmd.arg("--").arg(url).arg(refspec).run().await {
        Ok(_) => Ok(()),
        Err(GitError::Command { op, status, stderr }) => match classify_push_error(&stderr) {
            Some(reason) => Err(GitError::PushRejected { reason, stderr }),
            None => Err(GitError::Command { op, status, stderr }),
        },
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_urls_are_restricted() {
        check_push_url("https://gitlab.com/g/p.git", false).unwrap();
        check_push_url("HTTP://gitlab.local/g/p.git", false).unwrap();
        for bad in [
            "/srv/repos/1.git",
            "file:///srv/x.git",
            "git@gitlab.com:g/p.git",
            "ssh://git@gitlab.com/g/p.git",
            "ext::sh -c touch% /tmp/x",
            "",
        ] {
            assert!(check_push_url(bad, false).is_err(), "{bad:?}");
        }
        check_push_url("/srv/x.git", true).unwrap();
        check_push_url("file:///srv/x.git", true).unwrap();
        assert!(check_push_url("ext::sh", true).is_err());
    }

    #[test]
    fn hosts_are_extracted() {
        let h = |u: &str| url_host(u);
        assert_eq!(
            h("https://oauth2:t@GitLab.com:8443/g/p.git").as_deref(),
            Some("gitlab.com")
        );
        assert_eq!(h("git@gitlab.com:g/p.git").as_deref(), Some("gitlab.com"));
        assert_eq!(
            h("ssh://git@gitlab.example.com:2222/g/p").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(h("http://[::1]:80/x").as_deref(), Some("::1"));
        assert_eq!(h("/srv/x.git"), None);
    }

    #[test]
    fn gitlab_errors_are_classified() {
        let cases = [
            (
                "remote: HTTP Basic: Access denied.\nfatal: Authentication failed for 'https://gitlab.com/g/p.git/'",
                Some(PushRejection::Unauthenticated),
            ),
            (
                "fatal: could not read Username for 'https://gitlab.com': terminal prompts disabled",
                Some(PushRejection::Unauthenticated),
            ),
            (
                "remote: GitLab: You are not allowed to push code to protected branches on this project.\n ! [remote rejected] x -> main (pre-receive hook declined)",
                Some(PushRejection::PermissionDenied),
            ),
            (
                "remote: You are not allowed to push code to this project.\nfatal: unable to access 'https://gitlab.com/g/p.git/': The requested URL returned error: 403",
                Some(PushRejection::PermissionDenied),
            ),
            (
                "remote: The project you were looking for could not be found or you don't have permission to view it.",
                Some(PushRejection::PermissionDenied),
            ),
            (
                " ! [rejected]        abc -> feature (non-fast-forward)\nerror: failed to push some refs",
                Some(PushRejection::NonFastForward),
            ),
            (
                " ! [rejected]        abc -> feature (fetch first)",
                Some(PushRejection::NonFastForward),
            ),
            ("fatal: unable to access: Could not resolve host: x", None),
        ];
        for (stderr, want) in cases {
            assert_eq!(classify_push_error(stderr), want, "{stderr}");
        }
    }
}
