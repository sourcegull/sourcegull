//! Remotes, credentials and redaction.
//!
//! Credentials never appear in argv or in URLs written to disk. HTTPS
//! credentials reach git through `GIT_ASKPASS`: a tiny helper script prints
//! the username or password from environment variables that are set only on
//! the git child process. SSH uses a mounted key and `known_hosts` through
//! `GIT_SSH_COMMAND`. Every string that leaves a git subprocess (stderr,
//! progress, errors) goes through a [`Redactor`] first.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Environment variable read by the askpass helper for the username.
pub const ASKPASS_USERNAME_ENV: &str = "SOURCEGULL_GIT_USERNAME";
/// Environment variable read by the askpass helper for the password.
pub const ASKPASS_PASSWORD_ENV: &str = "SOURCEGULL_GIT_PASSWORD";

const ASKPASS_SCRIPT: &str = "#!/bin/sh\n\
# Written by sourcegull: answers git credential prompts from the environment.\n\
case \"$1\" in\n\
  [Uu]sername*) printf '%s\\n' \"$SOURCEGULL_GIT_USERNAME\" ;;\n\
  *) printf '%s\\n' \"$SOURCEGULL_GIT_PASSWORD\" ;;\n\
esac\n";

/// A string that must not be logged. `Debug` and `Display` print `[redacted]`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The secret value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// How to authenticate to a remote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RemoteAuth {
    /// Anonymous access.
    #[default]
    None,
    /// HTTP(S) basic auth answered through `GIT_ASKPASS`.
    Basic {
        /// Username.
        username: String,
        /// Password or token.
        password: Secret,
    },
    /// SSH with a private key file.
    Ssh {
        /// Path of the private key (mounted secret).
        private_key: PathBuf,
        /// `known_hosts` file; host keys are checked strictly when set.
        known_hosts: Option<PathBuf>,
    },
}

impl RemoteAuth {
    /// GitLab OAuth or personal access token over HTTPS (`oauth2:<token>`).
    pub fn gitlab_token(token: impl Into<String>) -> Self {
        RemoteAuth::Basic {
            username: "oauth2".to_owned(),
            password: Secret::new(token),
        }
    }

    fn secrets(&self) -> Vec<String> {
        match self {
            RemoteAuth::Basic { password, .. } => vec![password.expose().to_owned()],
            _ => Vec::new(),
        }
    }
}

/// A remote to clone or fetch from. The URL never contains credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    url: String,
    auth: RemoteAuth,
}

impl Remote {
    /// Creates a remote. Userinfo embedded in `url` is removed; if `auth` is
    /// [`RemoteAuth::None`] and the URL carried `user:password`, those become
    /// basic credentials instead of staying in the URL.
    pub fn new(url: &str, auth: RemoteAuth) -> Self {
        let (clean, userinfo) = split_userinfo(url);
        let auth = match (auth, userinfo) {
            (RemoteAuth::None, Some((username, Some(password)))) => RemoteAuth::Basic {
                username,
                password: Secret::new(password),
            },
            (auth, _) => auth,
        };
        Self { url: clean, auth }
    }

    /// The credential-free URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The credentials.
    pub fn auth(&self) -> &RemoteAuth {
        &self.auth
    }

    /// A redactor for this remote's secrets.
    pub fn redactor(&self) -> Redactor {
        Redactor::new(self.auth.secrets())
    }

    /// Environment variables for a git child process talking to this remote.
    /// `askpass` is the helper written by [`write_askpass`].
    pub fn git_env(&self, askpass: &Path) -> Vec<(String, OsString)> {
        let mut env = vec![
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".into()),
            // Abort transfers that stall below 1 KB/s for a minute.
            ("GIT_HTTP_LOW_SPEED_LIMIT".to_owned(), "1000".into()),
            ("GIT_HTTP_LOW_SPEED_TIME".to_owned(), "60".into()),
        ];
        match &self.auth {
            RemoteAuth::None => {}
            RemoteAuth::Basic { username, password } => {
                env.push(("GIT_ASKPASS".to_owned(), askpass.as_os_str().to_owned()));
                env.push((ASKPASS_USERNAME_ENV.to_owned(), username.into()));
                env.push((ASKPASS_PASSWORD_ENV.to_owned(), password.expose().into()));
            }
            RemoteAuth::Ssh {
                private_key,
                known_hosts,
            } => {
                let mut cmd = format!(
                    "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes",
                    shell_quote(&private_key.to_string_lossy())
                );
                match known_hosts {
                    Some(kh) => cmd.push_str(&format!(
                        " -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",
                        shell_quote(&kh.to_string_lossy())
                    )),
                    None => cmd.push_str(" -o StrictHostKeyChecking=accept-new"),
                }
                env.push(("GIT_SSH_COMMAND".to_owned(), cmd.into()));
            }
        }
        env
    }
}

/// Writes the askpass helper into `dir` (mode 0700) and returns its path.
/// Rewritten atomically on every call so a stale or tampered copy is replaced.
pub fn write_askpass(dir: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir)?;
    let path = dir.join("askpass.sh");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(ASKPASS_SCRIPT.as_bytes())?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

