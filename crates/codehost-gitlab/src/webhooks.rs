//! GitLab webhooks and system hooks.
//!
//! Requests are verified by comparing `X-Gitlab-Token` in constant time against the
//! connection's `webhooks[].secret` values, then dispatched on the body's `object_kind`
//! (project and group hooks) or `event_name` (system hooks):
//!
//! - `push`, `tag_push` → [`DomainEvent::RefsUpdated`] (fetch now).
//! - `merge_request` → [`DomainEvent::ChangesetChanged`] with what the payload says
//!   ([`ChangesetHook`]): the state after the change (`state` plus `draft`), title and
//!   description, labels, timeline events by `action` (`close`, `reopen`, `merge`,
//!   `approved`/`approval`, `unapproved`/`unapproval`, a draft toggle in `update`), the review
//!   state for `approved` (fully approved) and `unapproved`, and `resync` when an `update`
//!   changed more than the payload carries (new commits, target branch, …).
//! - `pipeline` → [`DomainEvent::ChangesetChecksChanged`] with the pipeline's check state and
//!   timeline event (keyed `pipeline:<id>`, as a sync keys it), for its `merge_request` or,
//!   without one, its branch.
//! - System hooks `project_create|destroy|rename|transfer` → [`DomainEvent::RepoChanged`]
//!   (targeted re-listing), when the connection enables `systemHook`.
//! - System hooks `user_add_to_team`, `user_remove_from_team`, `user_update_for_team`,
//!   `user_add_to_group`, `user_remove_from_group` → [`DomainEvent::UserPermsChanged`].
//! - Anything else → [`DomainEvent::Ignored`].

use chrono::{DateTime, NaiveDateTime, Utc};
use http::HeaderMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sourcegull_codehost::{
    ChangesetEvent, ChangesetEventKind, ChangesetHook, ChangesetState, ChecksHook, DomainEvent,
    HOOK_EVENT_PREFIX, RefChange, RepoChange, ReviewState, WebhookError, WebhookHandler,
};

use crate::changesets::{has_draft_prefix, pipeline_state};
use crate::config::GitLabConnection;

/// Header carrying the shared secret.
pub const TOKEN_HEADER: &str = "x-gitlab-token";
/// Header naming the event, e.g. `Push Hook`.
pub const EVENT_HEADER: &str = "x-gitlab-event";

/// The all-zero object id GitLab uses for created and deleted refs.
const NULL_SHA: &str = "0000000000000000000000000000000000000000";

/// Whether `token` equals one of `secrets` (constant time per comparison; an empty token never
/// matches).
pub fn token_matches<'a>(token: &str, secrets: impl IntoIterator<Item = &'a str>) -> bool {
    if token.is_empty() {
        return false;
    }
    secrets
        .into_iter()
        .filter(|s| !s.is_empty())
        .fold(false, |found, s| {
            sourcegull_crypto::constant_time_eq(token.as_bytes(), s.as_bytes()) | found
        })
}

/// The `X-Gitlab-Token` of a request, if present.
pub fn request_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok())
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    object_kind: Option<String>,
    #[serde(default)]
    event_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PushEvent {
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    project_id: Option<i64>,
    #[serde(default)]
    project: Option<ProjectRef>,
}

