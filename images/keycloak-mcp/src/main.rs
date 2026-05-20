use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{routing::get, Router};
use clap::Parser;
use reqwest::StatusCode;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::instrument;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(about = "Read-only Keycloak Admin REST MCP server.")]
struct Cli {
    /// Base URL for Keycloak, without a trailing slash.
    #[arg(long, env = "KEYCLOAK_BASE_URL")]
    keycloak_base_url: String,

    /// Realm used for client_credentials token acquisition.
    #[arg(long, env = "KEYCLOAK_TOKEN_REALM")]
    token_realm: Option<String>,

    /// Confidential client id used for Admin REST access.
    #[arg(long, env = "KEYCLOAK_CLIENT_ID")]
    client_id: String,

    /// Confidential client secret used for Admin REST access.
    #[arg(long, env = "KEYCLOAK_CLIENT_SECRET")]
    client_secret: String,

    /// Comma-separated realms included in snapshot_get by default.
    #[arg(long, env = "KEYCLOAK_REALMS", default_value = "internal,customer")]
    realms: String,

    /// Optional normalized declared snapshot JSON used by diff_declared.
    #[arg(long, env = "KEYCLOAK_DECLARED_SNAPSHOT_FILE")]
    declared_snapshot_file: Option<PathBuf>,

    /// Address to bind the HTTP server to.
    #[arg(long, env = "KEYCLOAK_MCP_BIND", default_value = "0.0.0.0:8000")]
    bind: SocketAddr,

    /// HTTP path to mount the MCP service at.
    #[arg(long, env = "KEYCLOAK_MCP_MOUNT", default_value = "/mcp")]
    mount: String,
}

#[derive(Clone)]
struct KeycloakMcp {
    client: Arc<KeycloakClient>,
    declared_snapshot: Option<Arc<Value>>,
}

#[derive(Clone)]
struct KeycloakClient {
    http: reqwest::Client,
    base_url: String,
    token_realm: Option<String>,
    client_id: String,
    client_secret: String,
    default_realms: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl KeycloakClient {
    fn new(
        base_url: String,
        token_realm: Option<String>,
        client_id: String,
        client_secret: String,
        default_realms: Vec<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token_realm,
            client_id,
            client_secret,
            default_realms,
        }
    }

    async fn token(&self, realm: &str) -> Result<String> {
        let token_realm = self.token_realm.as_deref().unwrap_or(realm);
        let token_url = format!(
            "{}/realms/{}/protocol/openid-connect/token",
            self.base_url, token_realm
        );
        let response = self
            .http
            .post(token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .context("requesting Keycloak access token")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Keycloak token request failed with {status}: {body}");
        }

        let token = response
            .json::<TokenResponse>()
            .await
            .context("decoding Keycloak token response")?;
        Ok(token.access_token)
    }

    async fn admin_get(&self, token: &str, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/admin/{}", self.base_url, path.trim_start_matches('/'));
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .query(query)
            .send()
            .await
            .with_context(|| format!("requesting Keycloak admin path {path}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Keycloak admin GET {path} failed with {status}: {body}");
        }

        let mut value = response
            .json::<Value>()
            .await
            .with_context(|| format!("decoding Keycloak admin path {path}"))?;
        redact_value(&mut value);
        Ok(canonicalize(value))
    }

