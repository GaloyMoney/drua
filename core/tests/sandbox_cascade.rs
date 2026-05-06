use std::sync::Arc;

use drua_core::primitives::ProjectId;
use drua_core::sandbox::{SandboxConfig, SandboxMode, SandboxSpecs, Sandboxes};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn insert_project(pool: &sqlx::PgPool) -> ProjectId {
    let id = ProjectId::new();
    let name = format!("test-project-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&name)
        .execute(pool)
        .await
        .expect("insert project");
    id
}

fn specs() -> SandboxSpecs {
    SandboxSpecs {
        cpu: "100m".into(),
        memory: "128Mi".into(),
        disk_size: "1Gi".into(),
    }
}

async fn build_sandboxes(pool: &sqlx::PgPool) -> Arc<Sandboxes> {
    Arc::new(
        Sandboxes::init(pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    )
}

/// Bulk soft-delete + admin teardown both fire and cover every sandbox
/// in the project — the audit's H2/H3 coverage gap that the old
/// `suspend_for_project_in_op` capped at 100 rows.
#[tokio::test]
async fn cascade_delete_marks_every_project_sandbox_deleted() {
    let pool = pool().await;
    let sandboxes = build_sandboxes(&pool).await;
    let project_id = insert_project(&pool).await;

    let mut op = sandboxes.begin_op().await.expect("begin op");
    let sb_a = sandboxes
        .create_in_op(&mut op, project_id, "a", specs(), SandboxMode::Scratch)
        .await
        .expect("create a");
    let sb_b = sandboxes
        .create_in_op(&mut op, project_id, "b", specs(), SandboxMode::Scratch)
        .await
        .expect("create b");
    op.commit().await.expect("commit creates");

    let mut op = sandboxes.begin_op().await.expect("begin op");
    sandboxes
        .cascade_delete_for_project_in_op(&mut op, project_id)
        .await
        .expect("cascade");
    op.commit().await.expect("commit cascade");

    for id in [sb_a.id, sb_b.id] {
        let row: (bool,) = sqlx::query_as("SELECT deleted FROM sandboxes WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch deleted flag");
        assert!(row.0, "sandbox {id} should be soft-deleted");
    }

    // Soft-deleted rows must no longer surface in service-level reads.
    assert!(
        sandboxes
            .maybe_find_by_id(sb_a.id)
            .await
            .expect("lookup")
            .is_none(),
        "soft-deleted sandbox should not be findable"
    );
}

#[tokio::test]
async fn cascade_delete_paginates_past_first_100() {
    let pool = pool().await;
    let sandboxes = build_sandboxes(&pool).await;
    let project_id = insert_project(&pool).await;

    let mut op = sandboxes.begin_op().await.expect("begin op");
    let mut ids = Vec::with_capacity(105);
    for i in 0..105 {
        let sb = sandboxes
            .create_in_op(
                &mut op,
                project_id,
                format!("sb-{i:03}"),
                specs(),
                SandboxMode::Scratch,
            )
            .await
            .expect("create");
        ids.push(sb.id);
    }
    op.commit().await.expect("commit creates");

    let mut op = sandboxes.begin_op().await.expect("begin op");
    sandboxes
        .cascade_delete_for_project_in_op(&mut op, project_id)
        .await
        .expect("cascade");
    op.commit().await.expect("commit cascade");

    let undeleted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sandboxes WHERE project_id = $1 AND deleted = FALSE",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        undeleted, 0,
        "cascade should soft-delete every sandbox row, not just the first 100"
    );
}

#[tokio::test]
async fn cascade_delete_is_noop_for_other_projects() {
    let pool = pool().await;
    let sandboxes = build_sandboxes(&pool).await;
    let project_a = insert_project(&pool).await;
    let project_b = insert_project(&pool).await;

    let mut op = sandboxes.begin_op().await.expect("begin op");
    let sb_a = sandboxes
        .create_in_op(&mut op, project_a, "a", specs(), SandboxMode::Scratch)
        .await
        .expect("create a");
    let sb_b = sandboxes
        .create_in_op(&mut op, project_b, "b", specs(), SandboxMode::Scratch)
        .await
        .expect("create b");
    op.commit().await.expect("commit creates");

    let mut op = sandboxes.begin_op().await.expect("begin op");
    sandboxes
        .cascade_delete_for_project_in_op(&mut op, project_a)
        .await
        .expect("cascade");
    op.commit().await.expect("commit cascade");

    let row_a: (bool,) = sqlx::query_as("SELECT deleted FROM sandboxes WHERE id = $1")
        .bind(sb_a.id)
        .fetch_one(&pool)
        .await
        .expect("fetch a");
    let row_b: (bool,) = sqlx::query_as("SELECT deleted FROM sandboxes WHERE id = $1")
        .bind(sb_b.id)
        .fetch_one(&pool)
        .await
        .expect("fetch b");
    assert!(row_a.0, "project A sandbox should be deleted");
    assert!(!row_b.0, "project B sandbox should be untouched");
}
