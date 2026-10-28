//! Merge requests for bulk changes against a `wiremock` GitLab serving recorded-style
//! fixtures (`tests/fixtures/changesets/`).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sourcegull_codehost::{
    ChangesetEventKind, ChangesetSource, ChangesetState, CheckState, CodeHostError, CommitAction,
    CommitFile, Credential, MergeOptions, NewChangeset, NewCommit, RepoMeta, ReviewState,
};
use sourcegull_codehost_gitlab::{
    ChangesetOptions, ClientOptions, GitLabChangesetSource, GitLabClient, GitLabConnection,
    GitLabSource, types::Project, validate_connection,
};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MR_PATH: &str = "/api/v4/projects/42/merge_requests/7";

fn fixture(name: &str) -> Value {
    let text = match name {
        "project" => include_str!("fixtures/changesets/project.json"),
        "fork_scheduled" => include_str!("fixtures/changesets/fork_scheduled.json"),
        "merge_request" => include_str!("fixtures/changesets/merge_request.json"),
        "notes" => include_str!("fixtures/changesets/notes.json"),
        "resource_state_events" => include_str!("fixtures/changesets/resource_state_events.json"),
        "pipelines" => include_str!("fixtures/changesets/pipelines.json"),
        "approvals_premium" => include_str!("fixtures/changesets/approvals_premium.json"),
        "user" => include_str!("fixtures/changesets/user.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// A mock GitLab of `version` with `GET /version` mounted.
async fn gitlab(version: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/version"))
        .respond_with(ok(json!({"version": version, "revision": "abc"})))
        .mount(&server)
        .await;
    server
}

fn gitlab_source(server: &MockServer) -> Arc<GitLabSource> {
    let text = format!(r#"{{"url": "{}", "token": "conn-token"}}"#, server.uri());
    let conn: GitLabConnection = validate_connection(&text).unwrap().config;
    let opts = ClientOptions {
        retry_backoff: Duration::from_millis(1),
        ..ClientOptions::default()
    };
    let client = Arc::new(GitLabClient::new(&conn, opts).unwrap());
    Arc::new(GitLabSource::new(conn, client).unwrap())
}

fn changesets(server: &MockServer) -> GitLabChangesetSource {
    GitLabChangesetSource::new(gitlab_source(server)).with_options(ChangesetOptions {
        fork_poll_interval: Duration::from_millis(5),
        fork_timeout: Duration::from_secs(5),
        branch_poll_interval: Duration::from_millis(5),
        branch_timeout: Duration::from_millis(200),
    })
}

fn repo(server: &MockServer) -> RepoMeta {
    let p: Project = serde_json::from_value(fixture("project")).unwrap();
    gitlab_source(server).repo_meta(&p)
}

fn new_changeset(draft: bool) -> NewChangeset {
    NewChangeset {
        title: "Update dependencies".into(),
        body: "Created by a bulk change.".into(),
        head_ref: "batch/update-deps".into(),
        base_ref: "main".into(),
        draft,
        labels: vec!["batch".into(), "deps".into()],
        remove_source_branch: true,
        head_repo_id: None,
    }
}

/// The MR fixture with `title`/`draft` adjusted.
fn mr(title: &str, draft: bool) -> Value {
    let mut mr = fixture("merge_request");
    mr["title"] = title.into();
    mr["draft"] = draft.into();
    mr["work_in_progress"] = draft.into();
    mr
}

#[tokio::test]
async fn creates_draft_with_draft_prefix_on_gitlab_16() {
    let server = gitlab("16.5.1-ee").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .and(header("PRIVATE-TOKEN", "conn-token"))
        .and(body_partial_json(json!({
            "source_branch": "batch/update-deps",
            "target_branch": "main",
            "title": "Draft: Update dependencies",
            "description": "Created by a bulk change.",
            "remove_source_branch": true,
            "labels": "batch,deps",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("merge_request")))
        .expect(1)
        .mount(&server)
        .await;
    let src = changesets(&server);
    let created = src
        .create(&repo(&server), &new_changeset(true))
        .await
        .unwrap();
    assert!(!created.already_existed);
    let cs = created.changeset;
    assert_eq!(cs.number, 7);
    assert_eq!(cs.state, ChangesetState::Draft);
    assert_eq!(cs.head_ref, "batch/update-deps");
    assert_eq!(cs.head_repo_id, None);
    assert_eq!(cs.labels, ["batch", "deps"]);
    assert_eq!(cs.check_state, Some(CheckState::Failed));
    assert!(cs.events.is_empty());
    assert_eq!(cs.metadata["mergeRequest"]["id"], 1007);
}

#[tokio::test]
async fn creates_draft_with_wip_prefix_on_gitlab_13() {
    let server = gitlab("13.12.15").await;
    let mut wip = mr("WIP: Update dependencies", true);
    wip.as_object_mut().unwrap().remove("draft");
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .and(body_partial_json(
            json!({"title": "WIP: Update dependencies"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(wip))
        .expect(1)
        .mount(&server)
        .await;
    let cs = changesets(&server)
        .create(&repo(&server), &new_changeset(true))
        .await
        .unwrap()
        .changeset;
    assert_eq!(cs.state, ChangesetState::Draft);
    assert_eq!(cs.title, "WIP: Update dependencies");
}

/// Right after a push, a busy GitLab may not list the branch yet (its branch cache is
/// refreshed asynchronously) and refuses the merge request. That is retried, not treated
/// as a permanent failure.
#[tokio::test]
async fn retries_while_gitlab_does_not_see_the_pushed_branch() {
    let server = gitlab("16.5.1").await;
    let missing = || {
        ResponseTemplate::new(400)
            .set_body_json(json!({"message": {"source_branch": ["does not exist"]}}))
    };
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(missing())
        .up_to_n_times(2)
        .with_priority(1)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("merge_request")))
        .expect(1)
        .mount(&server)
        .await;
    let created = changesets(&server)
        .create(&repo(&server), &new_changeset(true))
        .await
        .unwrap();
    assert_eq!(created.changeset.number, 7);

    // Still missing after the timeout: a temporary error, so the reconciler retries later.
    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(missing())
        .mount(&server)
        .await;
    let err = changesets(&server)
        .create(&repo(&server), &new_changeset(true))
        .await
        .unwrap_err();
    assert!(err.is_temporary(), "{err:?}");
    assert!(err.to_string().contains("batch/update-deps"), "{err}");

    // Other 400s are not retried.
    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"message": {"target_branch": ["does not exist"]}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let err = changesets(&server)
        .create(&repo(&server), &new_changeset(true))
        .await
        .unwrap_err();
    assert!(!err.is_temporary(), "{err:?}");
}

#[tokio::test]
async fn existing_merge_request_is_loaded_and_updated() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "message": ["Another open merge request already exists for this source branch: !7"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut other_fork = mr("Draft: Someone else's", true);
    other_fork["iid"] = 5.into();
    other_fork["source_project_id"] = 1234.into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .and(query_param("source_branch", "batch/update-deps"))
        .and(query_param("state", "opened"))
        .respond_with(ok(json!([other_fork, fixture("merge_request")])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(MR_PATH))
        .and(body_partial_json(json!({
            "title": "Update dependencies",
            "description": "Created by a bulk change.",
            "target_branch": "main",
            "add_labels": "batch,deps",
        })))
        .respond_with(ok(mr("Update dependencies", false)))
        .expect(1)
        .mount(&server)
        .await;
    let created = changesets(&server)
        .create(&repo(&server), &new_changeset(false))
        .await
        .unwrap();
    assert!(created.already_existed);
    assert_eq!(created.changeset.number, 7);
    assert_eq!(created.changeset.state, ChangesetState::Open);
}

#[tokio::test]
async fn undraft_update_close_reopen_merge_and_comment() {
    let server = gitlab("16.5.1").await;
    let r = repo(&server);
    let src = changesets(&server);
    // The MR is a draft on GitLab.
    Mock::given(method("GET"))
        .and(path(MR_PATH))
        .respond_with(ok(mr("Draft: Update dependencies", true)))
        .mount(&server)
        .await;

    // Undraft strips the prefix.
    Mock::given(method("PUT"))
        .and(path(MR_PATH))
        .and(body_partial_json(json!({"title": "Update dependencies"})))
        .respond_with(ok(mr("Update dependencies", false)))
        .expect(1)
        .named("undraft")
        .mount(&server)
        .await;
    let cs = src.undraft(&r, 7).await.unwrap();
    assert_eq!(cs.state, ChangesetState::Open);

    // Update of a draft keeps it a draft.
    Mock::given(method("PUT"))
        .and(path(MR_PATH))
        .and(body_partial_json(json!({
            "title": "Draft: New title", "description": "New body", "target_branch": "develop"
        })))
        .respond_with(ok(mr("Draft: New title", true)))
        .expect(1)
        .named("update")
        .mount(&server)
        .await;
    let new = NewChangeset {
        title: "New title".into(),
        body: "New body".into(),
        base_ref: "develop".into(),
        ..new_changeset(false)
    };
    assert_eq!(
        src.update(&r, 7, &new).await.unwrap().state,
        ChangesetState::Draft
    );

    // Close and reopen.
    let mut closed = mr("Update dependencies", false);
    closed["state"] = "closed".into();
    Mock::given(method("PUT"))
        .and(path(MR_PATH))
        .and(body_partial_json(json!({"state_event": "close"})))
        .respond_with(ok(closed))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(MR_PATH))
        .and(body_partial_json(json!({"state_event": "reopen"})))
        .respond_with(ok(mr("Update dependencies", false)))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        src.close(&r, 7).await.unwrap().state,
        ChangesetState::Closed
    );
    assert_eq!(src.reopen(&r, 7).await.unwrap().state, ChangesetState::Open);

    // Merge with squash and branch removal.
    let mut merged = mr("Update dependencies", false);
    merged["state"] = "merged".into();
    Mock::given(method("PUT"))
        .and(path(format!("{MR_PATH}/merge")))
        .and(body_partial_json(
            json!({"squash": true, "should_remove_source_branch": true}),
        ))
        .respond_with(ok(merged))
        .expect(1)
        .mount(&server)
        .await;
    let opts = MergeOptions {
        squash: true,
        remove_source_branch: true,
    };
    assert_eq!(
        src.merge(&r, 7, opts).await.unwrap().state,
        ChangesetState::Merged
    );

    // Comment.
    Mock::given(method("POST"))
        .and(path(format!("{MR_PATH}/notes")))
        .and(body_partial_json(json!({"body": "Rebased by Sourcegull"})))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({"id": 999, "body": "Rebased by Sourcegull", "system": false}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    src.create_comment(&r, 7, "Rebased by Sourcegull")
        .await
        .unwrap();
}

#[tokio::test]
async fn merge_refusal_is_not_mergeable() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("PUT"))
        .and(path(format!("{MR_PATH}/merge")))
        .respond_with(
            ResponseTemplate::new(405).set_body_json(json!({"message": "405 Method Not Allowed"})),
        )
        .mount(&server)
        .await;
    let err = changesets(&server)
        .merge(&repo(&server), 7, MergeOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::NotMergeable(_)), "{err:?}");
}

async fn mount_sync(server: &MockServer, approvals: ResponseTemplate, states: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(MR_PATH))
        .respond_with(ok(fixture("merge_request")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR_PATH}/notes")))
        .and(query_param("sort", "asc"))
        .respond_with(ok(fixture("notes")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR_PATH}/resource_state_events")))
        .respond_with(states)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR_PATH}/pipelines")))
        .respond_with(ok(fixture("pipelines")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{MR_PATH}/approvals")))
        .respond_with(approvals)
        .mount(server)
        .await;
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({"message": "404 Not Found"}))
}

#[tokio::test]
async fn sync_with_pipelines_approvals_notes_and_state_events() {
    let server = gitlab("16.5.1-ee").await;
    mount_sync(
        &server,
        ok(fixture("approvals_premium")),
        ok(fixture("resource_state_events")),
    )
    .await;
    let cs = changesets(&server).load(&repo(&server), 7).await.unwrap();
    assert_eq!(cs.state, ChangesetState::Draft);
    assert_eq!(cs.review_state, Some(ReviewState::Approved));
    // The head pipeline (502) failed; the older one passed.
    assert_eq!(cs.check_state, Some(CheckState::Failed));
    let timeline: Vec<(ChangesetEventKind, &str)> =
        cs.events.iter().map(|e| (e.kind, e.key.as_str())).collect();
    assert_eq!(
        timeline,
        [
            (ChangesetEventKind::ReviewRequested, "note:301"),
            (ChangesetEventKind::Check, "pipeline:497"),
            (ChangesetEventKind::Commented, "note:302"),
            (ChangesetEventKind::Approved, "note:303"),
            (ChangesetEventKind::MarkedReady, "note:304"),
            (ChangesetEventKind::Closed, "state:801"),
            (ChangesetEventKind::Reopened, "state:802"),
            (ChangesetEventKind::MarkedDraft, "note:305"),
            (ChangesetEventKind::Check, "pipeline:502"),
        ]
    );
    let check = cs.events.last().unwrap();
    assert_eq!(check.metadata["state"], "FAILED");
    assert_eq!(cs.events[3].metadata["author"], "bob");
    assert_eq!(cs.metadata["approvedBy"], json!(["bob"]));
    assert_eq!(cs.metadata["headPipeline"]["id"], 502);
}

#[tokio::test]
async fn sync_premium_approvals_still_needed_is_pending() {
    let server = gitlab("16.5.1-ee").await;
    let mut approvals = fixture("approvals_premium");
    approvals["approvals_required"] = 2.into();
    approvals["approvals_left"] = 1.into();
    approvals["approved"] = false.into();
    mount_sync(&server, ok(approvals), ok(fixture("resource_state_events"))).await;
    let cs = changesets(&server).load(&repo(&server), 7).await.unwrap();
    assert_eq!(cs.review_state, Some(ReviewState::Pending));
}

#[tokio::test]
async fn sync_falls_back_to_system_notes_without_approvals_and_state_events() {
    let server = gitlab("13.1.0").await;
    let mut notes = fixture("notes");
    let n = notes.as_array_mut().unwrap();
    n.push(json!({"id": 310, "body": "closed", "system": true,
        "author": {"id": 3, "username": "alice"}, "created_at": "2026-10-12T16:00:00.000Z"}));
    n.push(
        json!({"id": 311, "body": "unapproved this merge request", "system": true,
        "author": {"id": 4, "username": "bob"}, "created_at": "2026-10-12T16:01:00.000Z"}),
    );
    Mock::given(method("GET"))
        .and(path(format!("{MR_PATH}/notes")))
        .respond_with(ok(notes))
        .with_priority(1)
        .mount(&server)
        .await;
    mount_sync(&server, not_found(), not_found()).await;
    let cs = changesets(&server).load(&repo(&server), 7).await.unwrap();
    // Bob approved and then unapproved.
    assert_eq!(cs.review_state, Some(ReviewState::Pending));
    let closed: Vec<&str> = cs
        .events
        .iter()
        .filter(|e| e.kind == ChangesetEventKind::Closed)
        .map(|e| e.key.as_str())
        .collect();
    assert_eq!(closed, ["note:310"]);
}

#[tokio::test]
async fn sync_free_tier_approval_from_notes() {
    let server = gitlab("16.5.1").await;
    mount_sync(
        &server,
        ResponseTemplate::new(403).set_body_json(json!({"message": "403 Forbidden"})),
        ok(json!([])),
    )
    .await;
    let cs = changesets(&server).load(&repo(&server), 7).await.unwrap();
    assert_eq!(cs.review_state, Some(ReviewState::Approved));
}

#[tokio::test]
async fn sync_of_deleted_merge_request_is_not_found() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("GET"))
        .respond_with(not_found())
        .mount(&server)
        .await;
    let err = changesets(&server)
        .load(&repo(&server), 7)
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn archived_project_is_read_only() {
    let server = gitlab("16.5.1").await;
    let mut archived = fixture("project");
    archived["archived"] = true.into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42"))
        .respond_with(ok(archived))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/merge_requests"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "403 Forbidden"})))
        .mount(&server)
        .await;
    mount_sync(&server, not_found(), ok(json!([]))).await;
    let src = changesets(&server);

    // Discovery has not seen the archival yet: the 403 is checked against the project.
    let err = src
        .create(&repo(&server), &new_changeset(false))
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::Archived(_)), "{err:?}");

    let mut r = repo(&server);
    r.archived = true;
    assert_eq!(
        src.load(&r, 7).await.unwrap().state,
        ChangesetState::ReadOnly
    );
    let push = "remote: GitLab: You are not allowed to push code to an archived project.";
    assert!(matches!(
        src.classify_push_error(&r, push),
        Some(CodeHostError::Archived(_))
    ));
    assert!(matches!(
        src.classify_push_error(
            &repo(&server),
            "remote: You are not allowed to push code to this project."
        ),
        Some(CodeHostError::Forbidden(_))
    ));
    assert!(
        src.classify_push_error(&r, "rejected: non-fast-forward")
            .is_none()
    );
}

#[tokio::test]
async fn creates_fork_waits_for_import_and_opens_merge_request_from_it() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42"))
        .respond_with(ok(fixture("project")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", "alice-token"))
        .respond_with(ok(fixture("user")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/alice%2Facme-api"))
        .respond_with(not_found())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/fork"))
        .and(body_partial_json(
            json!({"namespace_path": "alice", "name": "acme-api", "path": "acme-api"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("fork_scheduled")))
        .expect(1)
        .mount(&server)
        .await;
    // Importing once, then finished.
    let mut started = fixture("fork_scheduled");
    started["import_status"] = "started".into();
    let mut finished = fixture("fork_scheduled");
    finished["import_status"] = "finished".into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/99"))
        .respond_with(ok(started))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/99"))
        .respond_with(ok(finished))
        .mount(&server)
        .await;

    let cred = Credential::Basic {
        username: "git".into(),
        password: "alice-token".into(),
    };
    let src = changesets(&server).for_credential(&cred);
    let fork = src.get_fork(&repo(&server), None).await.unwrap();
    assert!(fork.fork);
    assert_eq!(fork.external.id, "99");
    assert!(fork.name.ends_with("/alice/acme-api"), "{}", fork.name);

    let push = src.push_strategy(&fork, &cred).unwrap();
    assert_eq!(
        push.push_url.as_str(),
        "https://git:alice-token@gitlab.example.com/alice/acme-api.git"
    );
    assert_eq!(push.fork_namespace.as_deref(), Some("alice"));

    // The merge request is opened on the fork against the upstream project.
    let mut from_fork = mr("Update dependencies", false);
    from_fork["source_project_id"] = 99.into();
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/99/merge_requests"))
        .and(header("PRIVATE-TOKEN", "alice-token"))
        .and(body_partial_json(json!({"target_project_id": 42})))
        .respond_with(ResponseTemplate::new(201).set_body_json(from_fork))
        .expect(1)
        .mount(&server)
        .await;
    let new = NewChangeset {
        head_repo_id: Some(fork.external.id.clone()),
        ..new_changeset(false)
    };
    let cs = src.create(&repo(&server), &new).await.unwrap().changeset;
    assert_eq!(cs.head_repo_id.as_deref(), Some("99"));
}

#[tokio::test]
async fn existing_fork_in_configured_namespace_is_reused() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42"))
        .respond_with(ok(fixture("project")))
        .mount(&server)
        .await;
    let mut fork = fixture("fork_scheduled");
    fork["import_status"] = "finished".into();
    fork["path_with_namespace"] = "forks/acme-api".into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/forks%2Facme-api"))
        .respond_with(ok(fork))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let src = changesets(&server);
    let meta = src.get_fork(&repo(&server), Some("forks")).await.unwrap();
    assert_eq!(meta.external.id, "99");

    // A project with the fork's name that is not a fork of the target is an error.
    let server = gitlab("16.5.1").await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42"))
        .respond_with(ok(fixture("project")))
        .mount(&server)
        .await;
    let mut unrelated = fixture("project");
    unrelated["id"] = 77.into();
    unrelated["path_with_namespace"] = "forks/acme-api".into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/forks%2Facme-api"))
        .respond_with(ok(unrelated))
        .mount(&server)
        .await;
    let err = changesets(&server)
        .get_fork(&repo(&server), Some("forks"))
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::Config(_)), "{err:?}");
}

#[tokio::test]
async fn validates_credentials_and_scopes() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", "good"))
        .respond_with(ok(fixture("user")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/personal_access_tokens/self"))
        .and(header("PRIVATE-TOKEN", "good"))
        .respond_with(ok(
            json!({"scopes": ["api", "write_repository"], "active": true}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", "readonly"))
        .respond_with(ok(fixture("user")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/personal_access_tokens/self"))
        .and(header("PRIVATE-TOKEN", "readonly"))
        .respond_with(ok(json!({"scopes": ["read_api", "write_repository"]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("Authorization", "Bearer oauth-token"))
        .respond_with(ok(fixture("user")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    let src = changesets(&server);
    let pat = |t: &str| Credential::Basic {
        username: "git".into(),
        password: t.into(),
    };
    src.validate_credential(&pat("good")).await.unwrap();
    let err = src.validate_credential(&pat("readonly")).await.unwrap_err();
    assert!(
        matches!(err, CodeHostError::Forbidden(ref m) if m.contains("api scope")),
        "{err:?}"
    );
    let err = src.validate_credential(&pat("bad")).await.unwrap_err();
    assert_eq!(err, CodeHostError::Unauthorized);
    // OAuth tokens cannot report scopes; a working token is accepted.
    let oauth = Credential::Basic {
        username: "oauth2".into(),
        password: "oauth-token".into(),
    };
    src.validate_credential(&oauth).await.unwrap();
}

#[tokio::test]
async fn push_strategy_embeds_the_credential() {
    let server = gitlab("16.5.1").await;
    let src = changesets(&server);
    let r = repo(&server);
    let push = src
        .push_strategy(
            &r,
            &Credential::Basic {
                username: "oauth2".into(),
                password: "t0k".into(),
            },
        )
        .unwrap();
    assert_eq!(
        push.push_url.as_str(),
        "https://oauth2:t0k@gitlab.example.com/acme/api.git"
    );
    assert_eq!(push.fork_namespace, None);
}

fn file(action: CommitAction, path: &str, content: Option<&[u8]>) -> CommitFile {
    CommitFile {
        action,
        path: path.into(),
        previous_path: None,
        content: content.map(<[u8]>::to_vec),
        executable: None,
    }
}

fn new_commit(files: Vec<CommitFile>) -> NewCommit {
    NewCommit {
        branch: "batch/update-deps".into(),
        message: "Update dependencies".into(),
        author_name: "Alice".into(),
        author_email: "alice@example.com".into(),
        start_sha: "a".repeat(40),
        start_repo_id: None,
        force: true,
        files,
    }
}

#[tokio::test]
async fn creates_commits_through_the_api() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/repository/commits"))
        .and(header("PRIVATE-TOKEN", "user-token"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "c0ffee0000000000000000000000000000000000",
            "short_id": "c0ffee00",
            "title": "Update dependencies",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let src = changesets(&server).for_credential(&Credential::Basic {
        username: "alice".into(),
        password: "user-token".into(),
    });
    assert!(src.supports_commit_api());
    let mut exec = file(CommitAction::Create, "run.sh", Some(b"#!/bin/sh\n"));
    exec.executable = Some(true);
    let mut plain = file(CommitAction::Create, "plain.txt", Some(b"text\n"));
    plain.executable = Some(false);
    let mut renamed = file(
        CommitAction::Move,
        "docs/new.md",
        Some(b"moved and changed\n"),
    );
    renamed.previous_path = Some("docs/old.md".into());
    let mut pure_move = file(CommitAction::Move, "README.txt", None);
    pure_move.previous_path = Some("README.md".into());
    let mut chmod = file(CommitAction::Update, "tool.py", None);
    chmod.executable = Some(false);
    let mut update_exec = file(CommitAction::Update, "build.sh", Some(b"make\n"));
    update_exec.executable = Some(true);
    let commit = new_commit(vec![
        exec,
        plain,
        file(
            CommitAction::Create,
            "logo.bin",
            Some(&[0, 159, 146, 150, 0]),
        ),
        file(
            CommitAction::Update,
            "src/main.rs",
            Some("fn main() { \u{e9} }\n".as_bytes()),
        ),
        file(CommitAction::Delete, "src/lib.rs", None),
        renamed,
        pure_move,
        chmod,
        update_exec,
    ]);
    let sha = src.create_commit(&repo(&server), &commit).await.unwrap();
    assert_eq!(sha, "c0ffee0000000000000000000000000000000000");

    let reqs = server.received_requests().await.unwrap();
    let req = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({
            "branch": "batch/update-deps",
            "commit_message": "Update dependencies",
            "author_name": "Alice",
            "author_email": "alice@example.com",
            "start_sha": "a".repeat(40),
            "force": true,
            "actions": [
                {"action": "create", "file_path": "run.sh", "content": "#!/bin/sh\n"},
                {"action": "chmod", "file_path": "run.sh", "execute_filemode": true},
                {"action": "create", "file_path": "plain.txt", "content": "text\n"},
                {"action": "create", "file_path": "logo.bin", "content": "AJ+SlgA=",
                 "encoding": "base64"},
                {"action": "update", "file_path": "src/main.rs",
                 "content": "fn main() { \u{e9} }\n"},
                {"action": "delete", "file_path": "src/lib.rs"},
                {"action": "move", "file_path": "docs/new.md", "previous_path": "docs/old.md",
                 "content": "moved and changed\n"},
                {"action": "move", "file_path": "README.txt", "previous_path": "README.md"},
                {"action": "chmod", "file_path": "tool.py", "execute_filemode": false},
                {"action": "update", "file_path": "build.sh", "content": "make\n"},
                {"action": "chmod", "file_path": "build.sh", "execute_filemode": true},
            ],
        })
    );
}

#[tokio::test]
async fn commits_to_a_fork_start_from_the_upstream_project() {
    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/99/repository/commits"))
        .and(body_partial_json(json!({
            "branch": "batch/update-deps",
            "start_sha": "a".repeat(40),
            "start_project": 42,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "f".repeat(40)})))
        .expect(1)
        .mount(&server)
        .await;
    let src = changesets(&server);
    let mut fork = repo(&server);
    fork.external.id = "99".into();
    let mut commit = new_commit(vec![file(CommitAction::Delete, "x", None)]);
    commit.start_repo_id = Some("42".into());
    commit.force = false;
    assert_eq!(
        src.create_commit(&fork, &commit).await.unwrap(),
        "f".repeat(40)
    );
    let reqs = server.received_requests().await.unwrap();
    let req = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert!(body.get("force").is_none(), "{body}");
}

#[tokio::test]
async fn commit_errors_are_typed() {
    let server = gitlab("16.5.1").await;
    let mut archived = fixture("project");
    archived["archived"] = true.into();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/42"))
        .respond_with(ok(archived))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/repository/commits"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "403 Forbidden"})))
        .mount(&server)
        .await;
    let src = changesets(&server);
    let commit = new_commit(vec![file(CommitAction::Delete, "x", None)]);
    let err = src
        .create_commit(&repo(&server), &commit)
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::Archived(_)), "{err:?}");

    let server = gitlab("16.5.1").await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/42/repository/commits"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"message": "A file with this name doesn't exist"})),
        )
        .mount(&server)
        .await;
    let err = changesets(&server)
        .create_commit(&repo(&server), &commit)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CodeHostError::Http { status: 400, message } if message.contains("doesn't exist")),
        "{err:?}"
    );
}
