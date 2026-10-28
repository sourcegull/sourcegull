//! Notebook store tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): visibility
//! of private, public and org notebooks, full-text search, filters, ordering, stars, and the
//! hiding of blocks in unreadable repositories.

use std::sync::Arc;

use roaring::RoaringBitmap;
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{Db, ListedRepo, Namespace, NewExternalService, NewUser, migrate};
use sourcegull_notebooks::{
    Access, Block, BlockKind, FileInput, ListNotebooks, NotebookInput, NotebookOrder,
    NotebookStore, TextInput, hidden_blocks,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, OrgRole, RepoAccess, UserId, Visibility};

fn md(id: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        kind: BlockKind::Markdown {
            markdown_input: TextInput { text: text.into() },
        },
    }
}

fn query(id: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        kind: BlockKind::Query {
            query_input: TextInput { text: text.into() },
        },
    }
}

fn file(id: &str, repo: &str) -> Block {
    Block {
        id: id.into(),
        kind: BlockKind::File {
            file_input: FileInput {
                repository_name: repo.into(),
                file_path: "main.go".into(),
                revision: None,
                line_range: None,
            },
        },
    }
}

fn input(title: &str, blocks: Vec<Block>, public: bool, ns: Namespace) -> NotebookInput {
    NotebookInput {
        title: title.into(),
        blocks,
        public,
        namespace: ns,
    }
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

fn titles(nbs: &[sourcegull_notebooks::Notebook]) -> Vec<&str> {
    nbs.iter().map(|n| n.title.as_str()).collect()
}

#[tokio::test]
async fn visibility_search_and_stars() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let store = NotebookStore::new(t.pool.clone());
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    let carol = user(&db, "carol").await;
    let org = db.orgs().create("acme", None).await.unwrap().id;
    db.orgs()
        .add_member(org, bob, OrgRole::Member)
        .await
        .unwrap();

    let private = store
        .create(
            &input(
                "Alice private",
                vec![md("1", "Deploying the frontend service")],
                false,
                Namespace::User(alice),
            ),
            alice,
        )
        .await
        .unwrap();
    let public = store
        .create(
            &input(
                "Public tour",
                vec![
                    md("1", "How searching works"),
                    query("2", "repo:gitlab.com/acme/api lang:go"),
                ],
                true,
                Namespace::User(alice),
            ),
            alice,
        )
        .await
        .unwrap();
    let org_nb = store
        .create(
            &input(
                "Org runbook",
                vec![md("1", "Rotating credentials")],
                false,
                Namespace::Org(org),
            ),
            bob,
        )
        .await
        .unwrap();
    assert_eq!(private.creator, Some(alice));
    assert_eq!(org_nb.namespace, Namespace::Org(org));
    assert_eq!(org_nb.pattern_type, "standard");

    let list = |access: Access| ListNotebooks {
        access,
        order: NotebookOrder::CreatedAt,
        ..Default::default()
    };
    let visible = |access| {
        let store = store.clone();
        async move {
            let (nbs, total) = store.list(&list(access)).await.unwrap();
            assert_eq!(total as usize, nbs.len());
            nbs.into_iter().map(|n| n.title).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        visible(Access::Viewer(Some(alice))).await,
        ["Alice private", "Public tour"]
    );
    assert_eq!(
        visible(Access::Viewer(Some(bob))).await,
        ["Public tour", "Org runbook"]
    );
    assert_eq!(visible(Access::Viewer(Some(carol))).await, ["Public tour"]);
    assert_eq!(visible(Access::Viewer(None)).await, ["Public tour"]);
    assert_eq!(visible(Access::Internal).await.len(), 3);

    // get applies the same rule.
    assert!(
        store
            .get(private.id, Access::Viewer(Some(bob)))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get(private.id, Access::Viewer(Some(alice)))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get(org_nb.id, Access::Viewer(Some(bob)))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get(org_nb.id, Access::Viewer(Some(carol)))
            .await
            .unwrap()
            .is_none()
    );
    // Leaving the org (or the org being deleted) hides its private notebooks.
    db.orgs().remove_member(org, bob).await.unwrap();
    assert!(
        store
            .get(org_nb.id, Access::Viewer(Some(bob)))
            .await
            .unwrap()
            .is_none()
    );
    db.orgs()
        .add_member(org, bob, OrgRole::Member)
        .await
        .unwrap();

    // Full-text search: title substrings, word prefixes in Markdown and query blocks, stems.
    let search = |q: &str, access: Access| {
        let store = store.clone();
        let q = q.to_owned();
        async move {
            let (nbs, _) = store
                .list(&ListNotebooks {
                    query: Some(q),
                    ..list(access)
                })
                .await
                .unwrap();
            nbs.into_iter().map(|n| n.title).collect::<Vec<_>>()
        }
    };
    let a = Access::Viewer(Some(alice));
    assert_eq!(search("deploy", a).await, ["Alice private"]);
    assert_eq!(search("frontend servic", a).await, ["Alice private"]);
    assert_eq!(search("search", a).await, ["Public tour"]);
    assert_eq!(search("lang:go", a).await, ["Public tour"]);
    assert_eq!(search("vate", a).await, ["Alice private"]);
    assert_eq!(search("rotating", a).await, Vec::<String>::new());
    assert_eq!(
        search("rotating", Access::Viewer(Some(bob))).await,
        ["Org runbook"]
    );
    assert_eq!(search("&|!():'\\", a).await, Vec::<String>::new());
    assert_eq!(search("100%", a).await, Vec::<String>::new());

    // Filters.
    let (nbs, _) = store
        .list(&ListNotebooks {
            creator: Some(bob),
            ..list(Access::Viewer(Some(bob)))
        })
        .await
        .unwrap();
    assert_eq!(titles(&nbs), ["Org runbook"]);
    let (nbs, _) = store
        .list(&ListNotebooks {
            namespace: Some(Namespace::User(alice)),
            ..list(Access::Viewer(Some(carol)))
        })
        .await
        .unwrap();
    assert_eq!(titles(&nbs), ["Public tour"]);

    // Stars: idempotent, counted, filtered and ordered by.
    let s1 = store.create_star(public.id, bob).await.unwrap();
    let s2 = store.create_star(public.id, bob).await.unwrap();
    assert_eq!(s1, s2);
    store.create_star(public.id, carol).await.unwrap();
    store.create_star(org_nb.id, bob).await.unwrap();
    let (stars, total) = store.list_stars(public.id, 1, 0).await.unwrap();
    assert_eq!((stars.len(), total), (1, 2));
    let (nbs, _) = store
        .list(&ListNotebooks {
            starred_by: Some(bob),
            ..list(Access::Viewer(Some(bob)))
        })
        .await
        .unwrap();
    assert_eq!(titles(&nbs), ["Public tour", "Org runbook"]);
    // Stars on notebooks the viewer cannot see do not show.
    let (nbs, total) = store
        .list(&ListNotebooks {
            starred_by: Some(bob),
            ..list(Access::Viewer(Some(carol)))
        })
        .await
        .unwrap();
    assert_eq!((titles(&nbs), total), (vec!["Public tour"], 1));
    let (nbs, _) = store
        .list(&ListNotebooks {
            order: NotebookOrder::StarCount,
            descending: true,
            ..list(Access::Internal)
        })
        .await
        .unwrap();
    assert_eq!(
        titles(&nbs),
        ["Public tour", "Org runbook", "Alice private"]
    );
    store.delete_star(public.id, carol).await.unwrap();
    assert!(store.get_star(public.id, carol).await.unwrap().is_none());

    // Paging.
    let (nbs, total) = store
        .list(&ListNotebooks {
            limit: 1,
            offset: 1,
            ..list(Access::Internal)
        })
        .await
        .unwrap();
    assert_eq!((titles(&nbs), total), (vec!["Public tour"], 3));

    // Update: fields, namespace and the search index follow.
    let updated = store
        .update(
            private.id,
            &input(
                "Renamed",
                vec![md("1", "Kubernetes upgrade")],
                true,
                Namespace::Org(org),
            ),
            bob,
        )
        .await
        .unwrap();
    assert_eq!(updated.updater, Some(bob));
    assert_eq!(updated.creator, Some(alice));
    assert!(updated.updated_at >= private.updated_at);
    assert_eq!(
        search("kubernetes", Access::Viewer(Some(carol))).await,
        ["Renamed"]
    );
    assert_eq!(search("deploy", a).await, Vec::<String>::new());

    // Delete removes stars too.
    store.delete(public.id).await.unwrap();
    assert!(store.delete(public.id).await.unwrap_err().is_not_found());
    assert!(store.get_star(public.id, bob).await.unwrap().is_none());
}

#[tokio::test]
async fn blocks_in_unreadable_repositories_are_hidden() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let svc = db
        .external_services(Some(keyring))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "gl".into(),
            config: r#"{"url": "https://gitlab.example.com", "token": "t"}"#.into(),
        })
        .await
        .unwrap();
    let ids = db
        .repos()
        .upsert_listed(
            svc.id,
            &[listed(1, "public", false), listed(2, "secret", true)],
        )
        .await
        .unwrap()
        .ids;
    let (public, secret) = (ids[0], ids[1]);
    let blocks = vec![
        md("m", "text"),
        file("f1", "gitlab.example.com/acme/public"),
        file("f2", "GITLAB.EXAMPLE.COM/acme/secret"),
        file("f3", "gitlab.example.com/acme/missing"),
        Block {
            id: "s".into(),
            kind: BlockKind::Symbol {
                symbol_input: sourcegull_notebooks::SymbolInput {
                    repository_name: "gitlab.example.com/acme/secret".into(),
                    file_path: "a.go".into(),
                    revision: None,
                    line_context: 3,
                    symbol_name: "f".into(),
                    symbol_container_name: String::new(),
                    symbol_kind: "FUNCTION".into(),
                },
            },
        },
    ];
    let mut unrestricted = RoaringBitmap::new();
    unrestricted.insert(public.get() as u32);
    let outsider = RepoAccess::anonymous(false, Arc::new(unrestricted.clone()));
    let mut granted = RoaringBitmap::new();
    granted.insert(secret.get() as u32);
    let insider = RepoAccess::Restricted {
        user_id: None,
        user_mapping: false,
        unrestricted: Arc::new(unrestricted),
        granted: Arc::new(granted),
    };
    let mut hidden: Vec<String> = hidden_blocks(&db, &outsider, &blocks)
        .await
        .unwrap()
        .into_iter()
        .collect();
    hidden.sort();
    assert_eq!(hidden, ["f2", "s"]);
    assert!(
        hidden_blocks(&db, &insider, &blocks)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        hidden_blocks(&db, &RepoAccess::All, &blocks)
            .await
            .unwrap()
            .is_empty()
    );
}

fn listed(id: u32, name: &str, private: bool) -> ListedRepo {
    ListedRepo {
        name: format!("gitlab.example.com/acme/{name}"),
        uri: name.into(),
        url: String::new(),
        description: String::new(),
        fork: false,
        archived: false,
        private,
        visibility: if private {
            Visibility::Private
        } else {
            Visibility::Public
        },
        stars: 0,
        topics: Vec::new(),
        default_branch: None,
        last_activity_at: None,
        external_id: id.to_string(),
        external_service_type: "gitlab".into(),
        external_service_id: "https://gitlab.example.com/".into(),
        metadata: serde_json::json!({}),
        clone_url: format!("https://gitlab.example.com/acme/{name}.git"),
    }
}
