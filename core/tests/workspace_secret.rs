use drua_core::encryption::EncryptionKey;
use drua_core::primitives::{AuthSubject, UserId, WorkspaceId};
use drua_core::workspace_secret::{SecretType, WorkspaceSecrets};

fn test_sub() -> AuthSubject {
    AuthSubject::User(UserId::new())
}

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn test_key() -> EncryptionKey {
    EncryptionKey::new([42u8; 32])
}

fn workspace_secrets(pool: &sqlx::PgPool) -> WorkspaceSecrets {
    WorkspaceSecrets::new(pool, test_key())
}

async fn create_workspace(pool: &sqlx::PgPool) -> WorkspaceId {
    let id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind("test-workspace")
        .execute(pool)
        .await
        .expect("insert workspace");
    id
}

#[tokio::test]
async fn create_and_find_by_id() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    let created = svc
        .create(&sub, ws_id, "API_KEY", SecretType::EnvVar, "sk-test-123")
        .await
        .expect("create secret");

    assert_eq!(created.name, "API_KEY");
    assert_eq!(created.secret_type, SecretType::EnvVar);
    assert_eq!(created.workspace_id, ws_id);

    let found = svc.find_by_id(&sub, created.id).await.expect("find by id");
    assert_eq!(found.id, created.id);
    assert_eq!(found.name, "API_KEY");
}

#[tokio::test]
async fn create_and_decrypt_round_trip() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    svc.create(&sub, ws_id, "DB_PASSWORD", SecretType::File, "super-secret")
        .await
        .expect("create secret");

    let decrypted = svc
        .list_decrypted(&sub, ws_id)
        .await
        .expect("list decrypted");

    let entry = decrypted
        .iter()
        .find(|d| d.name == "DB_PASSWORD")
        .expect("should find DB_PASSWORD");
    assert_eq!(entry.value, "super-secret");
    assert_eq!(entry.secret_type, SecretType::File);
}

#[tokio::test]
async fn update_value_changes_decrypted_output() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    let created = svc
        .create(&sub, ws_id, "TOKEN", SecretType::EnvVar, "old-value")
        .await
        .expect("create");

    svc.update_value(&sub, created.id, "new-value")
        .await
        .expect("update value");

    let decrypted = svc
        .list_decrypted(&sub, ws_id)
        .await
        .expect("list decrypted");
    let entry = decrypted
        .iter()
        .find(|d| d.name == "TOKEN")
        .expect("should find TOKEN");
    assert_eq!(entry.value, "new-value");
}

#[tokio::test]
async fn delete_removes_from_listing() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    let secret = svc
        .create(
            &sub,
            ws_id,
            "TEMP_KEY",
            SecretType::EnvVar,
            "will-be-deleted",
        )
        .await
        .expect("create");

    svc.delete(&sub, secret.id).await.expect("delete");

    let listed = svc.list_by_workspace(&sub, ws_id).await.expect("list");
    assert!(
        !listed.iter().any(|s| s.id == secret.id),
        "deleted secret should not appear in listing"
    );
}

#[tokio::test]
async fn list_by_workspace_returns_only_own_secrets() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_a = create_workspace(&pool).await;
    let ws_b = create_workspace(&pool).await;
    let sub = test_sub();

    svc.create(&sub, ws_a, "SECRET_A", SecretType::EnvVar, "a-val")
        .await
        .expect("create A");
    svc.create(&sub, ws_b, "SECRET_B", SecretType::File, "b-val")
        .await
        .expect("create B");

    let listed_a = svc.list_by_workspace(&sub, ws_a).await.expect("list A");
    assert!(listed_a.iter().all(|s| s.workspace_id == ws_a));
    assert!(listed_a.iter().any(|s| s.name == "SECRET_A"));

    let listed_b = svc.list_by_workspace(&sub, ws_b).await.expect("list B");
    assert!(listed_b.iter().all(|s| s.workspace_id == ws_b));
    assert!(listed_b.iter().any(|s| s.name == "SECRET_B"));
}

#[tokio::test]
async fn duplicate_name_in_same_workspace_fails() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    svc.create(&sub, ws_id, "UNIQUE_KEY", SecretType::EnvVar, "first")
        .await
        .expect("create first");

    let result = svc
        .create(&sub, ws_id, "UNIQUE_KEY", SecretType::EnvVar, "second")
        .await;

    assert!(
        result.is_err(),
        "duplicate name in same workspace should fail"
    );
}

#[tokio::test]
async fn same_name_in_different_workspaces_ok() {
    let pool = pool().await;
    let svc = workspace_secrets(&pool);
    let ws_a = create_workspace(&pool).await;
    let ws_b = create_workspace(&pool).await;
    let sub = test_sub();

    svc.create(&sub, ws_a, "SHARED_NAME", SecretType::EnvVar, "val-a")
        .await
        .expect("create in workspace A");

    svc.create(&sub, ws_b, "SHARED_NAME", SecretType::EnvVar, "val-b")
        .await
        .expect("create in workspace B should succeed");
}

#[tokio::test]
async fn wrong_key_fails_decryption() {
    let pool = pool().await;
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    // Create with one key
    let svc = WorkspaceSecrets::new(&pool, EncryptionKey::new([1u8; 32]));
    svc.create(
        &sub,
        ws_id,
        "MISMATCHED",
        SecretType::EnvVar,
        "encrypted-with-key-1",
    )
    .await
    .expect("create");

    // Try to decrypt with a different key
    let svc_wrong_key = WorkspaceSecrets::new(&pool, EncryptionKey::new([2u8; 32]));
    let result = svc_wrong_key.list_decrypted(&sub, ws_id).await;

    assert!(result.is_err(), "decryption with wrong key should fail");
}
