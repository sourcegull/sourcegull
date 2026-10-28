//! The instance calls of a local run: GraphQL operations and repository archives.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::env::Instance;

/// `currentUser { id }`: the default namespace.
pub const CURRENT_USER_QUERY: &str = "query GetCurrentUserID { currentUser { id url } }";

/// A user or organization namespace by name.
pub const NAMESPACE_QUERY: &str = "query NamespaceQuery($name: String!) {
    user(username: $name) { id url }
    organization(name: $name) { id url }
}";

/// The workspaces of a spec.
pub const RESOLVE_WORKSPACES_QUERY: &str = "query ResolveWorkspacesForBatchSpec($spec: String!) {
    resolveWorkspacesForBatchSpec(batchSpec: $spec) {
        onlyFetchWorkspace
        ignored
        unsupported
        repository {
            id
            name
            url
            externalRepository { serviceType }
            defaultBranch { name target { oid } }
        }
        branch {
            abbrevName
            name
            target { oid }
        }
        path
        searchResultPaths
    }
}";

/// A repository's ID by name (for `importChangesets`).
pub const REPOSITORY_QUERY: &str = "query Repository($name: String!) {
    repository(name: $name) { id name }
}";

/// Uploads changeset specs.
pub const CREATE_CHANGESET_SPECS_MUTATION: &str =
    "mutation CreateChangesetSpecs($specs: [String!]!) {
    createChangesetSpecs(changesetSpecs: $specs) {
        ... on HiddenChangesetSpec { id }
        ... on VisibleChangesetSpec { id }
    }
}";

/// Creates the bulk change spec of a local run.
pub const CREATE_BATCH_SPEC_MUTATION: &str =
    "mutation CreateBatchSpec($namespace: ID!, $spec: String!, $changesetSpecs: [ID!]!) {
    createBatchSpec(namespace: $namespace, batchSpec: $spec,
                    changesetSpecs: $changesetSpecs) {
        id
        applyURL
    }
}";

/// Applies a bulk change spec.
pub const APPLY_BATCH_CHANGE_MUTATION: &str = "mutation ApplyBatchChange($batchSpec: ID!) {
    applyBatchChange(batchSpec: $batchSpec) {
        ...batchChangeFields
    }
}

fragment batchChangeFields on BatchChange {
    url
}";

/// Largest total size of changeset specs uploaded in one `createChangesetSpecs` call (the
/// instance's default body limit is 10 MiB).
pub const UPLOAD_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// A GraphQL client of the instance.
#[derive(Debug, Clone)]
pub struct Client {
    instance: Instance,
    http: reqwest::Client,
}

/// A failed call.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Transport(String),
    #[error("unexpected status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    /// The operation's `errors`.
    #[error("{}", .0.join("\n"))]
    GraphQl(Vec<String>),
    #[error("unexpected response: {0}")]
    Response(String),
}

impl Client {
    pub fn new(instance: Instance, insecure_skip_verify: bool) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure_skip_verify)
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("gull/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { instance, http })
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.instance.credential.header() {
            Some(h) => req.header(reqwest::header::AUTHORIZATION, h),
            None => req,
        }
    }

    /// Runs a GraphQL operation and decodes its `data`.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T, ClientError> {
        let name = operation_name(query);
        let url = match name {
            Some(n) => self.instance.url(&format!("/.api/graphql?{n}")),
            None => self.instance.url("/.api/graphql"),
        };
        let resp = self
            .authorize(self.http.post(url))
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ClientError::Status { status, body });
        }
        #[derive(Deserialize)]
        struct Envelope {
            data: Option<Value>,
            #[serde(default)]
            errors: Vec<GraphQlError>,
        }
        #[derive(Deserialize)]
        struct GraphQlError {
            message: String,
        }
        let env: Envelope =
            serde_json::from_str(&body).map_err(|e| ClientError::Response(e.to_string()))?;
        if !env.errors.is_empty() {
            return Err(ClientError::GraphQl(
                env.errors.into_iter().map(|e| e.message).collect(),
            ));
        }
        serde_json::from_value(env.data.unwrap_or(Value::Null))
            .map_err(|e| ClientError::Response(e.to_string()))
    }

    /// A GET of a path on the instance, as bytes; `None` for 404.
    pub async fn get_bytes(
        &self,
        path: &str,
        accept: &str,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        let resp = self
            .authorize(self.http.get(self.instance.url(path)))
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status { status, body });
        }
        resp.bytes()
            .await
            .map(|b| Some(b.to_vec()))
            .map_err(|e| ClientError::Transport(e.to_string()))
    }
}

/// The name of the first operation in `query` (`query Foo(...)` → `Foo`), which the client
/// puts in the URL so the instance's logs and metrics name it.
pub fn operation_name(query: &str) -> Option<&str> {
    let rest = query
        .trim_start()
        .strip_prefix("query ")
        .or_else(|| query.trim_start().strip_prefix("mutation "))?;
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The URL path of a repository archive at a commit, as a tar archive
/// (`/{repo}@{commit}/-/raw/{path}?format=tar`): the whole tree, or only `path`.
pub fn archive_path(repo: &str, commit: &str, path: &str) -> String {
    let path = path.trim_matches('/');
    if path.is_empty() {
        format!("/{}@{commit}/-/raw/?format=tar", enc(repo))
    } else {
        format!("/{}@{commit}/-/raw/{}?format=tar", enc(repo), enc(path))
    }
}

/// The URL path of one file at a commit.
pub fn raw_file_path(repo: &str, commit: &str, path: &str) -> String {
    format!(
        "/{}@{commit}/-/raw/{}",
        enc(repo),
        enc(path.trim_matches('/'))
    )
}

/// Percent-encodes each segment of a `/`-separated path.
fn enc(path: &str) -> String {
    path.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn percent_encode(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_names() {
        assert_eq!(operation_name(CURRENT_USER_QUERY), Some("GetCurrentUserID"));
        assert_eq!(
            operation_name(CREATE_BATCH_SPEC_MUTATION),
            Some("CreateBatchSpec")
        );
        assert_eq!(operation_name("{ currentUser { id } }"), None);
    }

    #[test]
    fn archive_paths() {
        assert_eq!(
            archive_path("gitlab.example.com/a/b", "abc", ""),
            "/gitlab.example.com/a/b@abc/-/raw/?format=tar"
        );
        assert_eq!(
            archive_path("gitlab.example.com/a/b", "abc", "/src/my dir/"),
            "/gitlab.example.com/a/b@abc/-/raw/src/my%20dir?format=tar"
        );
        assert_eq!(
            raw_file_path("gitlab.example.com/a/b", "abc", ".gitignore"),
            "/gitlab.example.com/a/b@abc/-/raw/.gitignore"
        );
    }
}
