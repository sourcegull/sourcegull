//! Project hooks registered through the API, against the mock GitLab.

use std::sync::Arc;

use serde_json::json;
use sourcegull_codehost::{CodeHostError, HookChange, HookTarget, WebhookRegistrar};
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_codehost_gitlab::{ClientOptions, GitLabClient, GitLabHooks, validate_connection};

async fn setup() -> (MockGitLab, GitLabHooks) {
    let mock = MockGitLab::start("tok").await;
    mock.with(|s| {
        s.projects = vec![
            project(1, "g/a", chrono::Utc::now()),
            project(2, "g/b", chrono::Utc::now()),
        ];
    });
    let conn = validate_connection(&format!(
        r#"{{"url": "{}", "token": "tok", "projectQuery": ["projects"]}}"#,
        mock.url
    ))
    .expect("config")
    .config;
    let client = GitLabClient::new(&conn, ClientOptions::default()).expect("client");
    (mock, GitLabHooks::new(Arc::new(client)))
}

fn target(secret_version: &str) -> HookTarget {
    HookTarget {
        url: format!("https://sourcegull.example.com/.api/webhooks/abc?v={secret_version}"),
        secret: format!("secret-{secret_version}"),
    }
}

#[tokio::test]
async fn creates_when_missing_and_is_idempotent() {
    let (mock, hooks) = setup().await;
    let t = target("1");
    assert_eq!(hooks.ensure("1", &t).await.unwrap(), HookChange::Created);
    assert_eq!(hooks.ensure("1", &t).await.unwrap(), HookChange::Unchanged);
    mock.with(|s| {
        let h = &s.hooks[&1];
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["url"], t.url);
        assert_eq!(h[0]["token"], "secret-1");
        assert_eq!(h[0]["push_events"], true);
        assert_eq!(h[0]["tag_push_events"], true);
        assert_eq!(h[0]["merge_requests_events"], true);
        assert_eq!(h[0]["pipeline_events"], true);
        assert_eq!(s.hook_writes, vec!["POST 1".to_owned()]);
    });
}

#[tokio::test]
async fn updates_on_drift_and_removes_duplicates() {
    let (mock, hooks) = setup().await;
    mock.with(|s| {
        s.hooks.insert(
            2,
            vec![
                // Another receiver: left alone.
                json!({"id": 5, "url": "https://ci.example.com/hook", "push_events": true}),
                // Ours, at an old host and without tag push events.
                json!({"id": 6, "url": "http://old.example.com/.api/webhooks/abc",
                       "push_events": true, "tag_push_events": false,
                       "merge_requests_events": true, "pipeline_events": true}),
                json!({"id": 7, "url": "https://sourcegull.example.com/.api/webhooks/abc?v=0",
                       "push_events": true}),
            ],
        );
    });
    let t = target("2");
    assert_eq!(hooks.ensure("2", &t).await.unwrap(), HookChange::Updated);
    mock.with(|s| {
        let h = &s.hooks[&2];
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!(h[0]["id"], 5);
        assert_eq!(h[1]["id"], 6);
        assert_eq!(h[1]["url"], t.url);
        assert_eq!(h[1]["token"], "secret-2");
        assert_eq!(h[1]["tag_push_events"], true);
    });
    // A new secret changes the URL's version: the hook is updated again.
    assert_eq!(
        hooks.ensure("2", &target("3")).await.unwrap(),
        HookChange::Updated
    );
    assert_eq!(hooks.remove("2", &t).await.unwrap(), 1);
    mock.with(|s| assert_eq!(s.hooks[&2].len(), 1));
}

#[tokio::test]
async fn forbidden_is_an_error() {
    let (mock, hooks) = setup().await;
    mock.with(|s| {
        s.hooks_forbidden.insert(1);
    });
    let err = hooks.ensure("1", &target("1")).await.unwrap_err();
    assert!(matches!(err, CodeHostError::Forbidden(_)), "{err:?}");
}