#[derive(Debug, Deserialize)]
struct ProjectRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct MergeRequestEvent {
    project: ProjectRef,
    object_attributes: MergeRequestAttributes,
    #[serde(default)]
    user: Option<HookUser>,
    #[serde(default)]
    labels: Option<Vec<HookLabel>>,
    #[serde(default)]
    changes: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct MergeRequestAttributes {
    iid: i64,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    work_in_progress: Option<bool>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// Set by `update` when new commits were pushed.
    #[serde(default)]
    oldrev: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HookUser {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HookLabel {
    title: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PipelineEvent {
    project: ProjectRef,
    #[serde(default)]
    object_attributes: Option<PipelineAttributes>,
    #[serde(default)]
    merge_request: Option<PipelineMergeRequest>,
}

#[derive(Debug, Deserialize)]
struct PipelineMergeRequest {
    iid: i64,
}

#[derive(Debug, Deserialize)]
struct PipelineAttributes {
    id: i64,
    #[serde(rename = "ref", default)]
    ref_name: Option<String>,
    #[serde(default)]
    tag: bool,
    #[serde(default)]
    sha: Option<String>,
    status: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SystemProjectEvent {
    #[serde(default)]
    project_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SystemUserEvent {
    user_id: i64,
    #[serde(default)]
    user_username: Option<String>,
}

fn malformed(e: serde_json::Error) -> WebhookError {
    WebhookError::Malformed(e.to_string())
}

fn oid(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty() && s != NULL_SHA)
}

/// Parses a GitLab webhook body into events (without verifying it).
pub fn parse_event(body: &[u8], system_hooks: bool) -> Result<Vec<DomainEvent>, WebhookError> {
    let value: Value = serde_json::from_slice(body).map_err(malformed)?;
    let env: Envelope = serde_json::from_value(value.clone()).map_err(malformed)?;
    let kind = env
        .object_kind
        .as_deref()
        .or(env.event_name.as_deref())
        .ok_or_else(|| WebhookError::Malformed("no object_kind or event_name".into()))?;
    let event = match kind {
        "push" | "tag_push" => {
            let push: PushEvent = serde_json::from_value(value).map_err(malformed)?;
            let project = push
                .project_id
                .or(push.project.map(|p| p.id))
                .ok_or_else(|| WebhookError::Malformed("push event without project".into()))?;
            DomainEvent::RefsUpdated {
                external_id: project.to_string(),
                refs: vec![RefChange {
                    name: push.ref_name,
                    old_oid: oid(push.before),
                    new_oid: oid(push.after),
                }],
            }
        }
        "merge_request" => {
            let mr: MergeRequestEvent = serde_json::from_value(value).map_err(malformed)?;
            DomainEvent::ChangesetChanged {
                external_id: mr.project.id.to_string(),
                number: mr.object_attributes.iid,
                hook: merge_request_hook(&mr),
            }
        }
        "pipeline" => {
            let p: PipelineEvent = serde_json::from_value(value).map_err(malformed)?;
            let branch = p
                .object_attributes
                .as_ref()
                .filter(|a| !a.tag)
                .and_then(|a| a.ref_name.as_deref())
                .map(|r| format!("refs/heads/{}", r.trim_start_matches("refs/heads/")));
            DomainEvent::ChangesetChecksChanged {
                external_id: p.project.id.to_string(),
                number: p.merge_request.map(|m| m.iid),
                branch,
                checks: p.object_attributes.as_ref().map(checks_hook),
            }
        }
        "project_create" | "project_destroy" | "project_rename" | "project_transfer"
            if system_hooks =>
        {
            let e: SystemProjectEvent = serde_json::from_value(value).map_err(malformed)?;
            DomainEvent::RepoChanged {
                external_id: e.project_id.map(|id| id.to_string()),
                change: match kind {
                    "project_create" => RepoChange::Created,
                    "project_destroy" => RepoChange::Destroyed,
                    "project_rename" => RepoChange::Renamed,
                    _ => RepoChange::Transferred,
                },
            }
        }
        "user_add_to_team"
        | "user_remove_from_team"
        | "user_update_for_team"
        | "user_add_to_group"
        | "user_remove_from_group"
            if system_hooks =>
        {
            let e: SystemUserEvent = serde_json::from_value(value).map_err(malformed)?;
            DomainEvent::UserPermsChanged {
                account_id: e.user_id.to_string(),
                username: e.user_username,
            }
        }
        other => DomainEvent::Ignored {
            event: other.to_owned(),
        },
    };
    Ok(vec![event])
}

/// Parses a webhook timestamp: RFC 3339 (`2026-10-19T09:00:00Z`, merge request events) or
/// GitLab's `2026-10-19 09:00:00 UTC` (pipeline events, older versions).
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    if let Ok(t) = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z") {
        return Some(t.with_timezone(&Utc));
    }
    let naive = s.strip_suffix(" UTC").unwrap_or(s);
    NaiveDateTime::parse_from_str(naive, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|t| t.and_utc())
}

/// `changes` keys of an `update` the payload fully describes; any other change (new
/// commits, target branch, assignees, …) asks for a sync.
const DESCRIBED_CHANGES: &[&str] = &[
    "title",
    "description",
    "draft",
    "work_in_progress",
    "labels",
    "updated_at",
    "updated_by_id",
    "last_edited_at",
    "last_edited_by_id",
    "state_id",
];

fn merge_request_hook(e: &MergeRequestEvent) -> ChangesetHook {
    use ChangesetEventKind as K;
    let a = &e.object_attributes;
    let draft = a
        .draft
        .or(a.work_in_progress)
        .unwrap_or_else(|| a.title.as_deref().is_some_and(has_draft_prefix));
    let state = a.state.as_deref().map(|s| match s {
        "merged" => ChangesetState::Merged,
        "closed" | "locked" => ChangesetState::Closed,
        _ if draft => ChangesetState::Draft,
        _ => ChangesetState::Open,
    });
    let updated_at = a.updated_at.as_deref().and_then(parse_time);
    let action = a.action.clone();
    let changes = e.changes.clone().unwrap_or_default();

    // Timeline events and review state by action.
    let (kind, review_state) = match action.as_deref() {
        Some("close") => (Some(K::Closed), None),
        Some("reopen") => (Some(K::Reopened), None),
        Some("merge") => (Some(K::Merged), None),
        Some("approved") => (Some(K::Approved), Some(ReviewState::Approved)),
        Some("approval") => (Some(K::Approved), None),
        Some("unapproved") => (Some(K::Unapproved), Some(ReviewState::Pending)),
        Some("unapproval") => (Some(K::Unapproved), None),
        Some("update") => (draft_toggle(&changes), None),
        _ => (None, None),
    };
    let actor = e.user.as_ref();
    let timestamp = updated_at.unwrap_or_else(Utc::now);
    let events = kind
        .map(|kind| ChangesetEvent {
            kind,
            key: format!(
                "{HOOK_EVENT_PREFIX}{}:{}:{}",
                action.as_deref().unwrap_or("update"),
                actor.and_then(|u| u.id).unwrap_or(0),
                timestamp.timestamp()
            ),
            timestamp,
            metadata: json!({
                "author": actor.and_then(|u| u.username.clone()),
                "action": action,
            }),
        })
        .into_iter()
        .collect();

    // The merge request fields the payload carries, as the API names them.
    let mut mr = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            mr.insert(k.to_owned(), v);
        }
    };
    put("title", a.title.clone().map(Value::from));
    put("description", a.description.clone().map(Value::from));
    put("state", a.state.clone().map(Value::from));
    put("draft", a.draft.map(Value::from));
    put("work_in_progress", a.work_in_progress.map(Value::from));
    put(
        "updated_at",
        updated_at.map(|t| Value::from(t.to_rfc3339())),
    );
    put("web_url", a.url.clone().map(Value::from));
    put(
        "labels",
        e.labels.as_ref().map(|ls| {
            ls.iter()
                .map(|l| json!({"name": l.title, "color": l.color, "description": l.description}))
                .collect()
        }),
    );
    let mut metadata = Map::new();
    if !mr.is_empty() {
        metadata.insert("mergeRequest".into(), Value::Object(mr));
    }

    let resync = action.as_deref() == Some("update")
        && (a.oldrev.as_deref().is_some_and(|r| !r.is_empty())
            || changes
                .keys()
                .any(|k| !DESCRIBED_CHANGES.contains(&k.as_str())));
    ChangesetHook {
        action,
        state,
        review_state,
        title: a.title.clone(),
        body: a.description.clone(),
        updated_at,
        events,
        metadata,
        resync,
    }
}

/// `MarkedDraft`/`MarkedReady` when an update toggled the draft status.
fn draft_toggle(changes: &Map<String, Value>) -> Option<ChangesetEventKind> {
    let flag = |key: &str| {
        let c = changes.get(key)?;
        Some((c.get("previous")?.as_bool()?, c.get("current")?.as_bool()?))
    };
    let toggle = flag("draft")
        .or_else(|| flag("work_in_progress"))
        .or_else(|| {
            let c = changes.get("title")?;
            Some((
                has_draft_prefix(c.get("previous")?.as_str()?),
                has_draft_prefix(c.get("current")?.as_str()?),
            ))
        });
    match toggle {
        Some((false, true)) => Some(ChangesetEventKind::MarkedDraft),
        Some((true, false)) => Some(ChangesetEventKind::MarkedReady),
        _ => None,
    }
}

fn checks_hook(p: &PipelineAttributes) -> ChecksHook {
    let state = pipeline_state(&p.status);
    let created = p.created_at.as_deref().and_then(parse_time);
    let finished = p.finished_at.as_deref().and_then(parse_time);
    let mut metadata = Map::new();
    metadata.insert(
        "headPipeline".into(),
        json!({
            "id": p.id,
            "sha": p.sha,
            "ref": p.ref_name,
            "status": p.status,
            "web_url": p.url,
            "created_at": created.map(|t| t.to_rfc3339()),
            "updated_at": finished.or(created).map(|t| t.to_rfc3339()),
        }),
    );
    ChecksHook {
        pipeline_id: p.id,
        sha: p.sha.clone(),
        state,
        event: ChangesetEvent {
            kind: ChangesetEventKind::Check,
            key: format!("pipeline:{}", p.id),
            timestamp: finished.or(created).unwrap_or_else(Utc::now),
            metadata: json!({
                "pipelineId": p.id,
                "status": p.status,
                "state": state.as_str(),
                "sha": p.sha,
                "webUrl": p.url,
            }),
        },
        metadata,
    }
}

/// [`WebhookHandler`] for one GitLab connection.
#[derive(Debug, Clone)]
pub struct GitLabWebhooks {
    secrets: Vec<String>,
    system_hooks: bool,
}

impl GitLabWebhooks {
    /// A handler using the connection's webhook secrets and `systemHook` setting.
    pub fn new(conn: &GitLabConnection) -> Self {
        Self {
            secrets: conn.webhook_secrets().map(str::to_owned).collect(),
            system_hooks: conn.system_hook,
        }
    }
}

impl WebhookHandler for GitLabWebhooks {
    fn verify(&self, headers: &HeaderMap, _body: &[u8]) -> Result<(), WebhookError> {
        let token = request_token(headers).unwrap_or("");
        if token_matches(token, self.secrets.iter().map(String::as_str)) {
            Ok(())
        } else {
            Err(WebhookError::Unauthorized)
        }
    }

