//! Changeset spec (de)serialization and validation, against the shapes of
//! `changeset_spec.schema.json`.

use serde_json::{Value, json};
use sourcegull_batches::changeset_spec::HEAD_BASE_MISMATCH;
use sourcegull_batches::{ChangesetSpec, PublishedValue, parse_changeset_spec};

fn branch_spec() -> Value {
    json!({
        "baseRepository": "UmVwb3NpdG9yeTo5",
        "baseRef": "refs/heads/main",
        "baseRev": "4095572721c6234cd72013fd49dff4fb48f0f8a4",
        "headRepository": "UmVwb3NpdG9yeTo5",
        "headRef": "refs/heads/update-go-deps",
        "title": "Update Go dependencies",
        "body": "Ran go get -u",
        "commits": [{
            "message": "Update Go dependencies",
            "diff": "diff --git a/go.mod b/go.mod\n",
            "authorName": "Batch Bot",
            "authorEmail": "batch-bot@example.com"
        }],
        "published": "draft"
    })
}

fn errors(text: &str) -> Vec<String> {
    parse_changeset_spec(text)
        .unwrap_err()
        .issues()
        .iter()
        .map(|i| i.to_string())
        .collect()
}

#[test]
fn branch_spec_v1() {
    let text = serde_json::to_string_pretty(&branch_spec()).unwrap();
    let spec = parse_changeset_spec(&text).unwrap();
    let b = spec.as_branch().unwrap();
    assert!(!spec.is_importing_existing());
    assert_eq!(spec.base_repository(), "UmVwb3NpdG9yeTo5");
    assert_eq!(b.head_branch(), "update-go-deps");
    assert_eq!(b.base_branch(), "main");
    assert_eq!(b.published, Some(PublishedValue::Draft));
    assert_eq!(b.fork, None);
    let c = b.commit().unwrap();
    assert_eq!(c.diff, b"diff --git a/go.mod b/go.mod\n");
    assert_eq!(c.author_email, "batch-bot@example.com");
    // Round trip: same JSON, no `version`, `fork` or `published` added.
    assert_eq!(
        serde_json::from_str::<Value>(&spec.to_json()).unwrap(),
        branch_spec()
    );
    let mut unpublished = branch_spec();
    unpublished.as_object_mut().unwrap().remove("published");
    let spec = ChangesetSpec::from_value(unpublished.clone()).unwrap();
    assert_eq!(spec.as_branch().unwrap().published, None);
    assert_eq!(serde_json::to_value(&spec).unwrap(), unpublished);
}

#[test]
fn branch_spec_v2_binary_diff() {
    let mut v = branch_spec();
    // "\x00\xffbinary" in base64.
    v["commits"][0]["version"] = json!(2);
    v["commits"][0]["diff"] = json!("AP9iaW5hcnk=");
    v["fork"] = json!(true);
    v["published"] = Value::Null;
    let spec = ChangesetSpec::from_value(v.clone()).unwrap();
    let b = spec.as_branch().unwrap();
    assert_eq!(b.commit().unwrap().diff, b"\x00\xffbinary");
    assert_eq!(b.fork, Some(true));
    assert_eq!(b.published, None);
    let out = serde_json::to_value(&spec).unwrap();
    assert_eq!(out["commits"][0], v["commits"][0]);

    v["commits"][0]["diff"] = json!("not base64!");
    let e = ChangesetSpec::from_value(v).unwrap_err();
    assert!(e.to_string().contains("base64"), "{e}");
}

#[test]
fn existing_spec() {
    let text = r#"{"baseRepository": "UmVwb3NpdG9yeTox", "externalID": "42"}"#;
    let spec = parse_changeset_spec(text).unwrap();
    assert!(spec.is_importing_existing());
    let ChangesetSpec::Existing(e) = &spec else {
        panic!()
    };
    assert_eq!(e.external_id, "42");
    assert_eq!(
        serde_json::from_str::<Value>(&spec.to_json()).unwrap(),
        serde_json::from_str::<Value>(text).unwrap()
    );
}

#[test]
fn schema_errors_with_positions() {
    assert_eq!(
        errors("{\n  \"baseRepository\": \"R\",\n  \"externalID\": 42\n}"),
        ["line 3, column 17: externalID: must be of type string, not integer"]
    );
    assert_eq!(
        errors("{\n  \"baseRepository\": \"R\",\n  \"externalID\": \"1\",\n  \"title\": \"x\"\n}"),
        ["line 4, column 3: additional property \"title\" is not allowed"]
    );

    let mut v = branch_spec();
    v["baseRef"] = json!("main");
    v["commits"] = json!([]);
    v["published"] = json!("yes");
    v.as_object_mut().unwrap().remove("body");
    let text = serde_json::to_string_pretty(&v).unwrap();
    assert_eq!(
        errors(&text),
        [
            "line 1, column 1: property \"body\" is required",
            "line 2, column 14: baseRef: must be a full branch ref (refs/heads/<branch>)",
            "line 5, column 14: commits: must have at least 1 item",
            "line 8, column 16: published: must be true, false or \"draft\"",
        ]
    );

    let mut v = branch_spec();
    v["commits"][0]["authorEmail"] = json!("nobody");
    v["commits"][0].as_object_mut().unwrap().remove("diff");
    let commit = v["commits"][0].clone();
    v["commits"].as_array_mut().unwrap().push(commit);
    let text = serde_json::to_string(&v).unwrap();
    let got = errors(&text);
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(
        got.iter().all(|e| e.starts_with("line 1, column ")),
        "{got:?}"
    );
    assert!(
        got.iter()
            .any(|e| e.ends_with("commits: must have at most 1 item"))
    );
    assert!(
        got.iter()
            .any(|e| e.ends_with("commits.0.authorEmail: must be an email address"))
    );
    assert!(
        got.iter()
            .any(|e| e.ends_with("commits.1: property \"diff\" is required"))
    );
}

#[test]
fn head_must_equal_base() {
    let mut v = branch_spec();
    v["headRepository"] = json!("UmVwb3NpdG9yeToxMA==");
    let text = serde_json::to_string_pretty(&v).unwrap();
    assert_eq!(
        errors(&text),
        [format!("line 15, column 21: {HEAD_BASE_MISMATCH}")]
    );
}

#[test]
fn not_json() {
    assert_eq!(
        errors("{\n  \"baseRepository\": \"R\",\n  oops\n}"),
        ["line 3, column 3: key must be a string at line 3 column 3"]
    );
    assert_eq!(
        errors("[]"),
        ["line 1, column 1: must be of type object, not array"]
    );
}
