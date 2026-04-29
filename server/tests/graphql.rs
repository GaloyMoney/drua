/// Integration tests for the GraphQL API layer.
///
/// These tests exercise the async-graphql schema directly (no HTTP server) to
/// verify that resolvers correctly wire up to domain services and return the
/// expected shapes. They hit a real Postgres database — run via:
///
///   DATABASE_URL=postgres://user:password@localhost:5432/drua cargo nextest run -p drua-server
use std::collections::HashMap;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

fn test_sub() -> drua_core::auth::AuthSubject {
    drua_core::auth::AuthSubject::User(drua_core::primitives::UserId::new())
}

/// Build a minimal App suitable for GraphQL integration tests.
///
/// Uses default configs with the minimum required agent role setup.
/// No real LLM keys needed — prompt executor boots with empty models.
async fn test_app(pool: &sqlx::PgPool) -> drua_core::App {
    use drua_core::agent::{AgentsConfig, ModelDefaults, RoleConfig};

    let model_name = "test-model".to_string();
    let mut builtin_roles = HashMap::new();
    for role in [
        drua_core::agent::AgentRole::WorkspaceLead,
        drua_core::agent::AgentRole::Agent,
    ] {
        builtin_roles.insert(
            role,
            RoleConfig {
                model: model_name.clone(),
                compaction: Default::default(),
            },
        );
    }
    let mut models = HashMap::new();
    models.insert(
        model_name.clone(),
        ModelDefaults {
            model: model_name,
            max_tokens_per_response: 1024,
            context_window_tokens: 200_000,
        },
    );

    let config = drua_core::AppConfig {
        agents: AgentsConfig {
            builtin_roles,
            models,
        },
        ..Default::default()
    };

    drua_core::App::init(pool, config)
        .await
        .expect("init test app")
}

/// Execute a GraphQL query/mutation against a schema with App + AuthSubject injected.
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

/// Helper: assert no GraphQL errors in the response and return the data object.
fn assert_no_errors(result: &serde_json::Value) -> &serde_json::Value {
    if let Some(errors) = result.get("errors") {
        panic!(
            "GraphQL errors: {}",
            serde_json::to_string_pretty(errors).unwrap()
        );
    }
    result.get("data").expect("response should have 'data'")
}

// ─── Baseline tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn ping_query_works() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new("{ ping }"))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["data"]["ping"], "pong");
}

#[tokio::test]
async fn ping_mutation_works() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new("mutation { ping }"))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["data"]["ping"], "pong");
}

// ─── Workspace mutation tests ───────────────────────────────────────────────

