//! Gitea webhooks.
//!
//! A delivery is verified by recomputing `X-Gitea-Signature` (the hex HMAC-SHA256 of the body)
//! with each of the connection's `webhooks[].secret` values and comparing in constant time. It
//! is then dispatched on `X-Gitea-Event`:
//!
//! - `push` → [`DomainEvent::RefsUpdated`] (fetch now).
//! - `create` and `delete` (a branch or tag was created or deleted; Gitea sends no `push` for
//!   a deletion) → [`DomainEvent::RefsUpdated`].
//! - `repository` with action `created` or `deleted`, and `fork` (`repository` is the new
//!   fork) → [`DomainEvent::RepoChanged`] (a sync of the connection, which also queues the
//!   permissions sync of a new repository when the connection syncs them per repository).
//! - Anything else → [`DomainEvent::Ignored`].
//!
//! Gitea sends no webhook for collaborator, team or organization membership changes, or for a
//! repository's visibility changing; with an `authorization` block, the scheduled permissions
//! syncs and connection syncs apply them.

use hmac::{Hmac, Mac};
use http::HeaderMap;
use serde::Deserialize;
use sha2::Sha256;
use sourcegull_codehost::{DomainEvent, RefChange, RepoChange, WebhookError, WebhookHandler};

use crate::config::GiteaConnection;

/// Header carrying the body's signature.
pub const SIGNATURE_HEADER: &str = "x-gitea-signature";
/// Header naming the event, e.g. `push`.
pub const EVENT_HEADER: &str = "x-gitea-event";

/// The `X-Gitea-Signature` value of `body` signed with `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Whether `header` is the signature of `body` with one of `secrets` (constant time per
/// comparison; empty secrets never match).
pub fn signature_matches<'a>(
    header: &str,
    body: &[u8],
    secrets: impl IntoIterator<Item = &'a str>,
) -> bool {
    let header = header.trim().to_ascii_lowercase();
    secrets
        .into_iter()
        .filter(|s| !s.is_empty())
        .fold(false, |found, s| {
            sourcegull_crypto::constant_time_eq(header.as_bytes(), signature(s, body).as_bytes())
                | found
        })
}

#[derive(Debug, Deserialize)]
struct PushEvent {
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    after: Option<String>,
    repository: RepoRef,
}

/// `create` and `delete`: `ref` is the short name, `ref_type` says of what.
#[derive(Debug, Deserialize)]
struct RefEvent {
    #[serde(rename = "ref")]
    ref_name: String,
    ref_type: String,
    /// The new object id (`create` only).
    #[serde(default)]
    sha: Option<String>,
    repository: RepoRef,
}

#[derive(Debug, Deserialize)]
struct RepositoryEvent {
    action: String,
    repository: RepoRef,
}

/// `fork`: `repository` is the new fork, `forkee` the repository it was forked from.
#[derive(Debug, Deserialize)]
struct ForkEvent {
    repository: RepoRef,
}

#[derive(Debug, Deserialize)]
struct RepoRef {
    id: i64,
}

/// Verifies and parses the webhooks of one Gitea connection.
#[derive(Clone)]
pub struct GiteaWebhooks {
    secrets: Vec<String>,
}

impl std::fmt::Debug for GiteaWebhooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiteaWebhooks")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

impl GiteaWebhooks {
    /// A handler accepting deliveries signed with one of the connection's webhook secrets.
    pub fn new(conn: &GiteaConnection) -> Self {
        Self {
            secrets: conn.webhook_secrets().map(str::to_owned).collect(),
        }
    }
}

fn malformed(e: impl std::fmt::Display) -> WebhookError {
    WebhookError::Malformed(e.to_string())
}

/// An object id, `None` for an empty or all-zero id (Gitea's id for created and deleted refs).
fn oid(sha: Option<String>) -> Option<String> {
    sha.filter(|s| !s.is_empty() && !s.bytes().all(|b| b == b'0'))
}

/// The full name of a `create` or `delete` event's ref, `None` for kinds other than branches
/// and tags.
fn full_ref(name: &str, ref_type: &str) -> Option<String> {
    match ref_type {
        "branch" => Some(format!("refs/heads/{name}")),
        "tag" => Some(format!("refs/tags/{name}")),
        _ => None,
    }
}