    async fn realm_snapshot(&self, token: &str, realm: &str) -> Result<Value> {
        let realm_info = self
            .admin_get(token, &format!("realms/{realm}"), &[])
            .await?;
        let clients = self
            .admin_get(
                token,
                &format!("realms/{realm}/clients"),
                &[("briefRepresentation", "false".to_string())],
            )
            .await?;
        let client_scopes = self
            .admin_get(token, &format!("realms/{realm}/client-scopes"), &[])
            .await?;
        let roles = self
            .admin_get(token, &format!("realms/{realm}/roles"), &[])
            .await?;
        let auth_flows = self
            .admin_get(token, &format!("realms/{realm}/authentication/flows"), &[])
            .await?;
        let required_actions = self
            .admin_get(
                token,
                &format!("realms/{realm}/authentication/required-actions"),
                &[],
            )
            .await?;
        let user_profile = self
            .admin_get(token, &format!("realms/{realm}/users/profile"), &[])
            .await?;

        Ok(canonicalize(json!({
            "realm": realm_info,
            "clients": clients,
            "clientScopes": client_scopes,
            "roles": roles,
            "authenticationFlows": auth_flows,
            "requiredActions": required_actions,
            "userProfile": user_profile,
        })))
    }
}

impl KeycloakMcp {
    fn new(client: KeycloakClient, declared_snapshot_file: Option<PathBuf>) -> Result<Self> {
        let declared_snapshot = if let Some(path) = declared_snapshot_file {
            let body = std::fs::read_to_string(&path)
                .with_context(|| format!("reading declared snapshot {}", path.display()))?;
            let mut value = serde_json::from_str::<Value>(&body)
                .with_context(|| format!("parsing declared snapshot {}", path.display()))?;
            redact_value(&mut value);
            Some(Arc::new(canonicalize(value)))
        } else {
            None
        };

        Ok(Self {
            client: Arc::new(client),
            declared_snapshot,
        })
    }

    fn into_service(self) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        config.stateful_mode = false;
        config.json_response = true;
        StreamableHttpService::new(
            move || Ok(self.clone()),
            LocalSessionManager::default().into(),
            config,
        )
    }
}

impl ServerHandler for KeycloakMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Read-only Keycloak Admin REST tools for deployment state inspection.",
        )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![
                tool(
                    "realm_list",
                    "Return configured Keycloak realms and read-access status.",
                    empty_object_schema(),
                ),
                tool(
                    "realm_get",
                    "Return a normalized read-only realm snapshot, including nested config relevant to drift checks.",
                    realm_schema(),
                ),
                tool(
                    "realm_diff_declared",
                    "Compare one live Keycloak realm against the configured declared snapshot.",
                    realm_schema(),
                ),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    #[instrument(name = "keycloak_mcp.call_tool", skip(self, _ctx), fields(tool = %request.name))]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let result = match request.name.as_ref() {
            "realm_list" => self.realm_list().await,
            "realm_get" => self.realm_get(&args).await,
            "realm_diff_declared" => self.realm_diff_declared(&args).await,
            other => Err(anyhow::anyhow!("unknown tool: {other}")),
        };

        match result {
            Ok(value) => Ok(json_result(value)),
            Err(error) => Ok(error_result(error.to_string())),
        }
    }
}

impl KeycloakMcp {
    async fn realm_list(&self) -> Result<Value> {
        let mut realms = Vec::new();

        for realm in &self.client.default_realms {
            let status = match self.client.token(realm).await {
                Ok(token) => match self
                    .client
                    .admin_get(&token, &format!("realms/{realm}"), &[])
                    .await
                {
                    Ok(info) => json!({
                        "realm": realm,
                        "accessible": true,
                        "enabled": info.get("enabled").cloned().unwrap_or(Value::Null),
                        "displayName": info.get("displayName").cloned().unwrap_or(Value::Null),
                    }),
                    Err(error) => json!({
                        "realm": realm,
                        "accessible": false,
                        "error": error.to_string(),
                    }),
                },
                Err(error) => json!({
                    "realm": realm,
                    "accessible": false,
                    "error": error.to_string(),
                }),
            };
            realms.push(status);
        }

        Ok(json!({
            "source": "configured",
            "realms": realms,
        }))
    }

    async fn realm_get(&self, args: &Map<String, Value>) -> Result<Value> {
        let realm = required_string(args, "realm")?;
        let token = self.client.token(&realm).await?;
        self.client.realm_snapshot(&token, &realm).await
    }

