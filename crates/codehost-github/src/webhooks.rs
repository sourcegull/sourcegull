//! GitHub webhooks.
//!
//! A delivery is verified by recomputing `X-Hub-Signature-256` (`sha256=` and the hex HMAC-SHA256
//! of the body) with each of the connection's `webhooks[].secret` values and comparing in
//! constant time. It is then dispatched on `X-GitHub-Event`:
//!
//! - `push` → [`DomainEvent::RefsUpdated`] (fetch now).
//! - `repository` with action `created`, `deleted`, `renamed`, `transferred`, `archived`,
//!   `unarchived`, `publicized`, `privatized` or `edited` → [`DomainEvent::RepoChanged`] (a
//!   sync of the connection).
//!   `privatized`, `publicized` and `transferred` also → [`DomainEvent::RepoPermsChanged`].
//! - `member` (a collaborator added, removed or changed), `membership` (a team member added or
//!   removed) and `organization` with action `member_added` or `member_removed` →
//!   [`DomainEvent::UserPermsChanged`] of that user.
//! - `team_add` (a repository added to a team) → [`DomainEvent::RepoPermsChanged`].
//! - Anything else, `ping` included → [`DomainEvent::Ignored`].

use hmac::{Hmac, Mac};
use http::HeaderMap;
use serde::Deserialize;
use sha2::Sha256;
use sourcegull_codehost::{DomainEvent, RefChange, RepoChange, WebhookError, WebhookHandler};

use crate::config::GitHubConnection;

/// Header carrying the body's signature.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
/// Header naming the event, e.g. `push`.
pub const EVENT_HEADER: &str = "x-github-event";

/// The all-zero object id GitHub uses for created and deleted refs.
const NULL_SHA: &str = "0000000000000000000000000000000000000000";

/// The `X-Hub-Signature-256` value of `body` signed with `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Whether `header` is the signature of `body` with one of `secrets` (constant time per
/// comparison; empty secrets never match).
pub fn signature_matches<'a>(
    header: &str,
    body: &[u8],
    secrets: impl IntoIterator<Item = &'a str>,
) -> bool {
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
    #[serde(default)]
    created: bool,
    #[serde(default)]
    deleted: bool,
    repository: RepoRef,
}

#[derive(Debug, Deserialize)]
struct RepositoryEvent {
    action: String,
    repository: RepoRef,
}

#[derive(Debug, Deserialize)]
struct RepoRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct UserRef {
    id: i64,
    login: String,
}

/// `member` (collaborator) and `membership` (team member) events.
#[derive(Debug, Deserialize)]
struct MemberEvent {
    member: UserRef,
}

/// `organization` events.
#[derive(Debug, Deserialize)]
struct OrganizationEvent {
    action: String,
    #[serde(default)]
    membership: Option<OrgMembership>,
}

#[derive(Debug, Deserialize)]
struct OrgMembership {
    user: UserRef,
}

/// `team_add` events.
#[derive(Debug, Deserialize)]
struct TeamAddEvent {
    repository: RepoRef,
}

/// Verifies and parses the webhooks of one GitHub connection.
#[derive(Clone)]
pub struct GitHubWebhooks {
    secrets: Vec<String>,
}

impl std::fmt::Debug for GitHubWebhooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubWebhooks")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

impl GitHubWebhooks {
    /// A handler accepting deliveries signed with one of the connection's webhook secrets.
    pub fn new(conn: &GitHubConnection) -> Self {
        Self {
            secrets: conn.webhook_secrets().map(str::to_owned).collect(),
        }
    }
}

fn malformed(e: impl std::fmt::Display) -> WebhookError {
    WebhookError::Malformed(e.to_string())
}

/// An object id, `None` for GitHub's all-zero id.
fn oid(sha: Option<String>) -> Option<String> {
    sha.filter(|s| !s.is_empty() && s != NULL_SHA)
}

