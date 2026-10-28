//! The environment: the instance and credentials (`SOURCEGULL_*`, or the alternative `SRC_*`), and what
//! GitLab CI tells a job about itself (`CI_*`).

use std::collections::HashMap;
use std::path::PathBuf;

use url::Url;

/// Environment variables. Empty values count as unset.
pub trait Env {
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

impl Env for HashMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).filter(|v| !v.is_empty()).cloned()
    }
}

/// How requests authenticate.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A Sourcegull access token: `Authorization: token …`.
    AccessToken(String),
    /// A GitLab CI job token (`CI_JOB_TOKEN`): `Authorization: token-gitlab-ci …`. The
    /// instance accepts it for SCIP uploads of the job's project when
    /// `codeIntel.gitlabJobTokenUploads` is on.
    GitLabJobToken(String),
    /// Anonymous.
    None,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AccessToken(_) => "AccessToken(<redacted>)",
            Self::GitLabJobToken(_) => "GitLabJobToken(<redacted>)",
            Self::None => "None",
        })
    }
}

impl Credential {
    /// The `Authorization` header value.
    pub fn header(&self) -> Option<String> {
        match self {
            Self::AccessToken(t) => Some(format!("token {t}")),
            Self::GitLabJobToken(t) => Some(format!("token-gitlab-ci {t}")),
            Self::None => None,
        }
    }
}

/// The Sourcegull instance to talk to.
#[derive(Debug, Clone)]
pub struct Instance {
    /// Base URL without a trailing slash, e.g. `https://sourcegull.example.com`.
    pub endpoint: Url,
    pub credential: Credential,
}

impl Instance {
    /// From `SOURCEGULL_ENDPOINT` (or `SRC_ENDPOINT`) and `SOURCEGULL_ACCESS_TOKEN` (or
    /// `SRC_ACCESS_TOKEN`). In GitLab CI without an access token, the job's `CI_JOB_TOKEN`.
    pub fn from_env(env: &dyn Env) -> anyhow::Result<Self> {
        let raw = env
            .var("SOURCEGULL_ENDPOINT")
            .or_else(|| env.var("SRC_ENDPOINT"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no Sourcegull instance configured: set SOURCEGULL_ENDPOINT (or SRC_ENDPOINT), \
                     e.g. https://sourcegull.example.com"
                )
            })?;
        let endpoint = Url::parse(raw.trim_end_matches('/'))
            .map_err(|e| anyhow::anyhow!("invalid endpoint {raw:?}: {e}"))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            anyhow::bail!("invalid endpoint {raw:?}: must be an http(s) URL");
        }
        let credential = match env
            .var("SOURCEGULL_ACCESS_TOKEN")
            .or_else(|| env.var("SRC_ACCESS_TOKEN"))
        {
            Some(t) => Credential::AccessToken(t),
            None => match GitLabCi::detect(env).and_then(|ci| ci.job_token) {
                Some(t) => Credential::GitLabJobToken(t),
                None => Credential::None,
            },
        };
        Ok(Self {
            endpoint,
            credential,
        })
    }

    /// The absolute URL of `path` (which starts with `/`) on the instance.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint.as_str().trim_end_matches('/'))
    }
}

/// What a GitLab CI job knows about itself (`GITLAB_CI=true`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitLabCi {
    /// The repository name as Sourcegull's default `repositoryPathPattern` (`{host}/{pathWithNamespace}`)
    /// makes it: from `CI_PROJECT_URL`, else `CI_SERVER_HOST`(`:CI_SERVER_PORT`) and
    /// `CI_PROJECT_PATH`.
    pub repo: Option<String>,
    /// `CI_COMMIT_SHA`.
    pub commit: Option<String>,
    /// `CI_PROJECT_DIR`, the checkout.
    pub project_dir: Option<PathBuf>,
    /// `CI_JOB_TOKEN`.
    pub job_token: Option<String>,
}