    async fn realm_diff_declared(&self, args: &Map<String, Value>) -> Result<Value> {
        let realm = required_string(args, "realm")?;
        let declared = self
            .declared_snapshot
            .as_ref()
            .context("KEYCLOAK_DECLARED_SNAPSHOT_FILE is not configured")?;
        let declared_realm = declared
            .get("realms")
            .and_then(|realms| realms.get(&realm))
            .with_context(|| format!("declared snapshot does not contain realm {realm}"))?;
        let live = self.realm_get(args).await?;
        Ok(diff_values(declared_realm, &live, 100))
    }
}

fn json_result(value: Value) -> CallToolResult {
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::success(vec![Content::text(pretty)]);
    result.structured_content = Some(value);
    result
}

fn error_result(message: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(message)]);
    result.is_error = Some(true);
    result
}

fn tool(name: &str, description: &str, schema: Map<String, Value>) -> Tool {
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(schema);
    tool
}

fn realm_schema() -> Map<String, Value> {
    object_schema(
        json!({
            "realm": {
                "type": "string",
                "description": "Keycloak realm name."
            }
        }),
        &["realm"],
    )
}

fn empty_object_schema() -> Map<String, Value> {
    object_schema(json!({}), &[])
}

fn object_schema(properties: Value, required: &[&str]) -> Map<String, Value> {
    let mut schema = Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), properties);
    schema.insert(
        "required".into(),
        Value::Array(
            required
                .iter()
                .map(|field| Value::String((*field).to_string()))
                .collect(),
        ),
    );
    schema.insert("additionalProperties".into(), Value::Bool(false));
    schema
}

fn required_string(args: &Map<String, Value>, name: &str) -> Result<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .with_context(|| format!("missing required string argument {name}"))
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if should_redact(&key) {
                    map.insert(key, Value::String("<redacted>".to_string()));
                    continue;
                }
                if should_drop_generated(&key) {
                    map.remove(&key);
                    continue;
                }
                if let Some(value) = map.get_mut(&key) {
                    redact_value(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value);
            }
        }
        _ => {}
    }
}

fn should_redact(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    normalized.contains("secret")
        || normalized.contains("password")
        || normalized == "token"
        || normalized.ends_with("token")
        || normalized == "credentials"
}

fn should_drop_generated(key: &str) -> bool {
    matches!(
        key,
        "id" | "containerId" | "createdTimestamp" | "access" | "adminUrl"
    )
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            let mut values: Vec<Value> = values.into_iter().map(canonicalize).collect();
            values.sort_by_key(stable_sort_key);
            Value::Array(values)
        }
        Value::Object(map) => {
            let mapped = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect();
            Value::Object(mapped)
        }
        other => other,
    }
}

