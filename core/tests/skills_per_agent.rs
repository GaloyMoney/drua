#![recursion_limit = "256"]

use std::sync::Arc;

use drua_core::library::SpaceMounts;
use drua_core::primitives::{AgentId, AuthSubject, ProjectId, SandboxId, SpaceId, UserId};
use drua_core::project::repo::ProjectRepo;
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

/// Inserts a row into `projects` so the `skills.project_id` FK is satisfied
/// without spinning up the full `Projects` service stack.
async fn seed_project(pool: &sqlx::PgPool, id: ProjectId, name_prefix: &str) {
    let unique_name = format!("{name_prefix}-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(uuid::Uuid::from(id))
        .bind(unique_name)
        .execute(pool)
        .await
        .expect("insert project");
}

/// Unattached agents (no sandbox) get only project + global DB skills, and
/// the rendered block must not carry the legacy `[sandbox]` tag.
#[tokio::test]
async fn skills_context_unattached_excludes_sandbox_tag() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let sub = AuthSubject::User(UserId::new());
    let project = ProjectId::new();
    seed_project(&pool, project, "proj-a").await;

    skills
        .create(
            &sub,
            project,
            "proj-a",
            "alpha-skill".into(),
            "alpha description".into(),
            "echo alpha".into(),
        )
        .await
        .expect("create skill");

    let context = skills
        .skills_context_for_agent(project, None)
        .await
        .expect("skills_context_for_agent")
        .expect("expected Some block");

    assert!(
        context.contains("alpha-skill"),
        "project skill should appear: {context}"
    );
    assert!(
        !context.contains("[sandbox]"),
        "no sandbox tag in unattached agent block: {context}"
    );
}

/// Two projects in the same DB must not bleed skills into each other, and
/// an unknown sandbox id resolves to no exported skills (so it is
/// equivalent to passing `None`).
#[tokio::test]
async fn skills_context_isolates_projects_and_unknown_sandbox() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let sub = AuthSubject::User(UserId::new());
    let proj_a = ProjectId::new();
    let proj_b = ProjectId::new();
    seed_project(&pool, proj_a, "proj-a").await;
    seed_project(&pool, proj_b, "proj-b").await;

    skills
        .create(
            &sub,
            proj_a,
            "proj-a",
            "alpha-only".into(),
            "alpha-only desc".into(),
            "echo alpha".into(),
        )
        .await
        .expect("create alpha");
    skills
        .create(
            &sub,
            proj_b,
            "proj-b",
            "beta-only".into(),
            "beta-only desc".into(),
            "echo beta".into(),
        )
        .await
        .expect("create beta");

    let ctx_a = skills
        .skills_context_for_agent(proj_a, None)
        .await
        .expect("proj_a context")
        .expect("proj_a Some");
    assert!(ctx_a.contains("alpha-only"));
    assert!(!ctx_a.contains("beta-only"));

    let ctx_b = skills
        .skills_context_for_agent(proj_b, None)
        .await
        .expect("proj_b context")
        .expect("proj_b Some");
    assert!(ctx_b.contains("beta-only"));
    assert!(!ctx_b.contains("alpha-only"));

    // Passing an unknown sandbox id is a no-op for the sandbox-skills source.
    let ctx_a_with_unknown = skills
        .skills_context_for_agent(proj_a, Some(SandboxId::new()))
        .await
        .expect("ctx with unknown sandbox")
        .expect("Some");
    assert_eq!(
        ctx_a, ctx_a_with_unknown,
        "unknown sandbox must not change the rendered block"
    );
}

/// Seeds a project with `mounted_spaces` containing `space_id` by writing
/// the canonical `Initialized` + `SpaceMounted` events directly. Avoids
/// standing up the full `Projects` service stack — `find_by_name` only
/// needs the entity to hydrate with the right `mounted_spaces` field.
async fn seed_project_with_mount(
    pool: &sqlx::PgPool,
    project: ProjectId,
    space: SpaceId,
    name: &str,
) {
    let lead_agent_id = AgentId::new();
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(uuid::Uuid::from(project))
        .bind(name)
        .execute(pool)
        .await
        .expect("insert project");
    let init_event = serde_json::json!({
        "type": "initialized",
        "id": uuid::Uuid::from(project),
        "lead_agent_id": uuid::Uuid::from(lead_agent_id),
        "name": name,
        "description": null,
    });
    let mount_event = serde_json::json!({
        "type": "space_mounted",
        "space_id": uuid::Uuid::from(space),
    });
    sqlx::query(
        "INSERT INTO project_events (id, sequence, event_type, event, recorded_at) \
         VALUES ($1, 0, 'initialized', $2::jsonb, NOW()), \
                ($1, 1, 'space_mounted', $3::jsonb, NOW())",
    )
    .bind(uuid::Uuid::from(project))
    .bind(init_event)
    .bind(mount_event)
    .execute(pool)
    .await
    .expect("insert project events");
}