impl GitLabCi {
    /// `None` outside GitLab CI.
    pub fn detect(env: &dyn Env) -> Option<Self> {
        if env.var("GITLAB_CI").as_deref() != Some("true") {
            return None;
        }
        let from_url = env.var("CI_PROJECT_URL").and_then(|u| {
            let u = Url::parse(&u).ok()?;
            let host = u.host_str()?;
            let host = match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            let path = u.path().trim_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            (!path.is_empty()).then(|| format!("{host}/{path}"))
        });
        let from_parts = || {
            let host = env.var("CI_SERVER_HOST")?;
            let path = env.var("CI_PROJECT_PATH")?;
            let host = match env.var("CI_SERVER_PORT").as_deref() {
                None | Some("80" | "443") => host,
                Some(port) => format!("{host}:{port}"),
            };
            Some(format!("{host}/{}", path.trim_matches('/')))
        };
        Some(Self {
            repo: from_url.or_else(from_parts),
            commit: env.var("CI_COMMIT_SHA"),
            project_dir: env.var("CI_PROJECT_DIR").map(PathBuf::from),
            job_token: env.var("CI_JOB_TOKEN"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn gitlab_ci_inference() {
        assert_eq!(GitLabCi::detect(&env(&[("CI_COMMIT_SHA", "abc")])), None);
        let ci = GitLabCi::detect(&env(&[
            ("GITLAB_CI", "true"),
            ("CI_PROJECT_URL", "https://gitlab.example.com/acme/sub/demo"),
            ("CI_SERVER_HOST", "ignored.example.com"),
            ("CI_PROJECT_PATH", "ignored/path"),
            ("CI_COMMIT_SHA", "0123456789abcdef0123456789abcdef01234567"),
            ("CI_PROJECT_DIR", "/builds/acme/sub/demo"),
            ("CI_JOB_TOKEN", "job-token"),
        ]))
        .unwrap();
        assert_eq!(ci.repo.as_deref(), Some("gitlab.example.com/acme/sub/demo"));
        assert_eq!(
            ci.commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            ci.project_dir.as_deref(),
            Some(std::path::Path::new("/builds/acme/sub/demo"))
        );
        assert_eq!(ci.job_token.as_deref(), Some("job-token"));

        // A non-default port is part of the repository name, as discovery names it.
        let ci = GitLabCi::detect(&env(&[
            ("GITLAB_CI", "true"),
            ("CI_PROJECT_URL", "http://gitlab.localhost:8929/acme/demo/"),
        ]))
        .unwrap();
        assert_eq!(ci.repo.as_deref(), Some("gitlab.localhost:8929/acme/demo"));

        // Without CI_PROJECT_URL: host, port and path.
        let ci = GitLabCi::detect(&env(&[
            ("GITLAB_CI", "true"),
            ("CI_SERVER_HOST", "gitlab.com"),
            ("CI_SERVER_PORT", "443"),
            ("CI_PROJECT_PATH", "acme/demo"),
        ]))
        .unwrap();
        assert_eq!(ci.repo.as_deref(), Some("gitlab.com/acme/demo"));
        let ci = GitLabCi::detect(&env(&[
            ("GITLAB_CI", "true"),
            ("CI_SERVER_HOST", "git.internal"),
            ("CI_SERVER_PORT", "8443"),
            ("CI_PROJECT_PATH", "acme/demo"),
        ]))
        .unwrap();
        assert_eq!(ci.repo.as_deref(), Some("git.internal:8443/acme/demo"));
    }

    #[test]
    fn instance_and_credentials() {
        assert!(Instance::from_env(&env(&[])).is_err());
        assert!(Instance::from_env(&env(&[("SOURCEGULL_ENDPOINT", "ftp://x")])).is_err());

        // The `SRC_*` variables work; `SOURCEGULL_*` ones win.
        let i = Instance::from_env(&env(&[
            ("SRC_ENDPOINT", "https://src.example.com/"),
            ("SRC_ACCESS_TOKEN", "src-token"),
        ]))
        .unwrap();
        assert_eq!(
            i.url("/.api/scip/upload"),
            "https://src.example.com/.api/scip/upload"
        );
        assert_eq!(i.credential, Credential::AccessToken("src-token".into()));
        let i = Instance::from_env(&env(&[
            ("SRC_ENDPOINT", "https://src.example.com"),
            ("SOURCEGULL_ENDPOINT", "https://sourcegull.example.com/"),
            ("SRC_ACCESS_TOKEN", "src-token"),
            ("SOURCEGULL_ACCESS_TOKEN", "sourcegull-token"),
            ("GITLAB_CI", "true"),
            ("CI_JOB_TOKEN", "job"),
        ]))
        .unwrap();
        assert_eq!(i.url("/x"), "https://sourcegull.example.com/x");
        assert_eq!(
            i.credential,
            Credential::AccessToken("sourcegull-token".into())
        );
        assert_eq!(
            i.credential.header().as_deref(),
            Some("token sourcegull-token")
        );

        // In GitLab CI without an access token, the job token.
        let i = Instance::from_env(&env(&[
            ("SOURCEGULL_ENDPOINT", "https://sourcegull.example.com"),
            ("GITLAB_CI", "true"),
            ("CI_JOB_TOKEN", "job"),
        ]))
        .unwrap();
        assert_eq!(i.credential, Credential::GitLabJobToken("job".into()));
        assert_eq!(
            i.credential.header().as_deref(),
            Some("token-gitlab-ci job")
        );
        // Outside CI the job token is not used.
        let i = Instance::from_env(&env(&[
            ("SOURCEGULL_ENDPOINT", "https://sourcegull.example.com"),
            ("CI_JOB_TOKEN", "job"),
        ]))
        .unwrap();
        assert_eq!(i.credential, Credential::None);
    }
}
