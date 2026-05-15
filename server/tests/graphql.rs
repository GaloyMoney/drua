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

/// Initialise a throwaway bare git repo under the test scratch dir and
/// return its path. `Library::init` requires a non-empty `repo_url`
/// and reachable upstream — this gives both without standing up a
/// real remote.
fn scratch_bare_repo() -> std::path::PathBuf {
    let scratch = std::env::var("CARGO_TARGET_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("drua-server-tests"));
    let stamp = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        uuid::Uuid::new_v4(),
    );
    let upstream = scratch.join(format!("graphql-upstream-{stamp}.git"));
    std::fs::create_dir_all(&upstream).expect("mkdir upstream");
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "--quiet", "--initial-branch=main"])
        .current_dir(&upstream)
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init --bare failed");

    // libgit2 won't clone an empty bare repo; seed it via a sibling
    // working clone with one commit, then push.
    let work = scratch.join(format!("graphql-work-{stamp}"));
    std::fs::create_dir_all(&work).expect("mkdir work");
    for args in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["remote", "add", "origin", &upstream.to_string_lossy()],
        vec!["commit", "--quiet", "--allow-empty", "-m", "init"],
        vec!["push", "--quiet", "-u", "origin", "main"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&work)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }
    upstream
}

/// Build a minimal App suitable for GraphQL integration tests.
///
/// Uses default configs with the minimum required agent role setup
/// plus a throwaway local bare repo so `Library::init` succeeds.
/// No real LLM keys needed — prompt executor boots with empty models.
async fn test_app(pool: &sqlx::PgPool) -> drua_core::App {
    use drua_core::agent::{AgentsConfig, ModelDefaults, RoleConfig};

    let model_name = "test-model".to_string();
    let mut builtin_roles = HashMap::new();
    for role in [
        drua_core::agent::AgentRole::ProjectLead,
        drua_core::agent::AgentRole::Agent,
        drua_core::agent::AgentRole::WorkflowStepAgent,
    ] {
        builtin_roles.insert(
            role,
            RoleConfig {
                chain: Some(drua_core::ModelChain::new(model_name.clone())),
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

    let upstream = scratch_bare_repo();
    let library_data_dir = upstream
        .parent()
        .map(|p| p.join(format!("library-{}", uuid::Uuid::new_v4())))
        .map(|p| p.to_string_lossy().into_owned());
    let library = drua_core::library::LibraryConfig {
        data_dir: library_data_dir,
        repo_url: Some(upstream.to_string_lossy().into_owned()),
        ..Default::default()
    };

    let config = drua_core::AppConfig {
        agents: AgentsConfig {
            builtin_roles,
            models,
            ..Default::default()
        },
        library,
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

// ─── Project mutation tests ───────────────────────────────────────────────

#[tokio::test]
async fn project_create_update_delete_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create
    let ws_name = format!("gql-project-test-{}", uuid::Uuid::new_v4());
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectCreateInput!) {
            projectCreate(input: $input) { project { id name description } }
        }"#,
        serde_json::json!({ "input": { "name": ws_name, "description": "test project" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let project = &data["projectCreate"]["project"];
    assert_eq!(project["name"], ws_name);
    assert_eq!(project["description"], "test project");
    let project_id = project["id"].as_str().unwrap().to_string();

    // Update
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectUpdateInput!) {
            projectUpdate(input: $input) { project { id name description } }
        }"#,
        serde_json::json!({ "input": { "id": project_id, "description": "updated" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(data["projectUpdate"]["project"]["description"], "updated");

    // Delete
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectDeleteInput!) {
            projectDelete(input: $input) { project { id } }
        }"#,
        serde_json::json!({ "input": { "id": project_id } }),
    )
    .await;
    assert_no_errors(&result);

    // Verify project is gone
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: ProjectId!) { project(id: $id) { id } }"#,
        serde_json::json!({ "id": project_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert!(
        data["project"].is_null(),
        "deleted project should return null"
    );
}

// ─── Project Secret mutation tests ────────────────────────────────────────

#[tokio::test]
async fn project_secret_create_delete_via_graphql() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    // Create project
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectCreateInput!) {
            projectCreate(input: $input) { project { id } }
        }"#,
        serde_json::json!({ "input": { "name": format!("secret-test-project-{}", uuid::Uuid::new_v4()) } }),
    )
    .await;
    let project_id = assert_no_errors(&result)["projectCreate"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create secret
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectSecretCreateInput!) {
            projectSecretCreate(input: $input) { projectSecret { id name secretType projectId } }
        }"#,
        serde_json::json!({ "input": {
            "projectId": project_id,
            "name": "TEST_API_KEY",
            "secretType": "ENV_VAR",
            "value": "sk-secret-123"
        }}),
    )
    .await;
    let data = assert_no_errors(&result);
    let secret = &data["projectSecretCreate"]["projectSecret"];
    assert_eq!(secret["name"], "TEST_API_KEY");
    assert_eq!(secret["secretType"], "ENV_VAR");
    let secret_id = secret["id"].as_str().unwrap().to_string();

    // Verify secret appears in project secrets list
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: ProjectId!) { project(id: $id) { secrets { id name secretType } } }"#,
        serde_json::json!({ "id": project_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let secrets = data["project"]["secrets"].as_array().unwrap();
    assert!(secrets.iter().any(|s| s["name"] == "TEST_API_KEY"));
    // Ensure no 'value' field is exposed
    assert!(secrets.iter().all(|s| s.get("value").is_none()));

    // Delete secret
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectSecretDeleteInput!) {
            projectSecretDelete(input: $input) { deletedId }
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
        r#"query($id: ProjectId!) { project(id: $id) { secrets { id } } }"#,
        serde_json::json!({ "id": project_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let secrets = data["project"]["secrets"].as_array().unwrap();
    assert!(!secrets.iter().any(|s| s["id"].as_str() == Some(&secret_id)));

    // Cleanup
    let _ = execute_graphql(
        &schema, &app, &sub,
        r#"mutation($input: ProjectDeleteInput!) { projectDelete(input: $input) { project { id } } }"#,
        serde_json::json!({ "input": { "id": project_id } }),
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

    // Create project
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: ProjectCreateInput!) {
            projectCreate(input: $input) { project { id } }
        }"#,
        serde_json::json!({ "input": { "name": format!("agent-test-project-{}", uuid::Uuid::new_v4()) } }),
    )
    .await;
    let project_id = assert_no_errors(&result)["projectCreate"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create agent
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: AgentCreateInput!) {
            agentCreate(input: $input) { agent { id name role projectId } }
        }"#,
        serde_json::json!({ "input": { "projectId": project_id, "name": "research" } }),
    )
    .await;
    let data = assert_no_errors(&result);
    let agent = &data["agentCreate"]["agent"];
    assert_eq!(agent["name"], "research");
    assert_eq!(agent["role"], "AGENT");
    assert_eq!(agent["projectId"], project_id);

    // Verify agent appears in project agents list
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($id: ProjectId!) { project(id: $id) { agents { id name role } } }"#,
        serde_json::json!({ "id": project_id }),
    )
    .await;
    let data = assert_no_errors(&result);
    let agents = data["project"]["agents"].as_array().unwrap();
    assert!(agents.iter().any(|a| a["name"] == "research"));
    // Lead should be first
    assert_eq!(agents[0]["role"], "PROJECT_LEAD");

    // Cleanup
    let _ = execute_graphql(
        &schema, &app, &sub,
        r#"mutation($input: ProjectDeleteInput!) { projectDelete(input: $input) { project { id } } }"#,
        serde_json::json!({ "input": { "id": project_id } }),
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

// ─── Workflow query / mutation tests ──────────────────────────────────────

#[tokio::test]
async fn workflow_definitions_query_exposes_yaml_and_structured_fields() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    let project_name = format!("workflow-query-project-{}", uuid::Uuid::new_v4());
    let project = app
        .projects()
        .create(&sub, project_name, None)
        .await
        .expect("create project");
    let skill = app
        .skills()
        .create(
            &sub,
            project.id,
            &project.name,
            "workflow-query-skill".to_string(),
            "GraphQL workflow test skill".to_string(),
            "Return a short test result.".to_string(),
        )
        .await
        .expect("create skill");
    let workflow = app
        .workflows()
        .create(
            &sub,
            project.id,
            &project.name,
            "graphql-workflow-query".to_string(),
            Some("GraphQL workflow query test".to_string()),
            drua_core::workflow::WorkflowTrigger::Manual { condition: None },
            vec![drua_core::workflow::WorkflowStepDef::AgentStep {
                name: "investigate".to_string(),
                skill: skill.name.clone(),
                sandbox: None,
                sandbox_mode: None,
                timeout_seconds: Some(60),
                model_chain: None,
                output_schema: Box::new(drua_core::workflow::default_output_schema()),
                condition: None,
            }],
            Vec::new(),
            None,
        )
        .await
        .expect("create workflow");

    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($projectId: ProjectId!) {
            workflowDefinitions(projectId: $projectId) {
                id
                name
                description
                yaml
                trigger { kind condition }
                steps { name stepType skill timeoutSeconds outputSchema }
                recentRuns(first: 5) { id }
            }
        }"#,
        serde_json::json!({ "projectId": project.id.to_string() }),
    )
    .await;
    let data = assert_no_errors(&result);
    let definitions = data["workflowDefinitions"].as_array().unwrap();
    let found = definitions
        .iter()
        .find(|d| d["id"] == workflow.id.to_string())
        .expect("workflow returned");
    assert_eq!(found["name"], "graphql-workflow-query");
    assert_eq!(found["description"], "GraphQL workflow query test");
    assert_eq!(found["trigger"]["kind"], "MANUAL");
    assert!(found["yaml"]
        .as_str()
        .unwrap()
        .contains("name: graphql-workflow-query"));
    assert_eq!(found["steps"][0]["name"], "investigate");
    assert_eq!(found["steps"][0]["stepType"], "AGENT_STEP");
    assert_eq!(found["steps"][0]["skill"], skill.name);
    assert!(found["steps"][0]["outputSchema"].is_object());
}

#[tokio::test]
async fn workflow_trigger_mutation_returns_run_or_filtered() {
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));
    let sub = test_sub();

    let project_name = format!("workflow-trigger-project-{}", uuid::Uuid::new_v4());
    let project = app
        .projects()
        .create(&sub, project_name, None)
        .await
        .expect("create project");
    let skill = app
        .skills()
        .create(
            &sub,
            project.id,
            &project.name,
            "workflow-trigger-skill".to_string(),
            "GraphQL workflow trigger skill".to_string(),
            "Return a short test result.".to_string(),
        )
        .await
        .expect("create skill");

    let steps = || {
        vec![drua_core::workflow::WorkflowStepDef::AgentStep {
            name: "act".to_string(),
            skill: skill.name.clone(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: Some(60),
            model_chain: None,
            output_schema: Box::new(drua_core::workflow::default_output_schema()),
            condition: None,
        }]
    };
    let runnable = app
        .workflows()
        .create(
            &sub,
            project.id,
            &project.name,
            "graphql-workflow-trigger".to_string(),
            None,
            drua_core::workflow::WorkflowTrigger::Manual { condition: None },
            steps(),
            Vec::new(),
            None,
        )
        .await
        .expect("create runnable workflow");
    let filtered = app
        .workflows()
        .create(
            &sub,
            project.id,
            &project.name,
            "graphql-workflow-filtered".to_string(),
            None,
            drua_core::workflow::WorkflowTrigger::Manual {
                condition: Some("false".to_string()),
            },
            steps(),
            Vec::new(),
            None,
        )
        .await
        .expect("create filtered workflow");

    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkflowTriggerInput!) {
            workflowTrigger(input: $input) {
                filtered
                run { id definitionId projectId state triggerContext }
            }
        }"#,
        serde_json::json!({
            "input": {
                "definitionId": runnable.id.to_string(),
                "payload": { "source": "graphql-test" }
            }
        }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(data["workflowTrigger"]["filtered"], false);
    assert_eq!(
        data["workflowTrigger"]["run"]["definitionId"],
        runnable.id.to_string()
    );
    assert_eq!(
        data["workflowTrigger"]["run"]["triggerContext"]["source"],
        "graphql-test"
    );

    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"mutation($input: WorkflowTriggerInput!) {
            workflowTrigger(input: $input) { filtered run { id } }
        }"#,
        serde_json::json!({ "input": { "definitionId": filtered.id.to_string() } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert_eq!(data["workflowTrigger"]["filtered"], true);
    assert!(data["workflowTrigger"]["run"].is_null());
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
            librarySearch(input: $input) { id title type projectId snippet score }
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

    // Project filter — non-existent project fails service-layer
    // find_by_id (this is a 404, not a silent empty result).
    let bogus_ws = "00000000-0000-0000-0000-000000000000";
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": { "query": "x", "projectId": bogus_ws, "limit": 10 } }),
    )
    .await;
    assert!(
        result.get("errors").is_some(),
        "non-existent project_id should error: {result:#}"
    );

    // Both filters combined — same non-existent project, same error.
    let result = execute_graphql(
        &schema,
        &app,
        &sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": {
            "query": "x",
            "types": ["SKILL"],
            "projectId": bogus_ws,
            "limit": 10
        }}),
    )
    .await;
    assert!(
        result.get("errors").is_some(),
        "non-existent project_id should error: {result:#}"
    );
}

