//! M8 E9-1: `user_devices` (migration 078).
use aionui_db::{CreateUserDeviceParams, IUserDeviceRepository, SqliteUserDeviceRepository, init_database_memory};

fn device<'a>(user_id: &'a str, token_hash: &'a str, created_at: i64) -> CreateUserDeviceParams<'a> {
    CreateUserDeviceParams {
        user_id,
        name: "Pixel 9",
        platform: "android",
        token_hash,
        scopes: r#"["mobile_default"]"#,
        created_at,
    }
}

#[tokio::test]
async fn create_find_touch_revoke_round_trip() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserDeviceRepository::new(db.pool().clone());

    let row = repo
        .create_within_limit(&device("u1", "hash-a", 100), 5)
        .await
        .unwrap()
        .unwrap();
    assert!(row.id.starts_with("dev"));
    assert_eq!(row.push_kind, "none");
    assert_eq!(row.last_active_at(), 100);

    let found = repo.find_active_by_token_hash("hash-a").await.unwrap().unwrap();
    assert_eq!(found.id, row.id);
    assert!(repo.find_active_by_token_hash("hash-b").await.unwrap().is_none());

    repo.touch_last_seen(&row.id, 200).await.unwrap();
    let touched = repo.find_for_user("u1", &row.id).await.unwrap().unwrap();
    assert_eq!(touched.last_seen_at, Some(200));
    assert!(repo.find_for_user("u2", &row.id).await.unwrap().is_none());

    // Rename is owner-only.
    assert!(!repo.rename("u2", &row.id, "Stolen").await.unwrap());
    assert!(repo.rename("u1", &row.id, "Work phone").await.unwrap());
    assert_eq!(
        repo.find_for_user("u1", &row.id).await.unwrap().unwrap().name,
        "Work phone"
    );

    // Someone else cannot revoke it.
    assert!(!repo.revoke("u2", &row.id, 300, "user").await.unwrap());
    assert!(repo.revoke("u1", &row.id, 300, "user").await.unwrap());
    assert!(!repo.revoke("u1", &row.id, 301, "user").await.unwrap());
    assert!(repo.find_active_by_token_hash("hash-a").await.unwrap().is_none());

    let revoked = repo.find_for_user("u1", &row.id).await.unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(300));
    assert_eq!(revoked.revoked_by.as_deref(), Some("user"));

    // Revoked devices cannot be renamed.
    assert!(!repo.rename("u1", &row.id, "Again").await.unwrap());

    // Touch is a no-op once revoked.
    repo.touch_last_seen(&row.id, 400).await.unwrap();
    assert_eq!(
        repo.find_for_user("u1", &row.id).await.unwrap().unwrap().last_seen_at,
        Some(200)
    );
}

#[tokio::test]
async fn active_device_cap_is_per_user_and_ignores_revoked() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserDeviceRepository::new(db.pool().clone());

    let mut ids = Vec::new();
    for i in 0..5 {
        let hash = format!("h{i}");
        ids.push(
            repo.create_within_limit(&device("u1", &hash, i), 5)
                .await
                .unwrap()
                .unwrap()
                .id,
        );
    }
    assert!(
        repo.create_within_limit(&device("u1", "h5", 9), 5)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(repo.count_active("u1").await.unwrap(), 5);
    // Another user is unaffected.
    assert!(
        repo.create_within_limit(&device("u2", "x0", 9), 5)
            .await
            .unwrap()
            .is_some()
    );

    assert!(repo.revoke("u1", &ids[0], 10, "user").await.unwrap());
    assert!(
        repo.create_within_limit(&device("u1", "h5", 11), 5)
            .await
            .unwrap()
            .is_some()
    );

    let list = repo.list_for_user("u1").await.unwrap();
    assert_eq!(list.len(), 6);
    assert_eq!(list[0].token_hash, "h5");
}

#[tokio::test]
async fn token_hash_is_unique_and_platform_is_checked() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserDeviceRepository::new(db.pool().clone());
    repo.create_within_limit(&device("u1", "dup", 1), 5)
        .await
        .unwrap()
        .unwrap();
    assert!(repo.create_within_limit(&device("u2", "dup", 2), 5).await.is_err());

    let mut bad = device("u1", "other", 3);
    bad.platform = "symbian";
    assert!(repo.create_within_limit(&bad, 5).await.is_err());
}