impl WebhookHandler for GitHubWebhooks {
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
            .ok_or_else(|| malformed("missing X-GitHub-Event header"))?;
        match event {
            "push" => {
                let p: PushEvent = serde_json::from_slice(body).map_err(malformed)?;
                Ok(vec![DomainEvent::RefsUpdated {
                    external_id: p.repository.id.to_string(),
                    refs: vec![RefChange {
                        name: p.ref_name,
                        old_oid: if p.created { None } else { oid(p.before) },
                        new_oid: if p.deleted { None } else { oid(p.after) },
                    }],
                }])
            }
            "repository" => {
                let r: RepositoryEvent = serde_json::from_slice(body).map_err(malformed)?;
                let change = match r.action.as_str() {
                    "created" => RepoChange::Created,
                    "deleted" => RepoChange::Destroyed,
                    "renamed" | "edited" => RepoChange::Renamed,
                    "transferred" => RepoChange::Transferred,
                    // Archiving and visibility changes are picked up by the sync.
                    "archived" | "unarchived" | "publicized" | "privatized" => RepoChange::Renamed,
                    other => {
                        return Ok(vec![DomainEvent::Ignored {
                            event: format!("repository.{other}"),
                        }]);
                    }
                };
                let external_id = r.repository.id.to_string();
                let mut events = vec![DomainEvent::RepoChanged {
                    external_id: Some(external_id.clone()),
                    change,
                }];
                if matches!(
                    r.action.as_str(),
                    "privatized" | "publicized" | "transferred"
                ) {
                    events.push(DomainEvent::RepoPermsChanged { external_id });
                }
                Ok(events)
            }
            "member" | "membership" => {
                let m: MemberEvent = serde_json::from_slice(body).map_err(malformed)?;
                Ok(vec![DomainEvent::UserPermsChanged {
                    account_id: m.member.id.to_string(),
                    username: Some(m.member.login),
                }])
            }
            "organization" => {
                let o: OrganizationEvent = serde_json::from_slice(body).map_err(malformed)?;
                match (o.action.as_str(), o.membership) {
                    ("member_added" | "member_removed", Some(m)) => {
                        Ok(vec![DomainEvent::UserPermsChanged {
                            account_id: m.user.id.to_string(),
                            username: Some(m.user.login),
                        }])
                    }
                    (action, _) => Ok(vec![DomainEvent::Ignored {
                        event: format!("organization.{action}"),
                    }]),
                }
            }
            "team_add" => {
                let t: TeamAddEvent = serde_json::from_slice(body).map_err(malformed)?;
                Ok(vec![DomainEvent::RepoPermsChanged {
                    external_id: t.repository.id.to_string(),
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

    fn handler() -> GitHubWebhooks {
        GitHubWebhooks {
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
        let body = br#"{"zen": "Keep it logically awesome."}"#;
        // The example from GitHub's documentation.
        assert_eq!(
            signature("It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
        let h = handler();
        assert!(
            h.verify(&headers("ping", &signature("s3cret", body)), body)
                .is_ok()
        );
        assert!(
            h.verify(&headers("ping", &signature("old", body)), body)
                .is_ok()
        );
        assert_eq!(
            h.verify(&headers("ping", &signature("wrong", body)), body),
            Err(WebhookError::Unauthorized)
        );
        let mut missing = headers("ping", "x");
        missing.remove(SIGNATURE_HEADER);
        assert_eq!(h.verify(&missing, body), Err(WebhookError::Unauthorized));
        let none = GitHubWebhooks { secrets: vec![] };
        assert!(
            none.verify(&headers("ping", &signature("", body)), body)
                .is_err()
        );
    }

    #[test]
    fn parses_push_repository_and_others() {
        let h = handler();
        let push =
            br#"{"ref": "refs/heads/main", "before": "0000000000000000000000000000000000000000",
            "after": "abc", "created": true, "deleted": false, "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("push", ""), push).unwrap(),
            [DomainEvent::RefsUpdated {
                external_id: "42".into(),
                refs: vec![RefChange {
                    name: "refs/heads/main".into(),
                    old_oid: None,
                    new_oid: Some("abc".into()),
                }],
            }]
        );
        let deleted = br#"{"ref": "refs/tags/v1", "before": "abc",
            "after": "0000000000000000000000000000000000000000", "deleted": true, "repository": {"id": 7}}"#;
        let DomainEvent::RefsUpdated { refs, .. } =
            &h.parse(&headers("push", ""), deleted).unwrap()[0]
        else {
            panic!("expected RefsUpdated");
        };
        assert_eq!(refs[0].old_oid.as_deref(), Some("abc"));
        assert_eq!(refs[0].new_oid, None);

        let renamed = br#"{"action": "renamed", "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("repository", ""), renamed).unwrap(),
            [DomainEvent::RepoChanged {
                external_id: Some("42".into()),
                change: RepoChange::Renamed,
            }]
        );
        let privatized = br#"{"action": "privatized", "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("repository", ""), privatized).unwrap(),
            [
                DomainEvent::RepoChanged {
                    external_id: Some("42".into()),
                    change: RepoChange::Renamed,
                },
                DomainEvent::RepoPermsChanged {
                    external_id: "42".into()
                }
            ]
        );
        let unknown = br#"{"action": "starred", "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("repository", ""), unknown).unwrap(),
            [DomainEvent::Ignored {
                event: "repository.starred".into()
            }]
        );
        assert_eq!(
            h.parse(&headers("ping", ""), b"{}").unwrap(),
            [DomainEvent::Ignored {
                event: "ping".into()
            }]
        );
        assert!(matches!(
            h.parse(&headers("push", ""), b"{}"),
            Err(WebhookError::Malformed(_))
        ));
    }

    #[test]
    fn parses_membership_events() {
        let h = handler();
        let alice = DomainEvent::UserPermsChanged {
            account_id: "7".into(),
            username: Some("alice".into()),
        };
        let member = br#"{"action": "added", "member": {"id": 7, "login": "alice"},
            "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("member", ""), member).unwrap(),
            std::slice::from_ref(&alice)
        );
        let membership = br#"{"action": "removed", "scope": "team",
            "member": {"id": 7, "login": "alice"}, "team": {"id": 3}}"#;
        assert_eq!(
            h.parse(&headers("membership", ""), membership).unwrap(),
            std::slice::from_ref(&alice)
        );
        let org = br#"{"action": "member_removed",
            "membership": {"user": {"id": 7, "login": "alice"}, "role": "member"}}"#;
        assert_eq!(h.parse(&headers("organization", ""), org).unwrap(), [alice]);
        let renamed = br#"{"action": "renamed", "organization": {"login": "o"}}"#;
        assert_eq!(
            h.parse(&headers("organization", ""), renamed).unwrap(),
            [DomainEvent::Ignored {
                event: "organization.renamed".into()
            }]
        );
        let team_add = br#"{"team": {"id": 3}, "repository": {"id": 42}}"#;
        assert_eq!(
            h.parse(&headers("team_add", ""), team_add).unwrap(),
            [DomainEvent::RepoPermsChanged {
                external_id: "42".into()
            }]
        );
    }
}