impl WebhookHandler for GiteaWebhooks {
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<(), WebhookError> {
        let header = headers
            .get(SIGNATURE_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or(WebhookError::Unauthorized)?;
        if signature_matches(header, body, self.secrets.iter().map(String::as_str)) {
            Ok(())
        } else {
            Err(WebhookError::Unauthorized)
        }
    }

    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<DomainEvent>, WebhookError> {
        let event = headers
            .get(EVENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| malformed("missing X-Gitea-Event header"))?;
        match event {
            "push" => {
                let p: PushEvent = serde_json::from_slice(body).map_err(malformed)?;
                Ok(vec![DomainEvent::RefsUpdated {
                    external_id: p.repository.id.to_string(),
                    refs: vec![RefChange {
                        name: p.ref_name,
                        old_oid: oid(p.before),
                        new_oid: oid(p.after),
                    }],
                }])
            }
            "create" | "delete" => {
                let r: RefEvent = serde_json::from_slice(body).map_err(malformed)?;
                let Some(name) = full_ref(&r.ref_name, &r.ref_type) else {
                    return Ok(vec![DomainEvent::Ignored {
                        event: format!("{event}.{}", r.ref_type),
                    }]);
                };
                let created = event == "create";
                Ok(vec![DomainEvent::RefsUpdated {
                    external_id: r.repository.id.to_string(),
                    refs: vec![RefChange {
                        name,
                        old_oid: None,
                        new_oid: if created { oid(r.sha) } else { None },
                    }],
                }])
            }
            "fork" => {
                let r: ForkEvent = serde_json::from_slice(body).map_err(malformed)?;
                Ok(vec![DomainEvent::RepoChanged {
                    external_id: Some(r.repository.id.to_string()),
                    change: RepoChange::Created,
                }])
            }
            "repository" => {
                let r: RepositoryEvent = serde_json::from_slice(body).map_err(malformed)?;
                let change = match r.action.as_str() {
                    "created" => RepoChange::Created,
                    "deleted" => RepoChange::Destroyed,
                    other => {
                        return Ok(vec![DomainEvent::Ignored {
                            event: format!("repository.{other}"),
                        }]);
                    }
                };
                Ok(vec![DomainEvent::RepoChanged {
                    external_id: Some(r.repository.id.to_string()),
                    change,
                }])
            }
            other => Ok(vec![DomainEvent::Ignored {
                event: other.to_owned(),
            }]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn handler() -> GiteaWebhooks {
        GiteaWebhooks {
            secrets: vec!["old".into(), "s3cret".into()],
        }
    }

    fn headers(event: &str, sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(EVENT_HEADER, HeaderValue::from_str(event).unwrap());
        h.insert(SIGNATURE_HEADER, HeaderValue::from_str(sig).unwrap());
        h
    }

    #[test]
    fn verifies_signatures() {
        let body = br#"{"ref": "refs/heads/main"}"#;
        // HMAC-SHA256 test vector: a hex digest without a prefix.
        assert_eq!(
            signature("It's a Secret to Everybody", b"Hello, World!"),
            "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
        let h = handler();
        assert!(
            h.verify(&headers("push", &signature("s3cret", body)), body)
                .is_ok()
        );
        assert!(
            h.verify(&headers("push", &signature("old", body)), body)
                .is_ok()
        );
        assert_eq!(
            h.verify(&headers("push", &signature("wrong", body)), body),
            Err(WebhookError::Unauthorized)
        );
        let mut missing = headers("push", "x");
        missing.remove(SIGNATURE_HEADER);
        assert_eq!(h.verify(&missing, body), Err(WebhookError::Unauthorized));
        let none = GiteaWebhooks { secrets: vec![] };
        assert!(
            none.verify(&headers("push", &signature("", body)), body)
                .is_err()
        );
    }

    #[test]
    fn parses_push_create_delete_and_repository() {
        let h = handler();
        let push =
            br#"{"ref": "refs/heads/feat", "before": "0000000000000000000000000000000000000000",
            "after": "abc", "repository": {"id": 42, "full_name": "acme/api"}}"#;
        assert_eq!(
            h.parse(&headers("push", ""), push).unwrap(),
            [DomainEvent::RefsUpdated {
                external_id: "42".into(),
                refs: vec![RefChange {
                    name: "refs/heads/feat".into(),
                    old_oid: None,
                    new_oid: Some("abc".into()),
                }],
            }]
        );
        let created = br#"{"sha": "abc", "ref": "v1", "ref_type": "tag", "repository": {"id": 7}}"#;
        assert_eq!(
            h.parse(&headers("create", ""), created).unwrap(),
            [DomainEvent::RefsUpdated {
                external_id: "7".into(),
                refs: vec![RefChange {
                    name: "refs/tags/v1".into(),
                    old_oid: None,
                    new_oid: Some("abc".into()),
                }],
            }]
        );
        let deleted = br#"{"ref": "feat", "ref_type": "branch", "repository": {"id": 7}}"#;
        let DomainEvent::RefsUpdated { refs, .. } =
            &h.parse(&headers("delete", ""), deleted).unwrap()[0]
        else {
            panic!("expected RefsUpdated");
        };
        assert_eq!(refs[0].name, "refs/heads/feat");
        assert_eq!(refs[0].new_oid, None);

        let repo_deleted = br#"{"action": "deleted", "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("repository", ""), repo_deleted).unwrap(),
            [DomainEvent::RepoChanged {
                external_id: Some("42".into()),
                change: RepoChange::Destroyed,
            }]
        );
        let fork = br#"{"forkee": {"id": 42}, "repository": {"id": 43}}"#;
        assert_eq!(
            h.parse(&headers("fork", ""), fork).unwrap(),
            [DomainEvent::RepoChanged {
                external_id: Some("43".into()),
                change: RepoChange::Created,
            }]
        );
        assert_eq!(
            h.parse(&headers("issues", ""), b"{}").unwrap(),
            [DomainEvent::Ignored {
                event: "issues".into()
            }]
        );
        assert!(matches!(
            h.parse(&headers("push", ""), b"{}"),
            Err(WebhookError::Malformed(_))
        ));
    }
}
