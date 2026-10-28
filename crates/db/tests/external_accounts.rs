//! `user_external_accounts` and `oauth_flows` store tests: linking, token encryption,
//! ownership conflicts, deleted users, and single-use flows.

use std::sync::Arc;

use serde_json::json;
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{AccountSpec, Db, LinkAccount, NewUser, OAuthFlow, migrate};
use sourcegull_testutil::TestDb;

const SPEC: AccountSpec<'static> = AccountSpec {
    service_type: "gitlab",
    service_id: "https://gitlab.example.com/",
    account_id: "42",
};

fn link<'a>(auth: Option<&'a [u8]>) -> LinkAccount<'a> {
    LinkAccount {
        spec: SPEC,
        client_id: if auth.is_some() { "app" } else { "" },
        account_data: json!({"username": "alice"}),
        auth_data: auth,
    }
}

#[tokio::test]
async fn link_tokens_and_ownership() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [9u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let store = db.external_accounts(Some(keyring));
    let user = |name: &str| NewUser {
        username: name.into(),
        ..Default::default()
    };
    let alice = db.users().create(&user("alice")).await.unwrap();
    let bob = db.users().create(&user("bob")).await.unwrap();

    // Linked by username first (no tokens), then by OAuth (tokens): one row.
    let id = store.link(alice.id, &link(None)).await.unwrap();
    assert_eq!(store.user_for(SPEC).await.unwrap(), Some(alice.id));
    let id2 = store
        .link(alice.id, &link(Some(b"{\"t\":1}")))
        .await
        .unwrap();
    assert_eq!(id, id2);
    let a = store
        .get_for_user(alice.id, SPEC.service_type, SPEC.service_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.auth_data.as_deref(), Some(&b"{\"t\":1}"[..]));
    assert_eq!(a.client_id, "app");

    // A link without tokens keeps them; expiry is cleared by new tokens only.
    store.mark_expired(id).await.unwrap();
    store.link(alice.id, &link(None)).await.unwrap();
    let a = store.list_for_user(alice.id).await.unwrap().remove(0);
    assert!(a.expired_at.is_some());
    assert_eq!(a.auth_data.as_deref(), Some(&b"{\"t\":1}"[..]));
    store.set_auth_data(id, b"{\"t\":2}").await.unwrap();
    let a = store.list_for_user(alice.id).await.unwrap().remove(0);
    assert!(a.expired_at.is_none());
    assert_eq!(a.auth_data.as_deref(), Some(&b"{\"t\":2}"[..]));

    // Another live user cannot take the account; it moves once its owner is deleted.
    assert!(store.link(bob.id, &link(None)).await.is_err());
    db.users().soft_delete(alice.id).await.unwrap();
    assert_eq!(store.user_for(SPEC).await.unwrap(), None);
    store.link(bob.id, &link(None)).await.unwrap();
    assert_eq!(store.user_for(SPEC).await.unwrap(), Some(bob.id));
    assert_eq!(
        store
            .users_for_accounts("gitlab", SPEC.service_id, &["42".into(), "43".into()])
            .await
            .unwrap(),
        vec![("42".to_owned(), bob.id)]
    );

    // Without a keyring, token reads fail but plain lookups work.
    let plain = db.external_accounts(None);
    assert_eq!(plain.user_for(SPEC).await.unwrap(), Some(bob.id));

    // By account ID or by login (case-insensitive); nothing without either.
    let by = |id: Option<&'static str>, login: Option<&'static str>| {
        let plain = plain.clone();
        async move {
            plain
                .user_by_account_or_login("gitlab", SPEC.service_id, id, login)
                .await
                .unwrap()
        }
    };
    assert_eq!(by(Some("42"), None).await, Some(bob.id));
    assert_eq!(by(None, Some("ALICE")).await, Some(bob.id));
    assert_eq!(by(Some("43"), Some("carol")).await, None);
    assert_eq!(by(None, None).await, None);
}

#[tokio::test]
async fn flows_are_single_use_and_expire() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    let store = db.external_accounts(None);
    let flow = OAuthFlow {
        provider: "app".into(),
        pkce_verifier: "v".into(),
        return_to: "/search".into(),
        nonce: None,
    };
    let soon = chrono::Utc::now() + chrono::Duration::minutes(10);
    store.create_flow(b"h1", &flow, soon).await.unwrap();
    store
        .create_flow(
            b"h2",
            &flow,
            chrono::Utc::now() - chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
    assert_eq!(store.take_flow(b"h1").await.unwrap(), Some(flow));
    assert_eq!(store.take_flow(b"h1").await.unwrap(), None);
    assert_eq!(store.take_flow(b"h2").await.unwrap(), None);
    assert_eq!(store.delete_expired_flows().await.unwrap(), 1);
}