#[tokio::test]
async fn library_search_project_scoped_agent_no_filter() {
    // Library content is openly discoverable: any non-anonymous subject
    // — including a project-scoped agent without the collection-level
    // Project(None) scope — can search globally and receive results
    // spanning every project plus global content.
    let pool = pool().await;
    let app = test_app(&pool).await;
    let schema = drua_server::graphql::schema(Some(app.clone()));

    let user = test_sub();
    let project = app
        .projects()
        .create(&user, "library-search-scope-test", None)
        .await
        .expect("create project");

    let agent_sub = drua_core::auth::AuthSubject::Agent(
        project.id,
        drua_core::primitives::AgentId::new(),
        vec![drua_core::auth::AuthScope::ProjectMember(project.id)],
    );

    let result = execute_graphql(
        &schema,
        &app,
        &agent_sub,
        r#"query($input: LibrarySearchInput!) {
            librarySearch(input: $input) { id }
        }"#,
        serde_json::json!({ "input": { "query": "anything", "limit": 10 } }),
    )
    .await;
    let data = assert_no_errors(&result);
    assert!(data["librarySearch"].is_array());
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
        "projectSecretCreate",
        "projectSecretDelete",
        "mcpCredentialsCreate",
        "mcpCredentialsRevoke",
        "workflowTrigger",
        "projectCreate",
        "projectUpdate",
        "projectDelete",
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
        "project",
        "projects",
        "librarySearch",
        "workflowDefinitions",
        "workflowDefinition",
        "workflowRuns",
        "workflowRun",
    ];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing query: {name}. Available: {fields:?}"
        );
    }
}

