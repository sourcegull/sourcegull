#[tokio::test]
async fn creates_and_uses_database() {
    let Some(db) = sourcegull_testutil::TestDb::new().await else {
        return;
    };
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(one, 1);
}
