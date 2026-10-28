//! GitLab `merge_request` and `pipeline` webhook payloads (fixtures in
//! `tests/fixtures/webhooks/`, shaped like GitLab 16's deliveries) parsed into changeset
//! updates.

use chrono::{DateTime, Utc};
use sourcegull_codehost::{
    ChangesetEventKind, ChangesetHook, ChangesetState, CheckState, ChecksHook, DomainEvent,
    ReviewState,
};
use sourcegull_codehost_gitlab::webhooks::{parse_event, parse_time};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/webhooks/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn mr(name: &str) -> ChangesetHook {
    match parse_event(&fixture(name), false).unwrap().remove(0) {
        DomainEvent::ChangesetChanged {
            external_id,
            number,
            hook,
        } => {
            assert_eq!((external_id.as_str(), number), ("42", 1), "{name}");
            hook
        }
        other => panic!("{name}: {other:?}"),
    }
}

fn pipeline(name: &str) -> (Option<i64>, Option<String>, ChecksHook) {
    match parse_event(&fixture(name), false).unwrap().remove(0) {
        DomainEvent::ChangesetChecksChanged {
            external_id,
            number,
            branch,
            checks,
        } => {
            assert_eq!(external_id, "42");
            (number, branch, checks.unwrap())
        }
        other => panic!("{name}: {other:?}"),
    }
}

#[test]
fn timestamps() {
    assert_eq!(
        parse_time("2026-10-19T09:00:00Z"),
        Some(at("2026-10-19T09:00:00Z"))
    );
    assert_eq!(
        parse_time("2026-10-19 09:00:00 UTC"),
        Some(at("2026-10-19T09:00:00Z"))
    );
    assert_eq!(
        parse_time("2026-10-19 11:00:00 +0200"),
        Some(at("2026-10-19T09:00:00Z"))
    );
    assert_eq!(parse_time("yesterday"), None);
}

/// Fixture → (state, review state, event kind, resync).
type Expected = (
    &'static str,
    ChangesetState,
    Option<ReviewState>,
    Option<ChangesetEventKind>,
    bool,
);

#[test]
fn merge_request_actions() {
    let table: &[Expected] = &[
        (
            "merge_request_open",
            ChangesetState::Open,
            None,
            None,
            false,
        ),
        (
            "merge_request_update_draft",
            ChangesetState::Draft,
            None,
            Some(ChangesetEventKind::MarkedDraft),
            false,
        ),
        (
            "merge_request_update_commits",
            ChangesetState::Open,
            None,
            None,
            true,
        ),
        (
            "merge_request_approved",
            ChangesetState::Open,
            Some(ReviewState::Approved),
            Some(ChangesetEventKind::Approved),
            false,
        ),
        (
            "merge_request_unapproved",
            ChangesetState::Open,
            Some(ReviewState::Pending),
            Some(ChangesetEventKind::Unapproved),
            false,
        ),
        (
            "merge_request_close",
            ChangesetState::Closed,
            None,
            Some(ChangesetEventKind::Closed),
            false,
        ),
        (
            "merge_request_reopen",
            ChangesetState::Open,
            None,
            Some(ChangesetEventKind::Reopened),
            false,
        ),
        (
            "merge_request_merge",
            ChangesetState::Merged,
            None,
            Some(ChangesetEventKind::Merged),
            false,
        ),
    ];
    for (name, state, review, kind, resync) in table {
        let h = mr(name);
        assert_eq!(h.state, Some(*state), "{name}");
        assert_eq!(h.review_state, *review, "{name}");
        assert_eq!(h.events.first().map(|e| e.kind), *kind, "{name}");
        assert_eq!(h.resync, *resync, "{name}");
        assert!(h.events.len() <= 1, "{name}");
        for e in &h.events {
            assert!(e.key.starts_with("hook:"), "{name}: {}", e.key);
            assert_eq!(e.metadata["author"], "bob", "{name}");
            assert_eq!(Some(e.timestamp), h.updated_at, "{name}");
        }
    }

    let open = mr("merge_request_open");
    assert_eq!(open.action.as_deref(), Some("open"));
    assert_eq!(open.title.as_deref(), Some("Fix the readme"));
    assert_eq!(open.body.as_deref(), Some("It says hello."));
    assert_eq!(open.updated_at, Some(at("2026-10-19T09:00:00Z")));
    let fields = &open.metadata["mergeRequest"];
    assert_eq!(
        fields["web_url"],
        "https://gitlab.example.com/a/one/-/merge_requests/1"
    );
    assert_eq!(fields["state"], "opened");
    assert_eq!(fields["labels"][0]["name"], "batch-change");
    assert_eq!(fields["labels"][0]["color"], "#dc143c");

    let draft = mr("merge_request_update_draft");
    assert_eq!(draft.title.as_deref(), Some("Draft: Fix the readme"));
    assert_eq!(
        draft.events[0].key,
        format!("hook:update:7:{}", at("2026-10-19T09:05:00Z").timestamp())
    );
    // Distinct actions at the same time do not collide.
    assert_ne!(
        mr("merge_request_approved").events[0].key,
        mr("merge_request_unapproved").events[0].key
    );
}

#[test]
fn pipelines() {
    let (number, branch, c) = pipeline("pipeline_success");
    assert_eq!(number, Some(1));
    assert_eq!(branch.as_deref(), Some("refs/heads/fix"));
    assert_eq!((c.pipeline_id, c.state), (3101, CheckState::Passed));
    assert_eq!(
        c.sha.as_deref(),
        Some("da1560886d4f094c3e6c9ef40349f7d38b5d27d7")
    );
    // Keyed like a sync's pipeline event.
    assert_eq!(c.event.kind, ChangesetEventKind::Check);
    assert_eq!(c.event.key, "pipeline:3101");
    assert_eq!(c.event.timestamp, at("2026-10-19T09:02:03Z"));
    assert_eq!(c.event.metadata["state"], "PASSED");
    assert_eq!(c.metadata["headPipeline"]["id"], 3101);
    assert_eq!(c.metadata["headPipeline"]["status"], "success");

    let (_, _, c) = pipeline("pipeline_failed");
    assert_eq!((c.pipeline_id, c.state), (3102, CheckState::Failed));

    // Without a merge request: found by branch; still running.
    let (number, branch, c) = pipeline("pipeline_branch_running");
    assert_eq!(number, None);
    assert_eq!(branch.as_deref(), Some("refs/heads/fix"));
    assert_eq!(c.state, CheckState::Pending);
    assert_eq!(c.event.timestamp, at("2026-10-19T09:20:00Z"));
}
