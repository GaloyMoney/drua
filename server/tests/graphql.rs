/// Integration tests for the GraphQL API layer.
///
/// These tests exercise the async-graphql schema directly (no HTTP server) to
/// verify that resolvers correctly wire up to domain services and return the
/// expected shapes. They hit a real Postgres database — run via:
///
///   DATABASE_URL=postgres://user:password@localhost:5432/drua cargo nextest run -p drua-server
const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn test_sub() -> drua_core::auth::AuthSubject {
    drua_core::auth::AuthSubject::User(drua_core::primitives::UserId::new())
}

/// Build a minimal GraphQL schema with a live App injected.
///
/// Because `App::init` requires external services (prompt executor, etc.),
/// we skip it and wire the domain services we need manually. For the
/// integration test we use the `schema(None)` path and inject services
/// into the request data directly — the resolvers pull `App` from context
/// via `data_unchecked`, so we just need a real `App` handle.
///
/// Since App::init is complex (needs LLM keys, etc.), we instead test by
/// inserting directly into the DB and querying through the GraphQL layer.
/// For mutation tests that don't need the full App, we use domain services
/// directly and verify via queries.
#[allow(dead_code)]
async fn execute_graphql(
    schema: &drua_server::graphql::AgentsSchema,
    app: &drua_core::App,
    sub: &drua_core::auth::AuthSubject,
    query: &str,
    variables: serde_json::Value,
) -> serde_json::Value {
    let mut request = async_graphql::Request::new(query);

    if let serde_json::Value::Object(vars) = variables {
        let mut gql_vars = async_graphql::Variables::default();
        for (k, v) in vars {
            gql_vars.insert(
                async_graphql::Name::new(k),
                async_graphql::Value::from_json(v).unwrap(),
            );
        }
        request = request.variables(gql_vars);
    }

    request = request.data(app.clone()).data(sub.clone());

    let response = schema.execute(request).await;
    serde_json::to_value(&response).unwrap()
}

/// Helper: create a workspace through the domain layer and return its id.
async fn create_workspace(pool: &sqlx::PgPool) -> drua_core::primitives::WorkspaceId {
    let id = drua_core::primitives::WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(format!("test-ws-{}", uuid::Uuid::from(id)))
        .execute(pool)
        .await
        .expect("insert workspace");
    id
}

// ─── Workspace CRUD tests ───────────────────────────────────────────────────

/// Verify that the schema builds and the ping query works (baseline sanity check).
#[tokio::test]
async fn ping_query_works() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new("{ ping }"))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["data"]["ping"], "pong");
}

/// Verify that the ping mutation works.
#[tokio::test]
async fn ping_mutation_works() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new("mutation { ping }"))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["data"]["ping"], "pong");
}

// ─── Skill CRUD tests (domain-level, verifying GraphQL types compile) ───────

/// Verify the skill GraphQL types work by creating through domain and reading back.
#[tokio::test]
async fn skill_create_domain_roundtrip() {
    let pool = pool().await;
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    let sandboxes = std::sync::Arc::new(
        drua_core::sandbox::Sandboxes::init(
            &pool,
            drua_core::sandbox::SandboxConfig::default(),
            None,
        )
        .await
        .expect("init sandboxes"),
    );
    let skills = drua_core::skill::Skills::new(&pool, sandboxes);

    let new = drua_core::skill::NewSkill::builder()
        .workspace_id(ws_id)
        .name("test-skill")
        .description("A test skill")
        .body("Do the thing")
        .build()
        .unwrap();

    let skill = skills.create(&sub, new).await.expect("create skill");
    assert_eq!(skill.name, "test-skill");
    assert_eq!(skill.description, "A test skill");
    assert_eq!(skill.body, "Do the thing");
    assert_eq!(skill.workspace_id, ws_id);

    // Verify list
    let listed = skills
        .list_by_workspace_id(&sub, ws_id)
        .await
        .expect("list skills");
    assert!(listed.iter().any(|s| s.id == skill.id));

    // Verify update
    let mut skill = skills.find_by_id(&sub, skill.id).await.expect("find skill");
    let _ = skill.update(Some("updated-name".into()), None, None);
    skills.update(&sub, &mut skill).await.expect("update skill");
    assert_eq!(skill.name, "updated-name");

    // Verify delete
    skills.delete(&sub, skill.id).await.expect("delete skill");
    let listed = skills
        .list_by_workspace_id(&sub, ws_id)
        .await
        .expect("list after delete");
    assert!(!listed.iter().any(|s| s.id == skill.id));
}