/// `find_by_name` resolves a space-scoped skill when the agent's project
/// has the space mounted. Regression test for cursor finding
/// `r3185924186` — agents could see `[space]`-tagged skills in the
/// `<available_skills>` block but invocation via `use_skill` failed
/// because `find_by_name` ignored space-scoped candidates.
#[tokio::test]
async fn find_by_name_resolves_mounted_space_skill() {
    let pool = pool().await;
    let sandboxes = Arc::new(
        Sandboxes::init(&pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    let project = ProjectId::new();
    let space = SpaceId::from(uuid::Uuid::new_v4());
    // Names/slugs must be unique per run against a persisted DB
    // (re-runs and parallel suites collide otherwise).
    let project_name = format!("proj-mount-{}", uuid::Uuid::from(project));
    // Seed the spaces row so the `skills.space_id` FK is satisfied. The
    // test never reads the entity itself — `find_by_name` only needs
    // the id to be present in `Project.mounted_spaces`.
    sqlx::query("INSERT INTO spaces (id, slug, created_at) VALUES ($1, $2, NOW())")
        .bind(uuid::Uuid::from(space))
        .bind(format!("test-space-{}", uuid::Uuid::from(space)))
        .execute(&pool)
        .await
        .expect("insert space row");
    seed_project_with_mount(&pool, project, space, &project_name).await;

    // Insert the skill row directly (no library wired). One Initialized
    // event with `space_id` set. Skill name carries a per-run suffix
    // because `idx_skills_global_name` enforces global uniqueness and
    // sibling tests in this suite use bare `alpha-skill`.
    let skill_id = uuid::Uuid::new_v4();
    let skill_name = format!("alpha-mount-{}", &skill_id.to_string()[..8]);
    let init = serde_json::json!({
        "type": "initialized",
        "id": skill_id,
        "project_id": null,
        "project_name": null,
        "space_id": uuid::Uuid::from(space),
        "space_slug": "test-space",
        "name": skill_name,
        "description": "alpha description",
        "body": "echo alpha",
    });
    sqlx::query("INSERT INTO skills (id, project_id, space_id, name, created_at) VALUES ($1, NULL, $2, $3, NOW())")
        .bind(skill_id)
        .bind(uuid::Uuid::from(space))
        .bind(&skill_name)
        .execute(&pool)
        .await
        .expect("insert skill row");
    sqlx::query("INSERT INTO skill_events (id, sequence, event_type, event, recorded_at) VALUES ($1, 0, 'initialized', $2::jsonb, NOW())")
        .bind(skill_id)
        .bind(init)
        .execute(&pool)
        .await
        .expect("insert skill event");

    // Skills with SpaceMounts wired against the same pool — same Project
    // entity hydration path as production, so the mount is visible.
    let project_repo = Arc::new(ProjectRepo::new(&pool));
    // SpaceMounts only consults `spaces` for `spaces_for_project` /
    // `spaces_block_for_project`; `space_ids_for_project` (the path
    // `find_by_name` uses) only needs `project_repo`. We don't have an
    // easy way to construct a real `AuthedSpaces` in this test, so go
    // through `SpaceMounts::empty` + a workaround: use `with_space_mounts`
    // on a `Skills` built with `new_without_library`, paired with a
    // dedicated test-only `SpaceMounts` constructor. Simpler: build
    // SpaceMounts via the public API by passing dummy spaces (skill
    // resolution doesn't touch it).
    let space_mounts = Arc::new(SpaceMounts::with_project_repo_for_test(project_repo));
    let skills = Skills::new_without_library(&pool, sandboxes).with_space_mounts(space_mounts);

    let body = skills
        .find_by_name(&skill_name, Some(project), None)
        .await
        .expect("find_by_name")
        .expect("space-scoped skill should be found via mounted space");
    let rendered: String = body.into();
    assert!(
        rendered.contains("echo alpha"),
        "expected space-skill body, got: {rendered}"
    );
}

/// With no DB skills and no real sandbox skills available, the context is
/// `None` regardless of which sandbox id is passed — proving that the
/// per-agent renderer never invents content.
#[tokio::test]
async fn skills_context_none_when_no_skills_anywhere() {
    let pool = pool().await;
    let skills = build_skills(&pool).await;
    let project = ProjectId::new();

    let unattached = skills
        .skills_context_for_agent(project, None)
        .await
        .expect("call");
    let with_sandbox = skills
        .skills_context_for_agent(project, Some(SandboxId::new()))
        .await
        .expect("call");
    assert!(unattached.is_none(), "unattached: {unattached:?}");
    assert!(with_sandbox.is_none(), "with-sandbox: {with_sandbox:?}");
}