    fn parse(&self, _headers: &HeaderMap, body: &[u8]) -> Result<Vec<DomainEvent>, WebhookError> {
        parse_event(body, self.system_hooks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn handler(system_hook: bool) -> GitLabWebhooks {
        let mut conn: GitLabConnection = serde_json::from_value(serde_json::json!({
            "url": "https://gitlab.example.com",
            "token": "t",
            "webhooks": [{"secret": "one"}, {"secret": "two"}],
        }))
        .unwrap();
        conn.system_hook = system_hook;
        GitLabWebhooks::new(&conn)
    }

    #[test]
    fn verifies_token() {
        let h = handler(false);
        let mut headers = HeaderMap::new();
        assert_eq!(h.verify(&headers, b""), Err(WebhookError::Unauthorized));
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("two"));
        assert!(h.verify(&headers, b"").is_ok());
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("tw"));
        assert!(h.verify(&headers, b"").is_err());
        assert!(!token_matches("", [""]));
    }

    #[test]
    fn parses_push_and_tag_push() {
        let body =
            br#"{"object_kind": "push", "before": "0000000000000000000000000000000000000000",
            "after": "abc", "ref": "refs/heads/main", "project_id": 15, "project": {"id": 15}}"#;
        assert_eq!(
            parse_event(body, false).unwrap(),
            [DomainEvent::RefsUpdated {
                external_id: "15".into(),
                refs: vec![RefChange {
                    name: "refs/heads/main".into(),
                    old_oid: None,
                    new_oid: Some("abc".into()),
                }],
            }]
        );
        let body = br#"{"object_kind": "tag_push", "before": "abc",
            "after": "0000000000000000000000000000000000000000", "ref": "refs/tags/v1",
            "project": {"id": 7}}"#;
        let DomainEvent::RefsUpdated { external_id, refs } = &parse_event(body, false).unwrap()[0]
        else {
            panic!("expected RefsUpdated");
        };
        assert_eq!(external_id, "7");
        assert_eq!(refs[0].new_oid, None);
        assert!(parse_event(br#"{"object_kind": "push", "ref": "x"}"#, false).is_err());
        assert!(parse_event(b"not json", false).is_err());
    }

    #[test]
    fn parses_mr_pipeline_and_system_hooks() {
        let mr = br#"{"object_kind": "merge_request", "project": {"id": 3}, "object_attributes": {"iid": 9}}"#;
        assert_eq!(
            parse_event(mr, false).unwrap(),
            [DomainEvent::ChangesetChanged {
                external_id: "3".into(),
                number: 9,
                hook: ChangesetHook::default(),
            }]
        );
        let pipeline =
            br#"{"object_kind": "pipeline", "project": {"id": 3}, "merge_request": {"iid": 9}}"#;
        assert_eq!(
            parse_event(pipeline, false).unwrap(),
            [DomainEvent::ChangesetChecksChanged {
                external_id: "3".into(),
                number: Some(9),
                branch: None,
                checks: None,
            }]
        );
        let rename =
            br#"{"event_name": "project_rename", "project_id": 74, "path_with_namespace": "a/b"}"#;
        assert_eq!(
            parse_event(rename, true).unwrap(),
            [DomainEvent::RepoChanged {
                external_id: Some("74".into()),
                change: RepoChange::Renamed
            }]
        );
        assert_eq!(
            parse_event(rename, false).unwrap(),
            [DomainEvent::Ignored {
                event: "project_rename".into()
            }]
        );
        let member = br#"{"event_name": "user_add_to_group", "user_id": 41, "group_id": 1}"#;
        assert_eq!(
            handler(true).parse(&HeaderMap::new(), member).unwrap(),
            [DomainEvent::UserPermsChanged {
                account_id: "41".into(),
                username: None,
            }]
        );
        let team = br#"{"event_name": "user_add_to_team", "user_id": 42,
            "user_username": "alice", "project_id": 7, "access_level": "Developer"}"#;
        assert_eq!(
            handler(true).parse(&HeaderMap::new(), team).unwrap(),
            [DomainEvent::UserPermsChanged {
                account_id: "42".into(),
                username: Some("alice".into()),
            }]
        );
        let note = br#"{"object_kind": "note"}"#;
        assert_eq!(
            parse_event(note, true).unwrap(),
            [DomainEvent::Ignored {
                event: "note".into()
            }]
        );
    }
}