#[tokio::test]
async fn schema_project_has_new_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "Project") { fields { name } } }"#,
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
            "Missing project field: {name}. Available: {fields:?}"
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
        "projectId",
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
async fn schema_workflow_types_have_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"
            {
                definition: __type(name: "WorkflowDefinition") { fields { name } }
                run: __type(name: "WorkflowRun") { fields { name } }
                triggerInput: __type(name: "WorkflowTriggerInput") { inputFields { name } }
            }
            "#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let definition_fields: Vec<String> = json["data"]["definition"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    for name in [
        "id",
        "projectId",
        "name",
        "description",
        "trigger",
        "steps",
        "sandboxes",
        "modelChain",
        "createdAt",
        "updatedAt",
        "recentRuns",
        "yaml",
    ] {
        assert!(
            definition_fields.contains(&name.to_string()),
            "Missing workflow definition field: {name}. Available: {definition_fields:?}"
        );
    }

    let run_fields: Vec<String> = json["data"]["run"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    for name in [
        "id",
        "definitionId",
        "projectId",
        "state",
        "startedAt",
        "completedAt",
        "triggerContext",
        "stepResults",
        "stepsSnapshot",
        "agents",
    ] {
        assert!(
            run_fields.contains(&name.to_string()),
            "Missing workflow run field: {name}. Available: {run_fields:?}"
        );
    }

    let trigger_input_fields: Vec<String> = json["data"]["triggerInput"]["inputFields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    for name in ["definitionId", "payload"] {
        assert!(
            trigger_input_fields.contains(&name.to_string()),
            "Missing workflow trigger input field: {name}. Available: {trigger_input_fields:?}"
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
        "projectId",
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
async fn schema_project_secret_type_has_expected_fields() {
    let schema = drua_server::graphql::schema(None);
    let response = schema
        .execute(async_graphql::Request::new(
            r#"{ __type(name: "ProjectSecret") { fields { name } } }"#,
        ))
        .await;
    let json = serde_json::to_value(&response).unwrap();
    let fields: Vec<String> = json["data"]["__type"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    let expected = ["id", "projectId", "name", "secretType", "createdAt"];
    for name in expected {
        assert!(
            fields.contains(&name.to_string()),
            "Missing project secret field: {name}. Available: {fields:?}"
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