/// Removes secrets and URL credentials from text that leaves a git process.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Redacts each of `secrets` (empty strings are ignored).
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    /// Returns `text` with secrets and `scheme://user:pass@` userinfo replaced.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for s in &self.secrets {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "[REDACTED]");
            }
        }
        redact_url_userinfo(&out)
    }
}

/// Splits `scheme://[user[:pass]@]host/…` into the URL without userinfo and
/// the userinfo parts. Non-URL remotes (`git@host:path`, local paths) are
/// returned unchanged.
fn split_userinfo(url: &str) -> (String, Option<(String, Option<String>)>) {
    let Some(scheme_end) = url.find("://") else {
        return (url.to_owned(), None);
    };
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return (url.to_owned(), None);
    };
    let userinfo = &authority[..at];
    let clean = format!("{}{}", &url[..scheme_end + 3], &rest[at + 1..]);
    let parts = match userinfo.split_once(':') {
        Some((u, p)) => (u.to_owned(), Some(p.to_owned())),
        None => (userinfo.to_owned(), None),
    };
    (clean, Some(parts))
}

fn redact_url_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '/' || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) if authority[..at].contains(':') => {
                out.push_str("[REDACTED]@");
                out.push_str(&authority[at + 1..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_moved_out_of_the_url() {
        let r = Remote::new(
            "https://bob:s3cret@gitlab.example.com/g/p.git",
            RemoteAuth::None,
        );
        assert_eq!(r.url(), "https://gitlab.example.com/g/p.git");
        assert_eq!(
            r.auth(),
            &RemoteAuth::Basic {
                username: "bob".into(),
                password: Secret::new("s3cret")
            }
        );
        // Explicit auth wins; the URL is still cleaned.
        let r = Remote::new(
            "https://bob:s3cret@gitlab.example.com/g/p.git",
            RemoteAuth::gitlab_token("tok"),
        );
        assert_eq!(r.url(), "https://gitlab.example.com/g/p.git");
        assert!(matches!(r.auth(), RemoteAuth::Basic { username, .. } if username == "oauth2"));
        // scp-style and local remotes are untouched.
        assert_eq!(
            Remote::new("git@gitlab.com:g/p.git", RemoteAuth::None).url(),
            "git@gitlab.com:g/p.git"
        );
        assert_eq!(
            Remote::new("/srv/x.git", RemoteAuth::None).url(),
            "/srv/x.git"
        );
    }

    #[test]
    fn secrets_do_not_leak_through_debug_or_redactor() {
        let auth = RemoteAuth::gitlab_token("glpat-abcdef123456");
        assert!(!format!("{auth:?}").contains("glpat"));
        let r = Remote::new("https://gitlab.com/g/p.git", auth);
        let red = r.redactor();
        let text =
            "fatal: auth failed for glpat-abcdef123456 at https://oauth2:other@gitlab.com/g/p.git";
        let out = red.redact(text);
        assert!(!out.contains("glpat-abcdef123456"), "{out}");
        assert!(!out.contains("other"), "{out}");
        assert!(
            out.contains("https://[REDACTED]@gitlab.com/g/p.git"),
            "{out}"
        );
        // A user without password is not a secret.
        assert_eq!(red.redact("ssh://git@host/x"), "ssh://git@host/x");
    }

    #[test]
    fn env_carries_credentials_only_in_env() {
        let r = Remote::new(
            "https://gitlab.com/g/p.git",
            RemoteAuth::gitlab_token("tok"),
        );
        let env = r.git_env(Path::new("/data/tmp/askpass.sh"));
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("GIT_ASKPASS"), Some("/data/tmp/askpass.sh".into()));
        assert_eq!(get(ASKPASS_PASSWORD_ENV), Some("tok".into()));
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0".into()));

        let ssh = Remote::new(
            "git@gitlab.com:g/p.git",
            RemoteAuth::Ssh {
                private_key: "/keys/id's".into(),
                known_hosts: Some("/keys/known_hosts".into()),
            },
        );
        let env = ssh.git_env(Path::new("/x"));
        let cmd = env
            .iter()
            .find(|(n, _)| n == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .clone();
        let cmd = cmd.to_string_lossy();
        assert!(cmd.contains(r"-i '/keys/id'\''s'"), "{cmd}");
        assert!(cmd.contains("StrictHostKeyChecking=yes"), "{cmd}");
    }

    #[test]
    fn askpass_script_answers_from_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_askpass(dir.path()).unwrap();
        let run = |prompt: &str| {
            // A concurrent fork in another test thread can briefly hold the
            // just-written file open (ETXTBSY); retry a few times.
            for _ in 0..50 {
                match std::process::Command::new(&path)
                    .arg(prompt)
                    .env(ASKPASS_USERNAME_ENV, "oauth2")
                    .env(ASKPASS_PASSWORD_ENV, "tok")
                    .output()
                {
                    Ok(out) => return String::from_utf8(out.stdout).unwrap(),
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            panic!("askpass kept failing with ETXTBSY");
        };
        assert_eq!(run("Username for 'https://gitlab.com': "), "oauth2\n");
        assert_eq!(run("Password for 'https://oauth2@gitlab.com': "), "tok\n");
        // Rewriting replaces the file in place.
        assert_eq!(write_askpass(dir.path()).unwrap(), path);
    }
}