fn stable_sort_key(value: &Value) -> String {
    if let Value::Object(map) = value {
        for key in [
            "realm",
            "clientId",
            "name",
            "alias",
            "username",
            "protocol",
            "providerId",
        ] {
            if let Some(value) = map.get(key).and_then(Value::as_str) {
                return format!("{key}:{value}");
            }
        }
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn diff_values(expected: &Value, actual: &Value, max_differences: usize) -> Value {
    let mut differences = Vec::new();
    collect_diff("$", expected, actual, max_differences, &mut differences);
    json!({
        "equal": differences.is_empty(),
        "differenceCount": differences.len(),
        "truncated": differences.len() >= max_differences,
        "differences": differences,
    })
}

fn collect_diff(
    path: &str,
    expected: &Value,
    actual: &Value,
    max_differences: usize,
    differences: &mut Vec<Value>,
) {
    if differences.len() >= max_differences || expected == actual {
        return;
    }

    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for key in expected.keys() {
                if differences.len() >= max_differences {
                    return;
                }
                let child_path = format!("{path}.{key}");
                match actual.get(key) {
                    Some(actual_value) => {
                        collect_diff(
                            &child_path,
                            &expected[key],
                            actual_value,
                            max_differences,
                            differences,
                        );
                    }
                    None => differences.push(json!({
                        "path": child_path,
                        "kind": "removed",
                        "expected": expected[key],
                        "actual": Value::Null,
                    })),
                }
            }

            for key in actual.keys() {
                if differences.len() >= max_differences {
                    return;
                }
                if !expected.contains_key(key) {
                    differences.push(json!({
                        "path": format!("{path}.{key}"),
                        "kind": "added",
                        "expected": Value::Null,
                        "actual": actual[key],
                    }));
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            let len = expected.len().max(actual.len());
            for index in 0..len {
                if differences.len() >= max_differences {
                    return;
                }
                let child_path = format!("{path}[{index}]");
                match (expected.get(index), actual.get(index)) {
                    (Some(expected_value), Some(actual_value)) => collect_diff(
                        &child_path,
                        expected_value,
                        actual_value,
                        max_differences,
                        differences,
                    ),
                    (Some(expected_value), None) => differences.push(json!({
                        "path": child_path,
                        "kind": "removed",
                        "expected": expected_value,
                        "actual": Value::Null,
                    })),
                    (None, Some(actual_value)) => differences.push(json!({
                        "path": child_path,
                        "kind": "added",
                        "expected": Value::Null,
                        "actual": actual_value,
                    })),
                    (None, None) => {}
                }
            }
        }
        _ => differences.push(json!({
            "path": path,
            "kind": "changed",
            "expected": expected,
            "actual": actual,
        })),
    }
}

fn parse_realms(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|realm| !realm.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_realms_trims_empty_entries() {
        assert_eq!(
            parse_realms(" internal, customer, ,"),
            vec!["internal".to_string(), "customer".to_string()]
        );
    }

    #[test]
    fn canonicalize_redacts_secrets_and_sorts_named_arrays() {
        let mut value = json!([
            {
                "id": "generated-2",
                "clientId": "z-client",
                "secret": "sensitive"
            },
            {
                "id": "generated-1",
                "clientId": "a-client",
                "clientSecret": "sensitive"
            }
        ]);

        redact_value(&mut value);
        let value = canonicalize(value);

        assert_eq!(value[0]["clientId"], "a-client");
        assert_eq!(value[0]["clientSecret"], "<redacted>");
        assert!(value[0].get("id").is_none());
        assert_eq!(value[1]["clientId"], "z-client");
        assert_eq!(value[1]["secret"], "<redacted>");
        assert!(value[1].get("id").is_none());
    }

    #[test]
    fn diff_values_reports_added_and_changed_paths() {
        let expected = json!({
            "realms": {
                "internal": {
                    "clients": [
                        { "clientId": "admin-panel", "enabled": true }
                    ]
                }
            }
        });
        let actual = json!({
            "realms": {
                "internal": {
                    "clients": [
                        { "clientId": "admin-panel", "enabled": false },
                        { "clientId": "extra-client", "enabled": true }
                    ]
                }
            }
        });

        let diff = diff_values(&expected, &actual, 100);

        assert_eq!(diff["equal"], false);
        assert_eq!(diff["differenceCount"], 2);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let default_realms = parse_realms(&cli.realms);
    if default_realms.is_empty() {
        anyhow::bail!("KEYCLOAK_REALMS must include at least one realm");
    }

    let client = KeycloakClient::new(
        cli.keycloak_base_url,
        cli.token_realm,
        cli.client_id,
        cli.client_secret,
        default_realms,
    );
    let mcp = KeycloakMcp::new(client, cli.declared_snapshot_file)?;
    let service = mcp.into_service();

    let app = Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .nest_service(&cli.mount, service);

    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!("keycloak-mcp listening on http://{local_addr}{}", cli.mount);
    axum::serve(listener, app).await?;
    Ok(())
}
