#![recursion_limit = "256"]

use std::sync::Arc;

use drua_core::primitives::{AuthSubject, SandboxId, UserId, WorkspaceId};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::skill::Skills;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn build_skills(pool: &sqlx::PgPool) -> Skills {
    let sandboxes = Arc::new(
        Sandboxes::init(pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    Skills::new_without_library(pool, sandboxes)
}

/// Inserts a row into `workspaces` so the `skills.workspace_id` FK can be
/// satisfied without spinning up the full `Workspaces` service stack.
async fn seed_workspace(pool: &sqlx::PgPool, id: WorkspaceId, name_prefix: &str) {
    let unique_name = format!("{name_prefix}-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO workspaces (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(uuid::Uuid::from(id))
        .bind(unique_name)
        .execute(pool)
        .await
        .expect("insert workspace");
}

/// Unattached agents (no sandbox) get only workspace + global DB skills,
/// and the rendered block must not carry the legacy `[sandbox]` tag.
#[tokio::test]
async fn skills_context_unattached_excludes_sandbox_tag() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let sub = AuthSubject::User(UserId::new());
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws, "ws-a").await;

    skills
        .create(
            &sub,
            ws,
            "ws-a",
            "alpha-skill".into(),
            "alpha description".into(),
            "echo alpha".into(),
        )
        .await
        .expect("create skill");

    let context = skills
        .skills_context_for_agent(ws, None)
        .await
        .expect("skills_context_for_agent")
        .expect("expected Some block");

    assert!(
        context.contains("alpha-skill"),
        "workspace skill should appear: {context}"
    );
    assert!(
        !context.contains("[sandbox]"),
        "no sandbox tag in unattached agent block: {context}"
    );
}

/// Two workspaces in the same DB must not bleed skills into each other,
/// and an unknown sandbox id resolves to no exported skills (so it is
/// equivalent to passing `None`).
#[tokio::test]
async fn skills_context_isolates_workspaces_and_unknown_sandbox() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let sub = AuthSubject::User(UserId::new());
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();
    seed_workspace(&pool, ws_a, "ws-a").await;
    seed_workspace(&pool, ws_b, "ws-b").await;

    skills
        .create(
            &sub,
            ws_a,
            "ws-a",
            "alpha-only".into(),
            "alpha-only desc".into(),
            "echo alpha".into(),
        )
        .await
        .expect("create alpha");
    skills
        .create(
            &sub,
            ws_b,
            "ws-b",
            "beta-only".into(),
            "beta-only desc".into(),
            "echo beta".into(),
        )
        .await
        .expect("create beta");

    let ctx_a = skills
        .skills_context_for_agent(ws_a, None)
        .await
        .expect("ws_a context")
        .expect("ws_a Some");
    assert!(ctx_a.contains("alpha-only"));
    assert!(!ctx_a.contains("beta-only"));

    let ctx_b = skills
        .skills_context_for_agent(ws_b, None)
        .await
        .expect("ws_b context")
        .expect("ws_b Some");
    assert!(ctx_b.contains("beta-only"));
    assert!(!ctx_b.contains("alpha-only"));

    // Passing an unknown sandbox id is a no-op for the sandbox-skills source.
    let ctx_a_with_unknown = skills
        .skills_context_for_agent(ws_a, Some(SandboxId::new()))
        .await
        .expect("ctx with unknown sandbox")
        .expect("Some");
    assert_eq!(
        ctx_a, ctx_a_with_unknown,
        "unknown sandbox must not change the rendered block"
    );
}

/// With no DB skills and no real sandbox skills available, the context is
/// `None` regardless of which sandbox id is passed — proving that the
/// per-agent renderer never invents content.
#[tokio::test]
async fn skills_context_none_when_no_skills_anywhere() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let ws = WorkspaceId::new();

    let unattached = skills
        .skills_context_for_agent(ws, None)
        .await
        .expect("call");
    let with_sandbox = skills
        .skills_context_for_agent(ws, Some(SandboxId::new()))
        .await
        .expect("call");
    assert!(unattached.is_none(), "unattached: {unattached:?}");
    assert!(with_sandbox.is_none(), "with-sandbox: {with_sandbox:?}");
}