#[tokio::test]
async fn workspace_create_update_delete_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create
    let ws_name = format!("gql-ws-test-{}", uuid::Uuid::new_v4());
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) { workspace { id name description } }
        }"#,
        serde_json::json!({ "input": { "name": ws_name, "description": "test workspace" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let ws = &data["workspaceCreate"]["workspace"];
    assert_eq!(ws["name"], ws_name);
    assert_eq!(ws["description"], "test workspace");
    let ws_id = ws["id"].as_str().unwrap().to_string();

    // Update
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceUpdateInput!) {
            workspaceUpdate(input: $input) { workspace { id name description } }
        }"#,
        serde_json::json!({ "input": { "id": ws_id, "description": "updated" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(
        data["workspaceUpdate"]["workspace"]["description"],
        "updated"
    );

    // Delete
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceDeleteInput!) {
            workspaceDelete(input: $input) { workspace { id } }
        }"#,
        serde_json::json!({ "input": { "id": ws_id } }),
    )
    .await;
    assert_no_errors(&result);

    // Verify workspace is gone
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: WorkspaceId!) { workspace(id: $id) { id } }"#,
        serde_json::json!({ "id": ws_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert!(
        data["workspace"].is_null(),
        "deleted workspace should return null"
    );
}

// ─── Workspace Secret mutation tests ────────────────────────────────────────

#[tokio::test]
async fn workspace_secret_create_delete_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create workspace
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) { workspace { id } }
        }"#,
        serde_json::json!({ "input": { "name": format!("secret-test-ws-{}", uuid::Uuid::new_v4()) } }),
    )
    .await;
    let ws_id = assert_no_errors(&result)["workspaceCreate"]["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create secret
    let result = execute_graphql(
        &schema, &app, &sub,
        r#"mutation($input: WorkspaceSecretCreateInput!) {
            workspaceSecretCreate(input: $input) { workspaceSecret { id name secretType workspaceId } }
        }"#,
        serde_json::json!({ "input": {
            "workspaceId": ws_id,
            "name": "TEST_API_KEY",
            "secretType": "ENV_VAR",
            "value": "sk-secret-123"
        }}),
    ).await;
    let data = assert_no_errors(&result);
    let secret = &data["workspaceSecretCreate"]["workspaceSecret"];
    assert_eq!(secret["name"], "TEST_API_KEY");
    assert_eq!(secret["secretType"], "ENV_VAR");
    let secret_id = secret["id"].as_str().unwrap().to_string();

    // Verify secret appears in workspace secrets list
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: WorkspaceId!) { workspace(id: $id) { secrets { id name secretType } } }"#,
        serde_json::json!({ "id": ws_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let secrets = data["workspace"]["secrets"].as_array().unwrap();
    assert!(secrets.iter().any(|s| s["name"] == "TEST_API_KEY"));
    // Ensure no 'value' field is exposed
    assert!(secrets.iter().all(|s| s.get("value").is_none()));

    // Delete secret
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceSecretDeleteInput!) {
            workspaceSecretDelete(input: $input) { deletedId }
        }"#,
        serde_json::json!({ "input": { "id": secret_id } }),
    )
    .await;
    assert_no_errors(&result);

    // Verify secret is gone
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: WorkspaceId!) { workspace(id: $id) { secrets { id } } }"#,
        serde_json::json!({ "id": ws_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let secrets = data["workspace"]["secrets"].as_array().unwrap();
    assert!(!secrets.iter().any(|s| s["id"].as_str() == Some(&secret_id)));

    // Cleanup
    let _ = execute_graphql(
        &schema, &app, &sub,
        r#"mutation($input: WorkspaceDeleteInput!) { workspaceDelete(input: $input) { workspace { id } } }"#,
        serde_json::json!({ "input": { "id": ws_id } }),
    ).await;
}

// ─── MCP Credentials mutation tests ─────────────────────────────────────────

#[tokio::test]
async fn mcp_credentials_create_revoke_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create credentials
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: McpCredentialsCreateInput!) {
            mcpCredentialsCreate(input: $input) { mcpCreds { id name revoked } token }
        }"#,
        serde_json::json!({ "input": { "name": "test-creds", "scopes": [] } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let payload = &data["mcpCredentialsCreate"];
    assert_eq!(payload["mcpCreds"]["name"], "test-creds");
    assert_eq!(payload["mcpCreds"]["revoked"], false);
    assert!(
        payload["token"].as_str().unwrap().len() > 10,
        "token should be non-trivial"
    );
    let creds_id = payload["mcpCreds"]["id"].as_str().unwrap().to_string();

    // Revoke credentials
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: McpCredentialsRevokeInput!) {
            mcpCredentialsRevoke(input: $input) { mcpCreds { id revoked revokedAt } }
        }"#,
        serde_json::json!({ "input": { "id": creds_id } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let creds = &data["mcpCredentialsRevoke"]["mcpCreds"];
    assert_eq!(creds["revoked"], true);
    assert!(
        creds["revokedAt"].as_str().is_some(),
        "revokedAt should be set"
    );
}

// ─── Agent mutation tests ───────────────────────────────────────────────────

#[tokio::test]
async fn agent_create_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create workspace
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) { workspace { id } }
        }"#,
        serde_json::json!({ "input": { "name": format!("agent-test-ws-{}", uuid::Uuid::new_v4()) } }),
    )
    .await;
    let ws_id = assert_no_errors(&result)["workspaceCreate"]["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create agent
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: AgentCreateInput!) {
            agentCreate(input: $input) { agent { id name role workspaceId } }
        }"#,
        serde_json::json!({ "input": { "workspaceId": ws_id, "name": "research" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let agent = &data["agentCreate"]["agent"];
    assert_eq!(agent["name"], "research");
    assert_eq!(agent["role"], "AGENT");
    assert_eq!(agent["workspaceId"], ws_id);

    // Verify agent appears in workspace agents list
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: WorkspaceId!) { workspace(id: $id) { agents { id name role } } }"#,
        serde_json::json!({ "id": ws_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let agents = data["workspace"]["agents"].as_array().unwrap();
    assert!(agents.iter().any(|a| a["name"] == "research"));
    // Lead should be first
    assert_eq!(agents[0]["role"], "WORKSPACE_LEAD");

    // Cleanup
    let _ = execute_graphql(
        &schema, &app, &sub,
        r#"mutation($input: WorkspaceDeleteInput!) { workspaceDelete(input: $input) { workspace { id } } }"#,
        serde_json::json!({ "input": { "id": ws_id } }),
    ).await;
}

// ─── Sandbox query test ─────────────────────────────────────────────────────

#[tokio::test]
async fn sandbox_query_returns_null_for_missing() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: SandboxId!) { sandbox(id: $id) { id } }"#,
        serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert!(data["sandbox"].is_null());
}

// ─── Library search query tests ────────────────────────────────────────────

#[tokio::test]
async fn library_search_returns_empty_for_empty_query_state() {
    // Even with no library data, the query should resolve cleanly
    // and respect filter shapes (no errors).
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // No filters
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id title type workspaceId snippet score }
        }"#,
        serde_json::json!({ "input": { "query": "anything", "limit": 10 } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert!(data["librarySearch"].is_array());

    // Type filter only
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id type }
        }"#,
        serde_json::json!({ "input": { "query": "x", "types": ["SKILL", "NOTE"], "limit": 10 } }),
    )
    .await;
    assert_no_errors(&result);

    // Workspace filter — workspace doesn't exist for this user, so empty
    let bogus_ws = "00000000-0000-0000-0000-000000000000";
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": { "query": "x", "workspaceId": bogus_ws, "limit": 10 } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(data["librarySearch"].as_array().unwrap().len(), 0);

    // Both filters combined
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": {
            "query": "x",
            "types": ["WORKFLOW"],
            "workspaceId": bogus_ws,
            "limit": 10
        }}),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(data["librarySearch"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn library_search_anonymous_is_unauthorized() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let anon = drua_core::auth::AuthSubject::Anonymous;

    let result = execute_graphql(
        &schema,
        &app,
        &anon,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": { "query": "anything", "limit": 5 } }),
    )
    .await;
    assert!(
        result.get("errors").is_some(),
        "anonymous subject should be rejected: {result:#}"
    );
}

// ─── Schema introspection tests ─────────────────────────────────────────────

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

    let expected = [
        "sandbox",
        "auditLog",
        "agent",
        "workspace",
        "workspaces",
        "librarySearch",
    ];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing query: {name}. Available: {fields:?}"
        );
    }
}

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
    assert!(
        !fields.contains(&"value".to_string()),
        "should NOT expose 'value'"
    );
    assert!(
        !fields.contains(&"encryptedValue".to_string()),
        "should NOT expose 'encryptedValue'"
    );
}

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
