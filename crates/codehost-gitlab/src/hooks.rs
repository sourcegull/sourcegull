//! Project webhooks registered through the API (`webhooks.autoRegister`).
//!
//! A project's hooks whose URL has the receiver's path are the connection's. [`ensure`]
//! creates one when there is none, updates it when its URL or events differ from the wanted
//! ones (the token is sent on every create and update; GitLab never returns it), and deletes
//! duplicates. Managing project hooks needs the Maintainer role on the project and a token with
//! the `api` scope; GitLab answers 403 otherwise.
//!
//! [`ensure`]: GitLabHooks::ensure

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use sourcegull_codehost::{CodeHostError, HookChange, HookTarget, Result, WebhookRegistrar};
use url::Url;

use crate::client::GitLabClient;

/// A project hook as `GET /projects/:id/hooks` returns it.
#[derive(Debug, Clone, Deserialize)]
struct ProjectHook {
    id: i64,
    url: String,
    #[serde(default)]
    push_events: bool,
    #[serde(default)]
    tag_push_events: bool,
    #[serde(default)]
    merge_requests_events: bool,
    #[serde(default)]
    pipeline_events: bool,
}

impl ProjectHook {
    /// Whether the hook sends every event the receiver handles.
    fn has_events(&self) -> bool {
        self.push_events
            && self.tag_push_events
            && self.merge_requests_events
            && self.pipeline_events
    }
}

/// The path of a URL, or `None` when it does not parse.
fn url_path(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()
        .map(|u| u.path().trim_end_matches('/').to_owned())
}

/// Registers the connection's hooks on its projects.
#[derive(Debug, Clone)]
pub struct GitLabHooks {
    client: Arc<GitLabClient>,
}

impl GitLabHooks {
    /// Hooks managed with `client` (and its rate limiter).
    pub fn new(client: Arc<GitLabClient>) -> Self {
        Self { client }
    }

    /// The project's hooks to `target`, oldest first.
    async fn matching(&self, project: &str, target: &HookTarget) -> Result<Vec<ProjectHook>> {
        let path = url_path(&target.url)
            .ok_or_else(|| CodeHostError::Config(format!("invalid hook URL {:?}", target.url)))?;
        let hooks: Vec<ProjectHook> = self
            .client
            .get(&format!("projects/{}/hooks?per_page=100", encode(project)))
            .await?;
        let mut hooks: Vec<ProjectHook> = hooks
            .into_iter()
            .filter(|h| url_path(&h.url).as_deref() == Some(path.as_str()))
            .collect();
        hooks.sort_by_key(|h| h.id);
        Ok(hooks)
    }

    async fn delete(&self, project: &str, hook: i64) -> Result<()> {
        let url = self
            .client
            .api_url(&format!("projects/{}/hooks/{hook}", encode(project)))?;
        match self.client.send(Method::DELETE, url, None).await {
            Ok(_) | Err(CodeHostError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// The body of a hook create or update.
fn hook_body(target: &HookTarget) -> Value {
    json!({
        "url": target.url,
        "token": target.secret,
        "push_events": true,
        "tag_push_events": true,
        "merge_requests_events": true,
        "pipeline_events": true,
        "enable_ssl_verification": true,
    })
}

/// A project id or path as a URL path segment.
fn encode(project: &str) -> String {
    url::form_urlencoded::byte_serialize(project.as_bytes()).collect()
}

#[async_trait]
impl WebhookRegistrar for GitLabHooks {
    async fn ensure(&self, external_id: &str, target: &HookTarget) -> Result<HookChange> {
        let mut hooks = self.matching(external_id, target).await?;
        let change = if hooks.is_empty() {
            let _: Value = self
                .client
                .post(
                    &format!("projects/{}/hooks", encode(external_id)),
                    &hook_body(target),
                )
                .await?;
            HookChange::Created
        } else {
            let first = hooks.remove(0);
            for extra in &hooks {
                self.delete(external_id, extra.id).await?;
            }
            if first.url == target.url && first.has_events() {
                HookChange::Unchanged
            } else {
                let _: Value = self
                    .client
                    .put(
                        &format!("projects/{}/hooks/{}", encode(external_id), first.id),
                        &hook_body(target),
                    )
                    .await?;
                HookChange::Updated
            }
        };
        Ok(change)
    }

    async fn remove(&self, external_id: &str, target: &HookTarget) -> Result<usize> {
        let hooks = match self.matching(external_id, target).await {
            Ok(h) => h,
            Err(CodeHostError::NotFound(_)) => return Ok(0),
            Err(e) => return Err(e),
        };
        for h in &hooks {
            self.delete(external_id, h.id).await?;
        }
        Ok(hooks.len())
    }
}