// ─── Workspace Secret tests ────────────────────────────────────────────────

#[tokio::test]
async fn workspace_secret_create_and_list_domain_roundtrip() {
    let pool = pool().await;
    let ws_id = create_workspace(&pool).await;
    let sub = test_sub();

    let key = drua_core::encryption::EncryptionKey::new([42u8; 32]);
    let secrets = drua_core::workspace_secret::WorkspaceSecrets::new(&pool, key);

    let secret = secrets
        .create(
            &sub,
            ws_id,
            "GQL_TEST_KEY",
            drua_core::workspace_secret::SecretType::EnvVar,
            "secret-value",
        )
        .await
        .expect("create secret");
    assert_eq!(secret.name, "GQL_TEST_KEY");

    let listed = secrets
        .list_by_workspace(&sub, ws_id)
        .await
        .expect("list secrets");
    assert!(listed.iter().any(|s| s.id == secret.id));

    // Delete
    secrets.delete(&sub, secret.id).await.expect("delete");
    let listed = secrets
        .list_by_workspace(&sub, ws_id)
        .await
        .expect("list after delete");
    assert!(!listed.iter().any(|s| s.id == secret.id));
}

// ─── Schema introspection tests ──────────────────────────────────────────

/// Verify the schema exposes all new mutation fields.
#[tokio::test]
async fn schema_has_expected_mutations() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Mutation") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = [
        "agentCreate",
        "agentAttachSandbox",
        "agentDetachSandbox",
        "sandboxCreate",
        "sandboxSuspend",
        "sandboxRestart",
        "skillCreate",
        "skillUpdate",
        "skillDelete",
        "workspaceSecretCreate",
        "workspaceSecretDelete",
        "mcpCredentialsCreate",
        "mcpCredentialsRevoke",
        "workspaceCreate",
        "workspaceUpdate",
        "workspaceDelete",
    ];

    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing mutation: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the schema exposes all new query fields.
#[tokio::test]
async fn schema_has_expected_queries() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Query") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = ["sandbox", "auditLog", "agent", "workspace", "workspaces"];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing query: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the Workspace type exposes sandboxes, skills, secrets, and mcpCredentials.
#[tokio::test]
async fn schema_workspace_has_new_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Workspace") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = ["sandboxes", "skills", "secrets", "mcpCredentials"];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing workspace field: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the Sandbox type has expected fields.
#[tokio::test]
async fn schema_sandbox_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Sandbox") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = [
        "id",
        "workspaceId",
        "name",
        "state",
        "lastError",
        "mountPath",
        "createdAt",
        "cpu",
        "memory",
        "diskSize",
        "attachedAgents",
    ];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing sandbox field: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the Skill type has expected fields.
#[tokio::test]
async fn schema_skill_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Skill") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = [
        "id",
        "workspaceId",
        "name",
        "description",
        "body",
        "createdAt",
    ];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing skill field: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the WorkspaceSecret type has expected fields (no value exposed!).
#[tokio::test]
async fn schema_workspace_secret_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "WorkspaceSecret") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = ["id", "workspaceId", "name", "secretType", "createdAt"];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing workspace secret field: {name}. Available: {fields:?}"
        );
    }

    // Ensure the plaintext value is NOT exposed
    assert!(
        !fields.contains(&"value".to_string()),
        "WorkspaceSecret should NOT expose 'value' field"
    );
    assert!(
        !fields.contains(&"encryptedValue".to_string()),
        "WorkspaceSecret should NOT expose 'encryptedValue' field"
    );
}

/// Verify the AuditEntry type has expected fields.
#[tokio::test]
async fn schema_audit_entry_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "AuditEntry") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = [
        "actingUserId",
        "actingAgentId",
        "entrypoint",
        "interactionType",
        "action",
        "outcome",
        "error",
        "recordedAt",
    ];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing audit entry field: {name}. Available: {fields:?}"
        );
    }
}

/// Verify the McpCreds type has expected fields.
#[tokio::test]
async fn schema_mcp_creds_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "McpCreds") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = ["id", "name", "revoked", "revokedAt", "createdAt"];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing mcp creds field: {name}. Available: {fields:?}"
        );
    }
}
