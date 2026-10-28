//! The outbound request log: every API call is recorded with its token redacted.
//! The log is process-wide and other tests in this binary call their own mock servers, so
//! only the entries for this test's server are checked.

use std::sync::Arc;

use sourcegull_codehost::outbound::{self, MemoryOutboundLog, REDACTED};
use sourcegull_codehost_gitlab::testing::MockGitLab;
use sourcegull_codehost_gitlab::{
    ClientOptions, GitLabClient, GitLabConnection, validate_connection,
};

#[tokio::test]
async fn requests_are_logged_without_secrets() {
    let m = MockGitLab::start("glpat-supersecret").await;
    let log = Arc::new(MemoryOutboundLog::new(10));
    outbound::set_global(log.clone());

    let text = format!(r#"{{"url": "{}", "token": "glpat-supersecret"}}"#, m.url);
    let conn: GitLabConnection = validate_connection(&text).unwrap().config;
    let client = GitLabClient::new(&conn, ClientOptions::default()).unwrap();
    let _: serde_json::Value = client
        .get("projects?private_token=glpat-supersecret&per_page=1")
        .await
        .unwrap();

    let entries: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.url.starts_with(&m.url))
        .collect();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.method, "GET");
    assert_eq!(e.status_code, 200);
    assert!(e.error.is_empty());
    assert!(e.client.starts_with("gitlab "));
    assert!(e.url.contains("per_page=1"), "{}", e.url);
    let all = format!("{e:?}");
    assert!(!all.contains("supersecret"), "secret leaked: {all}");
    let token = e
        .request_headers
        .iter()
        .find(|(n, _)| n == "private-token")
        .expect("token header logged");
    assert_eq!(token.1, vec![REDACTED]);
}
