use drua_core::audit::{
    Audit, AuditContextData, AuditLogQuery, InteractionOutcome, InteractionType,
    MAX_ERROR_MESSAGE_BYTES,
};
use drua_core::primitives::UserId;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn ctx_with_outcome(action: &str, outcome: InteractionOutcome) -> AuditContextData {
    AuditContextData {
        acting_user_id: Some(UserId::new()),
        interaction_type: Some(InteractionType::McpCall),
        action: Some(action.to_string()),
        entrypoint: Some(format!("mcp: {action}")),
        outcome: Some(outcome),
        ..Default::default()
    }
}

#[tokio::test]
async fn record_failed_call_persists_error_message() {
    let pool = pool().await;
    let audit = Audit::new(&pool);

    let user_id = UserId::new();
    let mut ctx = ctx_with_outcome(
        "test.failing_tool",
        InteractionOutcome::Error {
            message: "upstream MCP timeout after 30s".to_string(),
        },
    );
    ctx.acting_user_id = Some(user_id);

    let entry = audit.record(&ctx).await.expect("record entry");
    assert_eq!(entry.error, Some(true));
    assert_eq!(
        entry.error_message.as_deref(),
        Some("upstream MCP timeout after 30s"),
    );

    // Round-trip via the read path so we cover both insert RETURNING and
    // SELECT projections.
    let rows = audit
        .find(&AuditLogQuery {
            acting_user_id: Some(user_id),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("find entries");
    let found = rows.iter().find(|e| e.id == entry.id).expect("entry found");
    assert_eq!(
        found.error_message.as_deref(),
        Some("upstream MCP timeout after 30s"),
    );
}

#[tokio::test]
async fn record_successful_call_leaves_error_message_null() {
    let pool = pool().await;
    let audit = Audit::new(&pool);

    let user_id = UserId::new();
    let mut ctx = ctx_with_outcome("test.ok_tool", InteractionOutcome::Success);
    ctx.acting_user_id = Some(user_id);

    let entry = audit.record(&ctx).await.expect("record entry");
    assert_eq!(entry.error, Some(false));
    assert!(entry.error_message.is_none());
}

#[tokio::test]
async fn record_truncates_oversized_error_message() {
    let pool = pool().await;
    let audit = Audit::new(&pool);

    let user_id = UserId::new();
    let huge = "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1024);
    let mut ctx = ctx_with_outcome(
        "test.huge_error",
        InteractionOutcome::Error {
            message: huge.clone(),
        },
    );
    ctx.acting_user_id = Some(user_id);

    let entry = audit.record(&ctx).await.expect("record entry");
    let stored = entry.error_message.expect("error message");
    assert_eq!(stored.len(), MAX_ERROR_MESSAGE_BYTES);
    assert!(stored.chars().all(|c| c == 'x'));
}
